#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Access-boost worker integration tests.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{AccessBuffer, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{
    boosted_salience, AccessBoostWorker, Worker, WorkerConfig, WorkerContext, WorkerKind,
    WorkerScheduler, DEFAULT_BOOST_FACTOR, MAX_SALIENCE,
};
use redb::ReadableTable;
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

fn build_fixture_with_buffer(buffer: Arc<AccessBuffer>) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );
    let ctx = brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)
        .with_access_buffer(buffer);
    Fixture {
        ctx: Arc::new(ctx),
        metadata,
        _tempdir: tempdir,
    }
}

fn build_fixture() -> Fixture {
    build_fixture_with_buffer(Arc::new(AccessBuffer::default()))
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

fn seed_memory(metadata: &SharedMetadataDb, slot: u64, salience: f32) -> MemoryId {
    let id = make_id(slot);
    let wtxn = metadata.write_txn().unwrap();
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        let meta = MemoryMetadata::new_active(
            id,
            AgentId(Uuid::nil()),
            ContextId(1),
            slot,
            1,
            MemoryKind::Episodic,
            [0; 16],
            salience,
            16,
            now_unix_nanos(),
        );
        table.insert(id.to_be_bytes(), meta).unwrap();
    }
    wtxn.commit().unwrap();
    id
}

fn read_meta(metadata: &SharedMetadataDb, id: MemoryId) -> Option<MemoryMetadata> {
    let rtxn = metadata.read_txn().unwrap();
    let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
    table.get(id.to_be_bytes()).unwrap().map(|a| a.value())
}

async fn run_cycle(
    worker: &AccessBoostWorker,
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
// Pure-function (3).
// ===========================================================================

#[test]
fn boost_50_percent_to_55_percent() {
    assert!((boosted_salience(0.5, 0.10) - 0.55).abs() < 1e-6);
}

#[test]
fn boost_caps_at_one() {
    let r = boosted_salience(0.95, 0.10);
    assert!(r <= MAX_SALIENCE);
    assert!((r - MAX_SALIENCE).abs() < 1e-6);
    assert_eq!(boosted_salience(1.0, 0.10), 1.0);
}

#[test]
fn boost_of_zero_stays_zero() {
    assert_eq!(boosted_salience(0.0, 0.10), 0.0);
}

#[test]
fn default_boost_factor_is_ten_percent() {
    assert!((DEFAULT_BOOST_FACTOR - 0.10).abs() < 1e-6);
}

// ===========================================================================
// Buffer (3).
// ===========================================================================

#[test]
fn buffer_dedups_records() {
    let buf = AccessBuffer::new(100);
    let id = make_id(1);
    buf.record(id);
    buf.record(id);
    buf.record(id);
    assert_eq!(buf.len(), 1);
    let drained = buf.drain();
    assert_eq!(drained.len(), 1);
}

#[test]
fn buffer_overflow_drops_and_increments_counter() {
    let buf = AccessBuffer::new(4);
    for i in 1..=10 {
        buf.record(make_id(i));
    }
    assert!(buf.len() <= 4);
    assert!(buf.overflowed_count() > 0);
}

#[test]
fn drain_empties_buffer() {
    let buf = AccessBuffer::new(100);
    buf.record(make_id(1));
    buf.record(make_id(2));
    let first = buf.drain();
    assert_eq!(first.len(), 2);
    assert_eq!(buf.len(), 0);
    let second = buf.drain();
    assert!(second.is_empty());
}

// ===========================================================================
// Cycle (5).
// ===========================================================================

#[test]
fn cycle_boosts_one_recorded_memory() {
    glommio_run(|| async {
        let fix = build_fixture();
        let id = seed_memory(&fix.metadata, 1, 0.5);
        fix.ctx.access_buffer.record(id);

        let worker = AccessBoostWorker::new();
        let processed = run_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 1);
        let m = read_meta(&fix.metadata, id).unwrap();
        assert!(
            (m.salience - 0.55).abs() < 1e-3,
            "expected ~0.55, got {}",
            m.salience
        );
    });
}

#[test]
fn cycle_caps_at_one() {
    glommio_run(|| async {
        let fix = build_fixture();
        let id = seed_memory(&fix.metadata, 1, 0.95);
        fix.ctx.access_buffer.record(id);

        let worker = AccessBoostWorker::new();
        let processed = run_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 1);
        let m = read_meta(&fix.metadata, id).unwrap();
        assert!((m.salience - 1.0).abs() < 1e-6, "got {}", m.salience);
    });
}

#[test]
fn cycle_skips_missing_memory() {
    glommio_run(|| async {
        let fix = build_fixture();
        fix.ctx.access_buffer.record(make_id(9999));

        let worker = AccessBoostWorker::new();
        let processed = run_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0, "missing memory must be silently skipped");
    });
}

#[test]
fn cycle_increments_access_count() {
    glommio_run(|| async {
        let fix = build_fixture();
        let id = seed_memory(&fix.metadata, 1, 0.5);

        fix.ctx.access_buffer.record(id);
        let worker = AccessBoostWorker::new();
        run_cycle(&worker, fix.ctx.clone()).await.unwrap();
        let m1 = read_meta(&fix.metadata, id).unwrap();
        assert_eq!(m1.access_count, 1);

        fix.ctx.access_buffer.record(id);
        run_cycle(&worker, fix.ctx).await.unwrap();
        let m2 = read_meta(&fix.metadata, id).unwrap();
        assert_eq!(m2.access_count, 2);
    });
}

#[test]
fn cycle_requeues_overflow_when_batch_too_small() {
    glommio_run(|| async {
        let fix = build_fixture();
        // 15 memories seeded; we record all of them.
        for slot in 1..=15 {
            seed_memory(&fix.metadata, slot, 0.5);
            fix.ctx.access_buffer.record(make_id(slot));
        }
        assert_eq!(fix.ctx.access_buffer.len(), 15);

        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(1),
            batch_size: 10,
            max_runtime: Duration::from_secs(60),
        };
        let worker = AccessBoostWorker::new().with_config(cfg);
        let processed = run_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(processed, 10, "first cycle boosts 10 of 15");
        assert_eq!(
            fix.ctx.access_buffer.len(),
            5,
            "remaining 5 must be re-queued"
        );
    });
}

#[test]
fn empty_buffer_cycle_is_noop() {
    glommio_run(|| async {
        let fix = build_fixture();
        let worker = AccessBoostWorker::new();
        let processed = run_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
    });
}

// ===========================================================================
// Worker integration (1).
// ===========================================================================

#[test]
fn worker_registers_with_correct_kind_and_default_cadence() {
    glommio_run(|| async {
        let fix = build_fixture();
        let mut sched = WorkerScheduler::new();
        sched
            .register(Arc::new(AccessBoostWorker::new()), fix.ctx)
            .unwrap();
        let cfg = sched.config(WorkerKind::AccessBoost.name()).unwrap();
        assert_eq!(cfg.interval, Duration::from_secs(10));
        assert!(cfg.enabled);
        sched.shutdown().await.unwrap();
    });
}

// ===========================================================================
// Cross-handler integration: RECALL fills buffer, boost worker applies.
// ===========================================================================

#[test]
fn recall_fills_buffer_then_boost_worker_applies() {
    glommio_run(|| async {
        use brain_ops::dispatch;
        use brain_ops::test_support::single_body;
        use brain_protocol::envelope::request::{
            EncodeRequest, MemoryKindWire, RecallRequest, RequestBody,
        };
        use brain_protocol::envelope::response::ResponseBody;

        // Build a fixture with a real MockDispatcher so encode/recall
        // actually produce vectors.
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
                [0xAB; 16]
            }
        }

        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("metadata.redb");
        let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let executor = ExecutorContext::new(
            Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata.clone(),
            writer as Arc<dyn WriterHandle>,
        );
        let ctx = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));

        // Encode two memories.
        let encode_req = |rid: [u8; 16], text: &str| EncodeRequest {
            text: text.into(),
            context_id: 1,
            kind: MemoryKindWire::Episodic,
            salience_hint: 0.5,
            edges: vec![],
            request_id: rid,
            txn_id: None,
            deduplicate: false,
        };
        let _ = dispatch(
            RequestBody::Encode(encode_req([1; 16], "alpha")),
            brain_ops::RequestCaller::anonymous(),
            &ctx,
        )
        .await
        .unwrap();
        let _ = dispatch(
            RequestBody::Encode(encode_req([2; 16], "beta")),
            brain_ops::RequestCaller::anonymous(),
            &ctx,
        )
        .await
        .unwrap();

        assert_eq!(
            ctx.access_buffer.len(),
            0,
            "encode must not fill the buffer"
        );

        // RECALL fills the buffer.
        let recall = RecallRequest {
            cue_text: "alpha".into(),
            top_k: 5,
            confidence_threshold: 0.0,
            context_filter: None,
            age_bound_unix_nanos: None,
            kind_filter: None,
            salience_floor: 0.0,
            include_edges: false,
            include_graph: false,
            include_text: false,
            request_id: None,
            txn_id: None,
            agent_filter: Vec::new(),
            include_other_agents: false,
        };
        let outcome = dispatch(
            RequestBody::Recall(recall),
            brain_ops::RequestCaller::anonymous(),
            &ctx,
        )
        .await
        .unwrap();
        let n_results = match single_body(outcome) {
            ResponseBody::Recall(r) => r.results.len(),
            _ => unreachable!(),
        };
        assert!(n_results >= 1);
        assert_eq!(
            ctx.access_buffer.len(),
            n_results,
            "RECALL must record every returned hit"
        );

        // Run the boost worker via scheduler.
        let mut sched = WorkerScheduler::new();
        sched
            .register(
                Arc::new(AccessBoostWorker::new().with_config(WorkerConfig {
                    enabled: true,
                    interval: Duration::from_millis(20),
                    batch_size: 100,
                    max_runtime: Duration::from_secs(1),
                })),
                ctx,
            )
            .unwrap();
        let metrics = sched.metrics(WorkerKind::AccessBoost.name()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if metrics.processed_total.load(Ordering::Relaxed) >= n_results as u64 {
                break;
            }
            glommio::timer::sleep(Duration::from_millis(5)).await;
        }
        sched.shutdown().await.unwrap();

        let processed = metrics.processed_total.load(Ordering::Relaxed);
        assert!(
            processed >= n_results as u64,
            "expected at least {n_results} boosts, got {processed}"
        );

        // Confirm the boosted salience landed on at least one row.
        let alpha = read_meta(&metadata, make_id(1));
        // memory_id assignment depends on writer; can't pin slot=1 here.
        // Instead: scan all memories and require at least one has salience > 0.5.
        let rtxn = metadata.read_txn().unwrap();
        let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let mut any_boosted = false;
        for entry in table.iter().unwrap() {
            let (_, v) = entry.unwrap();
            if v.value().salience > 0.5 {
                any_boosted = true;
                break;
            }
        }
        assert!(any_boosted, "at least one memory must show salience > 0.5");
        let _ = alpha;
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
