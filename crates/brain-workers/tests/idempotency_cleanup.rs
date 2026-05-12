#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send post-9.7 (audit §4)
//! Idempotency cleanup worker integration tests (sub-task 8.6). Spec §11/05.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::idempotency::{response_kind, IdempotencyEntry, IDEMPOTENCY_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{
    IdempotencyCleanupWorker, Worker, WorkerConfig, WorkerContext, WorkerKind, WorkerScheduler,
};
use parking_lot::Mutex;
use redb::ReadableTable;

const HOUR_NS: u64 = 60 * 60 * 1_000_000_000;

// ---------------------------------------------------------------------------
// Fixture.
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

struct Fixture {
    ctx: Arc<OpsContext>,
    metadata: SharedMetadataDb,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx: Arc::new(OpsContext::new(executor)),
        metadata,
        _tempdir: tempdir,
    }
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn rid(i: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[15] = i;
    b
}

fn seed_entry(metadata: &SharedMetadataDb, byte: u8, created_at_unix_nanos: u64) {
    let mut db = metadata.lock();
    let wtxn = db.write_txn().unwrap();
    {
        let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        let entry = IdempotencyEntry::new(
            response_kind::ENCODE,
            None,
            vec![byte, byte ^ 0xAA],
            [byte; 32],
            created_at_unix_nanos,
        );
        t.insert(rid(byte), entry).unwrap();
    }
    wtxn.commit().unwrap();
}

fn count_entries(metadata: &SharedMetadataDb) -> usize {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
    t.iter().unwrap().count()
}

async fn run_one(
    worker: &IdempotencyCleanupWorker,
    ops: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wctx = WorkerContext {
        ops,
        shutdown: shutdown_flag.clone(),
    };
    worker.run_cycle(&wctx).await
}

// ===========================================================================
// Cycle behaviour (5).
// ===========================================================================

#[test]
fn expired_entries_are_removed() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // 25h ago — past the 24h TTL.
        for i in 1..=5u8 {
            seed_entry(&fix.metadata, i, now - 25 * HOUR_NS);
        }
        assert_eq!(count_entries(&fix.metadata), 5);
        let worker = IdempotencyCleanupWorker::new();
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 5);
        assert_eq!(count_entries(&fix.metadata), 0);
    });
}

#[test]
fn young_entries_are_kept() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // 1h ago — well within the 24h TTL.
        for i in 1..=5u8 {
            seed_entry(&fix.metadata, i, now - HOUR_NS);
        }
        let worker = IdempotencyCleanupWorker::new();
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
        assert_eq!(count_entries(&fix.metadata), 5);
    });
}

#[test]
fn mixed_entries_only_expired_removed() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        for i in 1..=3u8 {
            seed_entry(&fix.metadata, i, now - 30 * HOUR_NS); // expired
        }
        for i in 10..=12u8 {
            seed_entry(&fix.metadata, i, now - 2 * HOUR_NS); // young
        }
        assert_eq!(count_entries(&fix.metadata), 6);
        let worker = IdempotencyCleanupWorker::new();
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 3);
        assert_eq!(count_entries(&fix.metadata), 3);
    });
}

#[test]
fn multi_cycle_convergence() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // 25 expired entries, batch_size=10. Single cycle loops until
        // scanned_to_end or max_runtime; should remove all 25 in one
        // call because the loop continues across batches inside the
        // cycle.
        for i in 1..=25u8 {
            seed_entry(&fix.metadata, i, now - 50 * HOUR_NS);
        }
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(60),
            batch_size: 10,
            max_runtime: Duration::from_secs(60),
        };
        let worker = IdempotencyCleanupWorker::new().with_config(cfg);
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 25);
        assert_eq!(count_entries(&fix.metadata), 0);
    });
}

#[test]
fn custom_ttl_honoured() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // 2h-old entries — kept under default 24h TTL.
        for i in 1..=4u8 {
            seed_entry(&fix.metadata, i, now - 2 * HOUR_NS);
        }
        // Default TTL keeps them.
        let kept = run_one(&IdempotencyCleanupWorker::new(), fix.ctx.clone())
            .await
            .unwrap();
        assert_eq!(kept, 0);
        assert_eq!(count_entries(&fix.metadata), 4);

        // 1h TTL → 2h-old entries are expired.
        let short = IdempotencyCleanupWorker::new().with_ttl(Duration::from_secs(60 * 60));
        let removed = run_one(&short, fix.ctx).await.unwrap();
        assert_eq!(removed, 4);
        assert_eq!(count_entries(&fix.metadata), 0);
    });
}

// ===========================================================================
// Worker integration (3).
// ===========================================================================

#[test]
fn worker_registers_with_correct_kind_and_default_cadence() {
    glommio_run(|| async {
        let fix = build_fixture();
        let mut sched = WorkerScheduler::new();
        sched
            .register(Arc::new(IdempotencyCleanupWorker::new()), fix.ctx)
            .unwrap();
        let cfg = sched.config(WorkerKind::IdempotencyCleanup.name()).unwrap();
        assert_eq!(cfg.interval, Duration::from_secs(3600));
        sched.shutdown().await.unwrap();
    });
}

#[test]
fn disabled_worker_via_config_does_not_run() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        for i in 1..=5u8 {
            seed_entry(&fix.metadata, i, now - 30 * HOUR_NS);
        }
        let cfg = WorkerConfig {
            enabled: false,
            interval: Duration::from_millis(20),
            batch_size: 1000,
            max_runtime: Duration::from_secs(1),
        };
        let mut sched = WorkerScheduler::new();
        sched
            .register(
                Arc::new(IdempotencyCleanupWorker::new().with_config(cfg)),
                fix.ctx.clone(),
            )
            .unwrap();
        glommio::timer::sleep(Duration::from_millis(150)).await;
        sched.shutdown().await.unwrap();
        assert_eq!(
            count_entries(&fix.metadata),
            5,
            "disabled worker must not delete"
        );
    });
}

#[test]
fn cycle_processed_count_feeds_metrics() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        for i in 1..=4u8 {
            seed_entry(&fix.metadata, i, now - 30 * HOUR_NS);
        }
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_millis(20),
            batch_size: 100,
            max_runtime: Duration::from_secs(1),
        };
        let mut sched = WorkerScheduler::new();
        sched
            .register(
                Arc::new(IdempotencyCleanupWorker::new().with_config(cfg)),
                fix.ctx,
            )
            .unwrap();
        let metrics = sched
            .metrics(WorkerKind::IdempotencyCleanup.name())
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if metrics.processed_total.load(Ordering::Relaxed) >= 4 {
                break;
            }
            glommio::timer::sleep(Duration::from_millis(5)).await;
        }
        sched.shutdown().await.unwrap();
        assert!(metrics.processed_total.load(Ordering::Relaxed) >= 4);
    });
}

// ===========================================================================
// Edge cases (2).
// ===========================================================================

#[test]
fn empty_table_cycle_is_noop() {
    glommio_run(|| async {
        let fix = build_fixture();
        let worker = IdempotencyCleanupWorker::new();
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
    });
}

#[test]
fn batch_size_zero_returns_zero() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        for i in 1..=3u8 {
            seed_entry(&fix.metadata, i, now - 30 * HOUR_NS);
        }
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(60),
            batch_size: 0,
            max_runtime: Duration::from_secs(60),
        };
        let worker = IdempotencyCleanupWorker::new().with_config(cfg);
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
        assert_eq!(
            count_entries(&fix.metadata),
            3,
            "no deletions when batch_size=0"
        );
    });
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
