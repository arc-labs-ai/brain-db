//! End-to-end test for the Encode planner + executor (sub-task 6.4).
//!
//! Harness: real `MetadataDb` + real `SharedHnsw` + `MockDispatcher`
//! (deterministic vectors per text) + `FakeWriterHandle` that drives
//! both stores synchronously without the WAL — the writer trait's
//! contract is exercised; the durability story is Phase 8/9's job.
//!
//! Also covers: encode_then_recall_finds_it — runs ENCODE through 6.4
//! then RECALL through 6.3 against the same fixture.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, RequestId};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw, Writer as HnswWriter};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_planner::{
    execute_encode, execute_recall, plan_encode, plan_recall, EdgeOutcome, EncodeAck, EncodeOp,
    ExecutionPlan, ExecutorContext, ForgetAck, ForgetOp, ForgetOutcome, PlannerContext,
    SharedMetadataDb, WriterError, WriterHandle,
};
use brain_protocol::request::{EncodeRequest, MemoryKindWire, RecallRequest};
use parking_lot::Mutex;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Mock dispatcher (same shape as recall_end_to_end.rs).
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
// FakeWriterHandle — drives test stores directly without WAL.
//
// Spec §08/04 §4 idempotency: keeps a HashMap<RequestId, EncodeAck>;
// second submit with the same RequestId returns the cached ack with
// `replayed: true`.
// ---------------------------------------------------------------------------

struct FakeWriterHandle {
    inner: Mutex<FakeWriterState>,
    submit_count: AtomicU64,
}

struct FakeWriterState {
    next_slot: u64,
    metadata: SharedMetadataDb,
    hnsw_writer: HnswWriter<VECTOR_DIM>,
    /// Encode-side idempotency replay table; spec §08/04 §4.
    seen: HashMap<RequestId, EncodeAck>,
    /// Forget-side idempotency replay table; spec §08/06 §7.
    forget_seen: HashMap<RequestId, ForgetAck>,
    /// Memories we've already tombstoned via FORGET. Lets the next
    /// submit return `AlreadyTombstoned` for the same memory_id.
    tombstoned: std::collections::HashSet<brain_core::MemoryId>,
}

impl FakeWriterHandle {
    fn new(metadata: SharedMetadataDb, hnsw_writer: HnswWriter<VECTOR_DIM>) -> Self {
        Self {
            inner: Mutex::new(FakeWriterState {
                next_slot: 1,
                metadata,
                hnsw_writer,
                seen: HashMap::new(),
                forget_seen: HashMap::new(),
                tombstoned: std::collections::HashSet::new(),
            }),
            submit_count: AtomicU64::new(0),
        }
    }
    fn submit_count(&self) -> u64 {
        self.submit_count.load(Ordering::Relaxed)
    }
}

impl WriterHandle for FakeWriterHandle {
    fn submit_encode<'a>(
        &'a self,
        op: EncodeOp,
    ) -> Pin<Box<dyn Future<Output = Result<EncodeAck, WriterError>> + 'a>> {
        Box::pin(async move {
            self.submit_count.fetch_add(1, Ordering::Relaxed);
            let mut state = self.inner.lock();

            // Idempotency replay.
            if let Some(cached) = state.seen.get(&op.request_id) {
                let mut replayed = cached.clone();
                replayed.replayed = true;
                return Ok(replayed);
            }

            // Allocate a slot + pack a MemoryId.
            let slot = state.next_slot;
            state.next_slot += 1;
            let memory_id = MemoryId::pack(
                /* shard */ 0, /* slot */ slot, /* version */ 1,
            );

            // Write metadata row.
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
                        /* slot_version */ 1,
                        op.kind,
                        op.fingerprint,
                        op.salience_initial,
                        op.text.len() as u32,
                        /* created_at */ 1_000_000 + slot,
                    );
                    table
                        .insert(memory_id.to_be_bytes(), meta)
                        .map_err(|e| WriterError::Internal(format!("insert: {e:?}")))?;
                }
                wtxn.commit()
                    .map_err(|e| WriterError::Internal(format!("commit: {e:?}")))?;
            }

            // Insert into HNSW.
            state
                .hnsw_writer
                .insert(memory_id, &op.vector)
                .map_err(|e| WriterError::Internal(format!("hnsw insert: {e:?}")))?;

            // Edge outcomes: for the fake writer, every edge whose target
            // is a known MemoryId (sees it in the metadata table) counts
            // as Inserted; missing → TargetMissing. We don't actually
            // persist edges in this fake — Phase 7+ exercises real edges.
            let mut edge_results = Vec::with_capacity(op.edges.len());
            for edge in &op.edges {
                let exists = state
                    .metadata
                    .lock()
                    .read_txn()
                    .ok()
                    .and_then(|t| t.open_table(MEMORIES_TABLE).ok())
                    .and_then(|tbl| tbl.get(edge.target.to_be_bytes()).ok().flatten())
                    .is_some();
                edge_results.push(if exists {
                    EdgeOutcome::Inserted
                } else {
                    EdgeOutcome::TargetMissing
                });
            }

            let ack = EncodeAck {
                memory_id,
                edge_results,
                replayed: false,
            };
            state.seen.insert(op.request_id, ack.clone());
            Ok(ack)
        })
    }

    fn submit_forget<'a>(
        &'a self,
        op: ForgetOp,
    ) -> Pin<Box<dyn Future<Output = Result<ForgetAck, WriterError>> + 'a>> {
        Box::pin(async move {
            self.submit_count.fetch_add(1, Ordering::Relaxed);
            let mut state = self.inner.lock();

            // Idempotency replay.
            if let Some(cached) = state.forget_seen.get(&op.request_id) {
                let mut replayed = *cached;
                replayed.replayed = true;
                return Ok(replayed);
            }

            // Already tombstoned by an earlier FORGET for the same id?
            if state.tombstoned.contains(&op.memory_id) {
                let ack = ForgetAck {
                    memory_id: op.memory_id,
                    outcome: ForgetOutcome::AlreadyTombstoned,
                    replayed: false,
                };
                state.forget_seen.insert(op.request_id, ack);
                return Ok(ack);
            }

            // Look up the memory in metadata.
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

            // Tombstone in HNSW so recall skips this memory.
            state
                .hnsw_writer
                .mark_tombstoned(op.memory_id)
                .map_err(|e| WriterError::Internal(format!("hnsw mark_tombstoned: {e:?}")))?;
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
    writer: Arc<FakeWriterHandle>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata = MetadataDb::open(&db_path).unwrap();
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(metadata));

    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();

    let mock = Arc::new(MockDispatcher::new([0x11; 16]));
    let writer = Arc::new(FakeWriterHandle::new(Arc::clone(&metadata), hnsw_writer));

    let ctx = ExecutorContext::new(
        mock.clone() as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer.clone() as Arc<dyn WriterHandle>,
    );

    Fixture {
        ctx,
        mock,
        writer,
        _tempdir: tempdir,
    }
}

fn base_request(text: &str, request_id: [u8; 16]) -> EncodeRequest {
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

fn unwrap_encode(plan: ExecutionPlan) -> brain_planner::EncodePlan {
    match plan {
        ExecutionPlan::Encode(p) => p,
        other => panic!("expected Encode, got {other:?}"),
    }
}

fn unwrap_recall(plan: ExecutionPlan) -> brain_planner::RecallPlan {
    match plan {
        ExecutionPlan::Recall(p) => p,
        other => panic!("expected Recall, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn encode_returns_memory_id() {
    let fix = build_fixture();
    let plan = plan_encode(&base_request("hello", [1; 16]), &PlannerContext::default()).unwrap();
    let result = execute_encode(unwrap_encode(plan), &fix.ctx).await.unwrap();
    assert_eq!(result.memory_id.shard(), 0);
    assert_eq!(result.memory_id.slot(), 1);
    assert_eq!(result.memory_id.version(), 1);
    assert!(!result.replayed);
}

#[tokio::test]
async fn idempotent_replay_returns_cached_ack() {
    let fix = build_fixture();
    let req = base_request("hello", [2; 16]);
    let plan = plan_encode(&req, &PlannerContext::default()).unwrap();
    let first = execute_encode(unwrap_encode(plan), &fix.ctx).await.unwrap();
    assert!(!first.replayed);

    // Second submit with the same request_id → cached.
    let plan2 = plan_encode(&req, &PlannerContext::default()).unwrap();
    let second = execute_encode(unwrap_encode(plan2), &fix.ctx)
        .await
        .unwrap();
    assert!(second.replayed, "second encode should be replayed");
    assert_eq!(first.memory_id, second.memory_id);
    // Writer still saw two submits — its idempotency cache is what
    // de-duplicated, not anything earlier.
    assert_eq!(fix.writer.submit_count(), 2);
}

#[tokio::test]
async fn distinct_request_ids_produce_distinct_memory_ids() {
    let fix = build_fixture();
    let r1 = execute_encode(
        unwrap_encode(
            plan_encode(&base_request("a", [3; 16]), &PlannerContext::default()).unwrap(),
        ),
        &fix.ctx,
    )
    .await
    .unwrap();
    let r2 = execute_encode(
        unwrap_encode(
            plan_encode(&base_request("b", [4; 16]), &PlannerContext::default()).unwrap(),
        ),
        &fix.ctx,
    )
    .await
    .unwrap();
    let r3 = execute_encode(
        unwrap_encode(
            plan_encode(&base_request("c", [5; 16]), &PlannerContext::default()).unwrap(),
        ),
        &fix.ctx,
    )
    .await
    .unwrap();
    assert_ne!(r1.memory_id, r2.memory_id);
    assert_ne!(r2.memory_id, r3.memory_id);
    assert_ne!(r1.memory_id, r3.memory_id);
    assert_eq!(r1.memory_id.slot(), 1);
    assert_eq!(r2.memory_id.slot(), 2);
    assert_eq!(r3.memory_id.slot(), 3);
}

#[tokio::test]
async fn encode_with_edges_records_outcomes() {
    use brain_protocol::request::{EdgeKindWire, EdgeRequest};
    let fix = build_fixture();

    // First insert a memory we can target.
    let first = execute_encode(
        unwrap_encode(
            plan_encode(&base_request("target", [9; 16]), &PlannerContext::default()).unwrap(),
        ),
        &fix.ctx,
    )
    .await
    .unwrap();

    // Now an encode with two edges: one to the existing memory,
    // one to a non-existent id.
    let mut req = base_request("hello", [10; 16]);
    req.edges = vec![
        EdgeRequest {
            target: first.memory_id.raw(),
            kind: EdgeKindWire::References,
            weight: 0.5,
        },
        EdgeRequest {
            target: 0xDEADBEEFu128,
            kind: EdgeKindWire::References,
            weight: 0.5,
        },
    ];

    let result = execute_encode(
        unwrap_encode(plan_encode(&req, &PlannerContext::default()).unwrap()),
        &fix.ctx,
    )
    .await
    .unwrap();
    assert_eq!(result.edge_results.len(), 2);
    assert_eq!(result.edge_results[0], EdgeOutcome::Inserted);
    assert_eq!(result.edge_results[1], EdgeOutcome::TargetMissing);
}

#[tokio::test]
async fn encode_then_recall_finds_it() {
    let fix = build_fixture();
    let mut cue_vec = [0.0f32; VECTOR_DIM];
    cue_vec[42] = 1.0;
    fix.mock.install("the cat sat", cue_vec);

    let plan = plan_encode(
        &base_request("the cat sat", [11; 16]),
        &PlannerContext::default(),
    )
    .unwrap();
    let encode_result = execute_encode(unwrap_encode(plan), &fix.ctx).await.unwrap();

    // Now recall with the same cue.
    let req = RecallRequest {
        cue_text: "the cat sat".into(),
        cue_vector_offset: 0,
        cue_vector_dim: 0,
        top_k: 5,
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
    };
    let recall_plan = plan_recall(&req, &PlannerContext::default()).unwrap();
    let recall_result = execute_recall(unwrap_recall(recall_plan), &fix.ctx)
        .await
        .unwrap();
    assert!(!recall_result.hits.is_empty());
    assert_eq!(recall_result.hits[0].memory_id, encode_result.memory_id);
    assert_eq!(recall_result.hits[0].kind, MemoryKind::Episodic);
    assert_eq!(recall_result.hits[0].context_id, ContextId(42));
}
