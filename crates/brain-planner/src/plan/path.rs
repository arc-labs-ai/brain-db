//! `PathPlan` — the plan for a `PLAN` request (the cognitive
//! operation, not the planner's output).
//!
//! Named `PathPlan` to avoid `ExecutionPlan::Plan(PlanPlan)` confusion.
//! Spec §08/05 §3 describes the shape: embed both endpoints, RECALL
//! near each, traverse the graph between them via bidirectional BFS,
//! score paths, return.
//!
//! Phase 6.5 ships the **planner-side** shape. The executor is
//! deferred — bidirectional-BFS edge traversal lands with Phase 7
//! cognitive-ops alongside `LINK` / `UNLINK`.

use brain_core::EdgeKind;
use brain_protocol::request::{PlanBudget, PlanState, PlanStrategy};

use super::common::RecallSubStep;

#[derive(Debug, Clone)]
pub struct PathPlan {
    pub start: PlanState,
    pub goal: PlanState,
    pub budget: PlanBudget,
    pub strategy: PlanStrategy,
    /// `Some` when `start` is `ByText` / `ByVector`; `None` when
    /// `start = ByMemoryId` (the memory is already addressable, no
    /// need to embed-and-recall).
    pub starting_recall: Option<RecallSubStep>,
    /// Same shape as `starting_recall` for the goal endpoint.
    pub goal_recall: Option<RecallSubStep>,
    pub traversal: TraversalStep,
    pub scoring: ScoringStep,
    pub response: EvidenceResponseStep,
    pub estimated_cost_ms: f32,
}

/// Bidirectional BFS along the named edge kinds. Spec §08/05 §4-§5.
#[derive(Debug, Clone)]
pub struct TraversalStep {
    pub edge_kinds: Vec<EdgeKind>,
    pub max_depth: usize,
    pub bidirectional: bool,
    /// Hard cap on candidate paths the traversal accumulates.
    pub max_paths: usize,
}

/// Path scoring weights. Spec §08/05 §6.
#[derive(Debug, Clone, Copy)]
pub struct ScoringStep {
    pub include_length_score: bool,
    pub include_edge_weight_score: bool,
    pub include_salience_score: bool,
    /// Final cap on paths returned to the caller.
    pub top_n: usize,
}

impl Default for ScoringStep {
    fn default() -> Self {
        Self {
            include_length_score: true,
            include_edge_weight_score: true,
            include_salience_score: true,
            top_n: 10,
        }
    }
}

/// Response shape for PLAN / REASON. Distinct from the recall
/// `ResponseStep` because these return paths / evidence, not flat
/// hit lists.
#[derive(Debug, Clone, Copy)]
pub struct EvidenceResponseStep {
    pub include_paths: bool,
    pub include_text: bool,
    pub include_metadata: bool,
}

/// Default edge kinds for the PLAN traversal. Spec §08/05 §2 names
/// `[CAUSED, FOLLOWED_BY]`. The wire `PlanRequest` doesn't yet carry
/// an explicit list; this is what the planner uses.
#[must_use]
pub fn default_plan_edge_kinds() -> Vec<EdgeKind> {
    vec![EdgeKind::Caused, EdgeKind::FollowedBy]
}
