//! PLAN handler.
//!
//! Wires the planner + the bi-BFS path executor through the
//! dispatcher and projects a `PathStream` (the executor's per-path
//! frames plus a terminal summary) into a sequence of wire
//! `PlanResponseFrame`s.
//!
//! One mid-stream frame per scored path (top-N after sort + truncate)
//! followed by a single terminal frame carrying the `plan_status`. The
//! terminal frame has `is_final = true`; mid-stream frames have
//! `is_final = false` and `plan_status = None`. An empty path stream
//! (no path found, base set empty, etc.) is surfaced as a single
//! terminal frame — clients still see exactly one final frame
//! regardless of how much the executor produced.

use std::collections::{HashMap, HashSet};

use brain_core::{EdgeKind, MemoryId};
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_planner::{
    execute_path_stream, plan_path_inner, Path, PathFrame, PlanExecutionMetadata, PlanStatus,
    PlanTraceDirection as InternalPlanTraceDirection,
    PlanTraceMeetingPoint as InternalPlanTraceMeetingPoint, PlanTraceNode as InternalPlanTraceNode,
};
use brain_protocol::envelope::request::PlanRequest;
use brain_protocol::envelope::response::{
    PlanResponseFrame, PlanStatus as WirePlanStatus, PlanStep, PlanTrace,
    PlanTraceDirection as WirePlanTraceDirection, PlanTraceMeetingPoint, PlanTraceNode,
    TransitionKind,
};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::state::txn_lens::build_executor_with_lens;

pub async fn handle_plan(
    req: PlanRequest,
    ctx: &OpsContext,
) -> Result<Vec<PlanResponseFrame>, OpError> {
    let trace_requested = req.trace;
    let plan = plan_path_inner(&req, &ctx.planner_ctx)?;
    let exec_ctx = build_executor_with_lens(ctx, req.txn_id)?;
    let stream = execute_path_stream(plan, &exec_ctx, trace_requested).await?;

    let mut frames: Vec<PlanResponseFrame> = Vec::with_capacity(stream.paths.len() + 1);
    for path_frame in stream.paths {
        frames.push(path_frame_to_wire(path_frame));
    }
    // Terminal frame — always emitted, regardless of how many paths
    // the stream produced. Carries the aggregate status and marks
    // end-of-stream, plus the full per-stage trace when the caller
    // requested `trace = true`.
    let trace = match stream.terminal.trace {
        Some(t) => Some(build_plan_trace(t, ctx)?),
        None => None,
    };
    frames.push(PlanResponseFrame {
        steps: Vec::new(),
        is_final: true,
        plan_status: Some(to_wire_status(stream.terminal.status)),
        trace,
    });
    Ok(frames)
}

fn path_frame_to_wire(frame: PathFrame) -> PlanResponseFrame {
    PlanResponseFrame {
        steps: path_to_steps(&frame.path),
        is_final: false,
        plan_status: None,
        trace: None,
    }
}

fn to_wire_status(s: PlanStatus) -> WirePlanStatus {
    match s {
        PlanStatus::GoalReached => WirePlanStatus::GoalReached,
        PlanStatus::BudgetExhausted => WirePlanStatus::BudgetExhausted,
        PlanStatus::NoPathFound => WirePlanStatus::NoPathFound,
        // The wire enum has no `Timeout` variant — surface a wall-time
        // stop as BudgetExhausted. A future wire revision can add
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

// ---------------------------------------------------------------------------
// Trace bridging: internal `brain_planner::PlanExecutionMetadata` → wire
// `PlanTrace`.
// ---------------------------------------------------------------------------

/// Structure the executor's internal BFS trace into the wire `PlanTrace` a
/// `trace = true` caller receives. The internal types carry no text at all
/// (id/score-only captures) — every explored node and meeting point needs a
/// batched text fetch here, mirroring `handlers/recall.rs`'s
/// `fetch_candidate_texts` pattern.
fn build_plan_trace(meta: PlanExecutionMetadata, ctx: &OpsContext) -> Result<PlanTrace, OpError> {
    let PlanExecutionMetadata {
        explored,
        meeting_points,
    } = meta;

    let mut ids: HashSet<MemoryId> = HashSet::with_capacity(explored.len() + meeting_points.len());
    for n in &explored {
        ids.insert(n.memory_id);
    }
    for m in &meeting_points {
        ids.insert(m.memory_id);
    }
    let texts = fetch_trace_texts(&ids, ctx)?;
    let text_of = |id: MemoryId| texts.get(&id).cloned().unwrap_or_default();

    let explored_wire = explored
        .into_iter()
        .map(|n| node_to_wire(n, &text_of))
        .collect();
    let meeting_points_wire = meeting_points
        .into_iter()
        .map(|m| meeting_point_to_wire(m, &text_of))
        .collect();

    Ok(PlanTrace {
        explored: explored_wire,
        meeting_points: meeting_points_wire,
    })
}

fn node_to_wire(n: InternalPlanTraceNode, text_of: &impl Fn(MemoryId) -> String) -> PlanTraceNode {
    PlanTraceNode {
        memory_id: n.memory_id.raw(),
        text: text_of(n.memory_id),
        direction: direction_to_wire(n.direction),
        depth: u32::try_from(n.depth).unwrap_or(u32::MAX),
        // The wire field is named `parent_edge` but carries the parent
        // NODE's id (not the edge kind) — see `PlanTraceNode::parent_id`
        // on the internal side for the field this maps from.
        parent_edge: n.parent_id.map(MemoryId::raw),
        alignment_score: n.alignment_score,
    }
}

fn direction_to_wire(d: InternalPlanTraceDirection) -> WirePlanTraceDirection {
    match d {
        InternalPlanTraceDirection::Forward => WirePlanTraceDirection::Forward,
        InternalPlanTraceDirection::Backward => WirePlanTraceDirection::Backward,
    }
}

fn meeting_point_to_wire(
    m: InternalPlanTraceMeetingPoint,
    text_of: &impl Fn(MemoryId) -> String,
) -> PlanTraceMeetingPoint {
    PlanTraceMeetingPoint {
        memory_id: m.memory_id.raw(),
        text: text_of(m.memory_id),
        included_in_result: m.included_in_result,
    }
}

/// Fetch stored text for a set of memory ids in one batched redb read,
/// mirroring `handlers/recall.rs`'s `fetch_candidate_texts`. A
/// missing/tombstoned-since-trace row maps to an empty string rather than a
/// fatal error: an observability payload losing one candidate's text is not
/// the same failure class as a missing final answer row.
fn fetch_trace_texts(
    ids: &HashSet<MemoryId>,
    ctx: &OpsContext,
) -> Result<HashMap<MemoryId, String>, OpError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("plan trace read_txn: {e}")))?;
    let texts_table = rtxn
        .open_table(TEXTS_TABLE)
        .map_err(|e| OpError::Internal(format!("plan trace open TEXTS_TABLE: {e}")))?;

    let mut out = HashMap::with_capacity(ids.len());
    for &id in ids {
        let text = match texts_table.get(&id.to_be_bytes()) {
            Ok(Some(guard)) => std::str::from_utf8(guard.value())
                .map(str::to_owned)
                .map_err(|e| {
                    OpError::Internal(format!("plan trace TEXTS_TABLE non-UTF-8 for {id:?}: {e}"))
                })?,
            Ok(None) => String::new(),
            Err(e) => {
                return Err(OpError::Internal(format!(
                    "plan trace TEXTS_TABLE get: {e}"
                )));
            }
        };
        out.insert(id, text);
    }
    Ok(out)
}
