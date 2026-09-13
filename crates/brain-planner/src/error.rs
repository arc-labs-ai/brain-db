//! Planner-side errors. Distinct from the executor's runtime errors.
//!
//! `QueryTooExpensive` rejects over-budget plans; the other variants
//! collect malformed-request and not-yet-supported cases. `Unsupported`
//! is a deliberate catch-all so partial planner coverage doesn't panic
//! — the planner returns a structured error and the layer above
//! decides what to do (typically respond with a wire error).

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum PlanError {
    #[error("query too expensive: estimated {estimated_ms:.1} ms > budget {budget_ms:.1} ms")]
    QueryTooExpensive { estimated_ms: f32, budget_ms: f32 },

    /// Client-supplied parameter outside the allowed range.
    /// Examples: `k > max_k`, salience outside `[0, 1]`, ef below K.
    #[error("invalid parameter {field}: {reason}")]
    InvalidParameters { field: &'static str, reason: String },

    /// Request shape the planner does not yet handle. Single-shard,
    /// single-text plans ship today; cross-shard fan-out, plan
    /// caching, subscribe/transaction plans, etc. fall here.
    #[error("unsupported request shape: {0}")]
    Unsupported(&'static str),
}
