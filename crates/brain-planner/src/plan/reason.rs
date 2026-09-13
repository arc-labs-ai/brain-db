//! `ReasonPlan` — the plan for a `REASON` request.
//!
//! Embed the observation, RECALL similar memories, traverse
//! SUPPORTS / DERIVED_FROM edges (supporting evidence) and
//! CONTRADICTS edges (contradicting evidence), aggregate scores into
//! a confidence value.
//!
//! Ships the **planner-side** shape; the executor lands later
//! alongside `LINK` / `UNLINK`.

use brain_core::EdgeKind;
use brain_protocol::envelope::request::ObservationInput;

use super::common::RecallSubStep;
use super::path::{EvidenceResponseStep, TraversalStep};
use super::recall::EmbeddingStep;

#[derive(Debug, Clone)]
pub struct ReasonPlan {
    pub observation: ObservationInput,
    pub depth: u32,
    pub confidence_threshold: f32,
    pub max_inferences: u32,
    pub budget_wall_time_ms: u32,
    /// `Some` when observation is `ByText`; `None` when `ByMemoryId`
    /// (the memory is already addressable).
    pub embedding: Option<EmbeddingStep>,
    /// Same skip rule as `embedding`.
    pub base_recall: Option<RecallSubStep>,
    pub supports_traversal: TraversalStep,
    pub contradicts_traversal: TraversalStep,
    pub aggregation: AggregationStep,
    pub response: EvidenceResponseStep,
    pub estimated_cost_ms: f32,
}

/// Confidence aggregation for REASON.
#[derive(Debug, Clone, Copy)]
pub struct AggregationStep {
    pub max_supporting: usize,
    pub max_contradicting: usize,
    /// When `true`, the response carries an aggregate
    /// `supports / (supports + contradicts)` confidence value.
    pub include_aggregate_confidence: bool,
}

impl Default for AggregationStep {
    fn default() -> Self {
        Self {
            max_supporting: 5,
            max_contradicting: 5,
            include_aggregate_confidence: true,
        }
    }
}

/// Default edge kinds for REASON's supports-traversal.
#[must_use]
pub fn default_supports_edge_kinds() -> Vec<EdgeKind> {
    vec![EdgeKind::Supports, EdgeKind::DerivedFrom]
}

/// Default edge kinds for REASON's contradicts-traversal.
#[must_use]
pub fn default_contradicts_edge_kinds() -> Vec<EdgeKind> {
    vec![EdgeKind::Contradicts]
}
