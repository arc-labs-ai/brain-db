//! # brain-planner
//!
//! Logical plan tree + cost model + executor for Brain. Bridges
//! `brain-protocol`'s typed requests to the storage stack
//! (`brain-storage`, `brain-metadata`, `brain-index`, `brain-embed`).
//!
//! See `spec/08_query_planner/` for the authoritative design.
//!
//! ## Sub-task 6.1 surface
//!
//! - [`ExecutionPlan`] — one variant per cognitive operation, each
//!   carrying a per-request plan struct (spec §08/01 §2).
//! - [`PlannerConfig`] — spec-default knobs (`ef=64`, `max_ef=500`,
//!   `budget=1 s`, …).
//! - [`ShardStats`] — per-shard state the cost model consults.
//! - [`PlannerContext`] = (config, stats).
//! - [`PlanError`] — `QueryTooExpensive` + `InvalidParameters` +
//!   `Unsupported` (catch-all for not-yet-supported shapes).
//!
//! Logic (planner functions, executor) lands in sub-tasks 6.2–6.8.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod config;
pub mod context;
pub mod cost;
pub mod encode;
pub mod error;
pub mod executor;
pub mod explain;
pub mod forget;
pub mod path;
pub mod plan;
pub mod reason;
pub mod recall;
pub mod stats;

pub use config::PlannerConfig;
pub use context::PlannerContext;
pub use encode::{plan_encode, plan_encode_inner, MAX_TEXT_BYTES};
pub use error::PlanError;
pub use executor::{
    execute, execute_encode, execute_forget, execute_recall, EdgeOutcome, EncodeAck, EncodeOp,
    EncodeOpEdge, EncodeResult, ExecError, ExecutionResult, ExecutorContext, ForgetAck, ForgetOp,
    ForgetOutcome, ForgetResult, RecallHit, RecallResult, SharedMetadataDb, WriterError,
    WriterHandle,
};
pub use explain::explain;
pub use forget::{plan_forget, plan_forget_inner};
pub use path::{plan_path, plan_path_inner};
pub use plan::{
    default_contradicts_edge_kinds, default_plan_edge_kinds, default_supports_edge_kinds,
    AggregationStep, AnnSearchStep, ApplyStep, ContextResolutionStep, EdgeSpec, EdgeStep,
    EmbeddingStep, EncodePlan, EncodeResponseStep, EvidenceResponseStep, ExecutionPlan, FilterRule,
    FilterStage, FilterStep, ForgetApplyStep, ForgetPlan, ForgetResponseStep, ForgetWalStep,
    IdempotencyCheckStep, MergeStep, MetadataLookupStep, PathPlan, ReasonPlan, RecallPlan,
    RecallSubStep, ResponseStep, ScoringStep, ShardId, ShardSearchStep, SlotAllocationStep,
    SortKey, TextFetchStep, TraversalStep, WalAppendStep,
};
pub use reason::{plan_reason, plan_reason_inner};
pub use recall::{plan_recall, plan_recall_inner};
pub use stats::ShardStats;

/// Compile-time guard: every plan type must be `Send + Sync` so the
/// executor (when it lands in 6.7) can move plans across async-task
/// boundaries (Glommio per-shard executors, etc.).
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
    use brain_core::{AgentId, ContextId, MemoryKind, RequestId};
    use std::mem::size_of;
    use uuid::Uuid;

    fn fake_context_id() -> ContextId {
        ContextId(42)
    }

    fn fake_agent_id() -> AgentId {
        AgentId(Uuid::nil())
    }

    fn fake_request_id() -> RequestId {
        RequestId(Uuid::nil())
    }

    #[test]
    fn planner_config_defaults_match_spec() {
        let c = PlannerConfig::default();
        assert_eq!(c.default_ef_search, 64, "spec §08/03 §4");
        assert_eq!(c.max_ef_search, 500, "spec §08/03 §4");
        assert_eq!(c.max_candidates_per_search, 1000, "spec §08/03 §5");
        assert!(
            (c.cost_budget_ms - 1000.0).abs() < f32::EPSILON,
            "spec §08/07 §5"
        );
        assert_eq!(c.max_k, 1000, "spec §08/03 §1");
        assert_eq!(c.max_edges_per_encode, 64, "spec §08/04 §12");
    }

    #[test]
    fn shard_stats_default_is_all_zero() {
        let s = ShardStats::default();
        assert_eq!(s.memory_count, 0);
        assert_eq!(s.tombstone_count, 0);
        assert_eq!(s.tombstone_ratio, 0.0);
    }

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

    #[test]
    fn execution_plan_constructs_with_recall_shape() {
        let plan = ExecutionPlan::Recall(RecallPlan {
            embedding: EmbeddingStep {
                text: "hello".into(),
                cache_lookup: true,
            },
            shards: vec![ShardSearchStep {
                shard_id: 0u16,
                ann_search: AnnSearchStep {
                    ef: 64,
                    candidates_to_request: 80,
                    pre_filter: vec![],
                },
                metadata_lookup: MetadataLookupStep {
                    include_extra: false,
                },
                filter_apply: FilterStep {
                    stage: FilterStage::PostFilter,
                    rules: vec![],
                },
            }],
            merge: MergeStep {
                sort_by: SortKey::Score,
                final_top: 10,
                confidence_min: None,
            },
            text_fetch: None,
            response: ResponseStep {
                include_text: false,
                include_metadata: false,
            },
            estimated_cost_ms: 7.5,
        });
        // The variant matches what we built.
        assert!(matches!(plan, ExecutionPlan::Recall(_)));
        // And it can be cloned (planner → executor handoff may clone).
        let _cloned = plan.clone();
    }

    #[test]
    fn execution_plan_constructs_with_encode_shape() {
        let plan = ExecutionPlan::Encode(EncodePlan {
            shard: 0u16,
            idempotency_check: IdempotencyCheckStep {
                request_id: fake_request_id(),
            },
            embedding: EmbeddingStep {
                text: "hello".into(),
                cache_lookup: true,
            },
            context_resolution: ContextResolutionStep::Explicit(fake_context_id()),
            allocation: SlotAllocationStep {
                arena_grow_if_needed: true,
            },
            wal_append: WalAppendStep {
                kind: MemoryKind::Episodic,
                salience_initial: 0.5,
                fsync: true,
            },
            apply: ApplyStep {
                arena_write: true,
                metadata_write: true,
                hnsw_insert: true,
            },
            edges: vec![],
            response: EncodeResponseStep {
                persistent_id: true,
            },
            estimated_cost_ms: 7.5,
        });
        assert!(matches!(plan, ExecutionPlan::Encode(_)));

        // Named-context branch compiles.
        let _named = ContextResolutionStep::GetOrCreate {
            agent_id: fake_agent_id(),
            name: "_default".into(),
        };
    }

    /// Spec §08/01 §5: plan size < 4 KB. A heap-allocating plan
    /// (Vec, String) measures only the stack footprint via
    /// `size_of`; that's the right thing to bound — the heap content
    /// is dominated by the cue text, which the spec says is acceptable
    /// (spec §08/04 §13).
    #[test]
    fn plan_stack_size_under_four_kib() {
        assert!(
            size_of::<RecallPlan>() < 4096,
            "RecallPlan stack size = {} (spec §08/01 §5 budgets 4 KB)",
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
