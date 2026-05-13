//! End-to-end test for the Forget planner + executor (sub-task 6.6).
//!
//! Reuses the FakeWriterHandle pattern from encode_end_to_end.rs.
//! Tests cover:
//! - Encode-then-forget round-trip → `Tombstoned`.
//! - Recall-after-forget skips the tombstoned memory (via HNSW's
//!   tombstone bitmap, populated by `mark_tombstoned`).
//! - Forget of a never-encoded memory → `MemoryNotFound`.
//! - Soft vs hard forget — same observable behaviour from the read
//!   path; the apply step's zeroing is internal to the writer.
//! - Idempotent replay: same `request_id` twice → `replayed: true`.

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
    execute_encode, execute_forget, execute_recall, plan_encode, plan_forget, plan_recall,
    EncodeAck, EncodeOp, ExecutionPlan, ExecutorContext, ForgetAck, ForgetOp, ForgetOutcome,
    PlannerContext, SharedMetadataDb, WriterError, WriterHandle,
};
use brain_protocol::request::{
    EncodeRequest, ForgetMode, ForgetRequest, MemoryKindWire, RecallRequest,
};
use parking_lot::Mutex;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Mock dispatcher (copy of the encode test's; small enough to inline).
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
    fn install(&self, text: &str, v: [f32; VECTOR_DIM]) {
        self.map.lock().insert(text.to_string(), v);
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
// FakeWriterHandle — full encode + forget support.
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
    ) -> Pin<Box<dyn Future<Output = Result<EncodeAck, WriterError>> + 'a>> {
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
    ) -> Pin<Box<dyn Future<Output = Result<ForgetAck, WriterError>> + 'a>> {
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

    fn submit_link<'a>(
        &'a self,
        _: brain_planner::LinkOp,
    ) -> Pin<Box<dyn Future<Output = Result<brain_planner::LinkAck, WriterError>> + 'a>> {
        Box::pin(async move { Err(WriterError::Internal("fake writer: link unused".into())) })
    }

    fn submit_unlink<'a>(
        &'a self,
        _: brain_planner::UnlinkOp,
    ) -> Pin<Box<dyn Future<Output = Result<brain_planner::UnlinkAck, WriterError>> + 'a>> {
        Box::pin(async move { Err(WriterError::Internal("fake writer: unlink unused".into())) })
    }

    fn reserve_memory_id<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<brain_core::MemoryId, WriterError>> + 'a>,
    > {
        Box::pin(async move {
            Err(WriterError::Internal(
                "test writer: reserve_memory_id unused".into(),
            ))
        })
    }

    fn submit_batch<'a>(
        &'a self,
        _: brain_planner::TxnBatch,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<brain_planner::TxnBatchAck, WriterError>> + 'a>,
    > {
        Box::pin(async move {
            Err(WriterError::Internal(
                "test writer: submit_batch unused".into(),
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: ExecutorContext,
    mock: Arc<MockDispatcher>,
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
        mock.clone() as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );

    Fixture {
        ctx,
        mock,
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

fn forget_request(memory_id: MemoryId, mode: ForgetMode, request_id: [u8; 16]) -> ForgetRequest {
    ForgetRequest {
        memory_id: memory_id.raw(),
        mode,
        request_id,
        txn_id: None,
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
        txn_id: None,
    }
}

fn unwrap_encode(p: ExecutionPlan) -> brain_planner::EncodePlan {
    match p {
        ExecutionPlan::Encode(p) => p,
        other => panic!("expected Encode, got {other:?}"),
    }
}
fn unwrap_forget(p: ExecutionPlan) -> brain_planner::ForgetPlan {
    match p {
        ExecutionPlan::Forget(p) => p,
        other => panic!("expected Forget, got {other:?}"),
    }
}
fn unwrap_recall(p: ExecutionPlan) -> brain_planner::RecallPlan {
    match p {
        ExecutionPlan::Recall(p) => p,
        other => panic!("expected Recall, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_round_trips() {
    let fix = build_fixture();
    let encode_plan = plan_encode(
        &encode_request("hello", [1; 16]),
        &PlannerContext::default(),
    )
    .unwrap();
    let enc = execute_encode(unwrap_encode(encode_plan), &fix.ctx)
        .await
        .unwrap();

    let forget_plan = plan_forget(
        &forget_request(enc.memory_id, ForgetMode::Soft, [2; 16]),
        &PlannerContext::default(),
    )
    .unwrap();
    let result = execute_forget(unwrap_forget(forget_plan), &fix.ctx)
        .await
        .unwrap();
    assert_eq!(result.outcome, ForgetOutcome::Tombstoned);
    assert_eq!(result.memory_id, enc.memory_id);
    assert!(!result.replayed);
}

#[tokio::test]
async fn recall_after_forget_skips_memory() {
    let fix = build_fixture();
    // Two encodes with orthogonal vectors.
    let mut v0 = [0.0f32; VECTOR_DIM];
    v0[0] = 1.0;
    let mut v1 = [0.0f32; VECTOR_DIM];
    v1[1] = 1.0;
    fix.mock.install("alpha", v0);
    fix.mock.install("beta", v1);
    fix.mock.install("cue", v0); // cue is closest to alpha

    let alpha = execute_encode(
        unwrap_encode(
            plan_encode(
                &encode_request("alpha", [10; 16]),
                &PlannerContext::default(),
            )
            .unwrap(),
        ),
        &fix.ctx,
    )
    .await
    .unwrap();
    let _beta = execute_encode(
        unwrap_encode(
            plan_encode(
                &encode_request("beta", [11; 16]),
                &PlannerContext::default(),
            )
            .unwrap(),
        ),
        &fix.ctx,
    )
    .await
    .unwrap();

    // Recall before forget: alpha is top.
    let pre = execute_recall(
        unwrap_recall(plan_recall(&recall_request("cue", 5), &PlannerContext::default()).unwrap()),
        &fix.ctx,
    )
    .await
    .unwrap();
    assert!(!pre.hits.is_empty());
    assert_eq!(pre.hits[0].memory_id, alpha.memory_id);

    // Forget alpha.
    let f = execute_forget(
        unwrap_forget(
            plan_forget(
                &forget_request(alpha.memory_id, ForgetMode::Soft, [12; 16]),
                &PlannerContext::default(),
            )
            .unwrap(),
        ),
        &fix.ctx,
    )
    .await
    .unwrap();
    assert_eq!(f.outcome, ForgetOutcome::Tombstoned);

    // Recall after forget: alpha is gone (HNSW tombstone bitmap filters).
    let post = execute_recall(
        unwrap_recall(plan_recall(&recall_request("cue", 5), &PlannerContext::default()).unwrap()),
        &fix.ctx,
    )
    .await
    .unwrap();
    assert!(
        !post.hits.iter().any(|h| h.memory_id == alpha.memory_id),
        "forgotten memory should not appear in recall"
    );
}

#[tokio::test]
async fn forget_nonexistent_memory_returns_not_found() {
    let fix = build_fixture();
    let phantom = MemoryId::pack(0, 999, 1);
    let plan = plan_forget(
        &forget_request(phantom, ForgetMode::Soft, [20; 16]),
        &PlannerContext::default(),
    )
    .unwrap();
    let result = execute_forget(unwrap_forget(plan), &fix.ctx).await.unwrap();
    assert_eq!(result.outcome, ForgetOutcome::MemoryNotFound);
    assert_eq!(result.memory_id, phantom);
}

#[tokio::test]
async fn hard_forget_round_trips() {
    let fix = build_fixture();
    let enc = execute_encode(
        unwrap_encode(
            plan_encode(
                &encode_request("hard", [30; 16]),
                &PlannerContext::default(),
            )
            .unwrap(),
        ),
        &fix.ctx,
    )
    .await
    .unwrap();
    let plan = plan_forget(
        &forget_request(enc.memory_id, ForgetMode::Hard, [31; 16]),
        &PlannerContext::default(),
    )
    .unwrap();
    let f = unwrap_forget(plan);
    assert!(
        f.apply.arena_zero_vector,
        "hard mode plan should zero vector"
    );
    assert!(f.apply.text_zero, "hard mode plan should zero text");
    let result = execute_forget(f, &fix.ctx).await.unwrap();
    assert_eq!(result.outcome, ForgetOutcome::Tombstoned);
}

#[tokio::test]
async fn idempotent_replay_of_forget() {
    let fix = build_fixture();
    let enc = execute_encode(
        unwrap_encode(
            plan_encode(
                &encode_request("idem", [40; 16]),
                &PlannerContext::default(),
            )
            .unwrap(),
        ),
        &fix.ctx,
    )
    .await
    .unwrap();
    let req = forget_request(enc.memory_id, ForgetMode::Soft, [41; 16]);

    let first = execute_forget(
        unwrap_forget(plan_forget(&req, &PlannerContext::default()).unwrap()),
        &fix.ctx,
    )
    .await
    .unwrap();
    assert!(!first.replayed);

    let second = execute_forget(
        unwrap_forget(plan_forget(&req, &PlannerContext::default()).unwrap()),
        &fix.ctx,
    )
    .await
    .unwrap();
    assert!(
        second.replayed,
        "second submit with same request_id replays"
    );
    assert_eq!(first.memory_id, second.memory_id);
    assert_eq!(first.outcome, second.outcome);
}
