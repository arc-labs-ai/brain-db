#![allow(clippy::arc_with_non_send_sync)]
//! AutoEdgeWorker integration tests.
//!
//! Each test drives the full writer -> channel -> worker -> redb edge
//! table pipeline. The writer is a `RealWriterHandle` with a wired
//! `flume::Sender<AutoEdgeEnqueue>`; the matching `Receiver` is handed
//! to the worker. Tests construct two `RealWriterHandle` instances
//! sharing the same HNSW + metadata so we can re-use `submit_encode`
//! end-to-end without spinning up the full shard.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use brain_core::{AgentId, ContextId, EdgeKind, EdgeKindRef, MemoryId, MemoryKind, NodeRef};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{EdgeKey, EDGES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{AutoEdgeEnqueue, OpsContext, RealWriterHandle};
use brain_planner::{EncodeOp, ExecutorContext, ForgetOp, SharedMetadataDb, WriterHandle};
use brain_protocol::request::ForgetMode;
use brain_workers::{AutoEdgeKnobs, AutoEdgeWorker, Worker, WorkerContext};
use parking_lot::Mutex;
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
    queue_rx: flume::Receiver<AutoEdgeEnqueue>,
    queue_tx: flume::Sender<AutoEdgeEnqueue>,
    _tempdir: tempfile::TempDir,
}

/// `capacity = 0` means "unbounded for this test"; we still bound it
/// large enough (4096) that no test legitimately overflows.
fn build_fixture_with_capacity(capacity: usize) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let (queue_tx, queue_rx) = flume::bounded(capacity.max(1));
    let mut real_writer = RealWriterHandle::new(metadata.clone(), hnsw_writer);
    real_writer.set_auto_edge_sender(queue_tx.clone());
    let writer: Arc<dyn WriterHandle> = Arc::new(real_writer);
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer,
    );
    Fixture {
        ctx: Arc::new(OpsContext::new(executor)),
        metadata,
        queue_rx,
        queue_tx,
        _tempdir: tempdir,
    }
}

fn build_fixture() -> Fixture {
    build_fixture_with_capacity(4096)
}

/// Build a deterministic unit vector that's distinct per `slot` but
/// "near" the seed vector for slot `near` when `near` is set. Lets
/// tests force HNSW knn into producing predictable neighbours.
fn dense_vec(slot: u64) -> [f32; VECTOR_DIM] {
    let mut v = [0.0f32; VECTOR_DIM];
    // Bucket each slot into one of 8 lobes so within-lobe encodes are
    // near each other and across-lobe encodes are far. Lobe index is
    // (slot % 8). Within a lobe we add small `slot`-keyed jitter so
    // distinct memories don't collapse to identical vectors (HNSW
    // would treat them as duplicates).
    let lobe = (slot % 8) as usize;
    v[lobe * 32] = 1.0;
    let jitter = ((slot / 8) as f32).mul_add(0.001, 0.001);
    v[lobe * 32 + 1] = jitter;
    normalise(&mut v);
    v
}

fn normalise(v: &mut [f32; VECTOR_DIM]) {
    let mut sq = 0f32;
    for x in v.iter() {
        sq += x * x;
    }
    if sq <= 0.0 {
        return;
    }
    let inv = sq.sqrt().recip();
    for x in v.iter_mut() {
        *x *= inv;
    }
}

fn encode_op(req_seed: u8, slot: u64, vector: [f32; VECTOR_DIM]) -> EncodeOp {
    EncodeOp {
        request_id: brain_core::RequestId::from([req_seed; 16]),
        context_id: ContextId(1),
        kind: MemoryKind::Episodic,
        text: format!("slot-{slot}"),
        vector,
        salience_initial: 0.5,
        fingerprint: [0; 16],
        edges: vec![],
        deduplicate: false,
        content_hash: [0; 32],
        agent_id: AgentId(Uuid::nil()),
    }
}

/// Send an `EncodeOp` through the writer trait so the auto-edge
/// enqueue path fires.
async fn submit_encode(ctx: &OpsContext, op: EncodeOp) -> MemoryId {
    ctx.executor
        .writer
        .submit_encode(op)
        .await
        .expect("encode")
        .memory_id
}

async fn run_one_cycle(
    worker: &AutoEdgeWorker,
    ctx: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let wctx = WorkerContext { ops: ctx, shutdown };
    worker.run_cycle(&wctx).await
}

/// Count `SimilarTo` edges originating from `from`.
fn count_similar_out(metadata: &SharedMetadataDb, from: MemoryId) -> usize {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(EDGES_TABLE).unwrap();
    let prefix = NodeRef::Memory(from).to_bytes();
    let upper: Vec<u8> = {
        let mut v = prefix.to_vec();
        v.push(0xFF);
        v
    };
    let mut total = 0usize;
    let iter = t.range(prefix.as_slice()..upper.as_slice()).unwrap();
    for entry in iter {
        let (key, _) = entry.unwrap();
        let decoded = EdgeKey::decode(key.value()).unwrap();
        if matches!(decoded.kind, EdgeKindRef::Builtin(EdgeKind::SimilarTo)) {
            total += 1;
        }
    }
    total
}

fn count_similar_total(metadata: &SharedMetadataDb) -> usize {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(EDGES_TABLE).unwrap();
    let mut total = 0usize;
    for entry in t.iter().unwrap() {
        let (key, _) = entry.unwrap();
        let decoded = EdgeKey::decode(key.value()).unwrap();
        if matches!(decoded.kind, EdgeKindRef::Builtin(EdgeKind::SimilarTo)) {
            total += 1;
        }
    }
    total
}

fn high_recall_knobs() -> AutoEdgeKnobs {
    AutoEdgeKnobs {
        top_k: 8,
        // Lobe-aligned vectors land at cosine ~0.999, so 0.85 keeps the
        // intra-lobe pair and rejects across-lobe (which sit at 0.0).
        similarity_threshold: 0.85,
        ef_search: Some(64),
    }
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("auto-edge-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// After an encoded event with two near-neighbours, the worker should
/// write a logical `SimilarTo` edge per neighbour. The auto-mirror
/// inside `edge::link` doubles the physical row count (forward +
/// mirrored forward).
#[test]
fn worker_writes_similarto_after_encoded_event() {
    glommio_run(|| async {
        let fix = build_fixture();
        // Three vectors in the same lobe -> dense pairwise similarity.
        let a = submit_encode(&fix.ctx, encode_op(1, 0, dense_vec(0))).await;
        let b = submit_encode(&fix.ctx, encode_op(2, 8, dense_vec(8))).await;
        let _c = submit_encode(&fix.ctx, encode_op(3, 16, dense_vec(16))).await;

        let worker = AutoEdgeWorker::new(fix.queue_rx.clone()).with_knobs(high_recall_knobs());
        let drained = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert!(drained >= 3, "expected >=3 enqueues drained, got {drained}");

        // Every memory has 2 active siblings within lobe 0 -> per
        // source up to 2 SimilarTo rows + 2 mirrored rows (rows landing
        // in EDGES_TABLE under the source's prefix include both
        // forwards-from-source AND mirrors-pointing-to-source).
        let out_a = count_similar_out(&fix.metadata, a);
        let out_b = count_similar_out(&fix.metadata, b);
        assert!(
            out_a >= 2 && out_b >= 2,
            "expected each anchor >=2 similarto rows, got a={out_a} b={out_b}"
        );
        // The triangle has 3 logical pairs * 2 mirror rows = 6 total.
        // Order of enqueue (a,b,c) determines what's visible to each
        // knn call, so we bound rather than equate.
        let total = count_similar_total(&fix.metadata);
        assert!(
            (4..=12).contains(&total),
            "auto-edge fanout out of expected bound: {total}"
        );
    });
}

/// A memory MUST NOT be linked to itself, even though HNSW returns
/// the self-hit as the top result.
#[test]
fn worker_skips_self_edges() {
    glommio_run(|| async {
        let fix = build_fixture();
        let a = submit_encode(&fix.ctx, encode_op(1, 0, dense_vec(0))).await;

        let worker = AutoEdgeWorker::new(fix.queue_rx.clone()).with_knobs(high_recall_knobs());
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();

        // Only memory in the index is `a`. The sole knn hit is a
        // itself -> filtered. Zero edges written.
        assert_eq!(
            count_similar_out(&fix.metadata, a),
            0,
            "self-similarity must not produce an edge"
        );
        assert_eq!(count_similar_total(&fix.metadata), 0);
    });
}

/// A neighbour whose similarity is below the threshold is dropped.
/// Forcing this requires two memories in DIFFERENT lobes — their
/// cosine ~ 0 so the 0.85 threshold rejects them.
#[test]
fn worker_skips_low_similarity_neighbours() {
    glommio_run(|| async {
        let fix = build_fixture();
        // Slot 0 -> lobe 0, slot 1 -> lobe 1: orthogonal axes.
        let a = submit_encode(&fix.ctx, encode_op(1, 0, dense_vec(0))).await;
        let b = submit_encode(&fix.ctx, encode_op(2, 1, dense_vec(1))).await;

        let worker = AutoEdgeWorker::new(fix.queue_rx.clone()).with_knobs(AutoEdgeKnobs {
            top_k: 5,
            similarity_threshold: 0.85,
            ef_search: Some(64),
        });
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();

        assert_eq!(count_similar_out(&fix.metadata, a), 0);
        assert_eq!(count_similar_out(&fix.metadata, b), 0);
        assert_eq!(count_similar_total(&fix.metadata), 0);
    });
}

/// A memory FORGOTTEN between enqueue and worker drain must produce
/// zero edges — the worker checks `is_tombstoned(source)` first.
#[test]
fn worker_skips_tombstoned_memory_anchors() {
    glommio_run(|| async {
        let fix = build_fixture();
        let a = submit_encode(&fix.ctx, encode_op(1, 0, dense_vec(0))).await;
        let _b = submit_encode(&fix.ctx, encode_op(2, 8, dense_vec(8))).await;

        // Soft-FORGET `a` BEFORE the worker drains its enqueue. The
        // writer's forget path tombstones the HNSW reader, so the
        // worker's `is_tombstoned` check fires.
        let _ = fix
            .ctx
            .executor
            .writer
            .submit_forget(ForgetOp {
                request_id: brain_core::RequestId::from([99; 16]),
                memory_id: a,
                mode: ForgetMode::Soft,
                agent_id: AgentId(Uuid::nil()),
            })
            .await
            .expect("forget a");

        let worker = AutoEdgeWorker::new(fix.queue_rx.clone()).with_knobs(high_recall_knobs());
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();

        // `a`'s enqueue is dropped (source tombstoned). `b`'s
        // enqueue runs but HNSW's search_active filters tombstoned
        // ids, so the only candidate (a) is also filtered.
        assert_eq!(count_similar_out(&fix.metadata, a), 0);
        assert_eq!(count_similar_total(&fix.metadata), 0);
    });
}

/// FORGET must NOT remove the auto-edges already written for the
/// forgotten memory — that's EdgeScrubWorker's job, not FORGET's.
#[test]
fn forget_does_not_remove_auto_edges() {
    glommio_run(|| async {
        let fix = build_fixture();
        let a = submit_encode(&fix.ctx, encode_op(1, 0, dense_vec(0))).await;
        let _b = submit_encode(&fix.ctx, encode_op(2, 8, dense_vec(8))).await;

        let worker = AutoEdgeWorker::new(fix.queue_rx.clone()).with_knobs(high_recall_knobs());
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        let before = count_similar_total(&fix.metadata);
        assert!(before > 0, "auto edges must be written before forget");

        let _ = fix
            .ctx
            .executor
            .writer
            .submit_forget(ForgetOp {
                request_id: brain_core::RequestId::from([99; 16]),
                memory_id: a,
                mode: ForgetMode::Soft,
                agent_id: AgentId(Uuid::nil()),
            })
            .await
            .expect("forget a");

        let after = count_similar_total(&fix.metadata);
        assert_eq!(
            after, before,
            "forget must leave auto edges untouched (edge_scrub cleans them)"
        );
    });
}

/// Draining the same enqueue twice (manually re-pushing it through the
/// channel) must end at the same edge state — `edge::link` overwrites
/// in place rather than duplicating rows.
#[test]
fn idempotent_redrive() {
    glommio_run(|| async {
        let fix = build_fixture();
        let _a = submit_encode(&fix.ctx, encode_op(1, 0, dense_vec(0))).await;
        let _b = submit_encode(&fix.ctx, encode_op(2, 8, dense_vec(8))).await;

        let worker = AutoEdgeWorker::new(fix.queue_rx.clone()).with_knobs(high_recall_knobs());
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        let first = count_similar_total(&fix.metadata);
        assert!(first > 0);

        // Re-enqueue the same anchors -> worker runs idempotently.
        fix.queue_tx.send((_a, dense_vec(0))).unwrap();
        fix.queue_tx.send((_b, dense_vec(8))).unwrap();
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();

        let second = count_similar_total(&fix.metadata);
        assert_eq!(
            second, first,
            "re-drive must overwrite existing edges, not duplicate them"
        );
    });
}

/// When the channel is full, the writer's try_send returns Full -> the
/// encode still succeeds and the drop bumps the tracing counter (we
/// observe it indirectly by asserting the encode succeeded + the
/// channel didn't grow beyond capacity + the dropped vector never
/// reached the worker).
#[test]
fn channel_full_drops_enqueue_metric_bumps() {
    glommio_run(|| async {
        // Capacity=1. First encode fills the channel; the second
        // encode triggers the drop path. Encode itself MUST still
        // succeed.
        let fix = build_fixture_with_capacity(1);
        let a = submit_encode(&fix.ctx, encode_op(1, 0, dense_vec(0))).await;
        let b = submit_encode(&fix.ctx, encode_op(2, 8, dense_vec(8))).await;

        // Channel was capped at 1; the first enqueue made it; second
        // was dropped by `try_send` -> queue depth stays at 1.
        assert_eq!(
            fix.queue_rx.len(),
            1,
            "second encode's enqueue must drop on full channel"
        );

        // Drain the worker. It only processes the one survivor (a),
        // which means b never enters its `to_link` set.
        let worker = AutoEdgeWorker::new(fix.queue_rx.clone()).with_knobs(high_recall_knobs());
        let drained = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(drained, 1, "only the survivor enqueue is drained");

        // `a` saw an empty HNSW (no other memories had been inserted
        // yet at the moment `a` enqueued), but the HNSW state during
        // the worker's drain has both a + b. So a's knn over the live
        // HNSW finds b -> one edge a->b.
        let total = count_similar_total(&fix.metadata);
        assert!(
            total >= 1,
            "anchor a should link to at least b, got {total}"
        );
        // b never enqueued -> no edges anchored at b in the forward
        // table (the mirror of a->b lands at b's row though, so we
        // count b's *outgoing* rows by looking only at edges whose
        // from == b. count_similar_out conflates outgoing and
        // mirrored-incoming, so we instead just assert b's enqueue
        // was dropped via the queue-depth check above).
        let _ = (a, b);
    });
}

// ─────────────────────────────────────────────────────────────────────
// Regression: zero-vector + NaN-similarity guards.
//
// These cover the production crash diagnosed at 2026-05-20: the
// NopDispatcher (still wired in brain-server until Phase 9.10 lands the
// real BGE embedder) hands every encode a [0; VECTOR_DIM] vector. As
// soon as ≥2 such memories live in HNSW, cosine similarity between any
// two of them computes 0/0 = NaN. Without the guards in
// `do_auto_edge_cycle`, NaN-weighted edges leak into write_auto_edges
// and downstream consumers crash on the non-finite f32.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn zero_vector_source_writes_no_edges() {
    glommio_run(|| async {
        let fix = build_fixture();
        // Two zero-vector encodes. submit_encode pre-computes the
        // vector (bypassing the embedder), so we can hand it exactly
        // the NopDispatcher signature [0.0; VECTOR_DIM] without going
        // through brain-ops::handle_encode.
        let zero: [f32; VECTOR_DIM] = [0.0; VECTOR_DIM];
        let a = submit_encode(&fix.ctx, encode_op(1, 0, zero)).await;
        let b = submit_encode(&fix.ctx, encode_op(2, 1, zero)).await;

        let worker = AutoEdgeWorker::new(fix.queue_rx.clone()).with_knobs(high_recall_knobs());
        // Must not panic. Before the guard, this drove cosine(0, 0) = NaN
        // into the link list, which then panicked write_auto_edges or
        // downstream HNSW maintenance on the non-finite weight.
        let drained = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(drained, 2, "both encodes are drained from the queue");

        // The guard skips the source memory entirely when its vector
        // is all zeros — no edges land in redb.
        assert_eq!(
            count_similar_out(&fix.metadata, a),
            0,
            "zero-vector source must not produce SimilarTo edges"
        );
        assert_eq!(
            count_similar_out(&fix.metadata, b),
            0,
            "zero-vector source must not produce SimilarTo edges"
        );
        assert_eq!(
            count_similar_total(&fix.metadata),
            0,
            "no SimilarTo rows of any kind"
        );
    });
}

#[test]
fn mixed_zero_and_real_vectors_only_real_produces_edges() {
    glommio_run(|| async {
        let fix = build_fixture();
        // One real, two zeros. The real vector's worker tick searches
        // HNSW and gets back the two zero points as neighbours; the
        // NaN-finite guard inside the hits loop rejects them. So the
        // real source produces zero edges (no finite-similarity
        // neighbour exists), the zero sources also produce zero edges
        // (rejected by the source-side zero guard). Net: zero edges,
        // zero panics.
        let real = dense_vec(0);
        let zero: [f32; VECTOR_DIM] = [0.0; VECTOR_DIM];
        let _r = submit_encode(&fix.ctx, encode_op(1, 0, real)).await;
        let _z1 = submit_encode(&fix.ctx, encode_op(2, 1, zero)).await;
        let _z2 = submit_encode(&fix.ctx, encode_op(3, 2, zero)).await;

        let worker = AutoEdgeWorker::new(fix.queue_rx.clone()).with_knobs(high_recall_knobs());
        let drained = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(drained, 3);
        assert_eq!(
            count_similar_total(&fix.metadata),
            0,
            "mixed zero/real produces no edges until a second real \
             vector lands; defends against the NaN cross-similarity \
             that would otherwise leak through"
        );
    });
}
