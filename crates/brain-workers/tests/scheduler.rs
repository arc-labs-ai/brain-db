#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Scheduler integration tests.
//!
//! Drives the trait + scheduler with a `TestWorker` fixture that
//! exposes controllable per-cycle behaviour (work units, error
//! injection, sleep). The ops layer comes from a real `OpsContext`
//! built over a tempdir — these tests don't touch it, but the
//! scheduler's `register` signature requires one.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{
    drive_batch, Worker, WorkerConfig, WorkerContext, WorkerError, WorkerKind, WorkerScheduler,
};

// ---------------------------------------------------------------------------
// OpsContext fixture (shared by every test). These tests don't exercise
// the ops layer through workers, but the scheduler signature demands one.
// ---------------------------------------------------------------------------

struct NopDispatcher;

impl Dispatcher for NopDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}

fn make_ops_context() -> (Arc<OpsContext>, tempfile::TempDir) {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    (
        Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)),
        tempdir,
    )
}

// ---------------------------------------------------------------------------
// TestWorker: per-cycle behaviour driven by an injected closure.
// ---------------------------------------------------------------------------

// The Worker trait drops Send+Sync; the test CycleBody alias matches.
type CycleBody =
    Arc<dyn Fn(&WorkerContext) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>>>>>;

struct TestWorker {
    name: &'static str,
    kind: WorkerKind,
    config: WorkerConfig,
    body: CycleBody,
}

impl TestWorker {
    fn new(name: &'static str, kind: WorkerKind, config: WorkerConfig, body: CycleBody) -> Self {
        Self {
            name,
            kind,
            config,
            body,
        }
    }
}

impl Worker for TestWorker {
    fn name(&self) -> &'static str {
        self.name
    }
    fn kind(&self) -> WorkerKind {
        self.kind
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        (self.body)(ctx)
    }
}

fn fast_config() -> WorkerConfig {
    WorkerConfig {
        enabled: true,
        interval: Duration::from_millis(20),
        batch_size: 1,
        max_runtime: Duration::from_secs(1),
    }
}

// Poll a closure until it returns `true` or the deadline elapses.
async fn wait_until<F>(deadline_ms: u64, mut pred: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = std::time::Instant::now();
    let deadline = Duration::from_millis(deadline_ms);
    while start.elapsed() < deadline {
        if pred() {
            return true;
        }
        glommio::timer::sleep(Duration::from_millis(5)).await;
    }
    pred()
}

// ===========================================================================
// Trait conformance (3 tests).
// ===========================================================================

#[test]
fn worker_runs_at_least_once() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let mut sched = WorkerScheduler::new();
        let body: CycleBody = Arc::new(|_ctx| Box::pin(async { Ok(1) }));
        sched
            .register(
                Arc::new(TestWorker::new(
                    "decay",
                    WorkerKind::Decay,
                    fast_config(),
                    body,
                )),
                ops,
            )
            .unwrap();
        let metrics = sched.metrics("decay").unwrap();
        let ran = wait_until(500, || metrics.cycles_total.load(Ordering::Relaxed) >= 2).await;
        sched.shutdown().await.unwrap();
        assert!(
            ran,
            "expected >= 2 cycles, got {}",
            metrics.cycles_total.load(Ordering::Relaxed)
        );
    });
}

#[test]
fn worker_processed_count_feeds_processed_total() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let mut sched = WorkerScheduler::new();
        let body: CycleBody = Arc::new(|_ctx| Box::pin(async { Ok(7) }));
        sched
            .register(
                Arc::new(TestWorker::new(
                    "access_boost",
                    WorkerKind::AccessBoost,
                    fast_config(),
                    body,
                )),
                ops,
            )
            .unwrap();
        let metrics = sched.metrics("access_boost").unwrap();
        let ok = wait_until(500, || {
            let cycles = metrics.cycles_total.load(Ordering::Relaxed);
            let processed = metrics.processed_total.load(Ordering::Relaxed);
            cycles >= 3 && processed == cycles * 7
        })
        .await;
        sched.shutdown().await.unwrap();
        assert!(
            ok,
            "processed_total ({}) must equal 7 × cycles_total ({})",
            metrics.processed_total.load(Ordering::Relaxed),
            metrics.cycles_total.load(Ordering::Relaxed)
        );
    });
}

#[test]
fn disabled_worker_never_executes() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let mut sched = WorkerScheduler::new();
        let body: CycleBody = Arc::new(|_ctx| Box::pin(async { Ok(1) }));
        let cfg = WorkerConfig {
            enabled: false,
            ..fast_config()
        };
        sched
            .register(
                Arc::new(TestWorker::new("snapshot", WorkerKind::Snapshot, cfg, body)),
                ops,
            )
            .unwrap();
        let metrics = sched.metrics("snapshot").unwrap();
        glommio::timer::sleep(Duration::from_millis(200)).await;
        sched.shutdown().await.unwrap();
        assert_eq!(
            metrics.cycles_total.load(Ordering::Relaxed),
            0,
            "disabled worker must not run"
        );
    });
}

// ===========================================================================
// drive_batch helper (4 tests).
// ===========================================================================

#[test]
fn drive_batch_respects_batch_size_bound() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = WorkerContext {
            ops,
            shutdown: shutdown_flag.clone(),
        };
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(1),
            batch_size: 10,
            max_runtime: Duration::from_secs(60),
        };
        let processed = drive_batch(&cfg, &ctx, |_| async { Ok::<_, WorkerError>(true) })
            .await
            .unwrap();
        assert_eq!(processed, 10);
    });
}

#[test]
fn drive_batch_respects_max_runtime() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = WorkerContext {
            ops,
            shutdown: shutdown_flag.clone(),
        };
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(1),
            batch_size: 10_000,
            max_runtime: Duration::from_millis(120),
        };
        let processed = drive_batch(&cfg, &ctx, |_| async {
            glommio::timer::sleep(Duration::from_millis(20)).await;
            Ok::<_, WorkerError>(true)
        })
        .await
        .unwrap();
        // Each unit takes ~20ms; max_runtime=120ms allows ~6 units before
        // the check terminates. Wide bound to avoid flake.
        assert!(
            (1..=12).contains(&processed),
            "expected 1..=12 units within 120ms, got {processed}"
        );
    });
}

#[test]
fn drive_batch_stops_when_unit_returns_false() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = WorkerContext {
            ops,
            shutdown: shutdown_flag.clone(),
        };
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(1),
            batch_size: 100,
            max_runtime: Duration::from_secs(60),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let processed = drive_batch(&cfg, &ctx, move |_| {
            let calls = calls_clone.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::Relaxed) + 1;
                Ok::<_, WorkerError>(n < 3)
            }
        })
        .await
        .unwrap();
        assert_eq!(processed, 2, "stopped before third call returned false");
        assert_eq!(calls.load(Ordering::Relaxed), 3, "unit was called 3 times");
    });
}

#[test]
fn drive_batch_exits_on_shutdown() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = WorkerContext {
            ops,
            shutdown: shutdown_flag.clone(),
        };
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(1),
            batch_size: 10_000,
            max_runtime: Duration::from_secs(60),
        };
        // Fire shutdown after 30ms.
        let tx_handle = glommio::spawn_local(async move {
            glommio::timer::sleep(Duration::from_millis(30)).await;
            shutdown_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let processed = drive_batch(&cfg, &ctx, |_| async {
            glommio::timer::sleep(Duration::from_millis(5)).await;
            Ok::<_, WorkerError>(true)
        })
        .await
        .unwrap();
        tx_handle.await;
        // ~6 units in 30ms; bound generously.
        assert!(
            processed < 10_000,
            "shutdown must short-circuit drive_batch, got {processed}"
        );
    });
}

// ===========================================================================
// Lifecycle (3 tests).
// ===========================================================================

#[test]
fn shutdown_waits_for_in_progress_cycle() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let mut sched = WorkerScheduler::new();
        let cycle_finished = Arc::new(AtomicBool::new(false));
        let cycle_finished_clone = cycle_finished.clone();
        let body: CycleBody = Arc::new(move |_ctx| {
            let flag = cycle_finished_clone.clone();
            Box::pin(async move {
                glommio::timer::sleep(Duration::from_millis(150)).await;
                flag.store(true, Ordering::Release);
                Ok(1)
            })
        });
        sched
            .register(
                Arc::new(TestWorker::new(
                    "consolidation",
                    WorkerKind::Consolidation,
                    WorkerConfig {
                        enabled: true,
                        interval: Duration::from_millis(10),
                        batch_size: 1,
                        max_runtime: Duration::from_secs(1),
                    },
                    body,
                )),
                ops,
            )
            .unwrap();

        // Let the cycle kick off.
        glommio::timer::sleep(Duration::from_millis(30)).await;
        sched.shutdown().await.unwrap();
        assert!(
            cycle_finished.load(Ordering::Acquire),
            "in-progress cycle must complete before shutdown returns"
        );
    });
}

#[test]
fn errors_increment_errors_total_and_loop_continues() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let mut sched = WorkerScheduler::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let calls_clone = call_count.clone();
        let body: CycleBody = Arc::new(move |_ctx| {
            let calls = calls_clone.clone();
            Box::pin(async move {
                let n = calls.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    Err(WorkerError::Ops("first cycle fails".into()))
                } else {
                    Ok(2)
                }
            })
        });
        sched
            .register(
                Arc::new(TestWorker::new(
                    "edge_scrub",
                    WorkerKind::EdgeScrub,
                    fast_config(),
                    body,
                )),
                ops,
            )
            .unwrap();
        let metrics = sched.metrics("edge_scrub").unwrap();
        let ok = wait_until(500, || {
            metrics.errors_total.load(Ordering::Relaxed) >= 1
                && metrics.cycles_total.load(Ordering::Relaxed) >= 1
        })
        .await;
        sched.shutdown().await.unwrap();
        assert!(ok);
        assert!(call_count.load(Ordering::Relaxed) >= 2);
        assert_eq!(metrics.errors_total.load(Ordering::Relaxed), 1);
        // cycles_total only increments for successful cycles; errors don't
        // count toward it (the scheduler only bumps cycles on Ok).
        assert!(metrics.cycles_total.load(Ordering::Relaxed) >= 1);
    });
}

#[test]
fn scheduler_rejects_duplicate_worker_names() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let mut sched = WorkerScheduler::new();
        let body: CycleBody = Arc::new(|_| Box::pin(async { Ok(0) }));
        sched
            .register(
                Arc::new(TestWorker::new(
                    "decay",
                    WorkerKind::Decay,
                    fast_config(),
                    body.clone(),
                )),
                ops.clone(),
            )
            .unwrap();
        let dup = sched.register(
            Arc::new(TestWorker::new(
                "decay",
                WorkerKind::Decay,
                fast_config(),
                body,
            )),
            ops,
        );
        assert!(matches!(dup, Err(WorkerError::Internal(_))));
        sched.shutdown().await.unwrap();
    });
}

// ===========================================================================
// Multi-worker (1 test).
// ===========================================================================

#[test]
fn multiple_workers_run_independently() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let mut sched = WorkerScheduler::new();
        let body1: CycleBody = Arc::new(|_| Box::pin(async { Ok(1) }));
        let body2: CycleBody = Arc::new(|_| Box::pin(async { Ok(5) }));
        sched
            .register(
                Arc::new(TestWorker::new(
                    "decay",
                    WorkerKind::Decay,
                    WorkerConfig {
                        enabled: true,
                        interval: Duration::from_millis(20),
                        batch_size: 1,
                        max_runtime: Duration::from_secs(1),
                    },
                    body1,
                )),
                ops.clone(),
            )
            .unwrap();
        sched
            .register(
                Arc::new(TestWorker::new(
                    "wal_retention",
                    WorkerKind::WalRetention,
                    WorkerConfig {
                        enabled: true,
                        interval: Duration::from_millis(30),
                        batch_size: 1,
                        max_runtime: Duration::from_secs(1),
                    },
                    body2,
                )),
                ops,
            )
            .unwrap();
        assert_eq!(sched.len(), 2);
        let names = sched.names();
        assert!(names.contains(&"decay"));
        assert!(names.contains(&"wal_retention"));

        let m1 = sched.metrics("decay").unwrap();
        let m2 = sched.metrics("wal_retention").unwrap();
        let ok = wait_until(500, || {
            m1.cycles_total.load(Ordering::Relaxed) >= 2
                && m2.cycles_total.load(Ordering::Relaxed) >= 2
        })
        .await;
        sched.shutdown().await.unwrap();
        assert!(ok, "both workers must run independently");
        // Each worker's processed_total reflects its own per-cycle return.
        assert_eq!(
            m1.processed_total.load(Ordering::Relaxed),
            m1.cycles_total.load(Ordering::Relaxed)
        );
        assert_eq!(
            m2.processed_total.load(Ordering::Relaxed),
            m2.cycles_total.load(Ordering::Relaxed) * 5
        );
    });
}

// ===========================================================================
// Default cadence table (1 test).
//
// Pins each WorkerKind's default interval + enabled flag in one place
// so the per-worker test files don't each re-assert their own cadence.
// ===========================================================================

#[test]
fn default_cadence_table_is_pinned() {
    let cases: &[(WorkerKind, Duration, bool)] = &[
        (WorkerKind::Decay, Duration::from_secs(3600), true),
        (WorkerKind::AccessBoost, Duration::from_secs(10), true),
        (WorkerKind::Consolidation, Duration::from_secs(300), true),
        (WorkerKind::HnswMaintenance, Duration::from_secs(300), true),
        (WorkerKind::IdempotencyCleanup, Duration::from_secs(3600), true),
        (WorkerKind::SlotReclamation, Duration::from_secs(600), true),
        (WorkerKind::WalRetention, Duration::from_secs(60), true),
        (WorkerKind::EdgeScrub, Duration::from_secs(1800), true),
        (WorkerKind::CounterReconcile, Duration::from_secs(3600), true),
        (WorkerKind::Statistics, Duration::from_secs(300), true),
        (WorkerKind::EmbedderCacheEvict, Duration::from_secs(60), true),
        (WorkerKind::Snapshot, Duration::from_secs(3600), true),
    ];
    for (kind, interval, enabled) in cases {
        let cfg = WorkerConfig::defaults_for(*kind);
        assert_eq!(
            cfg.interval,
            *interval,
            "{} default interval drifted",
            kind.name()
        );
        assert_eq!(
            cfg.enabled,
            *enabled,
            "{} default enabled flag drifted",
            kind.name()
        );
    }
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("worker-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}
