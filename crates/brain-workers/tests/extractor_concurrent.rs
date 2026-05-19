#![allow(clippy::arc_with_non_send_sync)]
//! Concurrent / race-style ExtractorWorker tests.
//!
//! - Two cycles of the worker against the same shared DB resolve the
//!   same surface form to the same EntityId (deterministic via tier-1
//!   exact hit once the first cycle commits).
//! - "Concurrent ENCODE" race: two memories carrying the same entity
//!   surface form are encoded back-to-back; after one drain the
//!   resolver de-duplicates them into a single entity row. This is the
//!   single-writer-per-shard guarantee made executable.
//! - Channel-full backpressure: pushing more than channel capacity
//!   drops the excess deterministically. Encodes still succeed.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use brain_core::knowledge::ExtractorKind;
use brain_core::{
    AgentId, ContextId, EdgeKindRef, EntityId, ExtractorId, Memory as CoreMemory, MemoryId,
    MemoryKind, NodeRef,
};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_extractors::{
    EntityMention, ExtractedItem, ExtractionContext, ExtractionFuture, ExtractionResult, Extractor,
    ExtractorRegistry,
};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{EdgeKey, EDGES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{ExtractorEnqueue, OpsContext, RealWriterHandle};
use brain_planner::{EncodeOp, ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{ExtractorKnobs, ExtractorWorker, Worker, WorkerConfig, WorkerContext};
use parking_lot::Mutex;
use redb::ReadableTable;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture (mirrors extractor.rs's pattern).
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
    queue_rx: flume::Receiver<ExtractorEnqueue>,
    queue_tx: flume::Sender<ExtractorEnqueue>,
    _td: tempfile::TempDir,
}

fn build_fixture_with_capacity(cap: usize) -> Fixture {
    let td = tempfile::tempdir().unwrap();
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(
        MetadataDb::open(td.path().join("metadata.redb")).unwrap(),
    ));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let (tx, rx) = flume::bounded(cap.max(1));
    let mut real = RealWriterHandle::new(metadata.clone(), hnsw_writer);
    real.set_extractor_sender(tx.clone());
    let writer: Arc<dyn WriterHandle> = Arc::new(real);
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer,
    );
    Fixture {
        ctx: Arc::new(OpsContext::new(executor)),
        metadata,
        queue_rx: rx,
        queue_tx: tx,
        _td: td,
    }
}

fn encode_op(seed: u8, text: &str) -> EncodeOp {
    EncodeOp {
        request_id: brain_core::RequestId::from([seed; 16]),
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

fn install_pattern_registry(ctx: &OpsContext, items_by_text: HashMap<String, Vec<ExtractedItem>>) {
    let mut reg = ExtractorRegistry::new();
    reg.register(Arc::new(MockExtractor {
        id: ExtractorId::from(101),
        kind: ExtractorKind::Pattern,
        items: items_by_text,
    }));
    *ctx.extractor_registry.write() = reg;
}

async fn run_one_cycle(
    worker: &ExtractorWorker,
    ctx: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let wctx = WorkerContext { ops: ctx, shutdown };
    worker.run_cycle(&wctx).await
}

fn count_entities_with_name(metadata: &SharedMetadataDb, name: &str) -> (usize, Option<EntityId>) {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn
        .open_table(brain_metadata::tables::knowledge::entity::ENTITIES_TABLE)
        .unwrap();
    let mut count = 0;
    let mut id: Option<EntityId> = None;
    for entry in t.iter().unwrap() {
        let (_, v) = entry.unwrap();
        let row = v.value();
        if row.canonical_name == name {
            count += 1;
            id = Some(EntityId::from(row.entity_id_bytes));
        }
    }
    (count, id)
}

fn count_mention_edges_out(metadata: &SharedMetadataDb, from: MemoryId) -> usize {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(EDGES_TABLE).unwrap();
    let prefix = NodeRef::Memory(from).to_bytes();
    let upper = {
        let mut v = prefix.to_vec();
        v.push(0xFF);
        v
    };
    let mut total = 0usize;
    for entry in t.range(prefix.as_slice()..upper.as_slice()).unwrap() {
        let (key, _) = entry.unwrap();
        let decoded = EdgeKey::decode(key.value()).unwrap();
        if matches!(decoded.kind, EdgeKindRef::Mentions) {
            total += 1;
        }
    }
    total
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("extractor-concurrent-test")
        .spawn(move || async move { f().await })
        .expect("spawn")
        .join()
        .expect("join")
}

fn fast_worker(rx: flume::Receiver<ExtractorEnqueue>) -> ExtractorWorker {
    let cfg = WorkerConfig {
        enabled: true,
        interval: std::time::Duration::from_millis(50),
        batch_size: 4096,
        max_runtime: std::time::Duration::from_secs(5),
    };
    ExtractorWorker::new(rx)
        .with_config(cfg)
        .with_knobs(ExtractorKnobs {
            drain_per_cycle: 4096,
            llm_budget_per_cycle_micro_usd: 0,
            skip_already_extracted: true,
        })
}

// ---------------------------------------------------------------------------
// Mock pattern extractor.
// ---------------------------------------------------------------------------

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
        "mock-pattern"
    }
    fn extractor_version(&self) -> u32 {
        1
    }
    fn run<'a>(
        &'a self,
        _ctx: &'a ExtractionContext<'a>,
        mem: &'a CoreMemory,
    ) -> ExtractionFuture<'a> {
        let key = mem.text.clone().unwrap_or_default();
        let items = self.items.get(&key).cloned().unwrap_or_default();
        Box::pin(async move { ExtractionResult::success(items, 0, 0) })
    }
}

fn em(text: &str, qname: &str, confidence: f32) -> ExtractedItem {
    ExtractedItem::EntityMention(EntityMention {
        entity_type_qname: qname.into(),
        text: text.into(),
        start: 0,
        end: 0,
        confidence,
        extractor_id: 101,
        extractor_version: 1,
    })
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// Two memories encoded back-to-back, both mentioning "Priya"; one
/// extractor cycle drains both. After the cycle there must be exactly
/// one Priya entity row and two distinct memory→Priya mention edges.
#[test]
fn back_to_back_encodes_resolve_to_single_entity() {
    glommio_run(|| async {
        let fix = build_fixture_with_capacity(64);
        let t1 = "Priya joined";
        let t2 = "Priya left";
        let mut items = HashMap::new();
        items.insert(t1.into(), vec![em("Priya", "brain:Person", 0.9)]);
        items.insert(t2.into(), vec![em("Priya", "brain:Person", 0.9)]);
        install_pattern_registry(&fix.ctx, items);

        let m1 = submit_encode(&fix.ctx, encode_op(1, t1)).await;
        let m2 = submit_encode(&fix.ctx, encode_op(2, t2)).await;
        let w = fast_worker(fix.queue_rx.clone());
        let drained = run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        assert_eq!(drained, 2);

        let (count, _id) = count_entities_with_name(&fix.metadata, "Priya");
        assert_eq!(count, 1, "expected one Priya entity; got {count}");
        assert_eq!(count_mention_edges_out(&fix.metadata, m1), 1);
        assert_eq!(count_mention_edges_out(&fix.metadata, m2), 1);
    });
}

/// Multi-cycle case: cycle 1 processes one memory mentioning "Acme";
/// cycle 2 processes a second memory also mentioning "Acme". The
/// resolver hits tier-1 on cycle 2 (exact match), so the entity is
/// reused (single row).
#[test]
fn separate_cycles_reuse_existing_entity_via_tier_1() {
    glommio_run(|| async {
        let fix = build_fixture_with_capacity(64);
        let t1 = "Acme Corp launched";
        let t2 = "Acme Corp shipped";
        let mut items = HashMap::new();
        items.insert(t1.into(), vec![em("Acme Corp", "brain:Organization", 0.9)]);
        items.insert(t2.into(), vec![em("Acme Corp", "brain:Organization", 0.9)]);
        install_pattern_registry(&fix.ctx, items);

        let m1 = submit_encode(&fix.ctx, encode_op(1, t1)).await;
        let w = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        let (count_a, id_a) = count_entities_with_name(&fix.metadata, "Acme Corp");
        assert_eq!(count_a, 1);

        let m2 = submit_encode(&fix.ctx, encode_op(2, t2)).await;
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        let (count_b, id_b) = count_entities_with_name(&fix.metadata, "Acme Corp");
        assert_eq!(count_b, 1, "second cycle must reuse the row, not create");
        assert_eq!(id_a, id_b);

        assert_eq!(count_mention_edges_out(&fix.metadata, m1), 1);
        assert_eq!(count_mention_edges_out(&fix.metadata, m2), 1);
    });
}

/// Bounded channel under high load: encodes that overflow the queue
/// drop deterministically; encodes themselves always succeed. After
/// the burst, the worker drains exactly the channel-capacity number
/// of memories.
#[test]
fn channel_full_drops_excess_under_burst() {
    glommio_run(|| async {
        const CAP: usize = 8;
        const BURST: usize = 64;
        let fix = build_fixture_with_capacity(CAP);
        install_pattern_registry(&fix.ctx, HashMap::new());

        let mut ids = Vec::with_capacity(BURST);
        for i in 0..BURST {
            let op = encode_op((i % 250) as u8, &format!("burst-{i}"));
            // Vary the request_id seed so idempotency doesn't dedup the
            // encodes back into the same memory.
            let mut op = op;
            op.request_id = brain_core::RequestId::from([(i % 250) as u8 + 1; 16]);
            ids.push(submit_encode(&fix.ctx, op).await);
        }
        // All encodes succeeded.
        assert_eq!(ids.len(), BURST);
        // Queue holds exactly CAP enqueues; the other BURST-CAP got
        // dropped on `try_send` overflow.
        assert_eq!(fix.queue_rx.len(), CAP);

        // Worker drains its queue without panicking.
        let w = fast_worker(fix.queue_rx.clone());
        let drained = run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        assert_eq!(drained, CAP);
        assert_eq!(fix.queue_rx.len(), 0);
    });
}

/// Direct queue replay: enqueue twice for the same memory_id; the
/// worker drains both but the second is dropped by the audit
/// idempotency guard. Net effect = one set of writes.
#[test]
fn duplicate_queue_entry_is_idempotent_via_audit_guard() {
    glommio_run(|| async {
        let fix = build_fixture_with_capacity(64);
        let text = "Carol shipped";
        let mut items = HashMap::new();
        items.insert(text.into(), vec![em("Carol", "brain:Person", 0.9)]);
        install_pattern_registry(&fix.ctx, items);

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        // Enqueue a duplicate manually so the worker drains it after
        // the auto-enqueue from submit_encode.
        fix.queue_tx
            .send((memory_id, Arc::from(text)))
            .expect("manual enqueue");

        let w = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();

        // Only one mention edge — the first apply committed the audit
        // row inside the same txn, so the duplicate drain saw it.
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 1);
        let (count, _) = count_entities_with_name(&fix.metadata, "Carol");
        assert_eq!(count, 1);
    });
}
