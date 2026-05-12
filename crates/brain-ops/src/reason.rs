//! REASON handler (sub-task 7.6).
//!
//! Wires the planner (6.5) + the new evidence-traversal executor (7.6)
//! through the dispatcher and maps `ReasonResult` into one wire
//! `InferenceStep` envelope. Single-frame for v1; multi-step streaming
//! is Phase 9 server work.

use brain_planner::{execute_reason, plan_reason_inner, ReasonResult, ReasonStatus};
use brain_protocol::request::{ObservationInput, ReasonRequest};
use brain_protocol::response::{
    InferenceKind, InferenceStep, ReasonResponseFrame, ReasonStatus as WireReasonStatus,
};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::txn_lens::build_executor_with_lens;

pub async fn handle_reason(
    req: ReasonRequest,
    ctx: &OpsContext,
) -> Result<ReasonResponseFrame, OpError> {
    // Capture the claim text before the planner consumes the request.
    // ByMemoryId observations don't carry text — v1 leaves the claim
    // field empty (documented in the plan).
    let claim = match &req.observation {
        ObservationInput::ByText(t) => t.clone(),
        ObservationInput::ByMemoryId(_) => String::new(),
    };

    let plan = plan_reason_inner(&req, &ctx.planner_ctx)?;
    let exec_ctx = build_executor_with_lens(ctx, req.txn_id)?;
    let result = execute_reason(plan, &exec_ctx).await?;

    Ok(to_wire_frame(result, claim))
}

fn to_wire_frame(result: ReasonResult, claim: String) -> ReasonResponseFrame {
    let supporting_memories: Vec<u128> = result
        .supporting
        .iter()
        .map(|e| e.memory_id.into())
        .collect();
    let contradicting_memories: Vec<u128> = result
        .contradicting
        .iter()
        .map(|e| e.memory_id.into())
        .collect();

    let inference = InferenceStep {
        step_index: 0,
        claim,
        supporting_memories,
        contradicting_memories,
        confidence: result.confidence,
        // v1 packs everything as evidence accumulation. The other wire
        // variants (CausalExplanation, AnalogicalInference) fit
        // future sub-modes that we don't distinguish yet.
        inference_kind: InferenceKind::EvidenceAccumulation,
    };

    ReasonResponseFrame {
        inferences: vec![inference],
        is_final: true,
        reason_status: Some(to_wire_status(result.status)),
    }
}

fn to_wire_status(s: ReasonStatus) -> WireReasonStatus {
    match s {
        ReasonStatus::Complete => WireReasonStatus::Complete,
        ReasonStatus::BudgetExhausted => WireReasonStatus::BudgetExhausted,
        ReasonStatus::DepthLimitReached => WireReasonStatus::DepthLimitReached,
        ReasonStatus::Cancelled => WireReasonStatus::Cancelled,
    }
}
