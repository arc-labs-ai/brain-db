//! Statement physical-reclamation (GC) worker.
//!
//! Periodic low-priority worker that hard-deletes retracted statement
//! rows — and every secondary-index + evidence-overflow entry they own
//! — once the retract grace period has elapsed. This closes the
//! tombstone-grace-then-reclaim loop on the statement side: memories
//! reclaim via slot reclamation, entities via entity GC, and statements
//! here.
//!
//! **Off by default** (`enabled == false`). Retracted rows stay in redb
//! (invisible to retrieval — the lexical index drop and tombstone filter
//! already hide them) until an operator opts the worker in via
//! `BRAIN_STATEMENT_RECLAIM_ENABLED`. The grace window and cadence are
//! tunable through the env knobs below.
//!
//! Only rows carrying the durable `TombstoneReason::Retract` marker are
//! eligible — plain tombstones (kept for audit) and superseded rows
//! (kept forever for chain history) are never touched. See
//! [`brain_metadata::extractor::sweep::reclaim_retracted_statements`]
//! for the table-by-table delete and the dense-chain invariant the
//! reclaim honours.
//!
//! No WAL record: like the supersession sweeper, the redb commit is the
//! durability point. Reclaim is idempotent re-derivation — a row gone
//! from `STATEMENTS_TABLE` after the grace window stays gone, and a
//! crash mid-sweep simply re-runs the bounded scan next tick.

use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;

use brain_metadata::extractor::sweep::{reclaim_retracted_statements, SweepSummary};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Operator opt-in flag. The worker stays a registered no-op unless this
/// env var is a truthy value (`1` / `true` / `yes`, case-insensitive).
pub const ENABLED_ENV: &str = "BRAIN_STATEMENT_RECLAIM_ENABLED";

/// Operator override for the reclaim grace window (seconds). Falls back
/// to [`DEFAULT_GRACE_SECONDS`] when unset, empty, non-numeric, or zero.
pub const GRACE_SECONDS_ENV: &str = "BRAIN_STATEMENT_RECLAIM_GRACE_SECONDS";

/// Operator override for the sweep cadence (seconds). Falls back to the
/// `WorkerConfig::defaults_for` cadence when unset, empty, or zero.
pub const PERIOD_SECONDS_ENV: &str = "BRAIN_STATEMENT_RECLAIM_PERIOD_SECONDS";

/// 30 days. Matches `RETRACT_GRACE_NANOS` (the retract handler's
/// `will_zero_at` promise) and the §19.12 retention-table default.
pub const DEFAULT_GRACE_SECONDS: u64 = 30 * 24 * 60 * 60;

/// 1 day default cadence. Mirrors the `WorkerKind::StatementReclaim`
/// default in `WorkerConfig`.
pub const DEFAULT_PERIOD_SECONDS: u64 = 86_400;

fn resolved_grace_nanos() -> u64 {
    let secs = crate::env::parse_positive_seconds(std::env::var(GRACE_SECONDS_ENV).ok().as_deref())
        .unwrap_or(DEFAULT_GRACE_SECONDS);
    secs.saturating_mul(1_000_000_000)
}

fn resolved_period() -> std::time::Duration {
    let secs = crate::env::parse_positive_seconds(std::env::var(PERIOD_SECONDS_ENV).ok().as_deref())
        .unwrap_or(DEFAULT_PERIOD_SECONDS);
    std::time::Duration::from_secs(secs)
}

pub struct StatementReclaimWorker {
    config: WorkerConfig,
    enabled: bool,
    grace_nanos: u64,
}

impl StatementReclaimWorker {
    /// New worker — **disabled** by default, resolving the grace and
    /// cadence knobs from the environment.
    #[must_use]
    pub fn new() -> Self {
        let mut config = WorkerConfig::defaults_for(WorkerKind::StatementReclaim);
        config.interval = resolved_period();
        let enabled = crate::env::parse_enabled(std::env::var(ENABLED_ENV).ok().as_deref());
        config.enabled = enabled;
        Self {
            config,
            enabled,
            grace_nanos: resolved_grace_nanos(),
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn enabled(mut self) -> Self {
        self.enabled = true;
        self.config.enabled = true;
        self
    }

    #[must_use]
    pub fn with_grace_seconds(mut self, seconds: u64) -> Self {
        self.grace_nanos = seconds.saturating_mul(1_000_000_000);
        self
    }

    async fn reclaim_once(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        if !self.enabled || self.grace_nanos == 0 {
            return Ok(0);
        }
        let now_ns = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0);

        let metadata = ctx.ops.executor.metadata.as_ref();
        let wtxn = metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("statement reclaim wtxn: {e}")))?;
        // One bounded reclaim per cycle. A per-row failure inside the
        // sweeper surfaces as an Err here; we warn and continue (return
        // 0 for this tick) rather than poison the scheduler — the next
        // tick re-scans the same rows.
        let summary: SweepSummary = match reclaim_retracted_statements(
            &wtxn,
            self.grace_nanos,
            now_ns,
            self.config.batch_size,
            false,
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "brain_workers::statement_reclaim",
                    error = %e,
                    "statement reclaim failed; retrying next tick",
                );
                return Ok(0);
            }
        };
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("statement reclaim commit: {e}")))?;

        if summary.deleted > 0 {
            tracing::info!(
                target: "brain_workers::statement_reclaim",
                scanned = summary.scanned,
                deleted = summary.deleted,
                skipped = summary.skipped,
                "reclaimed retracted statements",
            );
        } else {
            tracing::debug!(
                target: "brain_workers::statement_reclaim",
                scanned = summary.scanned,
                "statement reclaim tick (nothing past grace)",
            );
        }
        Ok(summary.deleted as usize)
    }
}

impl Default for StatementReclaimWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for StatementReclaimWorker {
    fn name(&self) -> &'static str {
        WorkerKind::StatementReclaim.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::StatementReclaim
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(self.reclaim_once(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default() {
        // Construct directly (bypassing env) to assert the default.
        let w = StatementReclaimWorker {
            config: WorkerConfig::defaults_for(WorkerKind::StatementReclaim),
            enabled: false,
            grace_nanos: DEFAULT_GRACE_SECONDS * 1_000_000_000,
        };
        assert!(!w.enabled);
        assert!(!w.config.enabled);
    }

    #[test]
    fn enabled_builder_flips_both_flags() {
        let w = StatementReclaimWorker {
            config: WorkerConfig::defaults_for(WorkerKind::StatementReclaim),
            enabled: false,
            grace_nanos: 1,
        }
        .enabled();
        assert!(w.enabled);
        assert!(w.config.enabled);
    }

    #[test]
    fn worker_kind_name() {
        let w = StatementReclaimWorker {
            config: WorkerConfig::defaults_for(WorkerKind::StatementReclaim),
            enabled: false,
            grace_nanos: 0,
        };
        assert_eq!(w.name(), "statement_reclaim");
        assert_eq!(w.kind(), WorkerKind::StatementReclaim);
    }

    // env enable/seconds parsing is tested once in crate::env.

    #[test]
    fn default_grace_is_thirty_days() {
        assert_eq!(DEFAULT_GRACE_SECONDS, 30 * 24 * 60 * 60);
    }
}
