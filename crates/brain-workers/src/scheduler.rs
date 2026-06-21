//! The per-shard worker scheduler.
//!
//! The scheduler runs **inside a Glommio executor** — one per shard.
//! `register(...)` spawns one
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
//! soft budget. Tasks still alive after the budget
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

/// Per-worker control surface.
///
/// - `paused`: when true, the loop skips `run_cycle` but keeps
///   ticking on its interval. Set by `WorkerScheduler::pause` /
///   `::resume`.
/// - `wake_tx`/`wake_rx`: a bounded `flume` channel the loop races
///   against `sleep(interval)`. `WorkerScheduler::run_now` sends a
///   unit value; the loop wakes immediately and runs the next
///   cycle. Bounded(1) coalesces multiple run-now signals into one
///   wakeup.
pub struct WorkerControls {
    pub paused: AtomicBool,
    pub wake_tx: flume::Sender<()>,
    pub wake_rx: flume::Receiver<()>,
}

impl WorkerControls {
    fn new() -> Arc<Self> {
        let (wake_tx, wake_rx) = flume::bounded(1);
        Arc::new(Self {
            paused: AtomicBool::new(false),
            wake_tx,
            wake_rx,
        })
    }
}

/// Per-worker entry tracked by the scheduler.
pub struct WorkerHandle {
    pub name: &'static str,
    pub kind: WorkerKind,
    pub config: WorkerConfig,
    pub metrics: Arc<WorkerMetrics>,
    pub controls: Arc<WorkerControls>,
    task: Task<()>,
}

/// each shard owns one scheduler, living on the shard's single Glommio
/// executor. Construction is sync (no runtime needed); `register`
/// requires Glommio executor context.
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
        let controls = WorkerControls::new();
        let ctx = WorkerContext {
            ops,
            shutdown: self.shutdown.clone(),
        };
        let task =
            glommio::spawn_local(worker_loop(worker, ctx, metrics.clone(), controls.clone()));
        self.handles.insert(
            name,
            WorkerHandle {
                name,
                kind,
                config,
                metrics,
                controls,
                task,
            },
        );
        info!(worker = name, ?kind, "worker registered");
        Ok(())
    }

    /// Pause a registered worker. The loop keeps ticking on
    /// its interval but skips `run_cycle` until [`Self::resume`].
    /// Returns false if no such worker.
    pub fn pause(&self, name: &str) -> bool {
        match self.handles.get(name) {
            Some(h) => {
                h.controls.paused.store(true, Ordering::Relaxed);
                info!(worker = name, "worker paused");
                true
            }
            None => false,
        }
    }

    /// Resume a paused worker. Returns false if no such worker.
    pub fn resume(&self, name: &str) -> bool {
        match self.handles.get(name) {
            Some(h) => {
                h.controls.paused.store(false, Ordering::Relaxed);
                // Kick the loop so it doesn't wait out the rest of
                // its current sleep before running the next cycle.
                let _ = h.controls.wake_tx.try_send(());
                info!(worker = name, "worker resumed");
                true
            }
            None => false,
        }
    }

    /// Request an immediate cycle. The loop wakes from its
    /// current sleep and runs `run_cycle` once. No-op if the
    /// worker is paused. Returns false if no such worker.
    pub fn run_now(&self, name: &str) -> bool {
        match self.handles.get(name) {
            Some(h) => {
                // Bounded(1) channel coalesces — try_send drops on
                // overflow, which is fine; the loop runs at most one
                // extra cycle per wake.
                let _ = h.controls.wake_tx.try_send(());
                info!(worker = name, "worker run-now requested");
                true
            }
            None => false,
        }
    }

    /// Metrics for a registered worker.
    #[must_use]
    pub fn metrics(&self, name: &str) -> Option<Arc<WorkerMetrics>> {
        self.handles.get(name).map(|h| h.metrics.clone())
    }

    /// snapshot every registered worker's metrics.
    /// Wraps each handle's atomics into a plain
    /// [`crate::metrics::MetricsSnapshot`] so callers don't have to chase
    /// `Arc<AtomicU64>` instances. Used by `brain-server`'s admin
    /// `/metrics` endpoint.
    ///
    /// Returned order is HashMap iteration order (not registration
    /// order). Callers needing stable output should sort.
    #[must_use]
    pub fn metrics_snapshot(
        &self,
    ) -> Vec<(&'static str, WorkerKind, crate::metrics::MetricsSnapshot)> {
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

    /// signal shutdown, await every task with a soft
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

/// The per-worker loop task lifecycle:
/// `wake → run_cycle → update metrics → sleep`.
///
/// The loop has two control points:
///
/// - `controls.paused`: when true, skip `run_cycle` for this tick
///   (the loop still sleeps so it observes shutdown promptly).
/// - `controls.wake_rx`: races against `sleep(interval)`. A
///   `WorkerScheduler::run_now` send wakes the loop early; the
///   next cycle runs immediately.
///
/// Queue-bearing workers (extractor, auto_edge, temporal_edge,
/// causal_edge) additionally block inside their `run_cycle` on a
/// `recv_async().or(sleep(interval))` race so a fresh enqueue
/// during the cycle's wait window drains immediately. The combined
/// effect is: idle workers sleep at most `cfg.interval` between
/// cycles; once the next cycle starts, the queue's own wake fires
/// instantly — no per-interval latency floor inside the active cycle.
async fn worker_loop(
    worker: Arc<dyn Worker>,
    ctx: WorkerContext,
    metrics: Arc<WorkerMetrics>,
    controls: Arc<WorkerControls>,
) {
    let name = worker.name();
    let cfg = worker.config();
    let skip_first_tick = worker.skip_first_tick();
    let mut first_iter = true;
    loop {
        if ctx.is_shutdown() {
            break;
        }
        let paused = controls.paused.load(Ordering::Relaxed);
        // `skip_first_tick` workers (Snapshot) sleep the first interval
        // *before* ticking — see `Worker::skip_first_tick` for the
        // rationale. All other workers tick immediately so any pending
        // state from a previous run drains promptly.
        let skip_this_cycle = first_iter && skip_first_tick;
        if cfg.enabled && !paused && !skip_this_cycle {
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
        // Race the per-worker sleep against a run-now wake signal.
        // `flume::Receiver::recv_async` resolves when a sender
        // succeeds; `sleep` resolves on the timer. Whichever fires
        // first ends the wait — the loop checks shutdown + paused
        // on the next iteration.
        let sleeper = async { sleep(cfg.interval).await };
        let waker = async {
            let _ = controls.wake_rx.recv_async().await;
        };
        sleeper.or(waker).await;
        if ctx.is_shutdown() {
            break;
        }
        first_iter = false;
    }
    debug!(worker = name, "loop exiting");
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
