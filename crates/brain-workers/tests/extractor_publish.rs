#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send post-9.7
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
use parking_lot::Mutex;
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

fn build_fixture_with_registry(registry: ExtractorRegistry) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );
    let ops = OpsContext::new(executor).with_extractor_registry(registry);
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
    fixture
        .extractor_tx
        .send((memory_id, Arc::from("hello world")))
        .unwrap();

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
    fixture
        .extractor_tx
        .send((memory_id, Arc::from("broken pipeline")))
        .unwrap();

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
        let mut db = fixture.metadata.lock();
        let wtxn = db.write_txn().unwrap();
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

    fixture
        .extractor_tx
        .send((memory_id, Arc::from("already extracted memory")))
        .unwrap();

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
