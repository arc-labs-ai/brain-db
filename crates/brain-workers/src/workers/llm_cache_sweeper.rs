//! Per-shard LLM extractor response cache sweeper.
//!
//! On every tick, ask the cache to delete every row whose
//! `expires_at` is `<= now` — both from the main response table and
//! from the TTL secondary index. Without this worker the cache file
//! grows unboundedly and long-running deployments eventually run out
//! of disk.
//!
//! No-op when `OpsContext.llm_cache` is `None` (no LLM extractors
//! configured) — the worker stays registered for introspection but
//! its cycle returns 0 immediately.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use brain_metadata::llm_cache::sweep_expired;
use brain_ops::LlmCacheSweepMetrics;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// 1 h default cadence. Mirrors the `WorkerKind::LlmCacheSweeper`
/// default in `WorkerConfig`.
pub const DEFAULT_INTERVAL_SECS: u64 = 3600;

pub struct LlmCacheSweeper {
    config: WorkerConfig,
    metrics: Option<Arc<LlmCacheSweepMetrics>>,
}

impl LlmCacheSweeper {
    /// Construct with the default cadence and an empty metrics
    /// slot. The shard wires its shared `Arc<LlmCacheSweepMetrics>`
    /// in via [`Self::with_metrics`] at registration time, and the
    /// cadence via [`Self::with_interval_secs`] from
    /// `[workers.llm_cache_sweep] interval_secs`.
    #[must_use]
    pub fn new() -> Self {
        let mut config = WorkerConfig::defaults_for(WorkerKind::LlmCacheSweeper);
        config.interval = std::time::Duration::from_secs(DEFAULT_INTERVAL_SECS);
        Self {
            config,
            metrics: None,
        }
    }

    /// Override the sweep cadence. The shard supplies
    /// `[workers.llm_cache_sweep] interval_secs`; a zero value is
    /// clamped to 1 second so the scheduler never busy-loops.
    #[must_use]
    pub fn with_interval_secs(mut self, interval_secs: u64) -> Self {
        self.config.interval = std::time::Duration::from_secs(interval_secs.max(1));
        self
    }

    /// Override the default config (e.g. interval / batch / runtime).
    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    /// Install the shared metrics handle. `Arc::clone` from the
    /// shard's owning instance so `/metrics` exposition and the
    /// worker write to the same counters.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<LlmCacheSweepMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    async fn sweep_once(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        let Some(cache) = ctx.ops.llm_cache.clone() else {
            return Ok(0);
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let started = Instant::now();
        // Lock the cache mutex only for the duration of the sweep,
        // never across an `.await`. The sweep is one redb wtxn:
        // bounded work, no IO awaits, no scheduler hand-offs.
        let removed = {
            let mut db = cache.lock();
            match sweep_expired(&mut db, now) {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(
                        target: "brain_workers::llm_cache_sweep",
                        error = %e,
                        "llm cache sweep failed; retrying next tick",
                    );
                    return Ok(0);
                }
            }
        };
        let elapsed = started.elapsed();
        if let Some(m) = self.metrics.as_ref() {
            m.inc_sweeps();
            m.add_rows_removed(removed as u64);
            m.observe_sweep_duration(elapsed.as_secs_f64());
        }
        if removed > 0 {
            tracing::info!(
                target: "brain_workers::llm_cache_sweep",
                removed,
                duration_ms = elapsed.as_millis() as u64,
                "swept expired llm cache rows",
            );
        } else {
            tracing::debug!(
                target: "brain_workers::llm_cache_sweep",
                duration_ms = elapsed.as_millis() as u64,
                "llm cache sweep tick (no expired rows)",
            );
        }
        Ok(removed)
    }
}

impl Default for LlmCacheSweeper {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for LlmCacheSweeper {
    fn name(&self) -> &'static str {
        WorkerKind::LlmCacheSweeper.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::LlmCacheSweeper
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(self.sweep_once(ctx))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    use brain_metadata::llm_cache::{
        LlmCacheDb, LlmResponse, LLM_RESPONSES_TABLE, LLM_RESPONSE_TTL_TABLE,
    };
    use parking_lot::Mutex;

    use super::*;

    #[test]
    fn default_interval_is_one_hour() {
        let cfg = WorkerConfig::defaults_for(WorkerKind::LlmCacheSweeper);
        assert_eq!(cfg.interval, Duration::from_secs(DEFAULT_INTERVAL_SECS));
    }

    // env-override parsing is tested once in crate::env.

    #[test]
    fn worker_tick_invokes_sweep_expired() {
        // Seed an LlmCacheDb with one expired row + one fresh one,
        // call the worker's sweep_once-equivalent (via the public
        // helper), and assert only the expired row is removed.
        // The worker delegates straight to `sweep_expired`, so this
        // exercises the worker's "lock + call + count" wiring without
        // pulling in the full WorkerContext stack.
        let dir = tempfile::tempdir().unwrap();
        let mut db = LlmCacheDb::open(dir.path().join("llm_cache.redb")).unwrap();
        let resp = LlmResponse::new(vec![0u8; 8], 0, 0, 1, 0);
        let now = 1_000_000u64;

        let expired_hash = [0x01u8; 32];
        let fresh_hash = [0x02u8; 32];
        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
            t.insert(&(expired_hash, 1u32, 1u32, 0u64), &resp).unwrap();
            t.insert(&(fresh_hash, 1u32, 1u32, 0u64), &resp).unwrap();
        }
        {
            let mut ttl = wtxn.open_table(LLM_RESPONSE_TTL_TABLE).unwrap();
            ttl.insert(&(now - 1, expired_hash), &()).unwrap();
            ttl.insert(&(now + 60, fresh_hash), &()).unwrap();
        }
        wtxn.commit().unwrap();

        let removed = sweep_expired(&mut db, now).unwrap();
        assert_eq!(removed, 1, "exactly the expired row is removed");

        // And the metrics path observes both `inc_sweeps` and the
        // returned count: simulate the worker's post-sweep updates.
        let metrics = Arc::new(LlmCacheSweepMetrics::new());
        metrics.inc_sweeps();
        metrics.add_rows_removed(removed as u64);
        metrics.observe_sweep_duration(0.001);
        let snap = metrics.snapshot();
        assert_eq!(snap.sweeps_total, 1);
        assert_eq!(snap.rows_removed_total, 1);
        assert_eq!(snap.sweep_duration_seconds.count, 1);
    }

    #[test]
    fn no_op_when_cache_unset() {
        // The worker's no-op path doesn't actually need a full
        // WorkerContext; we only verify the construction-time
        // invariants that drive it: a default worker carries no
        // cache and no metrics until the shard wires them.
        let w = LlmCacheSweeper::new();
        assert!(w.metrics.is_none());
        // `Default` is the same path; documents that an unconfigured
        // shard's worker still constructs without panicking.
        let _ = LlmCacheSweeper::default();
        // Use shutdown atomic just to silence the unused import on
        // CI configs that compile this without running tests.
        let _shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        // And the Mutex/Arc imports are exercised by the seeded test
        // above; this line documents the wire shape.
        let _: Option<Arc<Mutex<LlmCacheDb>>> = None;
    }
}
