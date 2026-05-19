#![allow(clippy::arc_with_non_send_sync)]
//! Phase B observability — verify the shared `AutoEdgeMetrics` family
//! is bumped at the expected points: drops on a full channel, edges
//! on a successful cycle, plus cycle-duration and neighbours-found
//! histograms.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::{AutoEdgeEnqueue, AutoEdgeMetrics, OpsContext, RealWriterHandle};
use brain_planner::{EncodeOp, ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{AutoEdgeKnobs, AutoEdgeWorker, Worker, WorkerContext};
use parking_lot::Mutex;
use uuid::Uuid;

// ---------------------------------------------------------------------
// Fixture (mirrors the layout used by tests/auto_edge.rs but installs
// the shared metric handle on both the writer and the worker).
// ---------------------------------------------------------------------

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
    queue_rx: flume::Receiver<AutoEdgeEnqueue>,
    queue_tx: flume::Sender<AutoEdgeEnqueue>,
    metrics: Arc<AutoEdgeMetrics>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture_with_capacity(capacity: usize) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let (queue_tx, queue_rx) = flume::bounded(capacity.max(1));
    let metrics = Arc::new(AutoEdgeMetrics::new());
    let mut real_writer = RealWriterHandle::new(metadata.clone(), hnsw_writer);
    real_writer.set_auto_edge_sender(queue_tx.clone());
    real_writer.set_auto_edge_metrics(metrics.clone());
    let writer: Arc<dyn WriterHandle> = Arc::new(real_writer);
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer,
    );
    Fixture {
        ctx: Arc::new(OpsContext::new(executor)),
        queue_rx,
        queue_tx,
        metrics,
        _tempdir: tempdir,
    }
}

fn dense_vec(slot: u64) -> [f32; VECTOR_DIM] {
    let mut v = [0.0f32; VECTOR_DIM];
    let lobe = (slot % 8) as usize;
    v[lobe * 32] = 1.0;
    let jitter = ((slot / 8) as f32).mul_add(0.001, 0.001);
    v[lobe * 32 + 1] = jitter;
    let mut sq = 0f32;
    for x in v.iter() {
        sq += x * x;
    }
    if sq > 0.0 {
        let inv = sq.sqrt().recip();
        for x in v.iter_mut() {
            *x *= inv;
        }
    }
    v
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

fn high_recall_knobs() -> AutoEdgeKnobs {
    AutoEdgeKnobs {
        top_k: 8,
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
        .name("auto-edge-metrics-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}

// ---------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------

#[test]
fn worker_publishes_edges_written_and_cycle_duration() {
    glommio_run(|| async {
        let fix = build_fixture_with_capacity(4096);
        // Three encodes inside the same lobe -> each pair lands above
        // the threshold.
        let _a = submit_encode(&fix.ctx, encode_op(1, 0, dense_vec(0))).await;
        let _b = submit_encode(&fix.ctx, encode_op(2, 8, dense_vec(8))).await;
        let _c = submit_encode(&fix.ctx, encode_op(3, 16, dense_vec(16))).await;

        let worker = AutoEdgeWorker::new(fix.queue_rx.clone())
            .with_knobs(high_recall_knobs())
            .with_metrics(fix.metrics.clone());
        let drained = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert!(drained >= 3, "expected at least the 3 encodes drained");

        let snap = fix.metrics.snapshot();
        assert!(
            snap.edges_written_total >= 1,
            "expected >=1 logical edges; got {}",
            snap.edges_written_total
        );
        // Drops counter must not bump on a healthy fixture.
        assert_eq!(snap.drops_total, 0, "no drops expected");
        assert_eq!(snap.cycle_duration_seconds.count, 1, "one cycle observed");
        assert!(
            snap.cycle_duration_seconds.sum >= 0.0,
            "cycle duration sum must be non-negative",
        );
        assert_eq!(
            snap.neighbours_found_per_cycle.count, 1,
            "neighbours-found is observed once per cycle (even on zero)",
        );
        assert!(
            snap.neighbours_found_per_cycle.sum >= 1.0,
            "expected at least one above-threshold neighbour; got sum={}",
            snap.neighbours_found_per_cycle.sum,
        );
    });
}

/// `brain_auto_edge_drops_total` must increment when the bounded
/// channel is saturated and the writer's enqueue is dropped.
#[test]
fn writer_full_channel_bumps_drops_total() {
    glommio_run(|| async {
        // capacity = 1 so the second encode's enqueue lands on a full
        // channel before the worker drains anything.
        let fix = build_fixture_with_capacity(1);
        let _a = submit_encode(&fix.ctx, encode_op(1, 0, dense_vec(0))).await;
        // Pre-fill: the writer pushed one. Now block the channel by
        // never draining and send several more encodes — the writer's
        // try_send returns `Full` for each, bumping drops.
        let _b = submit_encode(&fix.ctx, encode_op(2, 8, dense_vec(8))).await;
        let _c = submit_encode(&fix.ctx, encode_op(3, 16, dense_vec(16))).await;
        let _d = submit_encode(&fix.ctx, encode_op(4, 24, dense_vec(24))).await;

        let snap = fix.metrics.snapshot();
        assert!(
            snap.drops_total >= 1,
            "expected drops_total to count overflow enqueues; got {}",
            snap.drops_total
        );
    });
}

/// An empty cycle still records the cycle-duration histogram and
/// reports zero neighbours. This keeps `_count` aligned with
/// `brain_worker_cycles_total` so PromQL can divide cleanly.
#[test]
fn empty_cycle_observes_histograms() {
    glommio_run(|| async {
        let fix = build_fixture_with_capacity(4096);
        let worker = AutoEdgeWorker::new(fix.queue_rx.clone())
            .with_knobs(high_recall_knobs())
            .with_metrics(fix.metrics.clone());
        // No encodes -> drained == 0.
        let drained = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(drained, 0);
        let snap = fix.metrics.snapshot();
        assert_eq!(snap.cycle_duration_seconds.count, 1);
        assert_eq!(snap.neighbours_found_per_cycle.count, 1);
        assert_eq!(snap.neighbours_found_per_cycle.sum, 0.0);
        assert_eq!(snap.edges_written_total, 0);
    });
}

/// Sanity guard: silence the unused-field warning on `queue_tx` —
/// holding the sender alongside the writer is part of the fixture
/// contract (it keeps the channel open even when the writer's clone
/// is dropped early).
#[test]
fn fixture_holds_sender_alive() {
    glommio_run(|| async {
        let fix = build_fixture_with_capacity(4);
        drop(fix.queue_tx);
        // ensure metrics handle still usable after fixture's tx drop.
        let _ = fix.metrics.snapshot();
    });
}
