//! Planner configuration with defaults baked in.
//!
//! Default values:
//! - `default_ef_search = 64`
//! - `max_ef_search = 500`
//! - `max_candidates_per_search = 1000`
//! - `cost_budget_ms = 1000.0`
//! - `max_k = 1000`
//! - `max_edges_per_encode = 64`
//!
//! Operators may override these at startup; the substrate uses
//! `PlannerConfig::default()` if no overrides are provided.

#[derive(Debug, Clone, Copy)]
pub struct PlannerConfig {
    pub default_ef_search: usize,
    pub max_ef_search: usize,
    pub max_candidates_per_search: usize,
    pub cost_budget_ms: f32,
    pub max_k: usize,
    pub max_edges_per_encode: usize,
    /// PLAN / REASON traversal hard cap.
    pub max_traversal_depth: usize,
    /// PLAN / REASON result cap.
    pub max_plan_results: usize,
    /// Hard ceiling on a PLAN request's `budget.max_branches_explored`. An
    /// over-budget request is rejected, not clamped.
    ///
    /// The wire field is a `u32` and the cost model only bounds the product
    /// `depth * branches`, which leaves tens of thousands of branches
    /// reachable at depth 1 — far past anything a caller needs, and cheap
    /// enough to be a resource-exhaustion lever.
    ///
    /// The default is the width the hosted surface runs: `brain-edge`'s HTTP
    /// plan body defaults `max_branches` to 32, and `brain-mcp` sets no
    /// default of its own, so every request arriving over those paths is at
    /// or under this bound.
    ///
    /// It is a usage bound, not a byte bound. A `trace = true` PLAN response
    /// carries one explored node per branch, each with that node's full
    /// untruncated memory text (`MAX_TEXT_BYTES` = 1 MiB, enforced in
    /// `planner::encode`), so at this ceiling a worst-case trace can exceed
    /// the 16 MiB frame cap and be rejected on the egress path. The
    /// byte-exact ceiling is `MAX_PAYLOAD_BYTES / MAX_TEXT_BYTES` = 15 nodes,
    /// which no real caller could live with; the frame-size check is what
    /// enforces the byte bound, and an operator who wants it guaranteed up
    /// front lowers this field.
    pub max_branches_explored: usize,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            default_ef_search: 64,
            max_ef_search: 500,
            max_candidates_per_search: 1000,
            cost_budget_ms: 1000.0,
            max_k: 1000,
            max_edges_per_encode: 64,
            max_traversal_depth: 10,
            max_plan_results: 100,
            max_branches_explored: 32,
        }
    }
}
