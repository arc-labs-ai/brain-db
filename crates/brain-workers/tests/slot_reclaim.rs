#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send post-9.7 (audit §4)
//! Slot reclamation worker tests (sub-task 8.7). Spec §11/06.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{derived_by, origin, EdgeData, EDGES_IN_TABLE, EDGES_OUT_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{dispatch, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::request::{
    EncodeRequest, ForgetMode, ForgetRequest, MemoryKindWire, RequestBody,
};
use brain_protocol::response::ResponseBody;
use brain_workers::{
    SlotReclamationWorker, Worker, WorkerConfig, WorkerContext, WorkerKind, WorkerScheduler,
};
use parking_lot::Mutex;
use redb::ReadableTable;
use uuid::Uuid;

const DAY_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

// ---------------------------------------------------------------------------
// Fixture.
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
        Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
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

fn seed_memory(
    metadata: &SharedMetadataDb,
    slot: u64,
    tombstoned_at_unix_nanos: Option<u64>,
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
        meta.tombstoned_at_unix_nanos = tombstoned_at_unix_nanos;
        table.insert(id.to_be_bytes(), meta).unwrap();
    }
    wtxn.commit().unwrap();
    id
}

fn seed_edge(metadata: &SharedMetadataDb, src: MemoryId, kind: EdgeKind, tgt: MemoryId) {
    let mut db = metadata.lock();
    let wtxn = db.write_txn().unwrap();
    {
        let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let mut in_ = wtxn.open_table(EDGES_IN_TABLE).unwrap();
        let data = EdgeData::new(1.0, origin::EXPLICIT, derived_by::CLIENT, now_unix_nanos());
        brain_metadata::tables::edge::link(&mut out, &mut in_, src, kind, tgt, &data).unwrap();
    }
    wtxn.commit().unwrap();
}

fn count_memories(metadata: &SharedMetadataDb) -> usize {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
    table.iter().unwrap().count()
}

fn memory_exists(metadata: &SharedMetadataDb, id: MemoryId) -> bool {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
    table.get(id.to_be_bytes()).unwrap().is_some()
}

fn read_meta(metadata: &SharedMetadataDb, id: MemoryId) -> Option<MemoryMetadata> {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
    table.get(id.to_be_bytes()).unwrap().map(|a| a.value())
}

fn edges_out_count(metadata: &SharedMetadataDb, src: MemoryId) -> usize {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let table = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
    let lo = (src.to_be_bytes(), 0u8, [0u8; 16]);
    let hi = (src.to_be_bytes(), u8::MAX, [0xFFu8; 16]);
    table.range(lo..=hi).unwrap().count()
}

fn edges_in_count(metadata: &SharedMetadataDb, tgt: MemoryId) -> usize {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let table = rtxn.open_table(EDGES_IN_TABLE).unwrap();
    let lo = (tgt.to_be_bytes(), 0u8, [0u8; 16]);
    let hi = (tgt.to_be_bytes(), u8::MAX, [0xFFu8; 16]);
    table.range(lo..=hi).unwrap().count()
}

async fn run_one(
    worker: &SlotReclamationWorker,
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
// Cycle behaviour (8).
// ===========================================================================

#[test]
fn tombstoned_past_grace_is_reclaimed() {
    glommio_run(|| async {
        let fix = build_fixture();
        let id = seed_memory(&fix.metadata, 1, Some(now_unix_nanos() - 8 * DAY_NS));
        let worker = SlotReclamationWorker::new(); // 7-day default grace
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 1);
        assert!(!memory_exists(&fix.metadata, id));
    });
}

#[test]
fn tombstoned_within_grace_is_kept() {
    glommio_run(|| async {
        let fix = build_fixture();
        let id = seed_memory(&fix.metadata, 1, Some(now_unix_nanos() - 3 * DAY_NS));
        let worker = SlotReclamationWorker::new();
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
        assert!(memory_exists(&fix.metadata, id));
    });
}

#[test]
fn active_memory_never_reclaimed() {
    glommio_run(|| async {
        let fix = build_fixture();
        let id = seed_memory(&fix.metadata, 1, None);
        let worker = SlotReclamationWorker::new();
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
        assert!(memory_exists(&fix.metadata, id));
    });
}

#[test]
fn multiple_eligible_rows_all_reclaimed_within_batch_size() {
    glommio_run(|| async {
        let fix = build_fixture();
        for slot in 1..=5u64 {
            seed_memory(&fix.metadata, slot, Some(now_unix_nanos() - 10 * DAY_NS));
        }
        assert_eq!(count_memories(&fix.metadata), 5);
        let worker = SlotReclamationWorker::new();
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 5);
        assert_eq!(count_memories(&fix.metadata), 0);
    });
}

#[test]
fn batch_size_caps_per_cycle() {
    glommio_run(|| async {
        let fix = build_fixture();
        for slot in 1..=50u64 {
            seed_memory(&fix.metadata, slot, Some(now_unix_nanos() - 10 * DAY_NS));
        }
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(60),
            batch_size: 10,
            max_runtime: Duration::from_secs(60),
        };
        let worker = SlotReclamationWorker::new().with_config(cfg);
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 10);
        assert_eq!(count_memories(&fix.metadata), 40);
    });
}

#[test]
fn adjacent_out_edges_purged() {
    glommio_run(|| async {
        let fix = build_fixture();
        let doomed = seed_memory(&fix.metadata, 1, Some(now_unix_nanos() - 10 * DAY_NS));
        let live = seed_memory(&fix.metadata, 2, None);
        seed_edge(&fix.metadata, doomed, EdgeKind::FollowedBy, live);
        assert_eq!(edges_out_count(&fix.metadata, doomed), 1);

        let worker = SlotReclamationWorker::new();
        run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(
            edges_out_count(&fix.metadata, doomed),
            0,
            "EDGES_OUT for reclaimed source must be purged"
        );
    });
}

#[test]
fn adjacent_in_edges_purged() {
    glommio_run(|| async {
        let fix = build_fixture();
        let doomed = seed_memory(&fix.metadata, 1, Some(now_unix_nanos() - 10 * DAY_NS));
        let live = seed_memory(&fix.metadata, 2, None);
        // Edge from live → doomed. EDGES_IN[doomed][..][live] holds.
        seed_edge(&fix.metadata, live, EdgeKind::FollowedBy, doomed);
        assert_eq!(edges_in_count(&fix.metadata, doomed), 1);

        let worker = SlotReclamationWorker::new();
        run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(
            edges_in_count(&fix.metadata, doomed),
            0,
            "EDGES_IN for reclaimed target must be purged"
        );
    });
}

#[test]
fn dangling_edges_other_direction_are_left_for_edge_scrub() {
    glommio_run(|| async {
        let fix = build_fixture();
        let doomed = seed_memory(&fix.metadata, 1, Some(now_unix_nanos() - 10 * DAY_NS));
        let live = seed_memory(&fix.metadata, 2, None);
        // Edge from live → doomed. After reclamation:
        //   - EDGES_OUT[live][..][doomed] (source = live) MUST survive
        //     — spec §6 leaves this for the edge-scrub worker.
        //   - EDGES_IN[doomed][..][live] is purged (verified in test 7).
        seed_edge(&fix.metadata, live, EdgeKind::FollowedBy, doomed);
        assert_eq!(edges_out_count(&fix.metadata, live), 1);

        let worker = SlotReclamationWorker::new();
        run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(
            edges_out_count(&fix.metadata, live),
            1,
            "dangling EDGES_OUT survives slot reclamation (edge scrub's job)"
        );
    });
}

// ===========================================================================
// FORGET stamping integration (2).
// ===========================================================================

#[test]
fn forget_stamps_tombstoned_at_unix_nanos() {
    glommio_run(|| async {
        let fix = build_fixture();
        // Real ENCODE → real FORGET via dispatcher.
        let encode = EncodeRequest {
            text: "doomed".into(),
            context_id: 1,
            kind: MemoryKindWire::Episodic,
            salience_hint: 0.5,
            edges: vec![],
            request_id: [1; 16],
            txn_id: None,
            deduplicate: false,
        };
        let memory_id = match dispatch(RequestBody::Encode(encode), &fix.ctx)
            .await
            .unwrap()
        {
            ResponseBody::Encode(r) => r.memory_id,
            _ => unreachable!(),
        };
        let forget = ForgetRequest {
            memory_id,
            mode: ForgetMode::Soft,
            request_id: [2; 16],
            txn_id: None,
        };
        let _ = dispatch(RequestBody::Forget(forget), &fix.ctx)
            .await
            .unwrap();

        let row = read_meta(&fix.metadata, MemoryId::from(memory_id)).unwrap();
        assert!(
            row.tombstoned_at_unix_nanos.is_some(),
            "FORGET must stamp tombstoned_at"
        );
    });
}

#[test]
fn forget_replay_does_not_overwrite_stamp() {
    glommio_run(|| async {
        let fix = build_fixture();
        let encode = EncodeRequest {
            text: "doomed-twice".into(),
            context_id: 1,
            kind: MemoryKindWire::Episodic,
            salience_hint: 0.5,
            edges: vec![],
            request_id: [10; 16],
            txn_id: None,
            deduplicate: false,
        };
        let memory_id = match dispatch(RequestBody::Encode(encode), &fix.ctx)
            .await
            .unwrap()
        {
            ResponseBody::Encode(r) => r.memory_id,
            _ => unreachable!(),
        };
        // Two FORGET calls with different request_ids → first stamps,
        // second is AlreadyTombstoned (no metadata write).
        for rid in [[11u8; 16], [12u8; 16]] {
            let _ = dispatch(
                RequestBody::Forget(ForgetRequest {
                    memory_id,
                    mode: ForgetMode::Soft,
                    request_id: rid,
                    txn_id: None,
                }),
                &fix.ctx,
            )
            .await
            .unwrap();
        }
        let row = read_meta(&fix.metadata, MemoryId::from(memory_id)).unwrap();
        let stamp = row.tombstoned_at_unix_nanos.unwrap();
        // Wait briefly and re-check that the stamp didn't shift.
        glommio::timer::sleep(Duration::from_millis(10)).await;
        let row2 = read_meta(&fix.metadata, MemoryId::from(memory_id)).unwrap();
        assert_eq!(row2.tombstoned_at_unix_nanos, Some(stamp));
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
            .register(Arc::new(SlotReclamationWorker::new()), fix.ctx)
            .unwrap();
        let cfg = sched.config(WorkerKind::SlotReclamation.name()).unwrap();
        assert_eq!(cfg.interval, Duration::from_secs(600));
        sched.shutdown().await.unwrap();
    });
}

#[test]
fn disabled_worker_via_config_does_not_reclaim() {
    glommio_run(|| async {
        let fix = build_fixture();
        seed_memory(&fix.metadata, 1, Some(now_unix_nanos() - 10 * DAY_NS));
        let cfg = WorkerConfig {
            enabled: false,
            interval: Duration::from_millis(20),
            batch_size: 100,
            max_runtime: Duration::from_secs(1),
        };
        let mut sched = WorkerScheduler::new();
        sched
            .register(
                Arc::new(SlotReclamationWorker::new().with_config(cfg)),
                fix.ctx.clone(),
            )
            .unwrap();
        glommio::timer::sleep(Duration::from_millis(150)).await;
        sched.shutdown().await.unwrap();
        assert_eq!(count_memories(&fix.metadata), 1);
    });
}

#[test]
fn custom_grace_period_honoured() {
    glommio_run(|| async {
        let fix = build_fixture();
        // 2-day-old tombstone — under default 7d grace, kept.
        let id = seed_memory(&fix.metadata, 1, Some(now_unix_nanos() - 2 * DAY_NS));
        let default = SlotReclamationWorker::new();
        let processed = run_one(&default, fix.ctx.clone()).await.unwrap();
        assert_eq!(processed, 0);
        assert!(memory_exists(&fix.metadata, id));

        // Drop grace to 1 day; now eligible.
        let short = SlotReclamationWorker::new().with_grace_period(Duration::from_secs(24 * 3600));
        let processed = run_one(&short, fix.ctx).await.unwrap();
        assert_eq!(processed, 1);
        assert!(!memory_exists(&fix.metadata, id));
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
