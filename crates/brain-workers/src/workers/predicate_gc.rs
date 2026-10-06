//! Orphaned-predicate reclamation (GC) worker.
//!
//! Brain's predicate vocabulary is open: a name the corpus uses but no schema
//! declares is interned on demand (`SchemaOrigin::ImplicitFromWrite`). Nothing
//! removed those rows, while the statements that justified them do get
//! removed — `statement_reclaim` hard-deletes retracted and retention-expired
//! rows, FORGET cascades delete more. The coined predicate then outlives its
//! last user, keeping a predicate row, a qname-index entry, possibly an
//! embedding, and an entry on the review queue naming vocabulary the corpus
//! no longer contains. Memories reclaim via slot reclamation, entities via
//! entity GC, statements via statement reclaim, and coined predicates here.
//!
//! A schema-declared predicate is never a candidate, however unused: the
//! schema is a statement of intent, and removing it belongs to SCHEMA_DROP.
//!
//! **Off by default** (`enabled == false`), like the other reclaimers. The
//! review queue is a shortlist of names worth promoting into a real schema,
//! so an operator may well want coined vocabulary to accumulate; opting in
//! says they would rather it be reclaimed once nothing references it.
//!
//! No WAL record: the redb commit is the durability point. Reclaim is
//! idempotent re-derivation — a predicate gone from `PREDICATES_TABLE` stays
//! gone, and a crash mid-sweep simply re-runs the bounded scan next tick.

use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;

use brain_metadata::schema::predicate::{predicate_reclaim_orphans, PredicateReclaimSummary};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// 7 days. A predicate is interned in the same write as the statement that
/// coined it, so this only has to cover the window where a name exists but
/// its statement does not yet — a crash between the two, or a row tombstoned
/// straight away. Shorter than the statement grace (30 days) because nothing
/// here is recoverable by an operator changing their mind: a reclaimed name
/// is re-coined the next time the corpus uses it.
pub const DEFAULT_GRACE_SECONDS: u64 = 7 * 24 * 60 * 60;

/// 1 day default cadence — vocabulary drifts slowly.
pub const DEFAULT_PERIOD_SECONDS: u64 = 86_400;

pub struct PredicateGcWorker {
    config: WorkerConfig,
    enabled: bool,
    grace_nanos: u64,
    /// Report what would be reclaimed, delete nothing. Lets an operator see
    /// the shape of their coined vocabulary before trusting a GC with it.
    dry_run: bool,
}

impl PredicateGcWorker {
    /// New worker — **disabled** by default, with the default grace and
    /// cadence. The shard opts it in from `[workers.predicate_gc]`.
    #[must_use]
    pub fn new() -> Self {
        let mut config = WorkerConfig::defaults_for(WorkerKind::PredicateGc);
        config.interval = std::time::Duration::from_secs(DEFAULT_PERIOD_SECONDS);
        config.enabled = false;
        Self {
            config,
            enabled: false,
            grace_nanos: DEFAULT_GRACE_SECONDS.saturating_mul(1_000_000_000),
            dry_run: false,
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    /// Set the on/off state explicitly. The shard wires
    /// `[workers.predicate_gc] enabled` here.
    #[must_use]
    pub fn set_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self.config.enabled = enabled;
        self
    }

    /// Report-only mode. The shard wires `[workers.predicate_gc] dry_run`.
    #[must_use]
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    #[must_use]
    pub fn with_grace_seconds(mut self, seconds: u64) -> Self {
        self.grace_nanos = seconds.saturating_mul(1_000_000_000);
        self
    }

    /// Override the sweep cadence; a zero value is clamped to 1 second so
    /// the scheduler never busy-loops.
    #[must_use]
    pub fn with_period_seconds(mut self, seconds: u64) -> Self {
        self.config.interval = std::time::Duration::from_secs(seconds.max(1));
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
            .map_err(|e| WorkerError::Internal(format!("predicate gc wtxn: {e}")))?;

        // One bounded pass per cycle. A failure warns and retries next tick
        // rather than poisoning the scheduler — the txn drops unwritten.
        let summary: PredicateReclaimSummary = match predicate_reclaim_orphans(
            &wtxn,
            now_ns,
            self.grace_nanos,
            self.config.batch_size,
            self.dry_run,
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "brain_workers::predicate_gc",
                    error = %e,
                    "predicate gc failed; retrying next tick",
                );
                return Ok(0);
            }
        };
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("predicate gc commit: {e}")))?;

        if summary.reclaimed > 0 {
            tracing::info!(
                target: "brain_workers::predicate_gc",
                examined = summary.examined,
                reclaimed = summary.reclaimed,
                still_referenced = summary.still_referenced,
                dry_run = self.dry_run,
                "predicate gc: coined predicates no live statement used",
            );
        } else {
            tracing::debug!(
                target: "brain_workers::predicate_gc",
                examined = summary.examined,
                still_referenced = summary.still_referenced,
                "predicate gc tick (nothing orphaned)",
            );
        }
        Ok(summary.reclaimed)
    }
}

impl Default for PredicateGcWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for PredicateGcWorker {
    fn name(&self) -> &'static str {
        WorkerKind::PredicateGc.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::PredicateGc
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

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    /// Off by default: an operator may want coined vocabulary to accumulate
    /// on the review queue for promotion into a schema.
    #[test]
    fn the_worker_is_disabled_until_opted_in() {
        let w = PredicateGcWorker::new();
        assert!(!w.enabled);
        assert!(!w.config.enabled);
        let w = w.set_enabled(true);
        assert!(w.enabled);
        assert!(w.config.enabled);
    }

    #[test]
    fn a_zero_period_is_clamped_so_the_scheduler_never_busy_loops() {
        let w = PredicateGcWorker::new().with_period_seconds(0);
        assert_eq!(w.config.interval, std::time::Duration::from_secs(1));
    }

    #[test]
    fn grace_converts_to_nanos() {
        let w = PredicateGcWorker::new().with_grace_seconds(2);
        assert_eq!(w.grace_nanos, 2_000_000_000);
    }
}
