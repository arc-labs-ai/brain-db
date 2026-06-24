#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send

//! TemporalEdgeWorker integration tests — verify the worker emits one
//! `Phase::Link` per derived `FollowedBy` edge via `submit(Write)`, with
//! WAL coverage + subscribe events.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{origin as edge_origin, EDGES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::writer::wal_sink::RecordingWalSink;
use brain_ops::{EventBus, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_storage::wal::kinds::WalRecordKind;
use brain_workers::{TemporalEdgeKnobs, TemporalEdgeWorker, Worker, WorkerContext};
use redb::ReadableTable;
use uuid::Uuid;

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
    writer: Arc<RealWriterHandle>,
    metadata: SharedMetadataDb,
    sink: Arc<RecordingWalSink>,
    bus: Arc<EventBus>,
    sender: flume::Sender<brain_ops::TemporalEdgeEnqueue>,
    receiver: flume::Receiver<brain_ops::TemporalEdgeEnqueue>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let bus = Arc::new(EventBus::default());
    let sink = Arc::new(RecordingWalSink::new());
    let (tx, rx) = flume::bounded(64);
    let writer = Arc::new(
        RealWriterHandle::new(metadata.clone(), hnsw_writer)
            .with_event_bus(bus.clone())
            .with_wal_sink(sink.clone()),
    );
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer.clone() as Arc<dyn WriterHandle>,
    );
    let ctx = Arc::new(
        brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)
            .with_event_bus(bus.clone()),
    );
    Fixture {
        ctx,
        writer,
        metadata,
        sink,
        bus,
        sender: tx,
        receiver: rx,
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

async fn seed_memory(
    fixture: &Fixture,
    slot: u64,
    agent: AgentId,
    context_id: ContextId,
    created_at: u64,
) -> MemoryId {
    seed_memory_with_vec(
        fixture,
        slot,
        agent,
        context_id,
        created_at,
        [0.0; VECTOR_DIM],
    )
    .await
}

async fn seed_memory_with_vec(
    fixture: &Fixture,
    slot: u64,
    agent: AgentId,
    context_id: ContextId,
    created_at: u64,
    vec: [f32; VECTOR_DIM],
) -> MemoryId {
    use brain_core::Salience;
    use brain_ops::{Phase, Write, WriteId};
    let id = make_id(slot);
    let phase = Phase::UpsertMemory {
        id,
        text: format!("seed-{slot}"),
        vector: Box::new(vec),
        kind: MemoryKind::Episodic,
        salience: Salience::default(),
        context: context_id,
        created_at_unix_nanos: created_at,
        occurred_at_unix_nanos: None,
        arena_slot: slot,
        embedding_model_fp: [0; 16],
        content_hash: None,
        deduplicate: false,
    };
    let mut write = Write::single(WriteId::new(), agent, phase);
    write.agent_id = agent;
    fixture.writer.submit(write).await.expect("seed submit");
    id
}

fn glommio_run<F, Fut>(body: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .make()
        .unwrap()
        .run(async move { body().await });
}

#[test]
fn cycle_writes_followed_by_link_through_unified_path() {
    glommio_run(|| async {
        let fix = build_fixture();
        let agent = AgentId(Uuid::nil());
        let context_id = ContextId(1);

        // Two memories on the same agent + context, 1 second apart.
        let t0 = now_unix_nanos();
        let t1 = t0 + 1_000_000_000; // +1 s
        let m0 = seed_memory(&fix, 1, agent, context_id, t0).await;
        let m1 = seed_memory(&fix, 2, agent, context_id, t1).await;

        let mut rx = fix.bus.receiver();

        // Enqueue the SECOND memory — the worker walks the timeline back
        // to m0 and writes m0 → m1. Zero vector deliberately: the
        // topical gate skips when no usable embedding signal is
        // present, so this fixture exercises the same logical path it
        // did before the gate was added.
        fix.sender
            .try_send((m1, agent, context_id, t1, [0.0_f32; VECTOR_DIM]))
            .expect("enqueue");

        let worker = TemporalEdgeWorker::new(fix.receiver.clone()).with_knobs(TemporalEdgeKnobs {
            window_seconds: 300,
            weight_min: 0.1,
            cross_context: false,
            topical_threshold: 0.4,
        });
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wctx = WorkerContext {
            ops: fix.ctx.clone(),
            shutdown,
        };
        let processed = worker.run_cycle(&wctx).await.unwrap();
        assert!(processed > 0, "worker drained the enqueue");

        // 1. WAL sink saw at least one Link record (the derived FollowedBy).
        let appended = fix.sink.appended();
        let link_count = appended
            .iter()
            .filter(|r| r.kind == WalRecordKind::Link)
            .count();
        assert!(
            link_count >= 1,
            "expected at least one Link WAL record, got {link_count}",
        );

        // 2. Bus saw the EdgeAdded(AUTO_DERIVED) envelope for the FollowedBy.
        let mut saw_followed_by_auto_derived = false;
        while let Ok(env) = rx.try_recv() {
            if env.event_type == brain_protocol::EventType::EdgeAdded {
                let ep = env.edge_payload.as_ref().expect("edge payload");
                if ep.origin == edge_origin::AUTO_DERIVED {
                    saw_followed_by_auto_derived = true;
                }
            }
        }
        assert!(
            saw_followed_by_auto_derived,
            "bus must publish EdgeAdded(AUTO_DERIVED) for the derived FollowedBy edge"
        );

        // 3. redb has exactly one auto-derived FollowedBy row (asymmetric,
        //    no mirror).
        let rtxn = fix.metadata.read_txn().unwrap();
        let t = rtxn.open_table(EDGES_TABLE).unwrap();
        let mut found = 0;
        for entry in t.iter().unwrap() {
            let (_, v) = entry.unwrap();
            let data = v.value();
            if data.origin == edge_origin::AUTO_DERIVED {
                found += 1;
            }
        }
        assert_eq!(
            found, 1,
            "FollowedBy is asymmetric — exactly one auto-derived row"
        );

        let _ = m0;
    });
}

/// Two memories whose embeddings sit at cosine ≈ 0 (orthogonal). The
/// topical gate must refuse the `FollowedBy` derivation: same agent +
/// same context + in-window, but the content has no overlap.
#[test]
fn temporal_edge_drops_candidate_below_topical_threshold() {
    glommio_run(|| async {
        let fix = build_fixture();
        let agent = AgentId(Uuid::nil());
        let context_id = ContextId(1);

        // Orthogonal vectors (cosine = 0): m0 is "one in slot 0", m1
        // is "one in slot 1". HNSW's similarity for the pair is 0 —
        // well below the 0.4 default topical threshold.
        let mut v0 = [0.0_f32; VECTOR_DIM];
        let mut v1 = [0.0_f32; VECTOR_DIM];
        v0[0] = 1.0;
        v1[1] = 1.0;

        let t0 = now_unix_nanos();
        let t1 = t0 + 1_000_000_000; // +1 s — well inside the window
        let _m0 = seed_memory_with_vec(&fix, 1, agent, context_id, t0, v0).await;
        let m1 = seed_memory_with_vec(&fix, 2, agent, context_id, t1, v1).await;

        fix.sender
            .try_send((m1, agent, context_id, t1, v1))
            .expect("enqueue");

        let worker = TemporalEdgeWorker::new(fix.receiver.clone()).with_knobs(TemporalEdgeKnobs {
            window_seconds: 300,
            weight_min: 0.1,
            cross_context: false,
            topical_threshold: 0.4,
        });
        let metrics = worker.metrics();
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wctx = WorkerContext {
            ops: fix.ctx.clone(),
            shutdown,
        };
        let processed = worker.run_cycle(&wctx).await.unwrap();
        assert_eq!(processed, 1, "worker drained the enqueue");

        // No Link record because the topical gate dropped the edge —
        // the only WAL writes are the two seed Encodes.
        let appended = fix.sink.appended();
        let link_count = appended
            .iter()
            .filter(|r| r.kind == WalRecordKind::Link)
            .count();
        assert_eq!(
            link_count, 0,
            "below-topical predecessor must NOT produce a FollowedBy WAL record"
        );

        // redb has zero auto-derived rows for the same reason.
        let rtxn = fix.metadata.read_txn().unwrap();
        let t = rtxn.open_table(EDGES_TABLE).unwrap();
        let mut found = 0;
        for entry in t.iter().unwrap() {
            let (_, v) = entry.unwrap();
            if v.value().origin == edge_origin::AUTO_DERIVED {
                found += 1;
            }
        }
        assert_eq!(found, 0, "no auto-derived edges expected");

        // The skip counter records the drop reason — operators reading
        // metrics can distinguish "no predecessor" from "below topical".
        let snap = metrics.snapshot();
        assert_eq!(
            snap.skipped_below_topical, 1,
            "expected the BelowTopical counter to bump exactly once"
        );
        assert_eq!(snap.edges_written_total, 0);
    });
}

/// Same fixture shape as above but with two nearly-identical vectors
/// (cosine ≈ 1). The gate must let the edge through.
#[test]
fn temporal_edge_keeps_candidate_above_topical_threshold() {
    glommio_run(|| async {
        let fix = build_fixture();
        let agent = AgentId(Uuid::nil());
        let context_id = ContextId(1);

        let mut v = [0.0_f32; VECTOR_DIM];
        v[0] = 1.0;

        let t0 = now_unix_nanos();
        let t1 = t0 + 1_000_000_000;
        let _m0 = seed_memory_with_vec(&fix, 1, agent, context_id, t0, v).await;
        let m1 = seed_memory_with_vec(&fix, 2, agent, context_id, t1, v).await;

        fix.sender
            .try_send((m1, agent, context_id, t1, v))
            .expect("enqueue");

        let worker = TemporalEdgeWorker::new(fix.receiver.clone()).with_knobs(TemporalEdgeKnobs {
            window_seconds: 300,
            weight_min: 0.1,
            cross_context: false,
            topical_threshold: 0.4,
        });
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wctx = WorkerContext {
            ops: fix.ctx.clone(),
            shutdown,
        };
        worker.run_cycle(&wctx).await.unwrap();

        let appended = fix.sink.appended();
        let link_count = appended
            .iter()
            .filter(|r| r.kind == WalRecordKind::Link)
            .count();
        assert!(link_count >= 1, "above-topical predecessor must link");
    });
}
