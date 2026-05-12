//! Idempotency cleanup worker (sub-task 8.6). Spec §11/05.
//!
//! Sweeps the idempotency table on a 1 h cadence (configurable),
//! removing entries whose `created_at + ttl` is past. Calls
//! `prune_expired_bounded` in a loop bounded by `max_runtime` and
//! `batch_size` so a single cycle can never block the writer for
//! too long (spec §3 + §11).

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_metadata::tables::idempotency::{prune_expired_bounded, IDEMPOTENCY_TABLE};
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Spec §11/05 §5 — default 24 h retention. Operators tune per
/// workload via `with_ttl()`.
pub const DEFAULT_IDEMPOTENCY_TTL: Duration = Duration::from_secs(24 * 3600);

pub struct IdempotencyCleanupWorker {
    config: WorkerConfig,
    ttl: Duration,
}

impl IdempotencyCleanupWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::IdempotencyCleanup),
            ttl: DEFAULT_IDEMPOTENCY_TTL,
        }
    }

    /// Override the default config (1 h interval, batch_size 10 000,
    /// max_runtime 5 s).
    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    /// Override the default 24 h TTL. Spec §11/05 §5: shorter →
    /// smaller table at the cost of retry tolerance.
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }
}

impl Default for IdempotencyCleanupWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for IdempotencyCleanupWorker {
    fn name(&self) -> &'static str {
        WorkerKind::IdempotencyCleanup.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::IdempotencyCleanup
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + Send + 'a>> {
        Box::pin(do_cleanup_cycle(self, ctx))
    }
}

async fn do_cleanup_cycle(
    worker: &IdempotencyCleanupWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }
    let now_nanos = now_unix_nanos();
    let ttl_nanos = u64::try_from(worker.ttl.as_nanos()).unwrap_or(u64::MAX);
    let metadata = ctx.ops.executor.metadata.clone();
    let started = Instant::now();
    let mut total_deleted = 0usize;

    loop {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }

        let (deleted, scanned_to_end) = {
            let mut db = metadata.lock();
            let wtxn = db
                .write_txn()
                .map_err(|e| WorkerError::Ops(format!("cleanup write_txn: {e:?}")))?;
            let result = {
                let mut table = wtxn
                    .open_table(IDEMPOTENCY_TABLE)
                    .map_err(|e| WorkerError::Ops(format!("open IDEMPOTENCY: {e:?}")))?;
                prune_expired_bounded(&mut table, now_nanos, ttl_nanos, cfg.batch_size)
                    .map_err(|e| WorkerError::Ops(format!("prune_expired_bounded: {e:?}")))?
            };
            wtxn.commit()
                .map_err(|e| WorkerError::Ops(format!("cleanup commit: {e:?}")))?;
            result
        };
        total_deleted = total_deleted.saturating_add(deleted as usize);

        if scanned_to_end {
            break;
        }
        // Yield between batches so we don't monopolise the mutex.
        // CLAUDE.md §9 guard: only `await` here, outside the lock.
        tokio::task::yield_now().await;
    }

    trace!(
        deleted = total_deleted,
        cycle_ms = started.elapsed().as_millis() as u64,
        "idempotency cleanup cycle"
    );
    Ok(total_deleted)
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// Compile-time Send + Sync guard.
const _: fn() = || {
    fn require<T: Send + Sync>() {}
    require::<IdempotencyCleanupWorker>();
};
