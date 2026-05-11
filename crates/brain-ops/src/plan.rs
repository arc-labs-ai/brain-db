//! PLAN handler (sub-task 7.5).
//!
//! Wires the planner (6.5) + the new BFS executor (7.5) through the
//! dispatcher and maps `PathResult` into the wire `PlanResponseFrame`.
//! Single-frame for v1: the wire carries one linear path; multi-path
//! streaming is Phase 9 server work.

use brain_core::EdgeKind;
use brain_planner::{execute_path, plan_path_inner, Path, PlanStatus};
use brain_protocol::request::PlanRequest;
use brain_protocol::response::{
    PlanResponseFrame, PlanStatus as WirePlanStatus, PlanStep, TransitionKind,
};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::txn_lens::build_executor_with_lens;

pub async fn handle_plan(req: PlanRequest, ctx: &OpsContext) -> Result<PlanResponseFrame, OpError> {
    let plan = plan_path_inner(&req, &ctx.planner_ctx)?;
    let exec_ctx = build_executor_with_lens(ctx, req.txn_id)?;
    let result = execute_path(plan, &exec_ctx).await?;

    let wire_status = to_wire_status(result.status);

    let steps = match result.paths.first() {
        Some(p) => path_to_steps(p),
        None => Vec::new(),
    };

    Ok(PlanResponseFrame {
        steps,
        is_final: true,
        plan_status: Some(wire_status),
    })
}

fn to_wire_status(s: PlanStatus) -> WirePlanStatus {
    match s {
        PlanStatus::GoalReached => WirePlanStatus::GoalReached,
        PlanStatus::BudgetExhausted => WirePlanStatus::BudgetExhausted,
        PlanStatus::NoPathFound => WirePlanStatus::NoPathFound,
        // The wire enum has no `Timeout` variant; spec §09/04 §17
        // calls a wall-time stop a partial result. Surface it as
        // BudgetExhausted for now; a future wire revision can add
        // the variant.
        PlanStatus::Timeout => WirePlanStatus::BudgetExhausted,
    }
}

fn path_to_steps(path: &Path) -> Vec<PlanStep> {
    let n = path.nodes.len();
    path.nodes
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let transition_kind = if i == 0 {
                TransitionKind::Initial
            } else {
                edge_to_transition(path.edges[i - 1])
            };
            #[allow(clippy::cast_precision_loss)]
            let estimated_distance_to_goal = (n - 1 - i) as f32;
            PlanStep {
                step_index: u32::try_from(i).unwrap_or(u32::MAX),
                memory_id: (*id).into(),
                text: path.node_text.get(i).cloned().unwrap_or_default(),
                transition_kind,
                confidence: path.score,
                estimated_distance_to_goal,
            }
        })
        .collect()
}

fn edge_to_transition(kind: EdgeKind) -> TransitionKind {
    match kind {
        EdgeKind::Caused => TransitionKind::Causal,
        EdgeKind::FollowedBy => TransitionKind::Temporal,
        EdgeKind::SimilarTo => TransitionKind::Similarity,
        other => TransitionKind::Other(format!("{other:?}")),
    }
}
