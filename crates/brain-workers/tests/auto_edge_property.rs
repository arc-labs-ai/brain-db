#![allow(clippy::arc_with_non_send_sync)]
//! Property tests for AutoEdgeWorker invariants.
//!
//! Invariants verified:
//!  1. **Bounded fan-out**: a single source memory writes at most
//!     `top_k` distinct `SimilarTo` neighbours per cycle (the worker
//!     filters the self-hit explicitly out of `top_k + 1` HNSW hits).
//!  2. **Threshold**: every written edge has weight >= the configured
//!     `similarity_threshold`.
//!  3. **Symmetric mirror**: every forward `(a, SimilarTo, b)` edge
//!     in `EDGES_TABLE` has its `b`-anchored mirror sibling — this
//!     check encodes the `edge::link` auto-mirror contract and
//!     transitively documents that the worker MUST NOT mirror in its
//!     own write path.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use brain_core::{AgentId, ContextId, EdgeKind, EdgeKindRef, MemoryKind, NodeRef};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{EdgeData, EdgeKey, EDGES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{AutoEdgeEnqueue, OpsContext, RealWriterHandle};
use brain_planner::{EncodeOp, ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{AutoEdgeKnobs, AutoEdgeWorker, Worker, WorkerContext};
use parking_lot::Mutex;
use proptest::prelude::*;
use redb::ReadableTable;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Minimal fixture (mirrors auto_edge.rs but exposed for prop runs).
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
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let (queue_tx, queue_rx) = flume::bounded(4096);
    let mut real_writer = RealWriterHandle::new(metadata.clone(), hnsw_writer);
    real_writer.set_auto_edge_sender(queue_tx);
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
        _tempdir: tempdir,
    }
}

fn vec_for(seed: u32, lobe_count: usize) -> [f32; VECTOR_DIM] {
    let mut v = [0.0f32; VECTOR_DIM];
    let lobe = (seed as usize) % lobe_count;
    // Lobes spaced 32 dims apart so they're orthogonal.
    let base = lobe * 32;
    v[base] = 1.0;
    // Slot-keyed jitter so distinct seeds within a lobe differ.
    let jitter = (seed as f32).mul_add(1e-4, 1e-4);
    v[base + 1] = jitter;
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

fn encode_op(seed: u32, vector: [f32; VECTOR_DIM]) -> EncodeOp {
    // Vary the request_id across encodes so idempotency doesn't
    // collapse them.
    let mut rid = [0u8; 16];
    rid[..4].copy_from_slice(&seed.to_be_bytes());
    EncodeOp {
        request_id: brain_core::RequestId::from(rid),
        context_id: ContextId(1),
        kind: MemoryKind::Episodic,
        text: format!("seed-{seed}"),
        vector,
        salience_initial: 0.5,
        fingerprint: [0; 16],
        edges: vec![],
        deduplicate: false,
        content_hash: [0; 32],
        agent_id: AgentId(Uuid::nil()),
    }
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
        .name("auto-edge-prop")
        .spawn(move || async move { f().await })
        .expect("spawn glommio")
        .join()
        .expect("join")
}

/// Iterate every SimilarTo edge and return `(from, to, weight)`.
fn collect_similar(
    metadata: &SharedMetadataDb,
) -> Vec<(brain_core::MemoryId, brain_core::MemoryId, f32)> {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(EDGES_TABLE).unwrap();
    let mut out = Vec::new();
    for entry in t.iter().unwrap() {
        let (key, val) = entry.unwrap();
        let k = EdgeKey::decode(key.value()).unwrap();
        if !matches!(k.kind, EdgeKindRef::Builtin(EdgeKind::SimilarTo)) {
            continue;
        }
        let (NodeRef::Memory(from), NodeRef::Memory(to)) = (k.from, k.to) else {
            continue;
        };
        let data: EdgeData = val.value();
        out.push((from, to, data.weight));
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, .. ProptestConfig::default() })]

    /// Generate a handful of encodes across 4 lobes with a configured
    /// top_k and threshold. Drive one worker cycle and assert all
    /// three invariants on the resulting edge table.
    #[test]
    fn auto_edge_invariants(
        seeds in prop::collection::vec(0u32..32, 3..16),
        top_k in 1usize..6,
        threshold in 0.5f32..0.95,
    ) {
        let seeds_owned = seeds.clone();
        let result = glommio_run(move || async move {
            let fix = build_fixture();
            for seed in &seeds_owned {
                let op = encode_op(*seed, vec_for(*seed, 4));
                let _ = fix.ctx.executor.writer.submit_encode(op).await;
            }
            let knobs = AutoEdgeKnobs {
                top_k,
                similarity_threshold: threshold,
                ef_search: Some(64),
            };
            let worker = AutoEdgeWorker::new(fix.queue_rx.clone()).with_knobs(knobs);
            run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
            collect_similar(&fix.metadata)
        });

        // --- Invariant 2: threshold honoured. -----------------------
        for (_, _, w) in &result {
            prop_assert!(
                *w >= threshold - 1e-3,
                "edge weight {} below threshold {}",
                w,
                threshold,
            );
        }

        // --- Invariant 1: bounded fan-out. --------------------------
        // Each enqueued anchor's knn pass produces at most `top_k`
        // logical SimilarTo pairs (the worker fetches `top_k + 1`
        // and filters the self-hit). Each logical pair becomes 2
        // physical rows in `EDGES_TABLE` (forward + mirror). So
        // total rows <= 2 * top_k * encoded_anchor_count.
        use std::collections::HashSet;
        let unique_seeds: HashSet<_> = seeds.iter().copied().collect();
        let upper = 2 * top_k * unique_seeds.len();
        prop_assert!(
            result.len() <= upper,
            "total SimilarTo rows {} exceeds 2*top_k*sources = {}",
            result.len(),
            upper,
        );

        // --- Invariant 3: symmetric mirror. -------------------------
        // Every (a, SimilarTo, b) must have its (b, SimilarTo, a)
        // mirror — `edge::link` writes both atomically. The worker
        // MUST NOT depend on this and MUST NOT write its own mirror.
        let pair_set: HashSet<_> = result.iter().map(|(f, t, _)| (*f, *t)).collect();
        for (from, to) in &pair_set {
            prop_assert!(
                pair_set.contains(&(*to, *from)),
                "missing mirror for ({:?} -> {:?})",
                from,
                to,
            );
        }
    }
}
