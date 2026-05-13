//! The per-shard worker scheduler. Spec §11/00 §2 + §11/01 §1, §4.
//!
//! After sub-task 9.7 (audit §6 + §8.2), the scheduler runs **inside
//! a Glommio executor** — one per shard. `register(...)` spawns one
//! `glommio::Task` per worker via `spawn_local`. Each task runs the
//! standard `worker_loop`:
//!
//! 1. If disabled → sleep on interval, never call `run_cycle`.
//! 2. Else: call `run_cycle`; update metrics; sleep on interval.
//! 3. Between sleeps, check the per-shard `Rc<Cell<bool>>` shutdown
//!    flag (no more `tokio::select!` — the executor is single-threaded
//!    so a cooperative check is enough).
//!
//! Shutdown: set the flag, await every spawned `Task<()>` with a 5 s
//! soft budget (spec §11/01 §13). Tasks still alive after the budget
//! are cancelled (Glommio `Task::cancel`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_ops::OpsContext;
use futures_lite::FutureExt;
use glommio::timer::sleep;
use glommio::Task;
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
    task: Task<()>,
}

/// Spec §11/00 §3: each shard owns one scheduler. After 9.7, lives on
/// the shard's single Glommio executor. Construction is sync (no
/// runtime needed); `register` requires Glommio executor context.
pub struct WorkerScheduler {
    handles: HashMap<&'static str, WorkerHandle>,
    shutdown: Arc<AtomicBool>,
}

impl WorkerScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            handles: HashMap::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Register a worker. Spawns the loop task immediately on the
    /// current Glommio executor. The registry uses `worker.name()` as
    /// the key — duplicate names are rejected.
    ///
    /// **Must be called from inside a Glommio executor.**
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
            shutdown: self.shutdown.clone(),
        };
        let task = glommio::spawn_local(worker_loop(worker, ctx, metrics.clone()));
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

    /// Spec §11/01 §15: snapshot every registered worker's metrics.
    /// Wraps each handle's atomics into a plain
    /// [`crate::metrics::Snapshot`] so callers don't have to chase
    /// `Arc<AtomicU64>` instances. Used by `brain-server`'s admin
    /// `/metrics` endpoint (sub-task 9.13).
    ///
    /// Returned order is HashMap iteration order (not registration
    /// order). Callers needing stable output should sort.
    #[must_use]
    pub fn metrics_snapshot(&self) -> Vec<(&'static str, WorkerKind, crate::metrics::Snapshot)> {
        self.handles
            .values()
            .map(|h| (h.name, h.kind, h.metrics.snapshot()))
            .collect()
    }

    /// Configuration as registered (post-default-resolution).
    #[must_use]
    pub fn config(&self, name: &str) -> Option<WorkerConfig> {
        self.handles.get(name).map(|h| h.config.clone())
    }

    /// Names of every registered worker — HashMap ordering, not
    /// registration order. Callers needing stable order should sort.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.handles.keys().copied().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Spec §11/01 §13: signal shutdown, await every task with a soft
    /// drain budget. Tasks still alive after the budget are cancelled.
    pub async fn shutdown(self) -> Result<(), WorkerError> {
        let WorkerScheduler {
            handles, shutdown, ..
        } = self;
        let count = handles.len();
        shutdown.store(true, Ordering::Relaxed);

        let drain_start = Instant::now();
        for (name, handle) in handles {
            let remaining = SHUTDOWN_DRAIN_BUDGET.saturating_sub(drain_start.elapsed());
            if remaining.is_zero() {
                warn!(
                    worker = name,
                    "shutdown drain budget exhausted; cancelling task"
                );
                handle.task.cancel().await;
                continue;
            }
            // Race the task against a timer. `done` resolves to
            // `false` (didn't time out) when the worker loop returns;
            // `timed_out` resolves to `true` after `remaining`.
            let task = handle.task;
            let done = async move {
                task.await;
                false
            };
            let timed_out = async move {
                sleep(remaining).await;
                true
            };
            if done.or(timed_out).await {
                warn!(worker = name, "shutdown drain timed out");
            } else {
                debug!(worker = name, "worker exited cleanly");
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
    loop {
        if ctx.is_shutdown() {
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
        // Sleep until next interval, but wake promptly on shutdown.
        // Single-threaded executor: a cooperative check after the
        // sleep is sufficient — no select needed.
        sleep(cfg.interval).await;
        if ctx.is_shutdown() {
            break;
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
