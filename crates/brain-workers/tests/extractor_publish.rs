#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Extractor worker — `StageCompleted` publish guarantees.
//!
//! Pins the wait-for-extraction contract: for every memory that
//! the writer enqueues onto the extractor channel, the worker
//! publishes exactly one `StageCompleted{Extractor}` envelope on
//! the per-shard event bus before the cycle ends. The earlier
//! cycle structure could drop the publish on the
//! skip-already-extracted branch and on the audit-failure-during-
//! apply-failure branch; this test file freezes the corrected
//! behavior.

use std::sync::Arc;
use std::time::Duration;

use brain_core::ExtractorKind;
use brain_core::{ExtractorId, Memory as CoreMemory, MemoryId};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_extractors::{
    ExtractedItem, ExtractionContext, ExtractionFuture, ExtractionResult, Extractor,
    ExtractorRegistry,
};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::extractor_audit::{
    pipeline_status, record_extracted, tier_status, ExtractorItemCounts,
    ExtractorPipelineAuditEntry,
};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::shared::enums::{
    EventType, StageAuditStatus, StageKind, StageOutcome, StagePayload,
};
use brain_workers::{ExtractorWorker, Worker, WorkerContext};
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};
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
    ops: Arc<OpsContext>,
    metadata: SharedMetadataDb,
    extractor_tx: flume::Sender<brain_ops::ExtractorEnqueue>,
    worker: ExtractorWorker,
    ctx: WorkerContext,
    _tempdir: tempfile::TempDir,
}

impl Fixture {
    /// Make a memory eligible for the durable extractor cycle: write
    /// its text into `TEXTS_TABLE` and durably enqueue it, both in one
    /// commit (mirrors `apply_upsert_memory`), then nudge the worker's
    /// wakeup channel. The worker reads its work from the durable
    /// queue + TEXTS_TABLE, not from the channel payload.
    fn enqueue(&self, memory_id: MemoryId, text: &str) {
        use brain_metadata::tables::text::TEXTS_TABLE;
        let wtxn = self.metadata.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&memory_id.to_be_bytes(), text.as_bytes()).unwrap();
        }
        brain_metadata::extraction_queue_enqueue(&wtxn, memory_id, now_unix_nanos()).unwrap();
        wtxn.commit().unwrap();
        // Wakeup hint only — contents ignored by the worker.
        let _ = self.extractor_tx.send((memory_id, Arc::from(text)));
    }
}

fn build_fixture_with_registry(registry: ExtractorRegistry) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );
    let ops = brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)
        .with_extractor_registry(registry);
    let ops = Arc::new(ops);

    let (tx, rx) = flume::bounded::<brain_ops::ExtractorEnqueue>(64);
    let worker = ExtractorWorker::new(rx);

    let ctx = WorkerContext {
        ops: ops.clone(),
        shutdown: Arc::new(AtomicBool::new(false)),
    };

    Fixture {
        ops,
        metadata,
        extractor_tx: tx,
        worker,
        ctx,
        _tempdir: tempdir,
    }
}

fn make_memory_id(slot: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[8..16].copy_from_slice(&slot.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

// ---------------------------------------------------------------------------
// Stub extractors. Empty-success exercises the happy decide_status
// path; pipeline-failure exercises the FAILURE pipeline_status path.
// ---------------------------------------------------------------------------

struct EmptySuccessStub {
    id: ExtractorId,
}

impl Extractor for EmptySuccessStub {
    fn id(&self) -> ExtractorId {
        self.id
    }
    fn kind(&self) -> ExtractorKind {
        ExtractorKind::Pattern
    }
    fn name(&self) -> &str {
        "test:empty_success"
    }
    fn extractor_version(&self) -> u32 {
        1
    }
    fn run<'a>(
        &'a self,
        _ctx: &'a ExtractionContext<'a>,
        _mem: &'a CoreMemory,
    ) -> ExtractionFuture<'a> {
        Box::pin(async { ExtractionResult::success(Vec::<ExtractedItem>::new(), 0, 0) })
    }
}

struct PipelineFailureStub {
    id: ExtractorId,
}

impl Extractor for PipelineFailureStub {
    fn id(&self) -> ExtractorId {
        self.id
    }
    fn kind(&self) -> ExtractorKind {
        ExtractorKind::Pattern
    }
    fn name(&self) -> &str {
        "test:pipeline_failure"
    }
    fn extractor_version(&self) -> u32 {
        1
    }
    fn run<'a>(
        &'a self,
        _ctx: &'a ExtractionContext<'a>,
        _mem: &'a CoreMemory,
    ) -> ExtractionFuture<'a> {
        Box::pin(async { ExtractionResult::failure("forced failure", 0, 0) })
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Drive `worker.run_cycle` to completion under a Tokio runtime.
/// The worker doesn't depend on Glommio's executor here because all
/// three tests drain ≤ 1 memory, so the periodic `yield_if_needed`
/// branch (every 4 drains) never fires.
async fn run_cycle(fixture: &Fixture) -> usize {
    fixture.worker.run_cycle(&fixture.ctx).await.unwrap()
}

/// Collect every event currently sitting on the bus receiver. Returns
/// after the first `Empty` to keep the tests bounded.
fn drain_bus(
    rx: &mut tokio::sync::broadcast::Receiver<brain_ops::EventEnvelope>,
) -> Vec<brain_ops::EventEnvelope> {
    let mut out = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(env) => out.push(env),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
        }
    }
    out
}

fn stage_completed_for(
    events: &[brain_ops::EventEnvelope],
    memory_id: MemoryId,
) -> Vec<&brain_ops::EventEnvelope> {
    events
        .iter()
        .filter(|e| e.event_type == EventType::StageCompleted && e.memory_id == memory_id)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// A single successful drain publishes exactly one
/// `StageCompleted{Extractor, Ok}` event with zero counts and
/// `audit_status = Succeeded`. Regression coverage: the happy path
/// must always publish.
#[tokio::test(flavor = "current_thread")]
async fn drain_success_publishes_one_ok_event() {
    let mut registry = ExtractorRegistry::new();
    registry.register(Arc::new(EmptySuccessStub {
        id: ExtractorId::from(1),
    }));
    let fixture = build_fixture_with_registry(registry);
    let mut rx = fixture.ops.events.receiver();

    let memory_id = make_memory_id(42);
    fixture.enqueue(memory_id, "hello world");

    let drained = run_cycle(&fixture).await;
    assert_eq!(drained, 1);

    // Yield once so the broadcast receiver sees the publish. The
    // broadcast channel delivers synchronously but a `tokio::yield_now`
    // is cheap insurance.
    tokio::time::sleep(Duration::from_millis(0)).await;

    let events = drain_bus(&mut rx);
    let stage = stage_completed_for(&events, memory_id);
    assert_eq!(
        stage.len(),
        1,
        "expected exactly one StageCompleted for memory; got {}",
        stage.len(),
    );
    let env = stage[0];
    assert_eq!(env.stage_kind, Some(StageKind::Extractor));
    assert_eq!(env.stage_outcome, Some(StageOutcome::Ok));
    match env.stage_payload.as_ref().expect("payload populated") {
        StagePayload::Extractor(p) => {
            assert_eq!(p.entity_count, 0);
            assert_eq!(p.statement_count, 0);
            assert_eq!(p.relation_count, 0);
            assert_eq!(p.audit_status, StageAuditStatus::Succeeded);
        }
        other => panic!("unexpected stage payload: {other:?}"),
    }
}

/// A drain whose pipeline run reports `Failure` from every tier
/// publishes exactly one `StageCompleted{Extractor, Failed}` event
/// with `audit_status = Failed`. The apply path itself succeeds
/// (writes a FAILURE audit row); the publish reflects the audit
/// byte. Regression coverage: failure outcomes still publish once.
#[tokio::test(flavor = "current_thread")]
async fn drain_pipeline_failure_publishes_one_failed_event() {
    let mut registry = ExtractorRegistry::new();
    registry.register(Arc::new(PipelineFailureStub {
        id: ExtractorId::from(2),
    }));
    let fixture = build_fixture_with_registry(registry);
    let mut rx = fixture.ops.events.receiver();

    let memory_id = make_memory_id(43);
    fixture.enqueue(memory_id, "broken pipeline");

    let drained = run_cycle(&fixture).await;
    assert_eq!(drained, 1);
    tokio::time::sleep(Duration::from_millis(0)).await;

    let events = drain_bus(&mut rx);
    let stage = stage_completed_for(&events, memory_id);
    assert_eq!(
        stage.len(),
        1,
        "expected exactly one StageCompleted for memory; got {}",
        stage.len(),
    );
    let env = stage[0];
    assert_eq!(env.stage_outcome, Some(StageOutcome::Failed));
    match env.stage_payload.as_ref().expect("payload populated") {
        StagePayload::Extractor(p) => {
            assert_eq!(p.audit_status, StageAuditStatus::Failed);
            assert_eq!(p.entity_count, 0);
            assert_eq!(p.statement_count, 0);
            assert_eq!(p.relation_count, 0);
        }
        other => panic!("unexpected stage payload: {other:?}"),
    }
}

/// End-to-end: the seeded `brain:entity_mentions` pattern extractor,
/// materialised verbatim from the system schema and driven through a
/// full worker cycle, must persist at least one entity for entity-rich
/// text. This reproduces the `entities=0` ENCODE bug at the apply
/// boundary — if the entity count comes back zero here, candidates are
/// being dropped at resolution / write, not at the tier.
#[tokio::test(flavor = "current_thread")]
async fn seeded_pattern_extractor_persists_entities_end_to_end() {
    // Build the fixture first so its DB is seeded with the system
    // schema; then read the seeded pattern extractor def back out and
    // materialise it into the worker's registry — exactly the shard
    // path minus the live GLiNER model.
    let mut fixture = build_fixture_with_registry(ExtractorRegistry::new());
    let pattern_def = {
        let rtxn = fixture.metadata.read_txn().unwrap();
        let defs = brain_metadata::extractor_list(&rtxn).expect("extractor_list");
        defs.into_iter()
            .find(|d| d.kind() == Some(ExtractorKind::Pattern))
            .expect("system schema seeds a pattern extractor")
    };
    let pattern = brain_extractors::materialize_pattern_extractor(&pattern_def)
        .expect("materialize seeded pattern extractor");
    let mut registry = ExtractorRegistry::new();
    registry.register(Arc::new(pattern));

    // Rebuild the fixture with the populated registry, keeping the same
    // seeded DB semantics (a fresh seeded DB is equivalent).
    fixture = build_fixture_with_registry(registry);
    let mut rx = fixture.ops.events.receiver();

    let memory_id = make_memory_id(99);
    fixture.enqueue(
        memory_id,
        "Priya Sharma joined Stripe as a Senior Engineer in San Francisco",
    );

    let drained = run_cycle(&fixture).await;
    assert_eq!(drained, 1);
    tokio::time::sleep(Duration::from_millis(0)).await;

    let events = drain_bus(&mut rx);
    let stage = stage_completed_for(&events, memory_id);
    assert_eq!(stage.len(), 1, "expected one StageCompleted");
    match stage[0].stage_payload.as_ref().expect("payload") {
        StagePayload::Extractor(p) => {
            assert!(
                p.entity_count > 0,
                "seeded pattern extractor must persist entities for entity-rich text; \
                 got entity_count=0 with audit_status={:?}",
                p.audit_status,
            );
        }
        other => panic!("unexpected payload: {other:?}"),
    }
}

/// Gated shard-faithful reproduction: build the registry with ALL
/// THREE tiers exactly as `brain-server` does — seeded defs + the
/// real GLiNER model + an entity-type-qname snapshot read from the
/// seeded DB — then drive a full cycle. This is the closest a test
/// can get to the live shard short of booting the server. If this
/// yields `entities=0`, the bug lives in the cross-tier wiring.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires BRAIN_NER_MODEL_PATH pointing at a GLiNER pickle directory"]
async fn shard_registry_with_real_gliner_persists_entities() {
    use brain_extractors::{ClassifierConfig, GlinerClassifier, MaterializeDeps, TierGate};
    use redb::ReadableTable;
    use std::path::PathBuf;

    let model_path: PathBuf = std::env::var("BRAIN_NER_MODEL_PATH")
        .expect("set BRAIN_NER_MODEL_PATH")
        .into();

    // Build a fixture (seeds the system schema), then construct the
    // registry the way the shard does.
    let fixture = build_fixture_with_registry(ExtractorRegistry::new());
    let (defs, entity_type_qnames) = {
        let rtxn = fixture.metadata.read_txn().unwrap();
        let defs = brain_metadata::extractor_list(&rtxn).expect("extractor_list");
        let t = rtxn
            .open_table(brain_metadata::tables::entity_type::ENTITY_TYPES_TABLE)
            .unwrap();
        let mut rows: Vec<(u32, String)> = Vec::new();
        for entry in t.iter().unwrap() {
            let (k, v) = entry.unwrap();
            rows.push((k.value(), v.value().name));
        }
        rows.sort_by_key(|(id, _)| *id);
        let qnames: Vec<String> = rows
            .into_iter()
            .map(|(_, name)| format!("brain:{name}"))
            .collect();
        (defs, qnames)
    };
    assert!(
        !entity_type_qnames.is_empty(),
        "system schema must seed entity types for the classifier labels",
    );

    let model = GlinerClassifier::load(&ClassifierConfig::with_model_path(model_path))
        .expect("load gliner");
    let deps = MaterializeDeps {
        classifier_model: Some(Arc::new(model)),
        entity_type_qnames: Arc::new(entity_type_qnames),
        model_router: None,
        llm_cache: None,
    };
    let (registry, errors) =
        brain_extractors::build_registry_with_gate(&defs, &deps, TierGate::all_enabled());
    assert!(errors.is_empty(), "registry build errors: {errors:?}");
    assert_eq!(
        registry.iter_enabled().count(),
        3,
        "expected 3 enabled tiers"
    );

    // Rebuild the fixture with this registry (fresh seeded DB).
    let fixture = build_fixture_with_registry(registry);
    let mut rx = fixture.ops.events.receiver();
    let memory_id = make_memory_id(123);
    fixture.enqueue(
        memory_id,
        "Priya Sharma joined Stripe as a Senior Engineer in San Francisco",
    );

    let drained = run_cycle(&fixture).await;
    assert_eq!(drained, 1);
    tokio::time::sleep(Duration::from_millis(0)).await;

    let events = drain_bus(&mut rx);
    let stage = stage_completed_for(&events, memory_id);
    assert_eq!(stage.len(), 1);
    match stage[0].stage_payload.as_ref().expect("payload") {
        StagePayload::Extractor(p) => {
            assert!(
                p.entity_count > 0,
                "shard-faithful registry yielded entity_count=0 (audit_status={:?})",
                p.audit_status,
            );
        }
        other => panic!("unexpected payload: {other:?}"),
    }
}

/// A drain whose memory already has an audit row publishes exactly
/// one `StageCompleted{Extractor, Empty}` event with
/// `audit_status = Skipped`. **This is the behavior change** — the
/// pre-fix cycle silently `continue`d on this branch and dropped
/// the publish, stranding wait-for-extraction subscribers.
#[tokio::test(flavor = "current_thread")]
async fn drain_already_extracted_publishes_one_empty_event() {
    let registry = ExtractorRegistry::new();
    let fixture = build_fixture_with_registry(registry);
    let mut rx = fixture.ops.events.receiver();

    let memory_id = make_memory_id(44);

    // Pre-seed the audit table so the gate probe finds the row and
    // the cycle takes the AlreadyExtracted branch.
    {
        let wtxn = fixture.metadata.write_txn().unwrap();
        let entry = ExtractorPipelineAuditEntry::new(
            memory_id,
            now_unix_nanos(),
            pipeline_status::SUCCESS,
            String::new(),
            tier_status::ABSENT,
            tier_status::ABSENT,
            tier_status::ABSENT,
            ExtractorItemCounts::zero(),
            0,
        );
        record_extracted(&wtxn, &entry).unwrap();
        wtxn.commit().unwrap();
    }

    fixture.enqueue(memory_id, "already extracted memory");

    let drained = run_cycle(&fixture).await;
    assert_eq!(drained, 1);
    tokio::time::sleep(Duration::from_millis(0)).await;

    let events = drain_bus(&mut rx);
    let stage = stage_completed_for(&events, memory_id);
    assert_eq!(
        stage.len(),
        1,
        "expected exactly one StageCompleted for already-extracted memory; got {} (pre-fix behavior was zero — wait-for-extraction would hang)",
        stage.len(),
    );
    let env = stage[0];
    assert_eq!(env.stage_outcome, Some(StageOutcome::Empty));
    match env.stage_payload.as_ref().expect("payload populated") {
        StagePayload::Extractor(p) => {
            assert_eq!(p.audit_status, StageAuditStatus::Skipped);
        }
        other => panic!("unexpected stage payload: {other:?}"),
    }
    // The `_uuid` is unused but suppresses dead-code warnings if the
    // test helper above ever stops needing it.
    let _ = Uuid::nil();
}

/// Read the durable extraction-queue depth.
fn queue_len(metadata: &SharedMetadataDb) -> u64 {
    let rtxn = metadata.read_txn().unwrap();
    brain_metadata::extraction_queue_len(&rtxn).unwrap()
}

/// Whether the durable queue holds a row for `memory_id`.
fn queue_contains(metadata: &SharedMetadataDb, memory_id: MemoryId) -> bool {
    let rtxn = metadata.read_txn().unwrap();
    brain_metadata::extraction_queue_drain(&rtxn, 1024)
        .unwrap()
        .iter()
        .any(|(id, _)| *id == memory_id)
}

/// After a memory's extraction commits, the worker removes its durable
/// `EXTRACTION_QUEUE_TABLE` row so a later cycle doesn't re-drain it.
/// The audit gate would prevent re-extraction anyway, but a stale row
/// would make the queue grow without bound.
#[tokio::test(flavor = "current_thread")]
async fn drained_memory_removes_its_queue_row() {
    let mut registry = ExtractorRegistry::new();
    registry.register(Arc::new(EmptySuccessStub {
        id: ExtractorId::from(1),
    }));
    let fixture = build_fixture_with_registry(registry);

    let memory_id = make_memory_id(77);
    fixture.enqueue(memory_id, "remove me after extraction");
    assert_eq!(queue_len(&fixture.metadata), 1, "row enqueued");

    let drained = run_cycle(&fixture).await;
    assert_eq!(drained, 1);

    assert!(
        !queue_contains(&fixture.metadata, memory_id),
        "extracted memory's queue row must be removed",
    );
    assert_eq!(queue_len(&fixture.metadata), 0, "queue drained empty");
}

/// A drained memory that already has an audit row (a stale queue row
/// left by, e.g., a crash between the extraction commit and the queue
/// remove) is removed on the next cycle via the audit-gate path —
/// WITHOUT re-running extraction. Pins the resumable-cleanup contract.
#[tokio::test(flavor = "current_thread")]
async fn already_audited_memory_removes_stale_queue_row_without_reextracting() {
    // A stub that PANICS if invoked — proves the audit gate short-
    // circuits before any tier runs.
    struct NeverRunStub;
    impl Extractor for NeverRunStub {
        fn id(&self) -> ExtractorId {
            ExtractorId::from(9)
        }
        fn kind(&self) -> ExtractorKind {
            ExtractorKind::Pattern
        }
        fn name(&self) -> &str {
            "test:never_run"
        }
        fn extractor_version(&self) -> u32 {
            1
        }
        fn run<'a>(
            &'a self,
            _ctx: &'a ExtractionContext<'a>,
            _mem: &'a CoreMemory,
        ) -> ExtractionFuture<'a> {
            Box::pin(async { panic!("extraction must not run for an already-audited memory") })
        }
    }

    let mut registry = ExtractorRegistry::new();
    registry.register(Arc::new(NeverRunStub));
    let fixture = build_fixture_with_registry(registry);

    let memory_id = make_memory_id(88);

    // Pre-seed an audit row (memory already extracted) AND a stale
    // durable queue row + text (the row that a crash would leave).
    {
        let wtxn = fixture.metadata.write_txn().unwrap();
        let entry = ExtractorPipelineAuditEntry::new(
            memory_id,
            now_unix_nanos(),
            pipeline_status::SUCCESS,
            String::new(),
            tier_status::ABSENT,
            tier_status::ABSENT,
            tier_status::ABSENT,
            ExtractorItemCounts::zero(),
            0,
        );
        record_extracted(&wtxn, &entry).unwrap();
        wtxn.commit().unwrap();
    }
    fixture.enqueue(memory_id, "stale queue row, already audited");
    assert_eq!(queue_len(&fixture.metadata), 1, "stale row present");

    // The cycle must take the AlreadyExtracted branch (NeverRunStub
    // would panic otherwise) and clean up the stale row.
    let drained = run_cycle(&fixture).await;
    assert_eq!(drained, 1);

    assert!(
        !queue_contains(&fixture.metadata, memory_id),
        "stale queue row must be removed via the audit-gate cleanup path",
    );
    assert_eq!(queue_len(&fixture.metadata), 0);
}
