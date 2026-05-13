#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send post-9.7 (audit §4)
//! Snapshot worker tests (sub-task 8.13). Spec §11/08 §6.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::snapshot::{DeleteFuture, ListFuture, TakeFuture};
use brain_workers::{
    decide_retention, DisabledSnapshotSource, RetentionPolicy, SnapshotDesc, SnapshotId,
    SnapshotSource, SnapshotSourceError, SnapshotWorker, Worker, WorkerConfig, WorkerContext,
    WorkerKind, WorkerScheduler,
};
use parking_lot::Mutex;

const DAY_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

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

fn make_ops_context() -> (Arc<OpsContext>, tempfile::TempDir) {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    (Arc::new(OpsContext::new(executor)), tempdir)
}

async fn run_one(
    worker: &SnapshotWorker,
    ops: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wctx = WorkerContext {
        ops,
        shutdown: shutdown_flag.clone(),
    };
    worker.run_cycle(&wctx).await
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn snap(id: u64, taken_at: u64) -> SnapshotDesc {
    SnapshotDesc {
        id: SnapshotId(id),
        taken_at_unix_nanos: taken_at,
        size_bytes: 0,
    }
}

// ===========================================================================
// decide_retention (5).
// ===========================================================================

#[test]
fn empty_snapshots_returns_empty() {
    let r = decide_retention(&[], now_unix_nanos(), RetentionPolicy::default());
    assert!(r.is_empty());
}

#[test]
fn under_max_count_keeps_everything() {
    let now = now_unix_nanos();
    let s = [
        snap(1, now - DAY_NS),
        snap(2, now - 2 * DAY_NS),
        snap(3, now - 3 * DAY_NS),
    ];
    let r = decide_retention(&s, now, RetentionPolicy::default());
    assert!(r.is_empty());
}

#[test]
fn over_max_count_drops_oldest() {
    let now = now_unix_nanos();
    // 10 snapshots, ages 1..=10 days; max_count=7 → drop the 3 oldest.
    let s: Vec<SnapshotDesc> = (1..=10).map(|i| snap(i, now - i * DAY_NS)).collect();
    let policy = RetentionPolicy {
        max_count: 7,
        max_age: Duration::from_secs(365 * 24 * 3600),
    };
    let r = decide_retention(&s, now, policy);
    let mut ids: Vec<u64> = r.into_iter().map(|i| i.0).collect();
    ids.sort();
    assert_eq!(ids, vec![8, 9, 10]);
}

#[test]
fn over_max_age_drops_old_regardless_of_count() {
    let now = now_unix_nanos();
    let s = [
        snap(1, now - DAY_NS),
        snap(2, now - 40 * DAY_NS),
        snap(3, now - 60 * DAY_NS),
    ];
    let policy = RetentionPolicy {
        max_count: 7,
        max_age: Duration::from_secs(30 * 24 * 3600),
    };
    let r = decide_retention(&s, now, policy);
    let mut ids: Vec<u64> = r.into_iter().map(|i| i.0).collect();
    ids.sort();
    assert_eq!(ids, vec![2, 3]);
}

#[test]
fn count_and_age_combined() {
    let now = now_unix_nanos();
    // 10 snapshots, 5 of them older than 30 days.
    let s: Vec<SnapshotDesc> = (1..=10)
        .map(|i| {
            let age_days = if i <= 5 { i * 10 } else { i }; // i=1..5 → 10..50d; 6..10 → 6..10d
            snap(i, now - age_days * DAY_NS)
        })
        .collect();
    let policy = RetentionPolicy {
        max_count: 7,
        max_age: Duration::from_secs(30 * 24 * 3600),
    };
    let r = decide_retention(&s, now, policy);
    let mut ids: Vec<u64> = r.into_iter().map(|i| i.0).collect();
    ids.sort();
    // Old snapshots (i=3,4,5 with age 30,40,50 days) are over max_age.
    // i=3 is age 30d which equals max_age → "age >= max_age" → drop.
    // Sorted newest first (idx 0 = i=10, idx 6 = i=4, idx 7 = i=5...):
    // sorted by taken_at desc: i=10 (6d), 9 (9d), 8 (8d), 7 (7d), 6 (6d), 1 (10d), 2 (20d), 3 (30d), 4 (40d), 5 (50d).
    // Hmm — i=1 has age 10d, i=6 has age 6d. So sort order is
    // 10,9,8,7,6 (all <=10d), then 1 (10d), 2 (20d), 3 (30d), 4 (40d), 5 (50d).
    // max_count=7 means idx 7,8,9 dropped via count → {3,4,5} (sorted by age desc).
    // age >= 30 days drops i=3,4,5. Union = {3,4,5}.
    assert_eq!(ids, vec![3, 4, 5]);
}

// ===========================================================================
// Stub sources.
// ===========================================================================

struct StubSource {
    next_id: AtomicU64,
    snapshots: Mutex<Vec<SnapshotDesc>>,
    deleted: Arc<Mutex<Vec<SnapshotId>>>,
    take_age_offset_nanos: u64,
}

impl StubSource {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            snapshots: Mutex::new(Vec::new()),
            deleted: Arc::new(Mutex::new(Vec::new())),
            take_age_offset_nanos: 0,
        })
    }
}

impl SnapshotSource for StubSource {
    fn take_snapshot(&self) -> TakeFuture<'_> {
        let id = SnapshotId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let taken_at = now_unix_nanos().saturating_sub(self.take_age_offset_nanos);
        self.snapshots.lock().push(SnapshotDesc {
            id,
            taken_at_unix_nanos: taken_at,
            size_bytes: 0,
        });
        Box::pin(async move { Ok(id) })
    }
    fn list_snapshots(&self) -> ListFuture<'_> {
        let snaps = self.snapshots.lock().clone();
        Box::pin(async move { Ok(snaps) })
    }
    fn delete_snapshot(&self, id: SnapshotId) -> DeleteFuture<'_> {
        self.snapshots.lock().retain(|s| s.id != id);
        self.deleted.lock().push(id);
        Box::pin(async move { Ok(()) })
    }
}

struct FailingSource;
impl SnapshotSource for FailingSource {
    fn take_snapshot(&self) -> TakeFuture<'_> {
        Box::pin(async { Err(SnapshotSourceError::Failed("disk full".into())) })
    }
    fn list_snapshots(&self) -> ListFuture<'_> {
        Box::pin(async { Err(SnapshotSourceError::Failed("io".into())) })
    }
    fn delete_snapshot(&self, _: SnapshotId) -> DeleteFuture<'_> {
        Box::pin(async { Err(SnapshotSourceError::Failed("io".into())) })
    }
}

// ===========================================================================
// Source surface (3).
// ===========================================================================

#[test]
fn disabled_source_returns_disabled_on_every_method() {
    glommio_run(|| async {
        let s = DisabledSnapshotSource;
        assert!(matches!(
            s.take_snapshot().await,
            Err(SnapshotSourceError::Disabled)
        ));
        assert!(matches!(
            s.list_snapshots().await,
            Err(SnapshotSourceError::Disabled)
        ));
        assert!(matches!(
            s.delete_snapshot(SnapshotId(1)).await,
            Err(SnapshotSourceError::Disabled)
        ));
    });
}

#[test]
fn stub_source_take_returns_monotonic_id() {
    glommio_run(|| async {
        let stub = StubSource::new();
        let a = stub.take_snapshot().await.unwrap();
        let b = stub.take_snapshot().await.unwrap();
        assert!(b.0 > a.0);
        assert_eq!(stub.list_snapshots().await.unwrap().len(), 2);
    });
}

#[test]
fn failed_source_propagates_as_worker_error() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let mut cfg = WorkerConfig::defaults_for(WorkerKind::Snapshot);
        cfg.enabled = true;
        let worker = SnapshotWorker::new(Arc::new(FailingSource)).with_config(cfg);
        let r = run_one(&worker, ops).await;
        assert!(
            matches!(r, Err(brain_workers::WorkerError::Ops(_))),
            "Failed must surface, got {r:?}"
        );
    });
}

// ===========================================================================
// Cycle (3).
// ===========================================================================

#[test]
fn disabled_worker_via_config_does_not_take() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let stub = StubSource::new();
        let deleted = stub.deleted.clone();
        let snaps = stub as Arc<dyn SnapshotSource>;
        let mut sched = WorkerScheduler::new();
        sched
            .register(
                Arc::new(SnapshotWorker::new(snaps).with_config(WorkerConfig {
                    enabled: false,
                    interval: Duration::from_millis(20),
                    batch_size: 1,
                    max_runtime: Duration::from_secs(1),
                })),
                ops,
            )
            .unwrap();
        glommio::timer::sleep(Duration::from_millis(150)).await;
        sched.shutdown().await.unwrap();
        assert!(
            deleted.lock().is_empty(),
            "disabled worker must not delete anything"
        );
    });
}

#[test]
fn enabled_worker_takes_snapshot_and_reports_count() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let stub = StubSource::new();
        let snaps = stub.clone();
        let mut cfg = WorkerConfig::defaults_for(WorkerKind::Snapshot);
        cfg.enabled = true;
        let worker = SnapshotWorker::new(snaps as Arc<dyn SnapshotSource>).with_config(cfg);
        let processed = run_one(&worker, ops).await.unwrap();
        assert_eq!(processed, 1, "took 1 new snapshot, no retention deletions");
        assert_eq!(stub.snapshots.lock().len(), 1);
    });
}

#[test]
fn enabled_worker_deletes_old_snapshots_per_retention() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let stub = StubSource::new();
        // Pre-seed 9 old snapshots.
        {
            let now = now_unix_nanos();
            let mut g = stub.snapshots.lock();
            for i in 1..=9u64 {
                g.push(SnapshotDesc {
                    id: SnapshotId(100 + i),
                    taken_at_unix_nanos: now.saturating_sub(i * DAY_NS),
                    size_bytes: 0,
                });
            }
            stub.next_id.store(110, Ordering::Relaxed);
        }
        let snaps = stub.clone();
        let mut cfg = WorkerConfig::defaults_for(WorkerKind::Snapshot);
        cfg.enabled = true;
        let worker = SnapshotWorker::new(snaps as Arc<dyn SnapshotSource>)
            .with_config(cfg)
            .with_retention(RetentionPolicy {
                max_count: 7,
                max_age: Duration::from_secs(365 * 24 * 3600),
            });
        // Cycle: takes 1 new (total 10), retention drops 3 → processed = 4.
        let processed = run_one(&worker, ops).await.unwrap();
        assert_eq!(processed, 4);
        assert_eq!(stub.snapshots.lock().len(), 7);
        assert_eq!(stub.deleted.lock().len(), 3);
    });
}

// ===========================================================================
// Worker integration (2).
// ===========================================================================

#[test]
fn worker_registers_with_correct_kind_and_default_cadence_disabled() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let mut sched = WorkerScheduler::new();
        sched
            .register(
                Arc::new(SnapshotWorker::new(Arc::new(DisabledSnapshotSource))),
                ops,
            )
            .unwrap();
        let cfg = sched.config(WorkerKind::Snapshot.name()).unwrap();
        assert_eq!(cfg.interval, Duration::from_secs(3600));
        assert!(!cfg.enabled, "spec §6.2 — Snapshot defaults to disabled");
        sched.shutdown().await.unwrap();
    });
}

#[test]
fn default_config_has_enabled_false_per_spec() {
    glommio_run(|| async {
        let cfg = WorkerConfig::defaults_for(WorkerKind::Snapshot);
        assert!(!cfg.enabled);
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
