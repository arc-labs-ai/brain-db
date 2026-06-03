//! REASON handler.
//!
//! Wires the planner + the evidence-traversal executor through the
//! dispatcher and projects an `InferenceStream` (the executor's
//! per-step frames plus a terminal summary) into a sequence of wire
//! `ReasonResponseFrame`s.
//!
//! **v1 scope:** the executor always produces exactly one inference
//! step — the aggregate of all supporting + contradicting evidence —
//! so the typical stream is one mid-stream frame + one terminal
//! frame. An empty inference stream (no base resolved) collapses to a
//! single terminal frame. The wire framing is multi-frame-ready: a
//! future iteration that walks supporting and contradicting passes
//! independently can emit a step per pass without touching the
//! contract.

use brain_planner::{execute_reason_stream, plan_reason_inner, InferenceStep, ReasonStatus};
use brain_protocol::envelope::request::{ObservationInput, ReasonRequest};
use brain_protocol::envelope::response::{
    InferenceKind, InferenceStep as WireInferenceStep, ReasonResponseFrame,
    ReasonStatus as WireReasonStatus,
};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::state::txn_lens::build_executor_with_lens;

pub async fn handle_reason(
    req: ReasonRequest,
    ctx: &OpsContext,
) -> Result<Vec<ReasonResponseFrame>, OpError> {
    // Capture the claim text before the planner consumes the request.
    // ByMemoryId observations don't carry text — v1 leaves the claim
    // field empty.
    let claim = match &req.observation {
        ObservationInput::ByText(t) => t.clone(),
        ObservationInput::ByMemoryId(_) => String::new(),
    };

    let plan = plan_reason_inner(&req, &ctx.planner_ctx)?;
    let exec_ctx = build_executor_with_lens(ctx, req.txn_id)?;
    let stream = execute_reason_stream(plan, &exec_ctx).await?;

    let mut frames: Vec<ReasonResponseFrame> = Vec::with_capacity(stream.steps.len() + 2);
    let had_steps = !stream.steps.is_empty();
    for step in stream.steps {
        frames.push(step_to_wire(step, &claim));
    }
    // "No support, no contradiction" case: the base set was empty (the
    // agent has no memory near the query). The executor emits no step,
    // but the contract still returns one inference echoing the claim with
    // empty evidence and confidence 0.0 — the client reads that as "I
    // don't know", not an absent answer.
    if !had_steps {
        frames.push(ReasonResponseFrame {
            inferences: vec![WireInferenceStep {
                step_index: 0,
                claim: claim.clone(),
                supporting_memories: Vec::new(),
                contradicting_memories: Vec::new(),
                confidence: stream.terminal.confidence,
                inference_kind: InferenceKind::EvidenceAccumulation,
            }],
            is_final: false,
            reason_status: None,
        });
    }
    // Terminal frame — carries the aggregate confidence + status and
    // marks end-of-stream.
    frames.push(ReasonResponseFrame {
        inferences: Vec::new(),
        is_final: true,
        reason_status: Some(to_wire_status(stream.terminal.status)),
    });
    Ok(frames)
}

fn step_to_wire(step: InferenceStep, claim: &str) -> ReasonResponseFrame {
    let supporting_memories: Vec<u128> =
        step.supporting.iter().map(|e| e.memory_id.into()).collect();
    let contradicting_memories: Vec<u128> = step
        .contradicting
        .iter()
        .map(|e| e.memory_id.into())
        .collect();

    let inference = WireInferenceStep {
        step_index: step.step_index,
        claim: claim.to_owned(),
        supporting_memories,
        contradicting_memories,
        confidence: step.confidence,
        // v1 packs evidence accumulation; future sub-modes
        // (CausalExplanation, AnalogicalInference) can route distinct
        // walks here.
        inference_kind: InferenceKind::EvidenceAccumulation,
    };
    ReasonResponseFrame {
        inferences: vec![inference],
        is_final: false,
        reason_status: None,
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
