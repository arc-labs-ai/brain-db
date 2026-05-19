//! Integration tests for `handle_plan` (sub-task 7.5; refactored in
//! 7.8 to insert edges through the wire LINK path).
//!
//! Drives the full pipeline:
//!   dispatcher → handle_plan → plan_path_inner → execute_path
//!   → wire PlanResponseFrame
//!
//! Memory rows are inserted directly via `MemoryMetadata::new_active`
//! so we can pin specific MemoryIds for the test scenarios; edges are
//! then created through `dispatch(RequestBody::Link(...))` so we
//! exercise the real LINK code path end-to-end.

use std::sync::Arc;

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::test_support::run_in_glommio;
use brain_ops::{dispatch, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::request::{
    EdgeKindWire, LinkRequest, PlanBudget, PlanRequest, PlanState, RequestBody,
};
use brain_protocol::response::{
    PlanResponseFrame, PlanStatus as WirePlanStatus, ResponseBody, TransitionKind,
};
use parking_lot::Mutex;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Mock dispatcher.
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

// ---------------------------------------------------------------------------
// Fixture: pre-populated memories + edges. Wires a RealWriterHandle so
// the OpsContext satisfies the executor's requirements even though
// PLAN doesn't write.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: OpsContext,
    ids: Vec<MemoryId>,
    _tempdir: tempfile::TempDir,
}

fn make_id(i: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&i.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

async fn build_fixture(n_memories: usize, edges: &[(usize, EdgeKind, usize)]) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let mut metadata = MetadataDb::open(&db_path).unwrap();

    let agent = AgentId(Uuid::nil());
    let mut ids = Vec::with_capacity(n_memories);

    // Insert memory rows directly so we can pin specific MemoryIds.
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
    wtxn.commit().unwrap();

    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(metadata));
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    let ctx = OpsContext::new(executor);

    // Create edges via the wire LINK path so we exercise the real
    // code (idempotency + count maintenance + redb writes).
    for (i, (src, kind, tgt)) in edges.iter().enumerate() {
        let mut request_id = [0u8; 16];
        request_id[..2].copy_from_slice(&(i as u16).to_be_bytes());
        request_id[2] = 0xEE;
        let req = LinkRequest {
            source: ids[*src].raw(),
            target: ids[*tgt].raw(),
            kind: EdgeKindWire::from(*kind),
            weight: 1.0,
            request_id,
            txn_id: None,
        };
        let _ = dispatch(
            RequestBody::Link(req),
            brain_ops::RequestCaller::anonymous(),
            &ctx,
        )
        .await
        .unwrap();
    }

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

fn unwrap_plan_resp(body: ResponseBody) -> PlanResponseFrame {
    match body {
        ResponseBody::Plan(p) => p,
        other => panic!("expected ResponseBody::Plan, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Full pipeline: 3 memories, A→B→C, PLAN returns 3 steps.
// ---------------------------------------------------------------------------

#[test]
fn plan_full_pipeline_returns_path() {
    run_in_glommio(|| async {
        let fix = build_fixture(3, &[(0, EdgeKind::Caused, 1), (1, EdgeKind::FollowedBy, 2)]).await;
        let req = plan_request(fix.ids[0], fix.ids[2], 4);
        let frame = unwrap_plan_resp(
            dispatch(
                RequestBody::Plan(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        assert!(frame.is_final);
        assert_eq!(frame.plan_status, Some(WirePlanStatus::GoalReached));
        assert_eq!(frame.steps.len(), 3);
        assert_eq!(frame.steps[0].step_index, 0);
        assert_eq!(frame.steps[0].transition_kind, TransitionKind::Initial);
        assert_eq!(frame.steps[1].transition_kind, TransitionKind::Causal);
        assert_eq!(frame.steps[2].transition_kind, TransitionKind::Temporal);
        assert_eq!(frame.steps[0].estimated_distance_to_goal, 2.0);
        assert_eq!(frame.steps[2].estimated_distance_to_goal, 0.0);
    })
}

// ---------------------------------------------------------------------------
// 2. No path → empty steps + NoPathFound status.
// ---------------------------------------------------------------------------

#[test]
fn plan_no_path_returns_no_path_status() {
    run_in_glommio(|| async {
        let fix = build_fixture(3, &[(0, EdgeKind::Caused, 1)]).await;
        let req = plan_request(fix.ids[0], fix.ids[2], 4);
        let frame = unwrap_plan_resp(
            dispatch(
                RequestBody::Plan(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(frame.steps.is_empty());
        assert_eq!(frame.plan_status, Some(WirePlanStatus::NoPathFound));
    })
}

// ---------------------------------------------------------------------------
// 3. Transition mapping: ensure CAUSED → Causal and FollowedBy → Temporal.
// ---------------------------------------------------------------------------

#[test]
fn plan_step_transitions_map_correctly() {
    run_in_glommio(|| async {
        let fix = build_fixture(2, &[(0, EdgeKind::Caused, 1)]).await;
        let req = plan_request(fix.ids[0], fix.ids[1], 2);
        let frame = unwrap_plan_resp(
            dispatch(
                RequestBody::Plan(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(frame.steps[0].transition_kind, TransitionKind::Initial);
        assert_eq!(frame.steps[1].transition_kind, TransitionKind::Causal);
    })
}

// ---------------------------------------------------------------------------
// 4. Validation: max_steps=0 → planner rejects.
// ---------------------------------------------------------------------------

#[test]
fn plan_invalid_budget_returns_plan_error() {
    run_in_glommio(|| async {
        let fix = build_fixture(2, &[(0, EdgeKind::Caused, 1)]).await;
        let mut req = plan_request(fix.ids[0], fix.ids[1], 0);
        req.budget.max_steps = 0;
        let err = dispatch(
            RequestBody::Plan(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, OpError::PlanError(_)),
            "max_steps=0 must be a planner validation failure, got {err:?}"
        );
        assert_eq!(err.error_code(), ErrorCode::InvalidRequest);
    })
}

// ---------------------------------------------------------------------------
// 5. ByMemoryId endpoints work (no embed required).
// ---------------------------------------------------------------------------

#[test]
fn plan_by_memory_id_skips_recall() {
    run_in_glommio(|| async {
        let fix = build_fixture(2, &[(0, EdgeKind::Caused, 1)]).await;
        // Plan with ByMemoryId on both endpoints; even though the
        // dispatcher is a no-op, the executor should not need to embed.
        let req = plan_request(fix.ids[0], fix.ids[1], 2);
        let frame = unwrap_plan_resp(
            dispatch(
                RequestBody::Plan(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(frame.plan_status, Some(WirePlanStatus::GoalReached));
        assert_eq!(frame.steps.len(), 2);
        let id0: u128 = fix.ids[0].into();
        let id1: u128 = fix.ids[1].into();
        assert_eq!(frame.steps[0].memory_id, id0);
        assert_eq!(frame.steps[1].memory_id, id1);
    })
}
