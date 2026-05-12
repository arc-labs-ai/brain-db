#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send post-9.7 (audit §4)
//! HNSW maintenance worker integration tests (sub-task 8.5). Spec §11/04.

use std::sync::Arc;
use std::time::Duration;

use brain_core::MemoryId;
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{
    decide_action, Action, DisabledRebuildSource, HnswMaintenanceWorker, IndexStats, RebuildSource,
    RebuildSourceError, RebuildThresholds, Worker, WorkerConfig, WorkerContext, WorkerKind,
    WorkerScheduler,
};
use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Fixture: real OpsContext + helpers to drive insert / forget on the
// underlying SharedHnsw + writer so the maintenance worker observes
// real stats.
// ---------------------------------------------------------------------------

struct MockDispatcher;
impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0f32; VECTOR_DIM];
        for (i, b) in text.as_bytes().iter().enumerate() {
            v[i % VECTOR_DIM] += f32::from(*b) / 255.0;
        }
        Ok(v)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0xCD; 16]
    }
}

struct Fixture {
    ctx: Arc<OpsContext>,
    index: SharedHnsw<VECTOR_DIM>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let index = shared.clone();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx: Arc::new(OpsContext::new(executor)),
        index,
        _tempdir: tempdir,
    }
}

fn make_id(slot: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[8..16].copy_from_slice(&slot.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

fn make_vector(seed: u64) -> [f32; VECTOR_DIM] {
    let mut v = [0.0f32; VECTOR_DIM];
    v[(seed as usize) % VECTOR_DIM] = 1.0;
    v
}

async fn run_one(
    worker: &HnswMaintenanceWorker,
    ops: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wctx = WorkerContext {
        ops,
        shutdown: shutdown_flag.clone(),
    };
    worker.run_cycle(&wctx).await
}

use brain_workers::hnsw_maint::SnapshotFuture;

/// Stub that returns a captured snapshot.
struct StubRebuildSource {
    vectors: Vec<(MemoryId, [f32; VECTOR_DIM])>,
}
impl RebuildSource<{ VECTOR_DIM }> for StubRebuildSource {
    fn snapshot_vectors(&self) -> SnapshotFuture<'_, { VECTOR_DIM }> {
        let v = self.vectors.clone();
        Box::pin(async move { Ok(v) })
    }
}

/// Stub that always fails.
struct FailingRebuildSource;
impl RebuildSource<{ VECTOR_DIM }> for FailingRebuildSource {
    fn snapshot_vectors(&self) -> SnapshotFuture<'_, { VECTOR_DIM }> {
        Box::pin(async { Err(RebuildSourceError::Failed("boom".into())) })
    }
}

// ===========================================================================
// decide_action (6).
// ===========================================================================

fn stats(ratio: f32, recall: f32) -> IndexStats {
    IndexStats {
        total_entries: 100,
        tombstone_count: (ratio * 100.0) as usize,
        tombstone_ratio: ratio,
        recall_estimate: recall,
    }
}

#[test]
fn none_below_all_thresholds() {
    let a = decide_action(stats(0.10, 1.0), RebuildThresholds::default());
    assert_eq!(a, Action::None);
}

#[test]
fn full_rebuild_when_tombstone_above_30() {
    let a = decide_action(stats(0.35, 1.0), RebuildThresholds::default());
    assert_eq!(a, Action::FullRebuild);
}

#[test]
fn full_rebuild_when_recall_below_90() {
    let a = decide_action(stats(0.0, 0.85), RebuildThresholds::default());
    assert_eq!(a, Action::FullRebuild);
}

#[test]
fn schedule_when_tombstone_between_15_and_30() {
    let a = decide_action(stats(0.20, 1.0), RebuildThresholds::default());
    assert_eq!(a, Action::ScheduleRebuildSoon);
}

#[test]
fn schedule_when_recall_between_90_and_93() {
    let a = decide_action(stats(0.0, 0.91), RebuildThresholds::default());
    assert_eq!(a, Action::ScheduleRebuildSoon);
}

#[test]
fn custom_thresholds_honoured() {
    let t = RebuildThresholds {
        tombstone_full_rebuild: 0.5,
        recall_full_rebuild: 0.5,
        tombstone_schedule: 0.4,
        recall_schedule: 0.6,
    };
    // ratio=0.35 would be FullRebuild under defaults, but under
    // custom thresholds it's below schedule and full → None.
    let a = decide_action(stats(0.35, 1.0), t);
    assert_eq!(a, Action::None);
}

// ===========================================================================
// Stats collection (2).
// ===========================================================================

#[test]
fn cycle_observes_zero_tombstones_initially() {
    glommio_run(|| async {
        let fix = build_fixture();
        let worker = HnswMaintenanceWorker::new(Arc::new(DisabledRebuildSource));
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
        assert_eq!(fix.index.tombstone_count(), 0);
    });
}

#[test]
fn cycle_reports_tombstone_after_forget_and_attempts_rebuild() {
    glommio_run(|| async {
        // Build an index with 4 entries and tombstone 2 → 50% > 30% →
        // FullRebuild. With DisabledRebuildSource, the rebuild is a
        // logged no-op (processed=0); the tombstones remain.
        let fix = build_fixture();
        let (_other, mut writer) =
            SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
        let _ = (_other, &mut writer); // sink unused
                                       // We need to mutate the live index. The fixture's `index` is a
                                       // reader; the writer side is owned by RealWriterHandle. Reach
                                       // through it: insert and mark_tombstoned via the shared mutex.
                                       // Easiest path: build a fresh (shared, writer) and rebuild into
                                       // the live index. But for this test we just want to observe
                                       // tombstone_count > 0 from the worker — we can swap a populated
                                       // index in directly.
        let (replacement_reader, mut replacement_writer) =
            SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
        for slot in 1..=4u64 {
            replacement_writer
                .insert(make_id(slot), &make_vector(slot))
                .unwrap();
        }
        replacement_writer.mark_tombstoned(make_id(1)).unwrap();
        replacement_writer.mark_tombstoned(make_id(2)).unwrap();
        // Take ownership of the new HnswIndex by re-creating with the
        // worker-side `swap` API. We can do this by reading what we have
        // through the new reader's params and rebuild — but simplest is
        // to construct an `HnswIndex` directly and call swap on the
        // fixture's index.
        let params = fix.index.params();
        let source: Vec<_> = (1..=4u64)
            .map(|slot| (make_id(slot), make_vector(slot)))
            .collect();
        let (mut new_idx, _r) =
            brain_index::HnswIndex::<{ VECTOR_DIM }>::rebuild(params, source).unwrap();
        new_idx.mark_tombstoned(make_id(1)).unwrap();
        new_idx.mark_tombstoned(make_id(2)).unwrap();
        fix.index.swap(new_idx);
        let _ = replacement_reader; // not used; exists to keep the writer alive

        assert_eq!(fix.index.len(), 4);
        assert_eq!(fix.index.tombstone_count(), 2);

        let worker = HnswMaintenanceWorker::new(Arc::new(DisabledRebuildSource));
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0, "disabled source can't rebuild");
        assert_eq!(
            fix.index.tombstone_count(),
            2,
            "tombstones remain after disabled-source cycle"
        );
    });
}

// ===========================================================================
// Rebuild source (3).
// ===========================================================================

#[test]
fn disabled_source_returns_disabled_error() {
    glommio_run(|| async {
        let s = DisabledRebuildSource;
        let r: Result<Vec<(MemoryId, [f32; VECTOR_DIM])>, _> =
            <DisabledRebuildSource as RebuildSource<{ VECTOR_DIM }>>::snapshot_vectors(&s).await;
        assert!(matches!(r, Err(RebuildSourceError::Disabled)));
    });
}

#[test]
fn stub_source_returns_provided_vectors() {
    glommio_run(|| async {
        let stub = StubRebuildSource {
            vectors: vec![(make_id(1), make_vector(1)), (make_id(2), make_vector(2))],
        };
        let r = <StubRebuildSource as RebuildSource<{ VECTOR_DIM }>>::snapshot_vectors(&stub)
            .await
            .unwrap();
        assert_eq!(r.len(), 2);
    });
}

#[test]
fn failed_source_propagates_error_as_worker_error() {
    glommio_run(|| async {
        let fix = build_fixture();
        // Force a FullRebuild action with low thresholds.
        let (params, _w) = (fix.index.params(), ()); // shadowing to avoid unused warning
        let _ = params;
        let source: Vec<_> = (1..=4u64)
            .map(|slot| (make_id(slot), make_vector(slot)))
            .collect();
        let (mut new_idx, _r) =
            brain_index::HnswIndex::<{ VECTOR_DIM }>::rebuild(fix.index.params(), source).unwrap();
        new_idx.mark_tombstoned(make_id(1)).unwrap();
        new_idx.mark_tombstoned(make_id(2)).unwrap();
        fix.index.swap(new_idx);

        let worker = HnswMaintenanceWorker::new(Arc::new(FailingRebuildSource));
        let res = run_one(&worker, fix.ctx).await;
        assert!(
            matches!(res, Err(brain_workers::WorkerError::Ops(_))),
            "failing source must surface as WorkerError::Ops, got {res:?}"
        );
    });
}

// ===========================================================================
// Cycle (3).
// ===========================================================================

#[test]
fn cycle_with_no_action_returns_zero() {
    glommio_run(|| async {
        let fix = build_fixture();
        let worker = HnswMaintenanceWorker::new(Arc::new(DisabledRebuildSource));
        assert_eq!(run_one(&worker, fix.ctx).await.unwrap(), 0);
    });
}

#[test]
fn full_rebuild_via_stub_source_swaps_index_and_returns_one() {
    glommio_run(|| async {
        let fix = build_fixture();
        // Seed 4 entries + 2 tombstones → 50% ratio → FullRebuild.
        let source: Vec<_> = (1..=4u64)
            .map(|slot| (make_id(slot), make_vector(slot)))
            .collect();
        let (mut new_idx, _r) =
            brain_index::HnswIndex::<{ VECTOR_DIM }>::rebuild(fix.index.params(), source).unwrap();
        new_idx.mark_tombstoned(make_id(1)).unwrap();
        new_idx.mark_tombstoned(make_id(2)).unwrap();
        fix.index.swap(new_idx);
        assert_eq!(fix.index.tombstone_count(), 2);

        // Stub returns only the 2 active ids so the rebuilt index has
        // zero tombstones.
        let stub = StubRebuildSource {
            vectors: vec![(make_id(3), make_vector(3)), (make_id(4), make_vector(4))],
        };
        let worker = HnswMaintenanceWorker::new(Arc::new(stub));
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 1, "FullRebuild + stub source → 1 rebuild");

        assert_eq!(fix.index.len(), 2);
        assert_eq!(fix.index.tombstone_count(), 0);
        assert!(fix.index.contains(make_id(3)));
        assert!(fix.index.contains(make_id(4)));
    });
}

#[test]
fn disabled_source_with_rebuild_needed_returns_zero_no_swap() {
    glommio_run(|| async {
        let fix = build_fixture();
        // Force FullRebuild action.
        let source: Vec<_> = (1..=4u64)
            .map(|slot| (make_id(slot), make_vector(slot)))
            .collect();
        let (mut new_idx, _r) =
            brain_index::HnswIndex::<{ VECTOR_DIM }>::rebuild(fix.index.params(), source).unwrap();
        new_idx.mark_tombstoned(make_id(1)).unwrap();
        new_idx.mark_tombstoned(make_id(2)).unwrap();
        fix.index.swap(new_idx);
        let pre_count = fix.index.tombstone_count();
        assert_eq!(pre_count, 2);

        let worker = HnswMaintenanceWorker::new(Arc::new(DisabledRebuildSource));
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
        assert_eq!(
            fix.index.tombstone_count(),
            pre_count,
            "disabled source must not swap the index"
        );
    });
}

// ===========================================================================
// Worker integration (2).
// ===========================================================================

#[test]
fn worker_registers_with_correct_kind_and_default_cadence() {
    glommio_run(|| async {
        let fix = build_fixture();
        let mut sched = WorkerScheduler::new();
        sched
            .register(
                Arc::new(HnswMaintenanceWorker::new(Arc::new(DisabledRebuildSource))),
                fix.ctx,
            )
            .unwrap();
        let cfg = sched.config(WorkerKind::HnswMaintenance.name()).unwrap();
        assert_eq!(cfg.interval, Duration::from_secs(300));
        sched.shutdown().await.unwrap();
    });
}

#[test]
fn disabled_worker_via_config_does_not_run() {
    glommio_run(|| async {
        let fix = build_fixture();
        let cfg = WorkerConfig {
            enabled: false,
            interval: Duration::from_millis(20),
            batch_size: 1,
            max_runtime: Duration::from_secs(1),
        };
        let mut sched = WorkerScheduler::new();
        sched
            .register(
                Arc::new(
                    HnswMaintenanceWorker::new(Arc::new(DisabledRebuildSource)).with_config(cfg),
                ),
                fix.ctx,
            )
            .unwrap();
        let metrics = sched.metrics(WorkerKind::HnswMaintenance.name()).unwrap();
        glommio::timer::sleep(Duration::from_millis(150)).await;
        sched.shutdown().await.unwrap();
        assert_eq!(
            metrics
                .cycles_total
                .load(std::sync::atomic::Ordering::Relaxed),
            0
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
