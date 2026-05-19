#![allow(clippy::arc_with_non_send_sync)]
//! ExtractorWorker integration tests.
//!
//! Wires the writer's post-encode channel into the worker, registers
//! a deterministic mock extractor in `OpsContext.extractor_registry`,
//! then drives the worker one cycle and asserts on the resulting
//! entity / statement / relation / mention-edge rows.

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
    ExtractorRegistry, RelationMention, StatementMention,
};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{EdgeKey, EDGES_REVERSE_TABLE, EDGES_TABLE};
use brain_metadata::tables::extractor_audit::{pipeline_status, EXTRACTOR_PIPELINE_AUDIT_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{ExtractorEnqueue, OpsContext, RealWriterHandle};
use brain_planner::{EncodeOp, ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{
    ExtractorKnobs, ExtractorWorker, Worker, WorkerConfig, WorkerContext, WorkerKind,
};
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
    queue_rx: flume::Receiver<ExtractorEnqueue>,
    queue_tx: flume::Sender<ExtractorEnqueue>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture_with_capacity(capacity: usize) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let (queue_tx, queue_rx) = flume::bounded(capacity.max(1));
    let mut real_writer = RealWriterHandle::new(metadata.clone(), hnsw_writer);
    real_writer.set_extractor_sender(queue_tx.clone());
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

fn install_failing_llm_plus_pattern(
    ctx: &OpsContext,
    items_by_text: HashMap<String, Vec<ExtractedItem>>,
) {
    let mut reg = ExtractorRegistry::new();
    reg.register(Arc::new(MockExtractor {
        id: ExtractorId::from(101),
        kind: ExtractorKind::Pattern,
        items: items_by_text,
    }));
    reg.register(Arc::new(FailingExtractor {
        id: ExtractorId::from(202),
        kind: ExtractorKind::Llm,
    }));
    let mut slot = ctx.extractor_registry.write();
    *slot = reg;
}

async fn run_one_cycle(
    worker: &ExtractorWorker,
    ctx: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let wctx = WorkerContext { ops: ctx, shutdown };
    worker.run_cycle(&wctx).await
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

fn count_mention_edges_in_reverse(metadata: &SharedMetadataDb, entity: EntityId) -> usize {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
    let prefix = NodeRef::Entity(entity).to_bytes();
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

fn audit_status(metadata: &SharedMetadataDb, memory_id: MemoryId) -> Option<u8> {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let table = match rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE) {
        Ok(t) => t,
        Err(_) => return None,
    };
    table
        .get(&memory_id.to_be_bytes())
        .unwrap()
        .map(|g| g.value().status)
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("extractor-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}

fn fast_worker(rx: flume::Receiver<ExtractorEnqueue>) -> ExtractorWorker {
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
}

// ---------------------------------------------------------------------------
// Mock extractors.
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

struct FailingExtractor {
    id: ExtractorId,
    kind: ExtractorKind,
}

impl Extractor for FailingExtractor {
    fn id(&self) -> ExtractorId {
        self.id
    }
    fn kind(&self) -> ExtractorKind {
        self.kind
    }
    fn name(&self) -> &str {
        "failing"
    }
    fn extractor_version(&self) -> u32 {
        1
    }
    fn run<'a>(
        &'a self,
        _ctx: &'a ExtractionContext<'a>,
        _mem: &'a CoreMemory,
    ) -> ExtractionFuture<'a> {
        Box::pin(async { ExtractionResult::failure("mock failure", 0, 0) })
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

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// Pattern tier emits two entities + one statement + one relation
/// over an encoded memory. The worker should resolve both entities,
/// write two mention edges, one statement, and one relation.
#[test]
fn worker_writes_entities_statements_mentions_for_encoded_memory() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "Priya works at Acme";
        let mut items = HashMap::new();
        items.insert(
            text.to_string(),
            vec![
                em("Priya", "brain:Person", 0.9),
                em("Acme", "brain:Organization", 0.9),
                sm("Priya", "test:works_at", "Acme", 0.85),
                rm("Priya", "test:works_at", "Acme", 0.95),
            ],
        );
        install_registry(&fix.ctx, items);

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let worker = fast_worker(fix.queue_rx.clone());
        let drained = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(drained, 1);

        // Mention edges: two forward rows from memory → (Priya, Acme).
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 2);
        // Audit row exists with SUCCESS.
        assert_eq!(
            audit_status(&fix.metadata, memory_id),
            Some(pipeline_status::SUCCESS)
        );
        // Statement table has at least one row for the works_at predicate.
        let db = fix.metadata.lock();
        let rtxn = db.read_txn().unwrap();
        let stmts = rtxn
            .open_table(brain_metadata::tables::knowledge::statement::STATEMENTS_TABLE)
            .unwrap();
        let count = stmts.iter().unwrap().count();
        assert!(count >= 1, "expected ≥1 statement; got {count}");
        // Relation table has at least one row.
        let rels = rtxn
            .open_table(brain_metadata::tables::knowledge::relation::RELATION_METADATA_TABLE)
            .unwrap();
        let rcount = rels.iter().unwrap().count();
        assert!(rcount >= 1, "expected ≥1 relation; got {rcount}");
    });
}

/// A second memory whose extractor output mentions the same surface
/// form should resolve to the same EntityId (tier-1 exact hit), not
/// mint a new one.
#[test]
fn worker_resolves_existing_entity_via_normalized_name() {
    glommio_run(|| async {
        let fix = build_fixture();
        let t1 = "Alice works at Acme";
        let t2 = "Alice owns the project";
        let mut items: HashMap<String, Vec<ExtractedItem>> = HashMap::new();
        items.insert(
            t1.to_string(),
            vec![
                em("Alice", "brain:Person", 0.9),
                em("Acme", "brain:Organization", 0.9),
            ],
        );
        items.insert(t2.to_string(), vec![em("Alice", "brain:Person", 0.9)]);
        install_registry(&fix.ctx, items);

        let m1 = submit_encode(&fix.ctx, encode_op(1, t1)).await;
        let m2 = submit_encode(&fix.ctx, encode_op(2, t2)).await;
        let worker = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();

        // Both memories mention "Alice" — there should be exactly one
        // Alice entity row in `entities`, and both memories should
        // have a Mentions edge pointing at it.
        let db = fix.metadata.lock();
        let rtxn = db.read_txn().unwrap();
        let entities_t = rtxn
            .open_table(brain_metadata::tables::knowledge::entity::ENTITIES_TABLE)
            .unwrap();
        // Count entities with canonical_name "Alice".
        let mut alice_count = 0;
        let mut alice_id: Option<EntityId> = None;
        for entry in entities_t.iter().unwrap() {
            let (_, v) = entry.unwrap();
            let row = v.value();
            if row.canonical_name == "Alice" {
                alice_count += 1;
                alice_id = Some(EntityId::from(row.entity_id_bytes));
            }
        }
        assert_eq!(alice_count, 1, "Alice deduped via tier-1");
        drop(rtxn);
        drop(db);
        let alice = alice_id.unwrap();
        assert_eq!(count_mention_edges_in_reverse(&fix.metadata, alice), 2);
        let _ = (m1, m2);
    });
}

/// Audit-table idempotency: re-draining the same memory after the
/// first apply should be a no-op (no double-writes).
#[test]
fn worker_skips_already_extracted_memory_via_audit() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "Bob owns Foo";
        let mut items = HashMap::new();
        items.insert(text.to_string(), vec![em("Bob", "brain:Person", 0.9)]);
        install_registry(&fix.ctx, items);

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let worker = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        let edges_before = count_mention_edges_out(&fix.metadata, memory_id);
        assert_eq!(edges_before, 1);

        // Re-enqueue manually + re-run; the worker should drop on the
        // audit-row idempotency guard.
        fix.queue_tx
            .send((memory_id, Arc::from(text)))
            .expect("re-enqueue");
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        let edges_after = count_mention_edges_out(&fix.metadata, memory_id);
        assert_eq!(edges_after, edges_before, "no double-write on replay");
    });
}

/// Failing-LLM-but-passing-pattern path: the worker should still
/// commit the pattern tier's items and audit the memory as
/// `PARTIAL_FAILURE` (so a replay doesn't loop indefinitely).
#[test]
fn worker_partial_failure_when_llm_unavailable_still_runs_pattern() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "Carol leads Acme";
        let mut items = HashMap::new();
        items.insert(text.to_string(), vec![em("Carol", "brain:Person", 0.9)]);
        install_failing_llm_plus_pattern(&fix.ctx, items);

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let worker = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();

        // Pattern's output landed.
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 1);
        // Audit says PARTIAL_FAILURE.
        assert_eq!(
            audit_status(&fix.metadata, memory_id),
            Some(pipeline_status::PARTIAL_FAILURE)
        );
    });
}

/// Channel-full drop: writer's enqueue path drops with a warn rather
/// than blocking. Encoding more memories than the channel capacity
/// should still succeed (the dropped ones simply don't get extracted).
#[test]
fn channel_full_drops_with_metric() {
    glommio_run(|| async {
        // Capacity 1 so the second encode's enqueue overflows.
        let fix = build_fixture_with_capacity(1);
        install_registry(&fix.ctx, HashMap::new());

        let _m1 = submit_encode(&fix.ctx, encode_op(1, "a")).await;
        let m2 = submit_encode(&fix.ctx, encode_op(2, "b")).await;
        // Both encodes succeed even though m2's enqueue overflowed.
        assert_ne!(m2.raw(), 0);
        // Queue holds exactly the first enqueue.
        assert_eq!(fix.queue_rx.len(), 1);
    });
}

/// Worker writes asymmetric Mention edges: forward `(memory, Mentions,
/// entity)` but no `(entity, Mentions, memory)` mirror.
#[test]
fn mention_edges_are_asymmetric_no_auto_mirror() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "Dan met Eve";
        let mut items = HashMap::new();
        items.insert(text.to_string(), vec![em("Dan", "brain:Person", 0.9)]);
        install_registry(&fix.ctx, items);

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let worker = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();

        // Locate the Dan entity.
        let dan = {
            let db = fix.metadata.lock();
            let rtxn = db.read_txn().unwrap();
            let entities_t = rtxn
                .open_table(brain_metadata::tables::knowledge::entity::ENTITIES_TABLE)
                .unwrap();
            let mut found: Option<EntityId> = None;
            for entry in entities_t.iter().unwrap() {
                let (_, v) = entry.unwrap();
                let row = v.value();
                if row.canonical_name == "Dan" {
                    found = Some(EntityId::from(row.entity_id_bytes));
                }
            }
            found.expect("Dan must exist")
        };
        // memory has one forward Mentions edge to Dan.
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 1);
        // Dan has exactly one reverse Mentions entry (the same edge).
        assert_eq!(count_mention_edges_in_reverse(&fix.metadata, dan), 1);
        // Dan has NO outgoing Mentions — auto-mirror should not have
        // fired. (We probe the forward table from Dan's prefix.)
        let db = fix.metadata.lock();
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(EDGES_TABLE).unwrap();
        let prefix = NodeRef::Entity(dan).to_bytes();
        let upper = {
            let mut v = prefix.to_vec();
            v.push(0xFF);
            v
        };
        let mut from_dan = 0usize;
        for entry in t.range(prefix.as_slice()..upper.as_slice()).unwrap() {
            let (key, _) = entry.unwrap();
            let decoded = EdgeKey::decode(key.value()).unwrap();
            if matches!(decoded.kind, EdgeKindRef::Mentions) {
                from_dan += 1;
            }
        }
        assert_eq!(from_dan, 0, "Mentions must NOT auto-mirror");
    });
}

/// When no tier emits anything, the worker still records an audit
/// row (so the memory isn't re-processed forever). Status is
/// `SKIPPED` because no tier ran (registry empty).
#[test]
fn worker_records_audit_even_when_registry_empty() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "no extractors here";
        // Explicitly install an empty registry.
        {
            let mut slot = fix.ctx.extractor_registry.write();
            *slot = ExtractorRegistry::new();
        }

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let worker = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();

        // Audit row written.
        assert_eq!(
            audit_status(&fix.metadata, memory_id),
            Some(pipeline_status::SKIPPED)
        );
        // No mention edges (no items emitted).
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 0);
    });
}

/// `WorkerKind::Extractor::name` returns the stable scheduler key
/// used in metrics. Catches accidental renaming.
#[test]
fn worker_kind_name_is_stable() {
    assert_eq!(WorkerKind::Extractor.name(), "extractor");
}

/// Predicate qname in a namespace with an active schema that doesn't
/// declare that predicate gets dropped (without breaking the rest of
/// the extraction). The system "brain" namespace is schema-active
/// at v1 on every fresh DB, and doesn't declare `made_up_predicate`.
#[test]
fn worker_drops_extractor_output_outside_schema() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "Filtered statement here";
        let mut items = HashMap::new();
        items.insert(
            text.to_string(),
            vec![
                em("Subject", "brain:Person", 0.9),
                em("Object", "brain:Person", 0.9),
                // `brain:made_up_predicate` lives in the schema-active
                // brain namespace but isn't declared by the system
                // schema — should be filtered out.
                sm("Subject", "brain:made_up_predicate", "Object", 0.9),
            ],
        );
        install_registry(&fix.ctx, items);

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let worker = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();

        // Entity mentions land regardless of predicate-schema filter.
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 2);
        // No statement row written (the only statement was filtered).
        let db = fix.metadata.lock();
        let rtxn = db.read_txn().unwrap();
        let stmts = rtxn
            .open_table(brain_metadata::tables::knowledge::statement::STATEMENTS_TABLE)
            .unwrap();
        assert_eq!(stmts.iter().unwrap().count(), 0);
    });
}
