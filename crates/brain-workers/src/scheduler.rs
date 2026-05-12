//! The worker scheduler. Spec §11/00 §2 + §11/01 §1, §4.
//!
//! `WorkerScheduler::register(...)` spawns one tokio task per worker.
//! Each task runs the standard `worker_loop`:
//!
//! 1. If disabled → sleep on interval, never call `run_cycle`.
//! 2. Else: call `run_cycle`; update metrics; sleep on interval.
//! 3. `tokio::select!` on sleep vs shutdown so shutdown is prompt.
//!
//! Shutdown is a graceful drain: flip the watch channel, then await
//! each `JoinHandle` with a 5 s soft budget (spec §11/01 §13).

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_ops::OpsContext;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::metrics::WorkerMetrics;
use crate::worker::Worker;

const SHUTDOWN_DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// Per-worker entry tracked by the scheduler.
pub struct WorkerHandle {
    pub name: &'static str,
    pub kind: WorkerKind,
    pub config: WorkerConfig,
    pub metrics: Arc<WorkerMetrics>,
    task: JoinHandle<()>,
}

/// Spec §11/00 §3: each shard owns one scheduler. v1 runs on the
/// default tokio runtime; Phase 9 will swap the spawn primitive for
/// Glommio's task spawner.
pub struct WorkerScheduler {
    handles: HashMap<&'static str, WorkerHandle>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl WorkerScheduler {
    #[must_use]
    pub fn new() -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            handles: HashMap::new(),
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Register a worker. Spawns the loop task immediately. The
    /// registry uses `worker.name()` as the key — duplicate names are
    /// rejected with `WorkerError::Internal`.
    pub fn register(
        &mut self,
        worker: Arc<dyn Worker>,
        ops: Arc<OpsContext>,
    ) -> Result<(), WorkerError> {
        let name = worker.name();
        let kind = worker.kind();
        let config = worker.config();
        if self.handles.contains_key(name) {
            return Err(WorkerError::Internal(format!(
                "worker '{name}' already registered"
            )));
        }
        let metrics = Arc::new(WorkerMetrics::default());
        let ctx = WorkerContext {
            ops,
            shutdown: self.shutdown_rx.clone(),
        };
        let task = tokio::spawn(worker_loop(worker, ctx, metrics.clone()));
        self.handles.insert(
            name,
            WorkerHandle {
                name,
                kind,
                config,
                metrics,
                task,
            },
        );
        info!(worker = name, ?kind, "worker registered");
        Ok(())
    }

    /// Metrics for a registered worker.
    #[must_use]
    pub fn metrics(&self, name: &str) -> Option<Arc<WorkerMetrics>> {
        self.handles.get(name).map(|h| h.metrics.clone())
    }

    /// Configuration as registered (post-default-resolution).
    #[must_use]
    pub fn config(&self, name: &str) -> Option<WorkerConfig> {
        self.handles.get(name).map(|h| h.config.clone())
    }

    /// Names of every registered worker, in registration order is
    /// not guaranteed (HashMap ordering); callers that need stable
    /// order should sort.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.handles.keys().copied().collect()
    }

    /// Number of registered workers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// `true` if no workers are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Spec §11/01 §13: signal shutdown, await every task with a soft
    /// drain budget. Tasks still alive after the budget are detached;
    /// their JoinHandles are dropped (which aborts them on drop —
    /// tokio semantics).
    pub async fn shutdown(self) -> Result<(), WorkerError> {
        let WorkerScheduler {
            handles,
            shutdown_tx,
            ..
        } = self;
        let count = handles.len();
        // Flip the watch; loops exit at the next select point.
        let _ = shutdown_tx.send(true);

        let drain_start = Instant::now();
        for (name, handle) in handles {
            let remaining = SHUTDOWN_DRAIN_BUDGET.saturating_sub(drain_start.elapsed());
            if remaining.is_zero() {
                warn!(
                    worker = name,
                    "shutdown drain budget exhausted; detaching task"
                );
                handle.task.abort();
                continue;
            }
            match tokio::time::timeout(remaining, handle.task).await {
                Ok(Ok(())) => debug!(worker = name, "worker exited cleanly"),
                Ok(Err(e)) => warn!(worker = name, error = %e, "worker task panicked"),
                Err(_) => {
                    warn!(worker = name, "shutdown drain timed out; aborting");
                }
            }
        }
        info!(workers = count, "scheduler shutdown complete");
        Ok(())
    }
}

impl Default for WorkerScheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// The per-worker loop task. Spec §11/01 §4 lifecycle:
/// `wake → run_cycle → update metrics → sleep`.
async fn worker_loop(worker: Arc<dyn Worker>, ctx: WorkerContext, metrics: Arc<WorkerMetrics>) {
    let name = worker.name();
    let cfg = worker.config();
    let mut shutdown = ctx.shutdown.clone();
    loop {
        if *shutdown.borrow() {
            break;
        }
        if cfg.enabled {
            let start = Instant::now();
            match worker.run_cycle(&ctx).await {
                Ok(processed) => {
                    metrics.cycles_total.fetch_add(1, Ordering::Relaxed);
                    metrics
                        .processed_total
                        .fetch_add(processed as u64, Ordering::Relaxed);
                    let duration_ms =
                        u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    metrics
                        .last_cycle_duration_ms
                        .store(duration_ms, Ordering::Relaxed);
                    metrics
                        .last_run_unix_secs
                        .store(now_unix_secs(), Ordering::Relaxed);
                    debug!(worker = name, processed, duration_ms, "cycle complete");
                }
                Err(e) => {
                    metrics.errors_total.fetch_add(1, Ordering::Relaxed);
                    warn!(worker = name, error = %e, "worker cycle error");
                }
            }
        }
        // Sleep until next interval, but wake immediately on shutdown.
        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => { break; }
        }
    }
    debug!(worker = name, "loop exiting");
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Compile-time guards.
const _: fn() = || {
    fn require<T: Send + Sync>() {}
    require::<WorkerScheduler>();
    require::<WorkerHandle>();
};
