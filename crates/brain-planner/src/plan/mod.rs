//! The `ExecutionPlan` enum + per-request plan types.
//!
//! Each variant carries an immutable, owned plan value. Plans don't
//! cross the wire, so they don't derive `serde` /
//! `rkyv`; they're built by the planner, consumed by the executor,
//! and dropped after the response is sent.

pub mod common;
pub mod encode;
pub mod forget;
pub mod path;
pub mod reason;
pub mod recall;

pub use common::{EdgeSpec, FilterRule, FilterStage, RecallSubStep, ShardId, SortKey};
pub use encode::{
    ApplyStep, ContextResolutionStep, EdgeStep, EncodePlan, EncodeResponseStep,
    IdempotencyCheckStep, SlotAllocationStep, WalAppendStep,
};
pub use forget::{ForgetApplyStep, ForgetPlan, ForgetResponseStep, ForgetWalStep};
pub use path::{
    default_plan_edge_kinds, EvidenceResponseStep, PathPlan, ScoringStep, TraversalStep,
};
pub use reason::{
    default_contradicts_edge_kinds, default_supports_edge_kinds, AggregationStep, ReasonPlan,
};
pub use recall::{
    AnnSearchStep, EmbeddingStep, FilterStep, MergeStep, MetadataLookupStep, RecallPlan,
    ResponseStep, ShardSearchStep, TextFetchStep,
};

/// The planner's output. One variant per cognitive operation.
///
/// Admin / Txn / Subscribe plans are deferred (they don't fit the
/// cognitive-operation shape and their lifecycles differ).
#[derive(Debug, Clone)]
pub enum ExecutionPlan {
    Encode(EncodePlan),
    Recall(RecallPlan),
    /// The `PLAN` cognitive operation (path-planning). Struct is named
    /// `PathPlan` to keep `ExecutionPlan::Plan(PathPlan { ... })`
    /// readable.
    Plan(PathPlan),
    Reason(ReasonPlan),
    Forget(ForgetPlan),
}
