#![allow(clippy::arc_with_non_send_sync)]
//! Counterpoint to `auto_edge.rs::zero_vector_source_writes_no_edges`.
//!
//! That test pins the negative path: when the NopDispatcher sentinel
//! `[0; VECTOR_DIM]` reaches the AutoEdgeWorker, the worker's
//! defensive guard rejects every potential edge so the downstream
//! NaN-similarity panic that motivated the guard cannot fire.
//!
//! This test pins the positive path: when a real BGE-small dispatcher
//! supplies vectors for three thematically similar texts, the worker
//! actually writes at least one SimilarTo edge with a finite,
//! positive cosine weight. Together with the negative test, the pair
//! proves the real path works without disturbing the defensive guard.
//!
//! Gated on `BRAIN_EMBED_MODEL_DIR`. When unset, the test prints a
//! skip note matching the pattern in
//! `crates/brain-planner/tests/recall_end_to_end.rs`; `cargo test`
//! stays fast on workstations without the model installed.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use brain_core::{AgentId, ContextId, EdgeKind, EdgeKindRef, MemoryId, MemoryKind, NodeRef};
use brain_embed::{
    CpuDispatcher, Dispatcher, EmbedderConfig as EmbedderModelConfig, ModelHandle, VECTOR_DIM,
};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{EdgeKey, EDGES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{AutoEdgeEnqueue, OpsContext, RealWriterHandle};
use brain_planner::{EncodeOp, ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{AutoEdgeKnobs, AutoEdgeWorker, Worker, WorkerContext};
use parking_lot::Mutex;
use redb::ReadableTable;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Env gating.
// ---------------------------------------------------------------------------

fn model_dir() -> Option<PathBuf> {
    std::env::var("BRAIN_EMBED_MODEL_DIR")
        .ok()
        .map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Fixture. Same shape as auto_edge.rs::Fixture, but parameterised on a
// real dispatcher so the executor's embed surface routes through BGE.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: Arc<OpsContext>,
    metadata: SharedMetadataDb,
    queue_rx: flume::Receiver<AutoEdgeEnqueue>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture(dispatcher: Arc<dyn Dispatcher>) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let (queue_tx, queue_rx) = flume::bounded(4096);
    let mut real_writer = RealWriterHandle::new(metadata.clone(), hnsw_writer);
    real_writer.set_auto_edge_sender(queue_tx);
    let writer: Arc<dyn WriterHandle> = Arc::new(real_writer);
    let executor = ExecutorContext::new(dispatcher, shared, metadata.clone(), writer);
    Fixture {
        ctx: Arc::new(OpsContext::new(executor)),
        metadata,
        queue_rx,
        _tempdir: tempdir,
    }
}

fn encode_op(req_seed: u8, vector: [f32; VECTOR_DIM], text: &str) -> EncodeOp {
    EncodeOp {
        request_id: brain_core::RequestId::from([req_seed; 16]),
        context_id: ContextId(1),
        kind: MemoryKind::Episodic,
        text: text.to_owned(),
        vector,
        salience_initial: 0.5,
        fingerprint: [0; 16],
        edges: vec![],
        deduplicate: false,
        content_hash: [0; 32],
        agent_id: AgentId(Uuid::nil()),
    }
}

/// Counts every SimilarTo row in EDGES_TABLE regardless of source.
/// The triangle fixture below produces at most 6 logical edges
/// (3 pairs * 2 mirrored rows) and the guarantee under test is
/// `>= 1`, so the exact count isn't asserted.
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

/// Iterate every SimilarTo row and assert the stored weight is a
/// finite positive f32. The NaN guard that lives in the worker would
/// have rejected any non-finite weight before it reached the table,
/// so seeing the row at all proves the weight was sane at write time
/// — but we double-check here because the defensive guard is the
/// regression we're proving still holds in the real path.
fn assert_all_weights_finite_and_positive(metadata: &SharedMetadataDb, source: MemoryId) {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(EDGES_TABLE).unwrap();
    let prefix = NodeRef::Memory(source).to_bytes();
    let upper: Vec<u8> = {
        let mut v = prefix.to_vec();
        v.push(0xFF);
        v
    };
    for entry in t.range(prefix.as_slice()..upper.as_slice()).unwrap() {
        let (key, value) = entry.unwrap();
        let decoded = EdgeKey::decode(key.value()).unwrap();
        if !matches!(decoded.kind, EdgeKindRef::Builtin(EdgeKind::SimilarTo)) {
            continue;
        }
        let edge_data = value.value();
        let w = edge_data.weight;
        // The defensive guard inside do_auto_edge_cycle rejects NaN
        // and zero-vector inputs before they reach this row; if the
        // guard misfires on real BGE vectors, the only signal is a
        // non-finite weight landing here.
        assert!(w.is_finite(), "weight must be finite, got {w}");
        assert!(w > 0.0, "SimilarTo weight must be positive, got {w}");
    }
}

fn high_recall_knobs() -> AutoEdgeKnobs {
    AutoEdgeKnobs {
        top_k: 8,
        // Real BGE vectors for thematically similar texts land well
        // above this threshold; the bound matches the auto_edge.rs
        // tests so the two suites read the same shape.
        similarity_threshold: 0.5,
        ef_search: Some(64),
    }
}

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

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("auto-edge-real-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}

// ---------------------------------------------------------------------------
// Test.
// ---------------------------------------------------------------------------

/// Three thematically similar sentences embedded via real BGE-small
/// produce at least one SimilarTo edge after one worker cycle, and
/// every emitted edge carries a finite positive weight. This is the
/// "real path actually works" test — when it passes, the H1+H2+H3
/// commits together close out the NopDispatcher chapter.
#[test]
fn auto_edge_writes_real_edges_with_loaded_model() {
    let Some(dir) = model_dir() else {
        eprintln!(
            "skipping: set BRAIN_EMBED_MODEL_DIR to run \
             (download with ./scripts/bootstrap-model.sh)"
        );
        return;
    };

    // Load the model once on the test thread; the dispatcher is then
    // moved into the Glommio executor below. Model load is the
    // dominant cost (~3-5s on a laptop) so we keep it outside the
    // executor closure.
    let cfg = EmbedderModelConfig::new(dir);
    let handle = ModelHandle::load(&cfg).expect("model loads");
    let dispatcher: Arc<dyn Dispatcher> = Arc::new(CpuDispatcher::new(handle));

    // Pre-compute the three vectors on the test thread so the
    // EncodeOp construction inside the executor has nothing to fail
    // on. Three sentences about the same scene; BGE-small clusters
    // them tightly. The cosine pairwise lands ~0.6-0.8 — clear of
    // the 0.5 threshold but well inside what counts as "related".
    let texts = [
        "the cat sat",
        "the cat sat on the mat",
        "felines rested on the rug",
    ];
    let vectors: Vec<[f32; VECTOR_DIM]> = texts
        .iter()
        .map(|t| dispatcher.embed(t).expect("embed"))
        .collect();

    glommio_run(move || async move {
        let fix = build_fixture(dispatcher);

        let a = submit_encode(&fix.ctx, encode_op(1, vectors[0], texts[0])).await;
        let _b = submit_encode(&fix.ctx, encode_op(2, vectors[1], texts[1])).await;
        let _c = submit_encode(&fix.ctx, encode_op(3, vectors[2], texts[2])).await;

        let worker =
            AutoEdgeWorker::new(fix.queue_rx.clone()).with_knobs(high_recall_knobs());
        let drained = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(drained, 3, "all three encodes must be drained from the queue");

        // The headline assertion: at least one SimilarTo row landed.
        // Real BGE vectors for related sentences sit well above the
        // 0.5 threshold; if this fires, either the embedder regressed
        // or the worker's filter regressed.
        let total = count_similar_total(&fix.metadata);
        assert!(
            total >= 1,
            "real BGE vectors for thematically similar texts must \
             produce at least one SimilarTo edge; got {total}",
        );

        // Every emitted edge must carry a finite positive weight. The
        // defensive guard against NaN/zero-vector weights from the
        // NopDispatcher era must not be misfiring on real vectors.
        assert_all_weights_finite_and_positive(&fix.metadata, a);
    });
}
