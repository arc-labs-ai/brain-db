//! Planner side for the `REASON` cognitive operation.
//!
//! Maps a wire `ReasonRequest` into a [`ReasonPlan`]. Pure.
//!
//! See `spec/08_query_planner/05_plan_reason_planning.md` §8-§10.

use brain_protocol::request::{ObservationInput, ReasonRequest};

use crate::config::PlannerConfig;
use crate::context::PlannerContext;
use crate::cost;
use crate::error::PlanError;
use crate::plan::{
    common::RecallSubStep,
    path::{EvidenceResponseStep, TraversalStep},
    reason::{
        default_contradicts_edge_kinds, default_supports_edge_kinds, AggregationStep, ReasonPlan,
    },
    recall::{
        AnnSearchStep, EmbeddingStep, FilterStep, MergeStep, MetadataLookupStep, ShardSearchStep,
    },
    ExecutionPlan, FilterStage, SortKey,
};

pub fn plan_reason(req: &ReasonRequest, ctx: &PlannerContext) -> Result<ExecutionPlan, PlanError> {
    Ok(ExecutionPlan::Reason(plan_reason_inner(req, ctx)?))
}

pub fn plan_reason_inner(
    req: &ReasonRequest,
    ctx: &PlannerContext,
) -> Result<ReasonPlan, PlanError> {
    validate(req, &ctx.config)?;

    let depth = req.depth as usize;
    let max_inferences = req.max_inferences as usize;
    let estimated = cost::cost_reason(depth, max_inferences, ctx);
    cost::check_budget(estimated, ctx)?;

    let embedding = embedding_for(&req.observation);
    let base_recall = recall_substep_for(&req.observation, ctx);

    let supports_traversal = TraversalStep {
        edge_kinds: default_supports_edge_kinds(),
        max_depth: depth,
        bidirectional: false, // walk only outward from base
        max_paths: max_inferences.max(8),
    };
    let contradicts_traversal = TraversalStep {
        edge_kinds: default_contradicts_edge_kinds(),
        max_depth: depth,
        bidirectional: false,
        max_paths: max_inferences.max(8),
    };

    let aggregation = AggregationStep::default();

    let response = EvidenceResponseStep {
        include_paths: true,
        include_text: false,
        include_metadata: false,
    };

    Ok(ReasonPlan {
        observation: req.observation.clone(),
        depth: req.depth,
        confidence_threshold: req.confidence_threshold,
        max_inferences: req.max_inferences,
        budget_wall_time_ms: req.budget_wall_time_ms,
        embedding,
        base_recall,
        supports_traversal,
        contradicts_traversal,
        aggregation,
        response,
        estimated_cost_ms: estimated,
    })
}

fn validate(req: &ReasonRequest, config: &PlannerConfig) -> Result<(), PlanError> {
    if req.depth == 0 {
        return Err(PlanError::InvalidParameters {
            field: "depth",
            reason: "must be > 0".to_string(),
        });
    }
    let depth = req.depth as usize;
    if depth > config.max_traversal_depth {
        return Err(PlanError::InvalidParameters {
            field: "depth",
            reason: format!(
                "{depth} exceeds max_traversal_depth = {}",
                config.max_traversal_depth
            ),
        });
    }
    if !(0.0..=1.0).contains(&req.confidence_threshold) {
        return Err(PlanError::InvalidParameters {
            field: "confidence_threshold",
            reason: format!("{} must be in [0, 1]", req.confidence_threshold),
        });
    }
    if req.max_inferences == 0 {
        return Err(PlanError::InvalidParameters {
            field: "max_inferences",
            reason: "must be > 0".to_string(),
        });
    }
    let inferences = req.max_inferences as usize;
    if inferences > config.max_plan_results {
        return Err(PlanError::InvalidParameters {
            field: "max_inferences",
            reason: format!(
                "{inferences} exceeds max_plan_results = {}",
                config.max_plan_results
            ),
        });
    }
    Ok(())
}

fn embedding_for(observation: &ObservationInput) -> Option<EmbeddingStep> {
    match observation {
        ObservationInput::ByMemoryId(_) => None,
        ObservationInput::ByText(t) => Some(EmbeddingStep {
            text: t.clone(),
            cache_lookup: true,
        }),
    }
}

fn recall_substep_for(
    observation: &ObservationInput,
    ctx: &PlannerContext,
) -> Option<RecallSubStep> {
    let cue_text = match observation {
        ObservationInput::ByMemoryId(_) => return None,
        ObservationInput::ByText(t) => t.clone(),
    };

    let selectivity = 1.0_f32;
    let ef = cost::pick_ef(20, selectivity, ctx);
    let candidates = 20usize.min(ctx.config.max_candidates_per_search);

    Some(RecallSubStep {
        embedding: EmbeddingStep {
            text: cue_text,
            cache_lookup: true,
        },
        shard: ShardSearchStep {
            shard_id: 0,
            ann_search: AnnSearchStep {
                ef,
                candidates_to_request: candidates,
                pre_filter: Vec::new(),
            },
            metadata_lookup: MetadataLookupStep {
                include_extra: false,
            },
            filter_apply: FilterStep {
                stage: FilterStage::PostFilter,
                rules: Vec::new(),
            },
        },
        merge: MergeStep {
            sort_by: SortKey::Score,
            final_top: 20,
            confidence_min: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::MemoryId;

    fn base_request() -> ReasonRequest {
        ReasonRequest {
            observation: ObservationInput::ByText("the cat sat".into()),
            depth: 3,
            confidence_threshold: 0.5,
            context_filter: None,
            max_inferences: 5,
            budget_wall_time_ms: 100,
            request_id: None,
            txn_id: None,
        }
    }

    fn unwrap_reason(plan: ExecutionPlan) -> ReasonPlan {
        match plan {
            ExecutionPlan::Reason(p) => p,
            other => panic!("expected Reason, got {other:?}"),
        }
    }

    #[test]
    fn default_request_shape() {
        let plan = unwrap_reason(plan_reason(&base_request(), &PlannerContext::default()).unwrap());
        assert!(plan.embedding.is_some());
        assert!(plan.base_recall.is_some());
        assert_eq!(plan.supports_traversal.max_depth, 3);
        assert_eq!(plan.contradicts_traversal.max_depth, 3);
        assert_eq!(
            plan.supports_traversal.edge_kinds,
            default_supports_edge_kinds()
        );
        assert_eq!(
            plan.contradicts_traversal.edge_kinds,
            default_contradicts_edge_kinds()
        );
        assert!(plan.estimated_cost_ms > 0.0);
    }

    #[test]
    fn by_memory_id_skips_embedding() {
        let mut r = base_request();
        r.observation = ObservationInput::ByMemoryId(MemoryId::from(7u128).raw());
        let plan = unwrap_reason(plan_reason(&r, &PlannerContext::default()).unwrap());
        assert!(plan.embedding.is_none());
        assert!(plan.base_recall.is_none());
    }

    #[test]
    fn zero_depth_rejected() {
        let mut r = base_request();
        r.depth = 0;
        match plan_reason(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, .. }) => assert_eq!(field, "depth"),
            other => panic!("expected InvalidParameters, got {other:?}"),
        }
    }

    #[test]
    fn depth_over_max_rejected() {
        let mut r = base_request();
        r.depth = 11;
        match plan_reason(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, .. }) => assert_eq!(field, "depth"),
            other => panic!("expected InvalidParameters, got {other:?}"),
        }
    }

    #[test]
    fn confidence_out_of_range_rejected() {
        let mut r = base_request();
        r.confidence_threshold = 2.0;
        match plan_reason(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, .. }) => {
                assert_eq!(field, "confidence_threshold");
            }
            other => panic!("expected InvalidParameters, got {other:?}"),
        }
    }

    #[test]
    fn zero_max_inferences_rejected() {
        let mut r = base_request();
        r.max_inferences = 0;
        match plan_reason(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, .. }) => {
                assert_eq!(field, "max_inferences");
            }
            other => panic!("expected InvalidParameters, got {other:?}"),
        }
    }

    #[test]
    fn max_inferences_over_cap_rejected() {
        let mut r = base_request();
        r.max_inferences = 101;
        match plan_reason(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, reason }) => {
                assert_eq!(field, "max_inferences");
                assert!(reason.contains("max_plan_results"));
            }
            other => panic!("expected InvalidParameters, got {other:?}"),
        }
    }

    #[test]
    fn aggregation_defaults_to_five_and_five() {
        let plan = unwrap_reason(plan_reason(&base_request(), &PlannerContext::default()).unwrap());
        assert_eq!(plan.aggregation.max_supporting, 5);
        assert_eq!(plan.aggregation.max_contradicting, 5);
        assert!(plan.aggregation.include_aggregate_confidence);
    }
}
