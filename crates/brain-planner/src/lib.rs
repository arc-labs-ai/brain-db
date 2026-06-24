//! # brain-planner
//!
//! Logical plan tree + cost model + executor for Brain. Bridges
//! `brain-protocol`'s typed requests to the storage stack
//! (`brain-storage`, `brain-metadata`, `brain-index`, `brain-embed`).
//!
//! ## Surface
//!
//! - [`ExecutionPlan`] — one variant per cognitive operation, each
//!   carrying a per-request plan struct.
//! - [`PlannerConfig`] — default knobs (`ef=64`, `max_ef=500`,
//!   `budget=1 s`, …).
//! - [`ShardStats`] — per-shard state the cost model consults.
//! - [`PlannerContext`] = (config, stats).
//! - [`PlanError`] — `QueryTooExpensive` + `InvalidParameters` +
//!   `Unsupported` (catch-all for not-yet-supported shapes).

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod config;
pub mod context;
pub mod cost;
pub mod error;
pub mod executor;
pub mod explain;
pub mod plan;
pub mod planner;
pub mod retrieval;
pub mod stats;
pub mod vsa;

pub use config::PlannerConfig;
pub use context::PlannerContext;
pub use error::PlanError;
pub use executor::{
    execute_path, execute_path_stream, execute_reason, execute_reason_stream, execute_recall,
    EdgeOutcome, EncodeOp, EncodeOpEdge, EncodeResult, EvidenceItem, ExecError, ExecutorContext,
    ForgetOp, ForgetOutcome, ForgetResult, InferenceStep, InferenceStream, InferenceStreamTerminal,
    LinkOp, Path, PathFrame, PathResult, PathStream, PathStreamTerminal, PendingMemorySnapshot,
    PlanStatus, ReasonResult, ReasonStatus, RecallHit, RecallResult, SharedMetadataDb, TxnSnapshot,
    UnlinkOp, WriterError, WriterHandle,
};
pub use explain::explain;
pub use plan::{
    default_contradicts_edge_kinds, default_plan_edge_kinds, default_supports_edge_kinds,
    AggregationStep, AnnSearchStep, ApplyStep, ContextResolutionStep, EdgeSpec, EdgeStep,
    EmbeddingStep, EncodePlan, EncodeResponseStep, EvidenceResponseStep, ExecutionPlan, FilterRule,
    FilterStage, FilterStep, ForgetApplyStep, ForgetPlan, ForgetResponseStep, ForgetWalStep,
    IdempotencyCheckStep, MergeStep, MetadataLookupStep, PathPlan, ReasonPlan, RecallPlan,
    RecallSubStep, ResponseStep, ScoringStep, ShardId, ShardSearchStep, SlotAllocationStep,
    SortKey, TextFetchStep, TraversalStep, WalAppendStep,
};
pub use planner::encode::{
    plan_encode, plan_encode_inner, validate_text, validate_vector_direct, DEFAULT_ENCODE_KIND,
    DEFAULT_ENCODE_SALIENCE, MAX_TEXT_BYTES,
};
pub use planner::forget::{plan_forget, plan_forget_inner};
pub use planner::path::{plan_path, plan_path_inner};
pub use planner::reason::{plan_reason, plan_reason_inner};
pub use planner::recall::{plan_recall, plan_recall_inner};
pub use stats::ShardStats;

/// Compile-time guard: every plan type must be `Send + Sync` so the
/// executor can move plans across async-task boundaries (Glommio
/// per-shard executors, etc.).
const _: fn() = || {
    fn require<T: Send + Sync>() {}
    require::<ExecutionPlan>();
    require::<EncodePlan>();
    require::<RecallPlan>();
    require::<PathPlan>();
    require::<ReasonPlan>();
    require::<ForgetPlan>();
    require::<PlannerContext>();
    require::<PlanError>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn plan_error_displays_readably() {
        let e = PlanError::QueryTooExpensive {
            estimated_ms: 1500.0,
            budget_ms: 1000.0,
        };
        let s = format!("{e}");
        assert!(s.contains("1500"));
        assert!(s.contains("1000"));

        let e2 = PlanError::InvalidParameters {
            field: "k",
            reason: "exceeds max_k=1000".to_string(),
        };
        assert!(format!("{e2}").contains("invalid parameter k"));

        let e3 = PlanError::Unsupported("cross-shard fan-out");
        assert!(format!("{e3}").contains("cross-shard"));
    }

    /// Plan size < 4 KB. A heap-allocating plan
    /// (Vec, String) measures only the stack footprint via
    /// `size_of`; that's the right thing to bound — the heap content
    /// is dominated by the cue text, which is acceptable.
    #[test]
    fn plan_stack_size_under_four_kib() {
        assert!(
            size_of::<RecallPlan>() < 4096,
            "RecallPlan stack size = {}",
            size_of::<RecallPlan>()
        );
        assert!(
            size_of::<EncodePlan>() < 4096,
            "EncodePlan stack size = {}",
            size_of::<EncodePlan>()
        );
        assert!(
            size_of::<ExecutionPlan>() < 4096,
            "ExecutionPlan stack size = {}",
            size_of::<ExecutionPlan>()
        );
    }
}
