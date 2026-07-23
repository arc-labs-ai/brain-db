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

use std::collections::HashMap;
use std::collections::HashSet;

use brain_core::MemoryId;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_planner::{
    execute_reason_stream, plan_reason_inner, InferenceKind as InternalInferenceKind,
    InferenceStep, ReasonStatus, ReasonTrace as InternalReasonTrace,
    ReasonTraceEdgeCandidate as InternalReasonTraceEdgeCandidate,
    ReasonTraceWalk as InternalReasonTraceWalk,
};
use brain_protocol::envelope::request::{EdgeKindWire, ObservationInput, ReasonRequest};
use brain_protocol::envelope::response::{
    InferenceKind, InferenceStep as WireInferenceStep, ReasonResponseFrame,
    ReasonStatus as WireReasonStatus, ReasonTrace, ReasonTraceBase, ReasonTraceCandidate,
    ReasonTraceCentroid, ReasonTraceEdgeCandidate, ReasonTraceIdWithText,
    ReasonTraceScoreBreakdown, ReasonTraceScoredId, ReasonTraceWalk,
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
    let trace_requested = req.trace;

    let plan = plan_reason_inner(&req, &ctx.planner_ctx)?;
    let exec_ctx = build_executor_with_lens(ctx, req.txn_id)?;
    let stream = execute_reason_stream(plan, &exec_ctx, trace_requested).await?;

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
            trace: None,
        });
    }
    // Terminal frame — carries the aggregate confidence + status and
    // marks end-of-stream, plus the full per-stage trace when the
    // caller requested `trace = true`.
    let trace = match stream.terminal.trace {
        Some(t) => Some(build_reason_trace(t, ctx)?),
        None => None,
    };
    frames.push(ReasonResponseFrame {
        inferences: Vec::new(),
        is_final: true,
        reason_status: Some(to_wire_status(stream.terminal.status)),
        trace,
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
        inference_kind: inference_kind_to_wire(step.inference_kind),
    };
    ReasonResponseFrame {
        inferences: vec![inference],
        is_final: false,
        reason_status: None,
        trace: None,
    }
}

/// Map the executor's internal `InferenceKind` (`brain_planner`) onto the
/// wire `InferenceKind` (`brain_protocol`) 1:1 — kept as two distinct enums
/// so `brain-planner`'s executor internals don't couple to the wire crate's
/// shape (see `InternalInferenceKind`'s doc comment). No `Other(String)`
/// case: the executor never produces an arbitrary-string kind.
fn inference_kind_to_wire(k: InternalInferenceKind) -> InferenceKind {
    match k {
        InternalInferenceKind::CausalExplanation => InferenceKind::CausalExplanation,
        InternalInferenceKind::EvidenceAccumulation => InferenceKind::EvidenceAccumulation,
        InternalInferenceKind::AnalogicalInference => InferenceKind::AnalogicalInference,
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

// ---------------------------------------------------------------------------
// Trace bridging: internal `brain_planner::ReasonTrace` → wire `ReasonTrace`.
// ---------------------------------------------------------------------------

/// Structure the executor's internal `ReasonTrace` into the wire `ReasonTrace`
/// a `trace = true` caller receives. `supports_walk`/`contradicts_walk` and
/// `supports_trim`/`contradicts_trim` are kept separate internally (the
/// executor runs the outward walk and the confidence trim once per
/// direction); the wire shape unifies both directions into one `walk`
/// bucket set, since every `ReasonTraceEdgeCandidate` already carries its
/// `edge_kind` — a consumer can still tell supports from contradicts by the
/// edge kind without a second top-level split.
fn build_reason_trace(
    trace: InternalReasonTrace,
    ctx: &OpsContext,
) -> Result<ReasonTrace, OpError> {
    let InternalReasonTrace {
        base,
        supports_walk,
        contradicts_walk,
        supports_trim,
        contradicts_trim,
        scoring,
        centroid,
    } = trace;

    // One batched text fetch for every distinct memory id appearing
    // anywhere in the trace: base candidates missing text, every
    // considered/dropped edge candidate (which carries no text
    // internally), and every scoring entry.
    let mut ids: HashSet<MemoryId> = HashSet::new();
    for c in &base.candidates {
        ids.insert(c.memory_id);
    }
    collect_walk_ids(&supports_walk, &mut ids);
    collect_walk_ids(&contradicts_walk, &mut ids);
    for (id, _) in &supports_trim.dropped_by_confidence {
        ids.insert(*id);
    }
    for (id, _) in &supports_trim.dropped_by_trim_cap {
        ids.insert(*id);
    }
    for (id, _) in &contradicts_trim.dropped_by_confidence {
        ids.insert(*id);
    }
    for (id, _) in &contradicts_trim.dropped_by_trim_cap {
        ids.insert(*id);
    }
    for s in &scoring {
        ids.insert(s.memory_id);
    }
    let texts = fetch_trace_texts(&ids, ctx)?;
    let text_of = |id: MemoryId| texts.get(&id).cloned().unwrap_or_default();

    let base_wire = ReasonTraceBase {
        candidates: base
            .candidates
            .into_iter()
            .map(|c| ReasonTraceCandidate {
                memory_id: c.memory_id.raw(),
                text: c.text.unwrap_or_else(|| text_of(c.memory_id)),
                score: c.score,
            })
            .collect(),
    };

    let mut considered =
        Vec::with_capacity(supports_walk.considered.len() + contradicts_walk.considered.len());
    considered.extend(
        supports_walk
            .considered
            .iter()
            .map(|e| edge_to_wire(e, &text_of)),
    );
    considered.extend(
        contradicts_walk
            .considered
            .iter()
            .map(|e| edge_to_wire(e, &text_of)),
    );

    let mut dropped_by_edge_kind = Vec::with_capacity(
        supports_walk.dropped_by_edge_kind.len() + contradicts_walk.dropped_by_edge_kind.len(),
    );
    dropped_by_edge_kind.extend(
        supports_walk
            .dropped_by_edge_kind
            .iter()
            .map(|e| edge_to_wire(e, &text_of)),
    );
    dropped_by_edge_kind.extend(
        contradicts_walk
            .dropped_by_edge_kind
            .iter()
            .map(|e| edge_to_wire(e, &text_of)),
    );

    let mut dropped_by_tombstone = Vec::with_capacity(
        supports_walk.dropped_by_tombstone.len() + contradicts_walk.dropped_by_tombstone.len(),
    );
    dropped_by_tombstone.extend(
        supports_walk
            .dropped_by_tombstone
            .iter()
            .map(|e| id_with_text(e.memory_id, &text_of)),
    );
    dropped_by_tombstone.extend(
        contradicts_walk
            .dropped_by_tombstone
            .iter()
            .map(|e| id_with_text(e.memory_id, &text_of)),
    );

    let mut dropped_by_visited = Vec::with_capacity(
        supports_walk.dropped_by_visited.len() + contradicts_walk.dropped_by_visited.len(),
    );
    dropped_by_visited.extend(
        supports_walk
            .dropped_by_visited
            .iter()
            .map(|e| id_with_text(e.memory_id, &text_of)),
    );
    dropped_by_visited.extend(
        contradicts_walk
            .dropped_by_visited
            .iter()
            .map(|e| id_with_text(e.memory_id, &text_of)),
    );

    let mut dropped_by_confidence = Vec::with_capacity(
        supports_trim.dropped_by_confidence.len() + contradicts_trim.dropped_by_confidence.len(),
    );
    dropped_by_confidence.extend(
        supports_trim
            .dropped_by_confidence
            .iter()
            .map(|(id, score)| scored_id(*id, *score, &text_of)),
    );
    dropped_by_confidence.extend(
        contradicts_trim
            .dropped_by_confidence
            .iter()
            .map(|(id, score)| scored_id(*id, *score, &text_of)),
    );

    let dropped_by_max_supporting = supports_trim
        .dropped_by_trim_cap
        .iter()
        .map(|(id, _)| id_with_text(*id, &text_of))
        .collect();
    let dropped_by_max_contradicting = contradicts_trim
        .dropped_by_trim_cap
        .iter()
        .map(|(id, _)| id_with_text(*id, &text_of))
        .collect();

    let walk = ReasonTraceWalk {
        considered,
        dropped_by_edge_kind,
        dropped_by_tombstone,
        dropped_by_visited,
        dropped_by_confidence,
        dropped_by_max_supporting,
        dropped_by_max_contradicting,
    };

    let scoring_wire = scoring
        .into_iter()
        .map(|s| ReasonTraceScoreBreakdown {
            memory_id: s.memory_id.raw(),
            text: text_of(s.memory_id),
            base_similarity: s.base_similarity,
            decay: s.decay,
            weight_product: s.weight_product,
            alignment: s.alignment,
            analogical_fit: s.analogical_fit,
            final_score: s.final_score,
        })
        .collect();

    Ok(ReasonTrace {
        base: base_wire,
        walk,
        scoring: scoring_wire,
        centroid: ReasonTraceCentroid {
            computed: centroid.computed,
            skipped_reason: centroid.skipped_reason,
        },
    })
}

fn collect_walk_ids(walk: &InternalReasonTraceWalk, ids: &mut HashSet<MemoryId>) {
    for e in &walk.considered {
        ids.insert(e.memory_id);
    }
    for e in &walk.dropped_by_edge_kind {
        ids.insert(e.memory_id);
    }
    for e in &walk.dropped_by_tombstone {
        ids.insert(e.memory_id);
    }
    for e in &walk.dropped_by_visited {
        ids.insert(e.memory_id);
    }
}

fn edge_to_wire(
    e: &InternalReasonTraceEdgeCandidate,
    text_of: &impl Fn(MemoryId) -> String,
) -> ReasonTraceEdgeCandidate {
    ReasonTraceEdgeCandidate {
        memory_id: e.memory_id.raw(),
        text: text_of(e.memory_id),
        edge_kind: EdgeKindWire::from(e.edge_kind),
        depth: u32::try_from(e.depth).unwrap_or(u32::MAX),
        from_memory_id: e.from_memory_id.raw(),
        raw_score: e.raw_score,
    }
}

fn id_with_text(id: MemoryId, text_of: &impl Fn(MemoryId) -> String) -> ReasonTraceIdWithText {
    ReasonTraceIdWithText {
        memory_id: id.raw(),
        text: text_of(id),
    }
}

fn scored_id(
    id: MemoryId,
    score: f32,
    text_of: &impl Fn(MemoryId) -> String,
) -> ReasonTraceScoredId {
    ReasonTraceScoredId {
        memory_id: id.raw(),
        text: text_of(id),
        score,
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
        .map_err(|e| OpError::Internal(format!("reason trace read_txn: {e}")))?;
    let texts_table = rtxn
        .open_table(TEXTS_TABLE)
        .map_err(|e| OpError::Internal(format!("reason trace open TEXTS_TABLE: {e}")))?;

    let mut out = HashMap::with_capacity(ids.len());
    for &id in ids {
        let text = match texts_table.get(&id.to_be_bytes()) {
            Ok(Some(guard)) => std::str::from_utf8(guard.value())
                .map(str::to_owned)
                .map_err(|e| {
                    OpError::Internal(format!(
                        "reason trace TEXTS_TABLE non-UTF-8 for {id:?}: {e}"
                    ))
                })?,
            Ok(None) => String::new(),
            Err(e) => {
                return Err(OpError::Internal(format!(
                    "reason trace TEXTS_TABLE get: {e}"
                )));
            }
        };
        out.insert(id, text);
    }
    Ok(out)
}
