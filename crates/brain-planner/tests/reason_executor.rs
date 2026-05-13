//! Tests for `execute_reason` — the evidence-traversal executor for
//! the REASON cognitive operation (sub-task 7.6).
//!
//! Each test:
//! - builds a `MetadataDb` + `SharedHnsw` fixture,
//! - inserts edges directly via `brain-metadata::tables::edge::link`
//!   (LINK handler ships in 7.8),
//! - runs `plan_reason_inner` + `execute_reason`,
//! - asserts on supporting / contradicting / confidence / status.

use std::sync::Arc;

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{link, EdgeData, EDGES_IN_TABLE, EDGES_OUT_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_planner::{
    execute_reason, plan_reason_inner, EncodeAck, EncodeOp, ExecutorContext, ForgetAck, ForgetOp,
    PlannerContext, ReasonStatus, SharedMetadataDb, WriterError, WriterHandle,
};
use brain_protocol::request::{ObservationInput, ReasonRequest};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// No-op dispatcher / writer.
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

struct NoopWriter;
impl WriterHandle for NoopWriter {
    fn submit_encode<'a>(
        &'a self,
        _: EncodeOp,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<EncodeAck, WriterError>> + 'a>>
    {
        Box::pin(async move { Err(WriterError::Internal("noop".into())) })
    }
    fn submit_forget<'a>(
        &'a self,
        _: ForgetOp,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ForgetAck, WriterError>> + 'a>>
    {
        Box::pin(async move { Err(WriterError::Internal("noop".into())) })
    }
    fn submit_link<'a>(
        &'a self,
        _: brain_planner::LinkOp,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<brain_planner::LinkAck, WriterError>> + 'a>,
    > {
        Box::pin(async move { Err(WriterError::Internal("noop".into())) })
    }
    fn submit_unlink<'a>(
        &'a self,
        _: brain_planner::UnlinkOp,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<brain_planner::UnlinkAck, WriterError>> + 'a>,
    > {
        Box::pin(async move { Err(WriterError::Internal("noop".into())) })
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
    ids: Vec<MemoryId>,
    _tempdir: tempfile::TempDir,
}

fn make_id(i: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&i.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

fn build_fixture(n_memories: usize, edges: &[(usize, EdgeKind, usize)]) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let mut metadata = MetadataDb::open(&db_path).unwrap();

    let agent = AgentId(Uuid::nil());
    let mut ids = Vec::with_capacity(n_memories);

    let wtxn = metadata.write_txn().unwrap();
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        for i in 0..n_memories {
            let id = make_id((i as u64) + 1);
            ids.push(id);
            let meta = MemoryMetadata::new_active(
                id,
                agent,
                ContextId(42),
                (i + 1) as u64,
                1,
                MemoryKind::Episodic,
                [0x11; 16],
                0.5,
                16,
                1_000_000 + i as u64,
            );
            table.insert(id.to_be_bytes(), meta).unwrap();
        }
    }
    {
        let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let mut inn = wtxn.open_table(EDGES_IN_TABLE).unwrap();
        for (src, kind, tgt) in edges {
            let data = EdgeData::new(1.0, 0, 0, 1_700_000_000_000_000_000);
            link(&mut out, &mut inn, ids[*src], *kind, ids[*tgt], &data).unwrap();
        }
    }
    wtxn.commit().unwrap();

    let (shared, _writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let metadata: SharedMetadataDb = Arc::new(parking_lot::Mutex::new(metadata));
    let ctx = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        Arc::new(NoopWriter) as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx,
        ids,
        _tempdir: tempdir,
    }
}

fn reason_req(base: MemoryId, depth: u32, max_inferences: u32) -> ReasonRequest {
    ReasonRequest {
        observation: ObservationInput::ByMemoryId(base.into()),
        depth,
        confidence_threshold: 0.0,
        context_filter: None,
        max_inferences,
        budget_wall_time_ms: 1000,
        request_id: None,
        txn_id: None,
    }
}

async fn run(fix: &Fixture, req: ReasonRequest) -> brain_planner::ReasonResult {
    let plan = plan_reason_inner(&req, &PlannerContext::default()).unwrap();
    execute_reason(plan, &fix.ctx).await.unwrap()
}

// ---------------------------------------------------------------------------
// 1. Supports one-hop.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_supports_one_hop() {
    let fix = build_fixture(2, &[(0, EdgeKind::Supports, 1)]);
    let r = run(&fix, reason_req(fix.ids[0], 2, 10)).await;
    assert!(
        r.supporting.iter().any(|e| e.memory_id == fix.ids[1]),
        "expected {} in supporting, got {:?}",
        fix.ids[1].raw(),
        r.supporting
    );
    assert!(r.contradicting.is_empty());
    assert!(r.confidence > 0.0);
}

// ---------------------------------------------------------------------------
// 2. Contradicts one-hop.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_contradicts_one_hop() {
    let fix = build_fixture(2, &[(0, EdgeKind::Contradicts, 1)]);
    let r = run(&fix, reason_req(fix.ids[0], 2, 10)).await;
    assert!(
        r.contradicting.iter().any(|e| e.memory_id == fix.ids[1]),
        "expected {} in contradicting",
        fix.ids[1].raw()
    );
    // The base memory itself is a direct-similarity supporter
    // (distance=0); confidence comes out as (1.0 - sum_c) / (1.0 + sum_c)
    // so it may be positive or negative depending on the contradicting
    // score. Just check magnitude < 1.
    assert!(r.confidence.abs() <= 1.0);
}

// ---------------------------------------------------------------------------
// 3. Confidence balance.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_confidence_balance() {
    // base (0) -- Supports --> 1, 2, 3
    //         -- Contradicts --> 4, 5
    let fix = build_fixture(
        6,
        &[
            (0, EdgeKind::Supports, 1),
            (0, EdgeKind::Supports, 2),
            (0, EdgeKind::Supports, 3),
            (0, EdgeKind::Contradicts, 4),
            (0, EdgeKind::Contradicts, 5),
        ],
    );
    let r = run(&fix, reason_req(fix.ids[0], 2, 20)).await;
    assert!(
        r.confidence > 0.0,
        "more supports than contradicts → positive confidence; got {}",
        r.confidence
    );
}

// ---------------------------------------------------------------------------
// 4. Empty base / no outgoing edges.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_isolated_base_returns_only_self_support() {
    // No edges. The base memory itself becomes a direct-similarity
    // supporter at distance 0; nothing else. Confidence == 1.0 because
    // sum_c == 0 → (1.0 - 0) / (1.0 + 0) = 1.0.
    let fix = build_fixture(1, &[]);
    let r = run(&fix, reason_req(fix.ids[0], 2, 10)).await;
    assert_eq!(r.supporting.len(), 1);
    assert_eq!(r.supporting[0].memory_id, fix.ids[0]);
    assert_eq!(r.supporting[0].distance, 0);
    assert!(r.contradicting.is_empty());
    assert_eq!(r.confidence, 1.0);
    assert_eq!(r.status, ReasonStatus::Complete);
}

// ---------------------------------------------------------------------------
// 5. max_inferences caps the supporting list.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_max_inferences_caps_results() {
    // Five supporting edges; ask for only 2 inferences.
    let fix = build_fixture(
        6,
        &[
            (0, EdgeKind::Supports, 1),
            (0, EdgeKind::Supports, 2),
            (0, EdgeKind::Supports, 3),
            (0, EdgeKind::Supports, 4),
            (0, EdgeKind::Supports, 5),
        ],
    );
    // max_inferences=2 caps the BFS to 2 supports + 2 contradicts traversal
    // items each. After aggregation.max_supporting (default 5) caps,
    // we still see at most 2 (from the BFS budget) plus the base item.
    let r = run(&fix, reason_req(fix.ids[0], 2, 2)).await;
    // 2 traversal-found + 1 base (direct-similarity) = 3 supporting at most.
    assert!(
        r.supporting.len() <= 3,
        "supporting should be capped, got {}",
        r.supporting.len()
    );
}

// ---------------------------------------------------------------------------
// Sub-task 9.16 — spec §16/01 §12 tombstone filter.
// ---------------------------------------------------------------------------

fn populate_hnsw_and_tombstone(fix: &Fixture, insert: &[MemoryId], tombstone: &[MemoryId]) {
    // Mirrors the helper in path_executor.rs: build a fresh
    // HnswIndex, insert the ids, mark some tombstoned, swap into
    // the shared reader.
    use brain_index::HnswIndex;
    let params = fix.ctx.index.params();
    let mut hnsw = HnswIndex::<VECTOR_DIM>::new(params).expect("HnswIndex::new");
    let v = [0.1_f32; VECTOR_DIM];
    for id in insert {
        hnsw.insert(*id, &v).expect("hnsw insert");
    }
    for id in tombstone {
        hnsw.mark_tombstoned(*id).expect("hnsw tombstone");
    }
    fix.ctx.index.swap(hnsw);
}

/// Seed has three `Supports` neighbours; tombstone one. REASON's
/// supporting set must exclude the tombstoned neighbour.
#[tokio::test]
async fn reason_excludes_tombstoned_seed_neighbour() {
    let fix = build_fixture(
        4,
        &[
            (0, EdgeKind::Supports, 1), // seed -> n1
            (0, EdgeKind::Supports, 2), // seed -> n2 (tombstoned)
            (0, EdgeKind::Supports, 3), // seed -> n3
        ],
    );
    populate_hnsw_and_tombstone(&fix, &fix.ids, &[fix.ids[2]]);

    let r = run(&fix, reason_req(fix.ids[0], 1, 16)).await;
    // The tombstoned n2 must NOT appear in supporting.
    let supporting_ids: Vec<MemoryId> = r.supporting.iter().map(|inf| inf.memory_id).collect();
    assert!(
        !supporting_ids.contains(&fix.ids[2]),
        "tombstoned memory n2 leaked into supporting: {:?}",
        supporting_ids
    );
    // n1 and n3 should still appear (BFS reached them via the
    // remaining supports edges).
    assert!(
        supporting_ids.contains(&fix.ids[1]) || supporting_ids.contains(&fix.ids[3]),
        "expected at least one of n1/n3 in supporting; got {:?}",
        supporting_ids
    );
}

/// A tombstoned seed yields an empty base; REASON returns empty
/// supporting + contradicting sets.
#[tokio::test]
async fn reason_silently_skips_tombstoned_seed() {
    let fix = build_fixture(2, &[(0, EdgeKind::Supports, 1)]);
    populate_hnsw_and_tombstone(&fix, &fix.ids, &[fix.ids[0]]);

    let r = run(&fix, reason_req(fix.ids[0], 1, 16)).await;
    assert!(
        r.supporting.is_empty() && r.contradicting.is_empty(),
        "tombstoned seed should yield empty sets; got supporting={} contradicting={}",
        r.supporting.len(),
        r.contradicting.len()
    );
}
