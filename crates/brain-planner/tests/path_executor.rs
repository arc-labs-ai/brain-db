//! Tests for `execute_path` — the bidirectional-BFS executor for the
//! PLAN cognitive operation (sub-task 7.5).
//!
//! Each test:
//! - builds a `MetadataDb` + `SharedHnsw` fixture with a few memories,
//! - writes edges directly via `brain-metadata::tables::edge::link`
//!   (the LINK handler ships in 7.8; tests sidestep it for now),
//! - runs `plan_path_inner` + `execute_path`,
//! - asserts on the resulting paths + status.

use std::sync::Arc;

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{link, EdgeData, EDGES_IN_TABLE, EDGES_OUT_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_planner::{
    execute_path, plan_path_inner, EncodeAck, EncodeOp, ExecutorContext, ForgetAck, ForgetOp,
    PlanStatus, PlannerContext, SharedMetadataDb, WriterError, WriterHandle,
};
use brain_protocol::request::{PlanBudget, PlanRequest, PlanState};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Mock dispatcher (constant zero vector — endpoint resolution by text
// is not exercised here; ByMemoryId is used everywhere).
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
    // Edges in the same txn so we can use `link`.
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

fn plan_request(start: MemoryId, goal: MemoryId, max_depth: u32) -> PlanRequest {
    PlanRequest {
        start: PlanState::ByMemoryId(start.into()),
        goal: PlanState::ByMemoryId(goal.into()),
        budget: PlanBudget {
            max_steps: max_depth,
            max_wall_time_ms: 1000,
            max_branches_explored: 256,
        },
        strategy_hint: None,
        context_filter: None,
        request_id: None,
        txn_id: None,
    }
}

async fn run(fix: &Fixture, req: PlanRequest) -> brain_planner::PathResult {
    let plan = plan_path_inner(&req, &PlannerContext::default()).unwrap();
    execute_path(plan, &fix.ctx).await.unwrap()
}

// ---------------------------------------------------------------------------
// 1. Direct edge.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bfs_finds_direct_edge() {
    let fix = build_fixture(2, &[(0, EdgeKind::Caused, 1)]);
    let result = run(&fix, plan_request(fix.ids[0], fix.ids[1], 2)).await;
    assert_eq!(result.status, PlanStatus::GoalReached);
    assert!(!result.paths.is_empty());
    let p = &result.paths[0];
    assert_eq!(p.nodes, vec![fix.ids[0], fix.ids[1]]);
    assert_eq!(p.edges, vec![EdgeKind::Caused]);
}

// ---------------------------------------------------------------------------
// 2. Two-hop path.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bfs_finds_two_hop_path() {
    let fix = build_fixture(3, &[(0, EdgeKind::Caused, 1), (1, EdgeKind::FollowedBy, 2)]);
    let result = run(&fix, plan_request(fix.ids[0], fix.ids[2], 3)).await;
    assert_eq!(result.status, PlanStatus::GoalReached);
    let p = &result.paths[0];
    assert_eq!(p.nodes, vec![fix.ids[0], fix.ids[1], fix.ids[2]]);
    assert_eq!(p.edges, vec![EdgeKind::Caused, EdgeKind::FollowedBy]);
}

// ---------------------------------------------------------------------------
// 3. No path: disconnected components.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bfs_no_path_returns_empty_status() {
    // Three memories; only (0 → 1). Goal is 2 with no incoming edge.
    let fix = build_fixture(3, &[(0, EdgeKind::Caused, 1)]);
    let result = run(&fix, plan_request(fix.ids[0], fix.ids[2], 4)).await;
    assert_eq!(result.status, PlanStatus::NoPathFound);
    assert!(result.paths.is_empty());
}

// ---------------------------------------------------------------------------
// 4. Edge-kind filter (planner defaults to [Caused, FollowedBy]).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bfs_respects_edge_kind_filter() {
    // The only edge is REFERENCES — not in the default kind set, so
    // BFS must reject it.
    let fix = build_fixture(2, &[(0, EdgeKind::References, 1)]);
    let result = run(&fix, plan_request(fix.ids[0], fix.ids[1], 3)).await;
    assert_eq!(result.status, PlanStatus::NoPathFound);
    assert!(result.paths.is_empty());
}

// ---------------------------------------------------------------------------
// 5. Self-loop guard: start == goal — zero-length path is the trivial
//    intersection, status GoalReached.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bfs_self_loop_guard_trivial_zero_length() {
    let fix = build_fixture(1, &[]);
    let result = run(&fix, plan_request(fix.ids[0], fix.ids[0], 2)).await;
    assert_eq!(result.status, PlanStatus::GoalReached);
    assert_eq!(result.paths.len(), 1);
    assert_eq!(result.paths[0].nodes, vec![fix.ids[0]]);
    assert!(result.paths[0].edges.is_empty());
}

// ---------------------------------------------------------------------------
// 6. Sub-task 9.16 — spec §16/01 §12 tombstone filter.
// ---------------------------------------------------------------------------
//
// The build_fixture above creates metadata + edges but doesn't
// populate the HNSW; `ctx.index.is_tombstoned(...)` returns false
// for unknown ids. The tests below add a tiny HNSW writer step
// (insert + mark_tombstoned) to exercise the real filter path.

fn populate_hnsw_and_tombstone(fix: &Fixture, insert: &[MemoryId], tombstone: &[MemoryId]) {
    // SharedHnsw construction in build_fixture drops the Writer.
    // We can rebuild a fresh (reader, writer) pair, then swap the
    // shared reader's inner index in place via the `swap` method —
    // but that's heavyweight. Simpler: build_fixture's internal
    // `SharedHnsw::new` returned a writer that was discarded. For
    // these tombstone tests we use a separate fixture builder
    // (`build_fixture_with_writer`) so the writer survives.
    //
    // Falling back to the swap-trick avoids changing build_fixture's
    // signature.
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

/// `FORGET m; PLAN through a chain that includes m` excludes `m`
/// (spec §16/01 §12 acceptance test).
#[tokio::test]
async fn plan_excludes_tombstoned_chain_member() {
    // a -> b -> c plus a detour a -> d -> c.
    let fix = build_fixture(
        4,
        &[
            (0, EdgeKind::FollowedBy, 1), // a -> b
            (1, EdgeKind::FollowedBy, 2), // b -> c
            (0, EdgeKind::FollowedBy, 3), // a -> d
            (3, EdgeKind::FollowedBy, 2), // d -> c
        ],
    );
    populate_hnsw_and_tombstone(
        &fix,
        &fix.ids,      // a, b, c, d all in HNSW
        &[fix.ids[1]], // tombstone b
    );

    let result = run(&fix, plan_request(fix.ids[0], fix.ids[2], 3)).await;
    assert_eq!(result.status, PlanStatus::GoalReached);
    assert!(!result.paths.is_empty());
    // Every returned path must avoid the tombstoned `b`.
    for p in &result.paths {
        assert!(
            !p.nodes.contains(&fix.ids[1]),
            "path traversed tombstoned node b: {:?}",
            p.nodes
        );
    }
}

/// A tombstoned start memory yields an empty endpoint set; the BFS
/// short-circuits to `NoPathFound`.
#[tokio::test]
async fn plan_silently_skips_tombstoned_start() {
    let fix = build_fixture(2, &[(0, EdgeKind::Caused, 1)]);
    populate_hnsw_and_tombstone(&fix, &fix.ids, &[fix.ids[0]]);

    let result = run(&fix, plan_request(fix.ids[0], fix.ids[1], 2)).await;
    assert_eq!(result.status, PlanStatus::NoPathFound);
    assert!(result.paths.is_empty());
}

/// A tombstoned goal memory yields an empty endpoint set; the BFS
/// short-circuits to `NoPathFound`.
#[tokio::test]
async fn plan_silently_skips_tombstoned_goal() {
    let fix = build_fixture(2, &[(0, EdgeKind::Caused, 1)]);
    populate_hnsw_and_tombstone(&fix, &fix.ids, &[fix.ids[1]]);

    let result = run(&fix, plan_request(fix.ids[0], fix.ids[1], 2)).await;
    assert_eq!(result.status, PlanStatus::NoPathFound);
    assert!(result.paths.is_empty());
}
