#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send

//! AutoEdgeWorker integration tests — exercise the unified
//! `submit(Write)` path. Each cycle should emit a Phase::Link per
//! derived edge, WAL each one, commit the redb rows, and publish
//! EdgeAdded envelopes on the subscribe bus.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{EdgeKind, MemoryId, MemoryKind, SessionId, SpaceId};
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
    // Pack the slot into the real slot field (bits 64..112). Writing it into
    // the low bytes instead collapses to `MemoryId::NULL` because
    // `from_be_bytes` masks the low 32 reserved bits — so `make_id(1)` and
    // `make_id(2)` would both decode to the same id and seed one memory.
    MemoryId::pack(0, slot, 1)
}

async fn seed_memory_with_vec(fixture: &Fixture, slot: u64, vector: [f32; VECTOR_DIM]) -> MemoryId {
    seed_memory_with_vec_space(fixture, slot, vector, SpaceId::default()).await
}

/// Same as [`seed_memory_with_vec`], but stamps the memory's owning space
/// explicitly. `apply_upsert_memory` derives `MEMORIES_TABLE.space_id_bytes`
/// from the enclosing `Write`'s `space_id` — this is what lets the
/// `StageCompleted{AutoEdge}` space-id tests seed a real, non-default owner.
async fn seed_memory_with_vec_space(
    fixture: &Fixture,
    slot: u64,
    vector: [f32; VECTOR_DIM],
    space_id: SpaceId,
) -> MemoryId {
    use brain_core::Salience;
    use brain_ops::{Phase, Write, WriteId};

    let id = make_id(slot);
    let phase = Phase::UpsertMemory {
        id,
        text: format!("seed-{slot}"),
        vector: Box::new(vector),
        kind: MemoryKind::Episodic,
        salience: Salience::default(),
        session_id: SessionId(1),
        created_at_unix_nanos: now_unix_nanos(),
        occurred_at_unix_nanos: None,
        arena_slot: slot,
        embedding_model_fp: [0; 16],
        content_hash: None,
        deduplicate: false,
    };
    let write = Write::single(WriteId::new(), space_id, phase);
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

/// The `StageCompleted{AutoEdge}` envelope carries the source memory's
/// REAL owning `space_id` — not `SpaceId::default()` — so an space-scoped
/// SUBSCRIBE filter (`filter.spaces: [space]`) actually matches the
/// event. Regression coverage for the bug where the publish site stamped
/// the nil space unconditionally.
#[test]
fn cycle_publishes_stage_completed_with_real_owning_space_id() {
    glommio_run(|| async {
        let fix = build_fixture();
        let v = unit_vec(0);
        let owner = SpaceId::new();

        let m1 = seed_memory_with_vec_space(&fix, 1, v, owner).await;
        let _m2 = seed_memory_with_vec_space(&fix, 2, v, owner).await;

        let mut rx = fix.bus.receiver();
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
        assert!(processed > 0);

        let mut found = false;
        while let Ok(env) = rx.try_recv() {
            if env.event_type == brain_protocol::EventType::StageCompleted && env.memory_id == m1 {
                assert_eq!(
                    env.space_id, owner,
                    "StageCompleted{{AutoEdge}} must carry the memory's real \
                     owning space_id, not SpaceId::default()",
                );
                assert_ne!(env.space_id, SpaceId::default());
                found = true;
            }
        }
        assert!(found, "expected a StageCompleted{{AutoEdge}} for m1");
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
    let write = Write::single(id, SpaceId::default(), phase).with_request_hash(hash_a);
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

/// The cycle must merge the real derived-edge detail (target memory id +
/// cosine similarity) into the source memory's durable write-artifact
/// bundle — not just bump a count — so `MEMORY_INSPECT` can show which
/// specific memory got linked and how strongly. Regression coverage for the
/// gap where `auto_edge` never called into `brain_ops::memory_artifact`.
#[test]
fn cycle_merges_real_target_and_weight_into_artifact_bundle() {
    glommio_run(|| async {
        let fix = build_fixture();
        let v = unit_vec(0);

        let m1 = seed_memory_with_vec(&fix, 1, v).await;
        let m2 = seed_memory_with_vec(&fix, 2, v).await;

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
        assert!(processed > 0);

        let bundle = brain_ops::memory_artifact::read_memory_artifact(&fix.metadata, m1)
            .unwrap()
            .expect("artifact read must succeed");
        let graph = bundle
            .graph
            .expect("auto_edge must merge a graph fragment into m1's bundle");

        let edge = graph
            .edges
            .iter()
            .find(|e| e.kind == "similar_to")
            .expect("bundle must carry a similar_to edge, not just a count");
        assert_eq!(edge.source, m1.to_be_bytes());
        assert_eq!(
            edge.target,
            m2.to_be_bytes(),
            "bundle must name the real linked memory"
        );
        assert!(
            edge.confidence > 0.9,
            "bundle must carry the real cosine similarity (identical vectors), got {}",
            edge.confidence
        );

        assert!(
            graph.nodes.iter().any(|n| n.id == m2.to_be_bytes()),
            "linked memory must appear as a node"
        );
    });
}

/// Two similar memories in the SAME (namespace, space) scope must receive a
/// `SimilarTo` edge. Regression coverage for the bug where the worker
/// submitted its batch as `SpaceId::default()` (NIL): the apply layer's Link
/// tenant wall then failed `memory_in_space(real_mem, nil)` for every real
/// memory and silently dropped every derived edge.
#[test]
fn same_scope_similar_memories_get_edge() {
    glommio_run(|| async {
        let fix = build_fixture();
        let v = unit_vec(0);
        let owner = SpaceId::new(); // real, non-nil owning space

        let m1 = seed_memory_with_vec_space(&fix, 1, v, owner).await;
        let _m2 = seed_memory_with_vec_space(&fix, 2, v, owner).await;

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
        assert!(processed > 0, "worker drained the enqueue");

        // Symmetric SimilarTo writes two forward rows for the one logical
        // pair — the edge must actually exist, no longer no-op'd by the wall.
        let found = count_auto_derived(&fix);
        assert!(
            found >= 2,
            "same-scope similar memories must produce a SimilarTo edge \
             (got {found} AUTO_DERIVED rows)"
        );
    });
}

/// Two similar memories in DIFFERENT spaces must NOT be linked: a cross-scope
/// `SimilarTo` edge is itself a tenancy violation, so the worker drops the
/// pair before it ever reaches the write path. HNSW is shard-wide (scope
/// blind), so the neighbour is found — the scope guard is what excludes it.
#[test]
fn cross_scope_similar_memories_get_no_edge() {
    glommio_run(|| async {
        let fix = build_fixture();
        let v = unit_vec(0);
        let space_a = SpaceId::new();
        let space_b = SpaceId::new();
        assert_ne!(space_a, space_b);

        let m1 = seed_memory_with_vec_space(&fix, 1, v, space_a).await;
        let _m2 = seed_memory_with_vec_space(&fix, 2, v, space_b).await;

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
        assert!(processed > 0, "worker drained the enqueue");

        let found = count_auto_derived(&fix);
        assert_eq!(
            found, 0,
            "cross-scope similar memories must NOT be linked (got {found} \
             AUTO_DERIVED rows)"
        );
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
