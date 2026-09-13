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
        }
    }
}
