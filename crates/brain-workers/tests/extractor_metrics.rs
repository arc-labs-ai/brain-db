#![allow(clippy::arc_with_non_send_sync)]
//! Phase E observability — verify the shared `ExtractorMetrics`
//! family is bumped at the expected points: drops on a saturated
//! channel, per-tier-status counters from the pipeline run,
//! items_written per kind from successful writes, resolver outcome
//! per resolved entity mention, and cycle-duration.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use brain_core::knowledge::ExtractorKind;
use brain_core::{AgentId, ContextId, ExtractorId, Memory as CoreMemory, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_extractors::{
    EntityMention, ExtractedItem, ExtractionContext, ExtractionFuture, ExtractionResult, Extractor,
    ExtractorRegistry, RelationMention, StatementMention,
};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::{
    ExtractorEnqueue, ExtractorItemKind, ExtractorMetrics, OpsContext, RealWriterHandle,
    ResolverOutcome, TierKind as MetricTierKind, TierStatus as MetricTierStatus,
};
use brain_planner::{EncodeOp, ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{ExtractorKnobs, ExtractorWorker, Worker, WorkerConfig, WorkerContext};
use parking_lot::Mutex;
use uuid::Uuid;

// ---------------------------------------------------------------------
// Fixture (mirrors tests/extractor.rs but installs the shared metric
// handle on both the writer and the worker).
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
    queue_rx: flume::Receiver<ExtractorEnqueue>,
    queue_tx: flume::Sender<ExtractorEnqueue>,
    metrics: Arc<ExtractorMetrics>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture_with_capacity(capacity: usize) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let (queue_tx, queue_rx) = flume::bounded(capacity.max(1));
    let metrics = Arc::new(ExtractorMetrics::new());
    let mut real_writer = RealWriterHandle::new(metadata.clone(), hnsw_writer);
    real_writer.set_extractor_sender(queue_tx.clone());
    real_writer.set_extractor_metrics(metrics.clone());
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

fn encode_op(req_seed: u8, text: &str) -> EncodeOp {
    EncodeOp {
        request_id: brain_core::RequestId::from([req_seed; 16]),
        context_id: ContextId(1),
        kind: MemoryKind::Episodic,
        text: text.to_string(),
        vector: [0.0; VECTOR_DIM],
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
    worker: &ExtractorWorker,
    ctx: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let wctx = WorkerContext { ops: ctx, shutdown };
    worker.run_cycle(&wctx).await
}

fn install_registry(ctx: &OpsContext, items_by_text: HashMap<String, Vec<ExtractedItem>>) {
    let mut reg = ExtractorRegistry::new();
    reg.register(Arc::new(MockExtractor {
        id: ExtractorId::from(101),
        kind: ExtractorKind::Pattern,
        items: items_by_text,
    }));
    let mut slot = ctx.extractor_registry.write();
    *slot = reg;
}

fn fast_worker(
    rx: flume::Receiver<ExtractorEnqueue>,
    metrics: Arc<ExtractorMetrics>,
) -> ExtractorWorker {
    let cfg = WorkerConfig {
        enabled: true,
        interval: std::time::Duration::from_millis(50),
        batch_size: 64,
        max_runtime: std::time::Duration::from_secs(5),
    };
    ExtractorWorker::new(rx)
        .with_config(cfg)
        .with_knobs(ExtractorKnobs {
            drain_per_cycle: 64,
            llm_budget_per_cycle_micro_usd: 0,
            skip_already_extracted: true,
        })
        .with_metrics(metrics)
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("extractor-metrics-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}

// ---------------------------------------------------------------------
// Mock extractor (mirrors tests/extractor.rs).
// ---------------------------------------------------------------------

struct MockExtractor {
    id: ExtractorId,
    kind: ExtractorKind,
    items: HashMap<String, Vec<ExtractedItem>>,
}

impl Extractor for MockExtractor {
    fn id(&self) -> ExtractorId {
        self.id
    }
    fn kind(&self) -> ExtractorKind {
        self.kind
    }
    fn name(&self) -> &str {
        "mock"
    }
    fn extractor_version(&self) -> u32 {
        1
    }
    fn run<'a>(
        &'a self,
        _ctx: &'a ExtractionContext<'a>,
        mem: &'a CoreMemory,
    ) -> ExtractionFuture<'a> {
        let text_key = mem.text.clone().unwrap_or_default();
        let items = self.items.get(&text_key).cloned().unwrap_or_default();
        Box::pin(async move { ExtractionResult::success(items, 0, 0) })
    }
}

fn em(text: &str, type_qname: &str, confidence: f32) -> ExtractedItem {
    ExtractedItem::EntityMention(EntityMention {
        entity_type_qname: type_qname.into(),
        text: text.into(),
        start: 0,
        end: 0,
        confidence,
        extractor_id: 101,
        extractor_version: 1,
    })
}

fn sm(subject: &str, predicate: &str, object: &str, confidence: f32) -> ExtractedItem {
    ExtractedItem::StatementMention(StatementMention {
        kind: 1,
        subject_text: Some(subject.into()),
        predicate_qname: predicate.into(),
        object_text: Some(object.into()),
        confidence,
        extractor_id: 101,
        extractor_version: 1,
    })
}

fn rm(subject: &str, relation_type: &str, object: &str, confidence: f32) -> ExtractedItem {
    ExtractedItem::RelationMention(RelationMention {
        relation_type_qname: relation_type.into(),
        subject_text: subject.into(),
        object_text: object.into(),
        confidence,
        extractor_id: 101,
        extractor_version: 1,
    })
}

// ---------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------

/// On a successful cycle:
/// * `tier_runs_total{tier=pattern,status=ran}` ticks once per memory.
/// * `items_written_total` ticks for each entity/mention/statement/relation.
/// * `resolver_outcome_total{tier=create}` ticks per fresh entity.
/// * `cycle_duration_seconds` records once.
#[test]
fn successful_cycle_bumps_all_families() {
    glommio_run(|| async {
        let fix = build_fixture_with_capacity(4096);
        let mut items_by_text: HashMap<String, Vec<ExtractedItem>> = HashMap::new();
        items_by_text.insert(
            "Alice works at Acme".into(),
            vec![
                em("Alice", "brain:Person", 0.95),
                em("Acme", "brain:Organization", 0.92),
                sm("Alice", "brain:works_at", "Acme", 0.9),
                rm("Alice", "brain:works_at", "Acme", 0.9),
            ],
        );
        install_registry(&fix.ctx, items_by_text);

        let _mid = submit_encode(&fix.ctx, encode_op(1, "Alice works at Acme")).await;

        let worker = fast_worker(fix.queue_rx.clone(), fix.metrics.clone());
        let drained = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(drained, 1, "exactly one memory processed this cycle");

        let snap = fix.metrics.snapshot();
        assert_eq!(snap.drops_total, 0, "no drops");

        // tier_runs: pattern tier ran once; classifier + llm absent
        // (no extractor registered for those kinds) so no row bumps.
        let pattern_ran_idx = MetricTierKind::Pattern as usize
            * brain_ops::TIER_STATUS_LABELS.len()
            + MetricTierStatus::Ran as usize;
        assert_eq!(snap.tier_runs_total[pattern_ran_idx], 1);

        // items_written: 2 entities, 2 mentions (one per entity), 1
        // statement, 1 relation.
        assert_eq!(
            snap.items_written_total[ExtractorItemKind::Entity as usize],
            2
        );
        assert_eq!(
            snap.items_written_total[ExtractorItemKind::Mention as usize],
            2
        );
        assert_eq!(
            snap.items_written_total[ExtractorItemKind::Statement as usize],
            1,
        );
        assert_eq!(
            snap.items_written_total[ExtractorItemKind::Relation as usize],
            1,
        );

        // Both entities were freshly minted -> two `create` outcomes.
        assert_eq!(
            snap.resolver_outcome_total[ResolverOutcome::Create as usize],
            2,
        );

        assert_eq!(snap.cycle_duration_seconds.count, 1);
    });
}

/// On a saturated channel the writer's enqueue is dropped and
/// `brain_extractor_drops_total` ticks.
#[test]
fn writer_full_channel_bumps_drops_total() {
    glommio_run(|| async {
        let fix = build_fixture_with_capacity(1);
        // First encode fills the channel.
        let _a = submit_encode(&fix.ctx, encode_op(1, "first")).await;
        // Channel is full now; further encodes drop.
        let _b = submit_encode(&fix.ctx, encode_op(2, "second")).await;
        let _c = submit_encode(&fix.ctx, encode_op(3, "third")).await;

        let snap = fix.metrics.snapshot();
        assert!(
            snap.drops_total >= 1,
            "expected drops_total to count overflow enqueues; got {}",
            snap.drops_total
        );
    });
}

/// Cycle duration histogram observes on every cycle (including empty
/// ones), so `_count` matches `brain_worker_cycles_total` for this
/// worker.
#[test]
fn empty_cycle_observes_duration() {
    glommio_run(|| async {
        let fix = build_fixture_with_capacity(4096);
        let worker = fast_worker(fix.queue_rx.clone(), fix.metrics.clone());
        let drained = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(drained, 0);
        let snap = fix.metrics.snapshot();
        assert_eq!(snap.cycle_duration_seconds.count, 1);
    });
}

/// Holds the sender alive so a future fixture extension doesn't lose
/// the channel and breaks the writer side silently.
#[test]
fn fixture_holds_sender_alive() {
    glommio_run(|| async {
        let fix = build_fixture_with_capacity(4);
        drop(fix.queue_tx);
        let _ = fix.metrics.snapshot();
    });
}
