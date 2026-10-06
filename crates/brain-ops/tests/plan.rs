//! Integration tests for `handle_plan`. Edges are inserted through the
//! wire LINK path.
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

use brain_core::{EdgeKind, MemoryId, MemoryKind, SessionId, SpaceId};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_metadata::MetadataDb;
use brain_ops::test_support::run_in_glommio;
use brain_ops::{dispatch, DispatchOutcome, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{
    EdgeKindWire, LinkRequest, PlanBudget, PlanRequest, PlanState, RequestBody,
};
use brain_protocol::envelope::response::{
    PlanResponseFrame, PlanStatus as WirePlanStatus, PlanTraceDirection, ResponseBody,
    TransitionKind,
};
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
    let metadata = MetadataDb::open(&db_path).unwrap();

    let space = SpaceId(Uuid::nil());
    let mut ids = Vec::with_capacity(n_memories);

    // Insert memory rows directly so we can pin specific MemoryIds.
    let wtxn = metadata.write_txn().unwrap();
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        let mut texts = wtxn.open_table(TEXTS_TABLE).unwrap();
        for i in 0..n_memories {
            let id = make_id((i as u64) + 1);
            ids.push(id);
            let meta = MemoryMetadata::new_active(
                id,
                brain_core::NamespaceId::SYSTEM,
                space,
                SessionId(42),
                (i + 1) as u64,
                1,
                MemoryKind::Episodic,
                [0x11; 16],
                0.5,
                16,
                1_000_000 + i as u64,
            );
            table.insert(id.to_be_bytes(), meta).unwrap();
            let text = format!("plan fixture memory {i}");
            texts.insert(id.to_be_bytes(), text.as_bytes()).unwrap();
        }
    }
    wtxn.commit().unwrap();

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let metadata: SharedMetadataDb = Arc::new(metadata);
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    let ctx = brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor);

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
            act_as: None,
        };
        let _ = dispatch(
            RequestBody::Link(req),
            brain_ops::RequestCaller::for_tests(),
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
    plan_request_traced(start, goal, max_depth, false)
}

fn plan_request_traced(
    start: MemoryId,
    goal: MemoryId,
    max_depth: u32,
    trace: bool,
) -> PlanRequest {
    PlanRequest {
        start: PlanState::ByMemoryId(start.into()),
        goal: PlanState::ByMemoryId(goal.into()),
        budget: PlanBudget {
            max_steps: max_depth,
            max_wall_time_ms: 1000,
            max_branches_explored: 256,
        },
        strategy_hint: None,
        session_filter: None,
        request_id: None,
        txn_id: None,
        trace,
        act_as: None,
    }
}

/// Collapse the streamed PLAN frames into a single observation:
/// concatenated steps from every mid-stream frame and the terminal
/// frame's `plan_status` + `is_final`. Mirrors the v1 single-frame
/// shape so the existing assertions read unchanged.
fn collect_plan_outcome(outcome: DispatchOutcome) -> PlanResponseFrame {
    let frames = unwrap_plan_stream(outcome);
    let mut steps = Vec::new();
    let mut terminal = None;
    for f in frames {
        if f.is_final {
            terminal = Some(f);
        } else {
            steps.extend(f.steps);
        }
    }
    let terminal = terminal.expect("PLAN stream must end with a terminal frame");
    PlanResponseFrame {
        steps,
        is_final: terminal.is_final,
        plan_status: terminal.plan_status,
        trace: terminal.trace,
    }
}

fn unwrap_plan_stream(outcome: DispatchOutcome) -> Vec<PlanResponseFrame> {
    match outcome {
        DispatchOutcome::Stream(bodies) => bodies
            .into_iter()
            .map(|b| match b {
                ResponseBody::Plan(p) => p,
                other => panic!("expected ResponseBody::Plan in stream, got {other:?}"),
            })
            .collect(),
        DispatchOutcome::Single(other) => {
            panic!("expected DispatchOutcome::Stream of Plan frames, got Single({other:?})")
        }
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
        let frame = collect_plan_outcome(
            dispatch(
                RequestBody::Plan(req),
                brain_ops::RequestCaller::for_tests(),
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
        let frame = collect_plan_outcome(
            dispatch(
                RequestBody::Plan(req),
                brain_ops::RequestCaller::for_tests(),
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
        let frame = collect_plan_outcome(
            dispatch(
                RequestBody::Plan(req),
                brain_ops::RequestCaller::for_tests(),
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
            brain_ops::RequestCaller::for_tests(),
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
        let frame = collect_plan_outcome(
            dispatch(
                RequestBody::Plan(req),
                brain_ops::RequestCaller::for_tests(),
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

// ---------------------------------------------------------------------------
// 6. trace = false (default): terminal frame carries no trace payload.
// ---------------------------------------------------------------------------

#[test]
fn plan_trace_false_omits_trace_payload() {
    run_in_glommio(|| async {
        let fix = build_fixture(3, &[(0, EdgeKind::Caused, 1), (1, EdgeKind::FollowedBy, 2)]).await;
        let req = plan_request(fix.ids[0], fix.ids[2], 4);
        let frame = collect_plan_outcome(
            dispatch(
                RequestBody::Plan(req),
                brain_ops::RequestCaller::for_tests(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(frame.trace.is_none());
    })
}

// ---------------------------------------------------------------------------
// 7. trace = true: terminal frame carries the full bidirectional-BFS trace,
//    with real (non-empty) text on every explored node and meeting point.
// ---------------------------------------------------------------------------

#[test]
fn plan_trace_true_populates_explored_and_meeting_points() {
    run_in_glommio(|| async {
        let fix = build_fixture(3, &[(0, EdgeKind::Caused, 1), (1, EdgeKind::FollowedBy, 2)]).await;
        let req = plan_request_traced(fix.ids[0], fix.ids[2], 4, true);
        let frame = collect_plan_outcome(
            dispatch(
                RequestBody::Plan(req),
                brain_ops::RequestCaller::for_tests(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(frame.plan_status, Some(WirePlanStatus::GoalReached));

        let trace = frame
            .trace
            .expect("trace = true must populate the trace payload");
        assert!(!trace.explored.is_empty());
        assert!(!trace.meeting_points.is_empty());

        let id0: u128 = fix.ids[0].raw();
        let id2: u128 = fix.ids[2].raw();

        // The forward seed (start) is explored at depth 0 with no parent.
        let seed = trace
            .explored
            .iter()
            .find(|n| n.memory_id == id0 && n.direction == PlanTraceDirection::Forward)
            .expect("forward seed must be in the explored set");
        assert_eq!(seed.depth, 0);
        assert!(seed.parent_edge.is_none());
        assert!(!seed.text.is_empty());

        // Every explored node carries real text.
        for n in &trace.explored {
            assert!(!n.text.is_empty(), "explored node missing text");
        }

        // The goal node is a meeting point that made it into the result.
        let meet = trace
            .meeting_points
            .iter()
            .find(|m| m.memory_id == id2)
            .expect("goal node must be a meeting point");
        assert!(meet.included_in_result);
        assert!(!meet.text.is_empty());
    })
}

// ---------------------------------------------------------------------------
// 8. Regression: ByText endpoints whose ANN neighbourhoods overlap.
//
// Live repro: a space holding just two memories, PLAN start "Priya is based
// in Bangalore" → goal "Priya leads the EU office in Lisbon". Both memories
// sit in the top-K of BOTH cues, so both used to seed both BFS sides; the
// executor then reported each as a trivial start == goal "path" — two frames
// of one step each (step_index 0, Initial, distance 0), goal memory first.
// With the endpoints split by anchor the plan is the single real path
// start → goal, numbered from the start.
// ---------------------------------------------------------------------------

/// Keyword-driven embedder: each city gets its own axis plus a small shared
/// component, so the two memories are mildly similar (both land in each
/// cue's top-K) but each cue's nearest memory is its own.
struct CityDispatcher;

impl Dispatcher for CityDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0f32; VECTOR_DIM];
        if text.contains("Bangalore") {
            v[0] = 1.0;
        } else if text.contains("Lisbon") {
            v[1] = 1.0;
        } else {
            v[3] = 1.0;
        }
        v[2] = 0.3;
        Ok(v)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0xC1; 16]
    }
}

#[test]
fn plan_by_text_overlapping_endpoints_returns_start_to_goal_path() {
    use brain_protocol::envelope::response::EncodeResponse;
    use brain_protocol::EncodeRequest;

    run_in_glommio(|| async {
        let tempdir = tempfile::tempdir().unwrap();
        let metadata: SharedMetadataDb =
            Arc::new(MetadataDb::open(tempdir.path().join("metadata.redb")).unwrap());
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let executor = ExecutorContext::new(
            Arc::new(CityDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata,
            writer as Arc<dyn WriterHandle>,
        );
        let ctx = brain_ops::test_support::ops_context_for_tests(executor, tempdir.path());

        let mut ids = Vec::new();
        for (i, text) in [
            "Priya is based in Bangalore",
            "Priya leads the EU office in Lisbon",
        ]
        .iter()
        .enumerate()
        {
            let out = dispatch(
                RequestBody::Encode(EncodeRequest {
                    text: (*text).into(),
                    session_id: 1,
                    request_id: [0x40 + i as u8; 16],
                    txn_id: None,
                    occurred_at_unix_nanos: None,
                    act_as: None,
                    wait: brain_protocol::WaitMode::Ack,
                    allow_duplicates: true,
                }),
                brain_ops::RequestCaller::for_tests(),
                &ctx,
            )
            .await
            .unwrap();
            match out {
                DispatchOutcome::Single(ResponseBody::Encode(EncodeResponse {
                    memory_id, ..
                })) => ids.push(memory_id),
                other => panic!("expected Encode, got {other:?}"),
            }
        }
        let (bangalore, lisbon) = (ids[0], ids[1]);
        assert_ne!(bangalore, lisbon);

        // The causal hop the plan should find: Bangalore → Lisbon.
        dispatch(
            RequestBody::Link(LinkRequest {
                source: bangalore,
                target: lisbon,
                kind: EdgeKindWire::from(EdgeKind::Caused),
                weight: 1.0,
                request_id: [0x5E; 16],
                txn_id: None,
                act_as: None,
            }),
            brain_ops::RequestCaller::for_tests(),
            &ctx,
        )
        .await
        .unwrap();

        let req = PlanRequest {
            start: PlanState::ByText("Priya is based in Bangalore".into()),
            goal: PlanState::ByText("Priya leads the EU office in Lisbon".into()),
            budget: PlanBudget {
                max_steps: 4,
                max_wall_time_ms: 1000,
                max_branches_explored: 256,
            },
            strategy_hint: None,
            session_filter: None,
            request_id: None,
            txn_id: None,
            trace: false,
            act_as: None,
        };
        let frames = unwrap_plan_stream(
            dispatch(
                RequestBody::Plan(req),
                brain_ops::RequestCaller::for_tests(),
                &ctx,
            )
            .await
            .unwrap(),
        );
        let paths: Vec<_> = frames.iter().filter(|f| !f.is_final).collect();
        let terminal = frames.last().expect("terminal frame");
        assert!(terminal.is_final);
        assert_eq!(terminal.plan_status, Some(WirePlanStatus::GoalReached));

        // No degenerate one-node "paths": the endpoints resolved to
        // different memories, so every path must span start → goal.
        for p in &paths {
            assert!(
                p.steps.len() >= 2,
                "one-node path means start/goal collapsed: {:?}",
                p.steps
            );
        }
        let best = &paths.first().expect("at least one path").steps;
        assert_eq!(best.len(), 2);
        assert_eq!(best[0].memory_id, bangalore, "path starts at the start");
        assert_eq!(best[1].memory_id, lisbon, "path ends at the goal");
        assert_eq!(best[0].step_index, 0);
        assert_eq!(best[1].step_index, 1);
        assert_eq!(best[0].transition_kind, TransitionKind::Initial);
        assert_ne!(best[1].transition_kind, TransitionKind::Initial);
        assert_eq!(best[0].estimated_distance_to_goal, 1.0);
        assert_eq!(best[1].estimated_distance_to_goal, 0.0);
    })
}
