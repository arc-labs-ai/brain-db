//! Counter reconciliation worker tests (sub-task 8.10). Spec §11/08 §2.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{derived_by, origin, EdgeData, EDGES_IN_TABLE, EDGES_OUT_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{
    CounterReconcileWorker, Worker, WorkerConfig, WorkerContext, WorkerKind, WorkerScheduler,
};
use parking_lot::Mutex;
use uuid::Uuid;

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

fn make_id(slot: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[8..16].copy_from_slice(&slot.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

/// Seed a memory with explicit stored counts (used to craft drift).
fn seed_memory_with_counts(
    metadata: &SharedMetadataDb,
    slot: u64,
    stored_out: u32,
    stored_in: u32,
) -> MemoryId {
    let id = make_id(slot);
    let mut db = metadata.lock();
    let wtxn = db.write_txn().unwrap();
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        let mut meta = MemoryMetadata::new_active(
            id,
            AgentId(Uuid::nil()),
            ContextId(1),
            slot,
            1,
            MemoryKind::Episodic,
            [0; 16],
            0.5,
            16,
            now_unix_nanos(),
        );
        meta.edges_out_count = stored_out;
        meta.edges_in_count = stored_in;
        table.insert(id.to_be_bytes(), meta).unwrap();
    }
    wtxn.commit().unwrap();
    id
}

/// Insert an edge directly without bumping any counters — lets us
/// build drift scenarios where stored counts don't match reality.
fn seed_edge_raw(metadata: &SharedMetadataDb, src: MemoryId, kind: EdgeKind, tgt: MemoryId) {
    let mut db = metadata.lock();
    let wtxn = db.write_txn().unwrap();
    {
        let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let mut in_t = wtxn.open_table(EDGES_IN_TABLE).unwrap();
        let data = EdgeData::new(1.0, origin::EXPLICIT, derived_by::CLIENT, now_unix_nanos());
        let s = src.to_be_bytes();
        let t = tgt.to_be_bytes();
        let k = kind as u8;
        out.insert(&(s, k, t), &data).unwrap();
        in_t.insert(&(t, k, s), &data).unwrap();
    }
    wtxn.commit().unwrap();
}

fn read_counts(metadata: &SharedMetadataDb, id: MemoryId) -> (u32, u32) {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
    let access = t.get(id.to_be_bytes()).unwrap().unwrap();
    let v = access.value();
    (v.edges_out_count, v.edges_in_count)
}

async fn run_one(
    worker: &CounterReconcileWorker,
    ops: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let wctx = WorkerContext { ops, shutdown: rx };
    worker.run_cycle(&wctx).await
}

fn batchy_config(batch_size: usize) -> WorkerConfig {
    WorkerConfig {
        enabled: true,
        interval: Duration::from_secs(60),
        batch_size,
        max_runtime: Duration::from_secs(60),
    }
}

// ===========================================================================
// Cycle behaviour (7).
// ===========================================================================

#[tokio::test]
async fn correctly_stamped_memory_needs_no_fix() {
    let fix = build_fixture();
    // a will have 1 incoming, 0 outgoing. b will have 1 outgoing, 0
    // incoming. Edge points b → a.
    let a = seed_memory_with_counts(&fix.metadata, 1, 0, 1);
    let b = seed_memory_with_counts(&fix.metadata, 2, 1, 0);
    seed_edge_raw(&fix.metadata, b, EdgeKind::FollowedBy, a);

    let worker = CounterReconcileWorker::new().with_config(batchy_config(100));
    let fixed = run_one(&worker, fix.ctx).await.unwrap();
    assert_eq!(fixed, 0);
    assert_eq!(read_counts(&fix.metadata, a), (0, 1));
    assert_eq!(read_counts(&fix.metadata, b), (1, 0));
}

#[tokio::test]
async fn under_counted_out_is_fixed() {
    let fix = build_fixture();
    let a = seed_memory_with_counts(&fix.metadata, 1, 0, 0); // stored=0
    let b = seed_memory_with_counts(&fix.metadata, 2, 0, 0);
    let c = seed_memory_with_counts(&fix.metadata, 3, 0, 0);
    seed_edge_raw(&fix.metadata, a, EdgeKind::FollowedBy, b);
    seed_edge_raw(&fix.metadata, a, EdgeKind::Caused, c);
    // a has 2 real outgoing but stored=0 → mismatch.

    let worker = CounterReconcileWorker::new().with_config(batchy_config(100));
    let fixed = run_one(&worker, fix.ctx).await.unwrap();
    assert!(fixed >= 1);
    assert_eq!(read_counts(&fix.metadata, a).0, 2);
}

#[tokio::test]
async fn over_counted_in_is_fixed() {
    let fix = build_fixture();
    let a = seed_memory_with_counts(&fix.metadata, 1, 0, 0);
    let b = seed_memory_with_counts(&fix.metadata, 2, 0, 5); // claims 5 in, has 1
    seed_edge_raw(&fix.metadata, a, EdgeKind::FollowedBy, b);

    let worker = CounterReconcileWorker::new().with_config(batchy_config(100));
    let fixed = run_one(&worker, fix.ctx).await.unwrap();
    assert!(fixed >= 1);
    assert_eq!(read_counts(&fix.metadata, b).1, 1);
}

#[tokio::test]
async fn mixed_drift_both_directions_fixed_in_one_cycle() {
    let fix = build_fixture();
    let a = seed_memory_with_counts(&fix.metadata, 1, 7, 0); // stored 7 out, real 1
    let b = seed_memory_with_counts(&fix.metadata, 2, 0, 9); // stored 9 in, real 1
    seed_edge_raw(&fix.metadata, a, EdgeKind::FollowedBy, b);

    let worker = CounterReconcileWorker::new().with_config(batchy_config(100));
    run_one(&worker, fix.ctx).await.unwrap();
    assert_eq!(read_counts(&fix.metadata, a), (1, 0));
    assert_eq!(read_counts(&fix.metadata, b), (0, 1));
}

#[tokio::test]
async fn multiple_memories_reconciled_in_one_cycle() {
    let fix = build_fixture();
    // 5 memories, each has 1 real outgoing edge to the next, all
    // stored with edges_out_count=0.
    let ids: Vec<MemoryId> = (1..=5)
        .map(|slot| seed_memory_with_counts(&fix.metadata, slot, 0, 0))
        .collect();
    for w in ids.windows(2) {
        seed_edge_raw(&fix.metadata, w[0], EdgeKind::FollowedBy, w[1]);
    }
    let worker = CounterReconcileWorker::new().with_config(batchy_config(100));
    let fixed = run_one(&worker, fix.ctx).await.unwrap();
    assert!(fixed >= 4); // ids[0..4] need fixing; ids[1..5] need in-fix too
    for (i, id) in ids.iter().enumerate().take(4) {
        assert_eq!(read_counts(&fix.metadata, *id).0, 1, "id[{i}] out");
    }
    for (i, id) in ids.iter().enumerate().skip(1) {
        assert_eq!(read_counts(&fix.metadata, *id).1, 1, "id[{i}] in");
    }
}

#[tokio::test]
async fn batch_size_caps_per_cycle() {
    let fix = build_fixture();
    // 20 memories all with drift; batch_size=5 → at most 5 fixed.
    for slot in 1..=20u64 {
        let id = seed_memory_with_counts(&fix.metadata, slot, 99, 99);
        // No real edges → reality is (0, 0); stored is (99, 99) → drift.
        let _ = id;
    }
    let worker = CounterReconcileWorker::new().with_config(batchy_config(5));
    let fixed = run_one(&worker, fix.ctx).await.unwrap();
    assert_eq!(fixed, 5);
}

#[tokio::test]
async fn cursor_advances_across_cycles() {
    let fix = build_fixture();
    for slot in 1..=12u64 {
        seed_memory_with_counts(&fix.metadata, slot, 7, 7);
    }
    let worker = CounterReconcileWorker::new().with_config(batchy_config(5));
    let c1 = run_one(&worker, fix.ctx.clone()).await.unwrap();
    let c2 = run_one(&worker, fix.ctx.clone()).await.unwrap();
    let c3 = run_one(&worker, fix.ctx.clone()).await.unwrap();
    assert_eq!(c1 + c2 + c3, 12, "all 12 fixed across cycles");
    for slot in 1..=12u64 {
        assert_eq!(read_counts(&fix.metadata, make_id(slot)), (0, 0));
    }
}

// ===========================================================================
// Worker integration (2).
// ===========================================================================

#[tokio::test]
async fn worker_registers_with_correct_kind_and_default_cadence() {
    let fix = build_fixture();
    let mut sched = WorkerScheduler::new();
    sched
        .register(Arc::new(CounterReconcileWorker::new()), fix.ctx)
        .unwrap();
    let cfg = sched.config(WorkerKind::CounterReconcile.name()).unwrap();
    assert_eq!(cfg.interval, Duration::from_secs(3600));
    sched.shutdown().await.unwrap();
}

#[tokio::test]
async fn disabled_worker_via_config_does_not_fix() {
    let fix = build_fixture();
    seed_memory_with_counts(&fix.metadata, 1, 99, 99); // drift
    let cfg = WorkerConfig {
        enabled: false,
        interval: Duration::from_millis(20),
        batch_size: 100,
        max_runtime: Duration::from_secs(1),
    };
    let mut sched = WorkerScheduler::new();
    sched
        .register(
            Arc::new(CounterReconcileWorker::new().with_config(cfg)),
            fix.ctx,
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    sched.shutdown().await.unwrap();
    assert_eq!(read_counts(&fix.metadata, make_id(1)), (99, 99));
}

// ===========================================================================
// Edge cases (1).
// ===========================================================================

#[tokio::test]
async fn empty_memories_table_cycle_is_noop() {
    let fix = build_fixture();
    let worker = CounterReconcileWorker::new();
    let fixed = run_one(&worker, fix.ctx).await.unwrap();
    assert_eq!(fixed, 0);
}
