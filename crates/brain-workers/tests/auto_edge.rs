#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send

//! AutoEdgeWorker integration tests — exercise the unified
//! `submit(Write)` path. Each cycle should emit a Phase::Link per
//! derived edge, WAL each one, commit the redb rows, and publish
//! EdgeAdded envelopes on the subscribe bus.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{origin as edge_origin, EDGES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::writer::wal_sink::RecordingWalSink;
use brain_ops::{EventBus, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_storage::wal::kinds::WalRecordKind;
use brain_workers::{
    AutoEdgeKnobs, AutoEdgeWorker, Worker, WorkerConfig, WorkerContext, WorkerScheduler,
};
use redb::ReadableTable;

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
    writer: Arc<RealWriterHandle>,
    metadata: SharedMetadataDb,
    sink: Arc<RecordingWalSink>,
    bus: Arc<EventBus>,
    sender: flume::Sender<brain_ops::AutoEdgeEnqueue>,
    receiver: flume::Receiver<brain_ops::AutoEdgeEnqueue>,
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

async fn seed_memory_with_vec(fixture: &Fixture, slot: u64, vector: [f32; VECTOR_DIM]) -> MemoryId {
    use brain_core::Salience;
    use brain_ops::{Phase, Write, WriteId};

    let id = make_id(slot);
    let phase = Phase::UpsertMemory {
        id,
        text: format!("seed-{slot}"),
        vector: Box::new(vector),
        kind: MemoryKind::Episodic,
        salience: Salience::default(),
        context: ContextId(1),
        created_at_unix_nanos: now_unix_nanos(),
        occurred_at_unix_nanos: None,
        arena_slot: slot,
        embedding_model_fp: [0; 16],
        content_hash: None,
        deduplicate: false,
    };
    let write = Write::single(WriteId::new(), AgentId::default(), phase);
    fixture.writer.submit(write).await.expect("seed submit");
    id
}

fn unit_vec(dim: usize) -> [f32; VECTOR_DIM] {
    let mut v = [0.0_f32; VECTOR_DIM];
    v[dim] = 1.0;
    v
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

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn cycle_writes_link_phase_through_unified_path() {
    glommio_run(|| async {
        let fix = build_fixture();
        let v = unit_vec(0);

        // Two memories that align (same vector → cosine 1.0). The worker
        // will derive a SimilarTo edge between them.
        let m1 = seed_memory_with_vec(&fix, 1, v).await;
        let _m2 = seed_memory_with_vec(&fix, 2, v).await;

        // Subscribe BEFORE draining so we observe the bus publish.
        let mut rx = fix.bus.receiver();

        // Trigger the worker by enqueueing m1's vector. It will knn-search
        // and find m2 above threshold.
        fix.sender.try_send((m1, v)).expect("enqueue");

        let worker = AutoEdgeWorker::new(fix.receiver.clone()).with_knobs(AutoEdgeKnobs {
            top_k: 5,
            similarity_threshold: 0.5,
            ef_search: Some(64),
        });
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wctx = WorkerContext {
            ops: fix.ctx.clone(),
            shutdown,
        };
        let processed = worker.run_cycle(&wctx).await.unwrap();
        assert!(processed > 0, "worker drained at least one enqueue");

        // 1. WAL sink saw a Link record beyond the seed Encode records.
        let appended = fix.sink.appended();
        let link_records: Vec<_> = appended
            .iter()
            .filter(|r| r.kind == WalRecordKind::Link)
            .collect();
        assert!(
            !link_records.is_empty(),
            "at least one Link WAL record per derived edge"
        );

        // 2. Subscribe bus saw an EdgeAdded envelope with AUTO_DERIVED origin.
        let mut saw_auto_derived_edge = false;
        while let Ok(env) = rx.try_recv() {
            if env.event_type == brain_protocol::EventType::EdgeAdded {
                let ep = env.edge_payload.as_ref().expect("edge payload");
                if ep.origin == edge_origin::AUTO_DERIVED {
                    saw_auto_derived_edge = true;
                }
            }
        }
        assert!(
            saw_auto_derived_edge,
            "bus must publish EdgeAdded(AUTO_DERIVED) per derived edge"
        );

        // 3. redb edges table contains the derived edge (symmetric mirror
        //    means two physical rows for one logical SimilarTo pair).
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
        assert!(
            found >= 2,
            "symmetric SimilarTo writes two forward rows, got {found}"
        );
    });
}

#[test]
fn deterministic_batch_hash_makes_retries_idempotent() {
    use brain_core::{EdgeKindRef, NodeRef};
    use brain_metadata::tables::edge::{derived_by, zero_disambiguator};
    use brain_ops::{Phase, Write, WriteId};

    // We rebuild the same `request_hash` two ways and compare. The
    // worker hashes the sorted (source, target) tuples; if we shuffle
    // the input vector the hash should still match.
    let pairs_a = vec![
        (make_id(1), make_id(2), 0.9_f32),
        (make_id(3), make_id(4), 0.8_f32),
    ];
    let pairs_b = vec![
        (make_id(3), make_id(4), 0.8_f32), // shuffled
        (make_id(1), make_id(2), 0.9_f32),
    ];
    let hash_a = hash_link_batch(&pairs_a);
    let hash_b = hash_link_batch(&pairs_b);
    assert_eq!(
        hash_a, hash_b,
        "batch hash must be invariant to drain ordering"
    );

    // Different pair set → different hash.
    let pairs_c = vec![(make_id(1), make_id(2), 0.9_f32)];
    let hash_c = hash_link_batch(&pairs_c);
    assert_ne!(hash_a, hash_c, "different pair sets must hash differently");

    // Build a real Phase::Link sequence and verify a Write with the
    // worker's hash + a deterministic WriteId round-trips.
    let phase = Phase::Link {
        from: NodeRef::Memory(make_id(1)),
        to: NodeRef::Memory(make_id(2)),
        kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
        weight: 0.9,
        origin: edge_origin::AUTO_DERIVED,
        derived_by: derived_by::SIMILARITY_WORKER,
        disambiguator: zero_disambiguator(),
        created_at_unix_nanos: now_unix_nanos(),
    };
    let id = WriteId::new();
    let write = Write::single(id, AgentId::default(), phase).with_request_hash(hash_a);
    assert_eq!(write.request_hash, Some(hash_a));
}

/// Replicates the worker's internal hash for the integration assert. Kept
/// crate-local so we can drive the round-trip without exposing the private
/// helper.
fn hash_link_batch(pairs: &[(MemoryId, MemoryId, f32)]) -> [u8; 32] {
    let mut sorted: Vec<(MemoryId, MemoryId)> = pairs.iter().map(|(s, t, _)| (*s, *t)).collect();
    sorted.sort();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"auto_edge:similar_to:v1");
    for (s, t) in &sorted {
        hasher.update(&s.to_be_bytes());
        hasher.update(&t.to_be_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// Pins the wake-on-enqueue contract: a worker registered with a
/// long interval (5 s) must still drain a fresh enqueue within
/// ~100 ms because the in-cycle `recv_async` blocks on the queue
/// — the scheduler doesn't periodically poll. Before the
/// `try_recv → recv_async` fix this test would hang for the full
/// interval, producing zero AUTO_DERIVED rows by the deadline.
#[test]
fn worker_drains_within_100ms_despite_5s_interval() {
    glommio_run(|| async {
        let fix = build_fixture();
        let v = unit_vec(0);

        // Two memories that align so the knn pass produces an edge.
        let m1 = seed_memory_with_vec(&fix, 1, v).await;
        let _m2 = seed_memory_with_vec(&fix, 2, v).await;

        let pre_edges = count_auto_derived(&fix);

        // Long interval. If the worker only drained on the periodic
        // tick, the test would have to wait 5 s; instead it must
        // unblock via the queue's own wakeup.
        let long_interval = WorkerConfig {
            enabled: true,
            interval: std::time::Duration::from_secs(5),
            batch_size: 32,
            max_runtime: std::time::Duration::from_secs(5),
        };
        let worker = AutoEdgeWorker::new(fix.receiver.clone())
            .with_config(long_interval)
            .with_knobs(AutoEdgeKnobs {
                top_k: 5,
                similarity_threshold: 0.5,
                ef_search: Some(64),
            });

        let mut sched = WorkerScheduler::new();
        sched.register(Arc::new(worker), fix.ctx.clone()).unwrap();

        // Enqueue AFTER the scheduler is running. The fix means the
        // worker is currently parked inside `recv_async`; the send
        // wakes it instantly.
        fix.sender.try_send((m1, v)).expect("enqueue");

        // Poll up to ~150 ms for the derived edge to land. The
        // tolerance covers redb commit + HNSW knn + WAL append.
        // Pre-fix this would never succeed (5 s interval).
        let started = std::time::Instant::now();
        let deadline = std::time::Duration::from_millis(150);
        let mut found = pre_edges;
        while started.elapsed() < deadline {
            found = count_auto_derived(&fix);
            if found > pre_edges {
                break;
            }
            glommio::timer::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            found > pre_edges,
            "auto_edge must drain enqueue within {} ms (found {found}, pre {pre_edges}); \
             pre-fix scheduler would have made this hang until the 5 s tick",
            deadline.as_millis(),
        );

        // Drop the scheduler — `shutdown().await` would wait up to
        // the 5 s drain budget for the worker that's currently
        // parked in `recv_async`. The test's assertion is already
        // proven; clean shutdown timing isn't what we're pinning.
        drop(sched);
    });
}

/// Count rows in `EDGES_TABLE` whose `origin == AUTO_DERIVED`. Used
/// by the wake-on-enqueue test as a side-effect probe.
fn count_auto_derived(fix: &Fixture) -> usize {
    let rtxn = fix.metadata.read_txn().unwrap();
    let t = rtxn.open_table(EDGES_TABLE).unwrap();
    let mut found = 0;
    for entry in t.iter().unwrap() {
        let (_, v) = entry.unwrap();
        if v.value().origin == edge_origin::AUTO_DERIVED {
            found += 1;
        }
    }
    found
}
