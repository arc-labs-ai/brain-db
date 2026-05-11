//! Top-level `execute` dispatch tests (sub-task 6.7).
//!
//! Verifies that `execute(plan, &ctx).await` routes to the correct
//! per-variant executor and that PLAN / REASON plans surface
//! `ExecError::Unsupported`.
//!
//! The harness mirrors the `FakeWriterHandle` pattern from
//! `encode_end_to_end.rs` / `forget_end_to_end.rs`; we accept the
//! duplication for now. If a fourth test file ever needs it, factor
//! to `tests/common/mod.rs`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use brain_core::{AgentId, MemoryId, RequestId};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw, Writer as HnswWriter};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_planner::{
    execute, plan_encode, plan_forget, plan_path, plan_reason, plan_recall, EncodeAck, EncodeOp,
    ExecError, ExecutionResult, ExecutorContext, ForgetAck, ForgetOp, ForgetOutcome,
    PlannerContext, SharedMetadataDb, WriterError, WriterHandle,
};
use brain_protocol::request::{
    EncodeRequest, ForgetMode, ForgetRequest, MemoryKindWire, ObservationInput, PlanBudget,
    PlanRequest, PlanState, ReasonRequest, RecallRequest,
};
use parking_lot::Mutex;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Mock dispatcher.
// ---------------------------------------------------------------------------

struct MockDispatcher {
    fp: [u8; 16],
    map: Mutex<HashMap<String, [f32; VECTOR_DIM]>>,
}

impl MockDispatcher {
    fn new(fp: [u8; 16]) -> Self {
        Self {
            fp,
            map: Mutex::new(HashMap::new()),
        }
    }
}

impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        if let Some(v) = self.map.lock().get(text) {
            return Ok(*v);
        }
        let mut v = [0.0f32; VECTOR_DIM];
        v[0] = text.bytes().next().unwrap_or(0) as f32;
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > 1e-8 {
            for x in &mut v {
                *x /= n;
            }
        }
        Ok(v)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        self.fp
    }
}

// ---------------------------------------------------------------------------
// FakeWriterHandle.
// ---------------------------------------------------------------------------

struct FakeWriterHandle {
    inner: Mutex<FakeWriterState>,
}

struct FakeWriterState {
    next_slot: u64,
    metadata: SharedMetadataDb,
    hnsw_writer: HnswWriter<VECTOR_DIM>,
    encode_seen: HashMap<RequestId, EncodeAck>,
    forget_seen: HashMap<RequestId, ForgetAck>,
    tombstoned: HashSet<MemoryId>,
}

impl FakeWriterHandle {
    fn new(metadata: SharedMetadataDb, hnsw_writer: HnswWriter<VECTOR_DIM>) -> Self {
        Self {
            inner: Mutex::new(FakeWriterState {
                next_slot: 1,
                metadata,
                hnsw_writer,
                encode_seen: HashMap::new(),
                forget_seen: HashMap::new(),
                tombstoned: HashSet::new(),
            }),
        }
    }
}

impl WriterHandle for FakeWriterHandle {
    fn submit_encode<'a>(
        &'a self,
        op: EncodeOp,
    ) -> Pin<Box<dyn Future<Output = Result<EncodeAck, WriterError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self.inner.lock();
            if let Some(cached) = state.encode_seen.get(&op.request_id) {
                let mut replayed = cached.clone();
                replayed.replayed = true;
                return Ok(replayed);
            }
            let slot = state.next_slot;
            state.next_slot += 1;
            let memory_id = MemoryId::pack(0, slot, 1);
            {
                let mut db = state.metadata.lock();
                let wtxn = db
                    .write_txn()
                    .map_err(|e| WriterError::Internal(format!("write_txn: {e:?}")))?;
                {
                    let mut table = wtxn
                        .open_table(MEMORIES_TABLE)
                        .map_err(|e| WriterError::Internal(format!("open_table: {e:?}")))?;
                    let meta = MemoryMetadata::new_active(
                        memory_id,
                        AgentId(Uuid::nil()),
                        op.context_id,
                        slot,
                        1,
                        op.kind,
                        op.fingerprint,
                        op.salience_initial,
                        op.text.len() as u32,
                        1_000_000 + slot,
                    );
                    table
                        .insert(memory_id.to_be_bytes(), meta)
                        .map_err(|e| WriterError::Internal(format!("insert: {e:?}")))?;
                }
                wtxn.commit()
                    .map_err(|e| WriterError::Internal(format!("commit: {e:?}")))?;
            }
            state
                .hnsw_writer
                .insert(memory_id, &op.vector)
                .map_err(|e| WriterError::Internal(format!("hnsw insert: {e:?}")))?;
            let ack = EncodeAck {
                memory_id,
                edge_results: Vec::new(),
                replayed: false,
            };
            state.encode_seen.insert(op.request_id, ack.clone());
            Ok(ack)
        })
    }

    fn submit_forget<'a>(
        &'a self,
        op: ForgetOp,
    ) -> Pin<Box<dyn Future<Output = Result<ForgetAck, WriterError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self.inner.lock();
            if let Some(cached) = state.forget_seen.get(&op.request_id) {
                let mut replayed = *cached;
                replayed.replayed = true;
                return Ok(replayed);
            }
            if state.tombstoned.contains(&op.memory_id) {
                let ack = ForgetAck {
                    memory_id: op.memory_id,
                    outcome: ForgetOutcome::AlreadyTombstoned,
                    replayed: false,
                };
                state.forget_seen.insert(op.request_id, ack);
                return Ok(ack);
            }
            let exists = state
                .metadata
                .lock()
                .read_txn()
                .ok()
                .and_then(|t| t.open_table(MEMORIES_TABLE).ok())
                .and_then(|tbl| tbl.get(op.memory_id.to_be_bytes()).ok().flatten())
                .is_some();
            if !exists {
                let ack = ForgetAck {
                    memory_id: op.memory_id,
                    outcome: ForgetOutcome::MemoryNotFound,
                    replayed: false,
                };
                state.forget_seen.insert(op.request_id, ack);
                return Ok(ack);
            }
            state
                .hnsw_writer
                .mark_tombstoned(op.memory_id)
                .map_err(|e| WriterError::Internal(format!("mark_tombstoned: {e:?}")))?;
            state.tombstoned.insert(op.memory_id);
            let ack = ForgetAck {
                memory_id: op.memory_id,
                outcome: ForgetOutcome::Tombstoned,
                replayed: false,
            };
            state.forget_seen.insert(op.request_id, ack);
            Ok(ack)
        })
    }
}

// ---------------------------------------------------------------------------
// Fixture builder.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: ExecutorContext,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let mock = Arc::new(MockDispatcher::new([0x11; 16]));
    let writer = Arc::new(FakeWriterHandle::new(Arc::clone(&metadata), hnsw_writer));
    let ctx = ExecutorContext::new(
        mock as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx,
        _tempdir: tempdir,
    }
}

fn encode_request(text: &str, request_id: [u8; 16]) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 42,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: Vec::new(),
        request_id,
        txn_id: None,
        deduplicate: false,
    }
}

fn recall_request(cue: &str, top_k: u32) -> RecallRequest {
    RecallRequest {
        cue_text: cue.into(),
        cue_vector_offset: 0,
        cue_vector_dim: 0,
        top_k,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        strategy_hint: None,
        include_vectors: false,
        include_edges: false,
        request_id: None,
    }
}

fn forget_request(memory_id: MemoryId, request_id: [u8; 16]) -> ForgetRequest {
    ForgetRequest {
        memory_id: memory_id.raw(),
        mode: ForgetMode::Soft,
        request_id,
        txn_id: None,
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dispatch_encode_returns_encode_variant() {
    let fix = build_fixture();
    let plan = plan_encode(&encode_request("hi", [1; 16]), &PlannerContext::default()).unwrap();
    let result = execute(plan, &fix.ctx).await.unwrap();
    match result {
        ExecutionResult::Encode(r) => {
            assert_eq!(r.memory_id.shard(), 0);
            assert_eq!(r.memory_id.slot(), 1);
            assert!(!r.replayed);
        }
        other => panic!("expected Encode variant, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_recall_returns_recall_variant() {
    let fix = build_fixture();
    // Encode something so recall has a non-empty index.
    let _ = execute(
        plan_encode(&encode_request("hi", [1; 16]), &PlannerContext::default()).unwrap(),
        &fix.ctx,
    )
    .await
    .unwrap();
    let plan = plan_recall(&recall_request("hi", 5), &PlannerContext::default()).unwrap();
    let result = execute(plan, &fix.ctx).await.unwrap();
    match result {
        ExecutionResult::Recall(r) => {
            assert!(!r.hits.is_empty());
        }
        other => panic!("expected Recall variant, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_forget_returns_forget_variant() {
    let fix = build_fixture();
    let enc = execute(
        plan_encode(&encode_request("hi", [1; 16]), &PlannerContext::default()).unwrap(),
        &fix.ctx,
    )
    .await
    .unwrap();
    let memory_id = match enc {
        ExecutionResult::Encode(r) => r.memory_id,
        _ => unreachable!(),
    };
    let plan = plan_forget(
        &forget_request(memory_id, [2; 16]),
        &PlannerContext::default(),
    )
    .unwrap();
    let result = execute(plan, &fix.ctx).await.unwrap();
    match result {
        ExecutionResult::Forget(r) => {
            assert_eq!(r.outcome, ForgetOutcome::Tombstoned);
            assert_eq!(r.memory_id, memory_id);
        }
        other => panic!("expected Forget variant, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_plan_variant_is_unsupported() {
    let fix = build_fixture();
    let req = PlanRequest {
        start: PlanState::ByText("origin".into()),
        goal: PlanState::ByText("destination".into()),
        budget: PlanBudget {
            max_steps: 4,
            max_wall_time_ms: 100,
            max_branches_explored: 64,
        },
        strategy_hint: None,
        context_filter: None,
        request_id: None,
    };
    let plan = plan_path(&req, &PlannerContext::default()).unwrap();
    match execute(plan, &fix.ctx).await {
        Err(ExecError::Unsupported(msg)) => {
            assert!(msg.contains("PLAN"), "expected PLAN message, got {msg}");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_reason_variant_is_unsupported() {
    let fix = build_fixture();
    let req = ReasonRequest {
        observation: ObservationInput::ByText("hello".into()),
        depth: 3,
        confidence_threshold: 0.5,
        context_filter: None,
        max_inferences: 5,
        budget_wall_time_ms: 100,
        request_id: None,
    };
    let plan = plan_reason(&req, &PlannerContext::default()).unwrap();
    match execute(plan, &fix.ctx).await {
        Err(ExecError::Unsupported(msg)) => {
            assert!(msg.contains("REASON"), "expected REASON message, got {msg}");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_encode_recall_forget_smoke_test() {
    let fix = build_fixture();

    // Encode.
    let enc = execute(
        plan_encode(
            &encode_request("alpha", [1; 16]),
            &PlannerContext::default(),
        )
        .unwrap(),
        &fix.ctx,
    )
    .await
    .unwrap();
    let memory_id = match enc {
        ExecutionResult::Encode(r) => r.memory_id,
        _ => panic!("expected Encode variant"),
    };

    // Recall — should find it.
    let recall = execute(
        plan_recall(&recall_request("alpha", 5), &PlannerContext::default()).unwrap(),
        &fix.ctx,
    )
    .await
    .unwrap();
    match recall {
        ExecutionResult::Recall(r) => {
            assert!(r.hits.iter().any(|h| h.memory_id == memory_id));
        }
        _ => panic!("expected Recall variant"),
    }

    // Forget.
    let forget = execute(
        plan_forget(
            &forget_request(memory_id, [2; 16]),
            &PlannerContext::default(),
        )
        .unwrap(),
        &fix.ctx,
    )
    .await
    .unwrap();
    match forget {
        ExecutionResult::Forget(r) => assert_eq!(r.outcome, ForgetOutcome::Tombstoned),
        _ => panic!("expected Forget variant"),
    }

    // Recall again — the memory is gone.
    let post = execute(
        plan_recall(&recall_request("alpha", 5), &PlannerContext::default()).unwrap(),
        &fix.ctx,
    )
    .await
    .unwrap();
    match post {
        ExecutionResult::Recall(r) => {
            assert!(!r.hits.iter().any(|h| h.memory_id == memory_id));
        }
        _ => panic!("expected Recall variant"),
    }
}
