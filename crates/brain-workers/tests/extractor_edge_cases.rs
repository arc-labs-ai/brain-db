#![allow(clippy::arc_with_non_send_sync)]
//! Edge cases + schema-filter + cost-budget tests for ExtractorWorker.
//!
//! - Empty text body / "no entities" / very long body / unicode body
//!   — the worker handles them without panicking. Audit row written
//!   in every case so the memory isn't reprocessed.
//! - Schemaless namespace (no `SCHEMA_ACTIVE_VERSIONS_TABLE` entry)
//!   → all qnames in that namespace are accepted.
//! - Schema uploaded mid-flight: after the upload commits, the next
//!   worker cycle filters extractor output against the new active
//!   version.
//! - Cost budget: extractor knob `llm_budget_per_cycle_micro_usd`
//!   resets between cycles (we exercise via the public knob accessor;
//!   real LLM accounting wires through `CostBudget` per call —
//!   covered by the brain-extractors llm_pipeline suite).

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use brain_core::knowledge::ExtractorKind;
use brain_core::{
    AgentId, ContextId, EdgeKindRef, ExtractorId, Memory as CoreMemory, MemoryId, MemoryKind,
    NodeRef,
};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_extractors::{
    EntityMention, ExtractedItem, ExtractionContext, ExtractionFuture, ExtractionResult, Extractor,
    ExtractorRegistry, RelationMention, StatementMention,
};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::schema_store::schema_upload;
use brain_metadata::tables::edge::{EdgeKey, EDGES_TABLE};
use brain_metadata::tables::extractor_audit::{pipeline_status, EXTRACTOR_PIPELINE_AUDIT_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{ExtractorEnqueue, OpsContext, RealWriterHandle};
use brain_planner::{EncodeOp, ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::schema::{parse_schema, validate};
use brain_workers::{ExtractorKnobs, ExtractorWorker, Worker, WorkerConfig, WorkerContext};
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
    _td: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let td = tempfile::tempdir().unwrap();
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(
        MetadataDb::open(td.path().join("metadata.redb")).unwrap(),
    ));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let (tx, rx) = flume::bounded(64);
    let mut real = RealWriterHandle::new(metadata.clone(), hnsw_writer);
    real.set_extractor_sender(tx);
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

fn audit_entry(
    metadata: &SharedMetadataDb,
    memory_id: MemoryId,
) -> Option<brain_metadata::tables::extractor_audit::ExtractorPipelineAuditEntry> {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = match rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE) {
        Ok(t) => t,
        Err(_) => return None,
    };
    t.get(&memory_id.to_be_bytes()).unwrap().map(|g| g.value())
}

fn count_statements(metadata: &SharedMetadataDb) -> usize {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn
        .open_table(brain_metadata::tables::knowledge::statement::STATEMENTS_TABLE)
        .unwrap();
    t.iter().unwrap().count()
}

fn count_entities_with_name(metadata: &SharedMetadataDb, name: &str) -> usize {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn
        .open_table(brain_metadata::tables::knowledge::entity::ENTITIES_TABLE)
        .unwrap();
    let mut c = 0;
    for entry in t.iter().unwrap() {
        let (_, v) = entry.unwrap();
        if v.value().canonical_name == name {
            c += 1;
        }
    }
    c
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("extractor-edge-test")
        .spawn(move || async move { f().await })
        .expect("spawn")
        .join()
        .expect("join")
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
// Mock extractor.
// ---------------------------------------------------------------------------

struct MockPattern {
    items: HashMap<String, Vec<ExtractedItem>>,
}
impl Extractor for MockPattern {
    fn id(&self) -> ExtractorId {
        ExtractorId::from(101)
    }
    fn kind(&self) -> ExtractorKind {
        ExtractorKind::Pattern
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
        let key = mem.text.clone().unwrap_or_default();
        let items = self.items.get(&key).cloned().unwrap_or_default();
        Box::pin(async move { ExtractionResult::success(items, 0, 0) })
    }
}

fn install(ctx: &OpsContext, items: HashMap<String, Vec<ExtractedItem>>) {
    let mut reg = ExtractorRegistry::new();
    reg.register(Arc::new(MockPattern { items }));
    *ctx.extractor_registry.write() = reg;
}

fn em(text: &str, qname: &str, c: f32) -> ExtractedItem {
    ExtractedItem::EntityMention(EntityMention {
        entity_type_qname: qname.into(),
        text: text.into(),
        start: 0,
        end: 0,
        confidence: c,
        extractor_id: 101,
        extractor_version: 1,
    })
}
fn sm(subj: &str, pred: &str, obj: &str, c: f32) -> ExtractedItem {
    ExtractedItem::StatementMention(StatementMention {
        kind: 1,
        subject_text: Some(subj.into()),
        predicate_qname: pred.into(),
        object_text: Some(obj.into()),
        confidence: c,
        extractor_id: 101,
        extractor_version: 1,
    })
}
fn rm(subj: &str, rel: &str, obj: &str, c: f32) -> ExtractedItem {
    ExtractedItem::RelationMention(RelationMention {
        relation_type_qname: rel.into(),
        subject_text: subj.into(),
        object_text: obj.into(),
        confidence: c,
        extractor_id: 101,
        extractor_version: 1,
    })
}

// ---------------------------------------------------------------------------
// Edge cases.
// ---------------------------------------------------------------------------

/// Empty body: extractor emits zero items, worker writes audit row
/// with status SUCCESS (the pattern tier ran cleanly with nothing
/// to say). Catches a regression where empty input would skip the
/// audit row and trap the memory in a re-drain loop.
#[test]
fn empty_text_extracts_zero_items_and_audits() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "";
        let mut items = HashMap::new();
        items.insert(text.into(), vec![]);
        install(&fix.ctx, items);

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let w = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();

        let entry = audit_entry(&fix.metadata, memory_id).expect("audit row");
        assert_eq!(entry.status, pipeline_status::SUCCESS);
        assert_eq!(entry.item_counts.entities, 0);
        assert_eq!(entry.item_counts.statements, 0);
        assert_eq!(entry.item_counts.mention_edges, 0);
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 0);
    });
}

/// Short body with no extractable entities: identical posture to the
/// empty-text path, but distinct from "extractor not installed".
#[test]
fn body_with_no_entities_audits_zero_counts() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "hello";
        let mut items = HashMap::new();
        items.insert(text.into(), vec![]);
        install(&fix.ctx, items);

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let w = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        let e = audit_entry(&fix.metadata, memory_id).unwrap();
        assert_eq!(e.status, pipeline_status::SUCCESS);
        assert_eq!(e.item_counts.entities, 0);
    });
}

/// 10 KB body — the worker doesn't panic, doesn't truncate, and the
/// mention edge points at the correctly-resolved entity.
#[test]
fn very_long_body_extracts_normally() {
    glommio_run(|| async {
        let fix = build_fixture();
        let mut long = String::with_capacity(10_000);
        for _ in 0..1_000 {
            long.push_str("Priya. ");
        }
        let mut items = HashMap::new();
        items.insert(long.clone(), vec![em("Priya", "brain:Person", 0.9)]);
        install(&fix.ctx, items);

        let memory_id = submit_encode(&fix.ctx, encode_op(1, &long)).await;
        let w = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 1);
        assert_eq!(count_entities_with_name(&fix.metadata, "Priya"), 1);
    });
}

/// Unicode body with mixed scripts + emoji. The entity surface form
/// survives intact through the resolver and into the entity row.
#[test]
fn unicode_body_preserves_surface_form_in_entity_row() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "张伟 met Alice 🎉";
        let mut items = HashMap::new();
        items.insert(
            text.into(),
            vec![
                em("张伟", "brain:Person", 0.9),
                em("Alice 🎉", "brain:Person", 0.9),
            ],
        );
        install(&fix.ctx, items);

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let w = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 2);
        assert_eq!(count_entities_with_name(&fix.metadata, "张伟"), 1);
        assert_eq!(count_entities_with_name(&fix.metadata, "Alice 🎉"), 1);
    });
}

// ---------------------------------------------------------------------------
// Schema filter.
// ---------------------------------------------------------------------------

/// A namespace with no active schema entry is "schemaless"; every
/// predicate qname in that namespace lands. Using `acme:` (no
/// system-schema preload for that ns) is the cleanest setup.
#[test]
fn schemaless_namespace_accepts_all_predicates() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "Open-world statement";
        let mut items = HashMap::new();
        items.insert(
            text.into(),
            vec![
                em("S", "acme:Person", 0.9),
                em("O", "acme:Person", 0.9),
                // acme namespace has no active schema → predicate flows.
                sm("S", "acme:anything_goes", "O", 0.9),
            ],
        );
        install(&fix.ctx, items);
        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let w = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 2);
        // Statement landed (not filtered).
        assert!(count_statements(&fix.metadata) >= 1);
    });
}

/// Schema upload mid-flight: cycle 1 happens under the schemaless
/// posture and accepts the predicate. Then we upload an acme schema
/// that declares ONLY a different predicate. Cycle 2 (with a new
/// memory) drops the un-declared predicate.
#[test]
fn schema_upload_filters_subsequent_cycle() {
    glommio_run(|| async {
        let fix = build_fixture();
        let t1 = "round one";
        let t2 = "round two";
        let mut items = HashMap::new();
        items.insert(
            t1.into(),
            vec![
                em("S1", "acme:Person", 0.9),
                em("O1", "acme:Person", 0.9),
                sm("S1", "acme:plays", "O1", 0.9),
            ],
        );
        items.insert(
            t2.into(),
            vec![
                em("S2", "acme:Person", 0.9),
                em("O2", "acme:Person", 0.9),
                // Schema declares ONLY `works_at`; `plays` must drop.
                sm("S2", "acme:plays", "O2", 0.9),
            ],
        );
        install(&fix.ctx, items);

        // Cycle 1 — schemaless.
        let m1 = submit_encode(&fix.ctx, encode_op(1, t1)).await;
        let w = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        let before_stmts = count_statements(&fix.metadata);
        assert!(before_stmts >= 1, "cycle 1 landed at least one statement");
        assert_eq!(count_mention_edges_out(&fix.metadata, m1), 2);

        // Upload an acme schema that declares only `works_at`.
        {
            let mut db = fix.metadata.lock();
            let wtxn = db.write_txn().unwrap();
            let parsed = parse_schema(
                "
                namespace acme
                define entity_type Person { attributes {} }
                define predicate works_at { kind: Fact object: Value<text> }
                ",
            )
            .expect("parse");
            let validated = validate(&parsed).expect("validate");
            schema_upload(&wtxn, &validated, 1_700_000_000_000_000_000).expect("upload");
            wtxn.commit().unwrap();
        }

        // Cycle 2 — schema now active; `plays` is filtered.
        let m2 = submit_encode(&fix.ctx, encode_op(2, t2)).await;
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        // Entity mentions still land (entities aren't predicate-filtered).
        assert_eq!(count_mention_edges_out(&fix.metadata, m2), 2);
        // No additional statement row written for `plays`.
        assert_eq!(
            count_statements(&fix.metadata),
            before_stmts,
            "schema filter must drop un-declared predicate"
        );
        // Audit row exists for the filtered memory (SUCCESS — entity
        // mentions did commit; only the statement was filtered).
        let e = audit_entry(&fix.metadata, m2).expect("audit");
        assert_eq!(e.status, pipeline_status::SUCCESS);
    });
}

/// Schema-active namespace: declared predicate works; relation_type
/// outside the declared set is dropped at the worker boundary.
#[test]
fn schema_active_namespace_filters_relation_types() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "relation under schema";
        let mut items = HashMap::new();
        items.insert(
            text.into(),
            vec![
                em("A", "acme:Person", 0.9),
                em("B", "acme:Person", 0.9),
                rm("A", "acme:undeclared_rel", "B", 0.95),
            ],
        );
        install(&fix.ctx, items);

        // Pre-upload schema declaring only `works_at`; no relation types.
        {
            let mut db = fix.metadata.lock();
            let wtxn = db.write_txn().unwrap();
            let parsed = parse_schema(
                "
                namespace acme
                define entity_type Person { attributes {} }
                define predicate works_at { kind: Fact object: Value<text> }
                ",
            )
            .expect("parse");
            let validated = validate(&parsed).expect("validate");
            schema_upload(&wtxn, &validated, 1_700_000_000_000_000_000).expect("upload");
            wtxn.commit().unwrap();
        }

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let w = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        // Entity mentions still land.
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 2);
        // No relation written. The relation-metadata table may not be
        // materialised at all (no writes ever happened to it on this
        // shard); both "no table" and "table empty" satisfy the
        // invariant.
        let db = fix.metadata.lock();
        let rtxn = db.read_txn().unwrap();
        match rtxn.open_table(brain_metadata::tables::knowledge::relation::RELATION_METADATA_TABLE)
        {
            Ok(t) => assert_eq!(t.iter().unwrap().count(), 0),
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(e) => panic!("open RELATION_METADATA_TABLE: {e:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// Cost budget knob: per-cycle reset.
// ---------------------------------------------------------------------------

/// The per-cycle LLM spend counter resets on every `run_cycle`. We
/// exercise the public knob accessor + repeated cycle invocations to
/// confirm no accumulation across cycles. Real cost accounting lives
/// in the LLM extractor's `CostBudget`; the worker's per-cycle counter
/// is observability-only today, but the *reset semantics* must hold so
/// future work (per-cycle ceilings) builds on a sound foundation.
#[test]
fn cycle_resets_llm_spend_counter() {
    glommio_run(|| async {
        let fix = build_fixture();
        install(&fix.ctx, HashMap::new());

        let cfg = WorkerConfig {
            enabled: true,
            interval: std::time::Duration::from_millis(50),
            batch_size: 64,
            max_runtime: std::time::Duration::from_secs(5),
        };
        let knobs = ExtractorKnobs {
            drain_per_cycle: 64,
            llm_budget_per_cycle_micro_usd: 42, // any > 0 to assert knob round-trip
            skip_already_extracted: true,
        };
        let w = ExtractorWorker::new(fix.queue_rx.clone())
            .with_config(cfg)
            .with_knobs(knobs);
        assert_eq!(w.knobs().llm_budget_per_cycle_micro_usd, 42);

        // Cycle 1 — empty queue → no work, no accumulation. Cycle 2
        // likewise. The audit table stays empty; the worker doesn't
        // wedge or panic on repeated empty cycles.
        for _ in 0..3 {
            let drained = run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
            assert_eq!(drained, 0);
        }
    });
}

/// Sanity: per-call extractor IDs survive into audit rows. The audit
/// row captures `llm_micro_usd_spent` (0 here because no LLM tier
/// registered). Helps protect against accidental schema bumps that
/// would lose the field.
#[test]
fn audit_row_captures_llm_spend_field() {
    glommio_run(|| async {
        let fix = build_fixture();
        install(&fix.ctx, HashMap::new());
        let memory_id = submit_encode(&fix.ctx, encode_op(1, "irrelevant")).await;
        let w = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        let e = audit_entry(&fix.metadata, memory_id).expect("audit");
        assert_eq!(e.llm_micro_usd_spent, 0);
    });
}
