//! Planner side for the `PLAN` cognitive operation (path-planning).
//!
//! Maps a wire `PlanRequest` into a [`PathPlan`]. Pure: no I/O, no
//! async. The executor side lands later — bidirectional-BFS edge
//! traversal naturally fits with the cognitive-ops scaffolding.

use brain_protocol::envelope::request::{PlanRequest, PlanState, PlanStrategy};

use crate::config::PlannerConfig;
use crate::context::PlannerContext;
use crate::cost;
use crate::error::PlanError;
use crate::plan::{
    common::RecallSubStep,
    path::{default_plan_edge_kinds, EvidenceResponseStep, PathPlan, ScoringStep, TraversalStep},
    recall::{
        AnnSearchStep, EmbeddingStep, FilterStep, MergeStep, MetadataLookupStep, ShardSearchStep,
    },
    ExecutionPlan, FilterStage, SortKey,
};

pub fn plan_path(req: &PlanRequest, ctx: &PlannerContext) -> Result<ExecutionPlan, PlanError> {
    Ok(ExecutionPlan::Plan(plan_path_inner(req, ctx)?))
}

pub fn plan_path_inner(req: &PlanRequest, ctx: &PlannerContext) -> Result<PathPlan, PlanError> {
    validate(req, &ctx.config)?;

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let max_depth = req.budget.max_steps as usize;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let max_branches = req.budget.max_branches_explored as usize;

    let estimated = cost::cost_path(max_depth, max_branches, ctx);
    cost::check_budget(estimated, ctx)?;

    let starting_recall = recall_substep_for(&req.start, ctx);
    let goal_recall = recall_substep_for(&req.goal, ctx);

    let traversal = TraversalStep {
        edge_kinds: default_plan_edge_kinds(),
        max_depth,
        bidirectional: true,
        max_paths: max_branches.max(8),
    };

    let scoring = ScoringStep::default();

    let response = EvidenceResponseStep {
        include_paths: true,
        include_text: false,
        include_metadata: false,
    };

    let strategy = req.strategy_hint.unwrap_or(PlanStrategy::Auto);

    Ok(PathPlan {
        start: req.start.clone(),
        goal: req.goal.clone(),
        budget: req.budget,
        strategy,
        starting_recall,
        goal_recall,
        traversal,
        scoring,
        response,
        estimated_cost_ms: estimated,
    })
}

fn validate(req: &PlanRequest, config: &PlannerConfig) -> Result<(), PlanError> {
    if req.budget.max_steps == 0 {
        return Err(PlanError::InvalidParameters {
            field: "budget.max_steps",
            reason: "must be > 0".to_string(),
        });
    }
    let depth = req.budget.max_steps as usize;
    if depth > config.max_traversal_depth {
        return Err(PlanError::InvalidParameters {
            field: "budget.max_steps",
            reason: format!(
                "{depth} exceeds max_traversal_depth = {}",
                config.max_traversal_depth
            ),
        });
    }
    if req.budget.max_branches_explored == 0 {
        return Err(PlanError::InvalidParameters {
            field: "budget.max_branches_explored",
            reason: "must be > 0".to_string(),
        });
    }
    Ok(())
}

/// Build a `RecallSubStep` for an endpoint expressed as text or
/// vector. Returns `None` for `ByMemoryId` — that endpoint is already
/// addressable; no embed/recall needed.
fn recall_substep_for(state: &PlanState, ctx: &PlannerContext) -> Option<RecallSubStep> {
    let cue_text = match state {
        PlanState::ByMemoryId(_) => return None,
        PlanState::ByText(t) => t.clone(),
        // For ByVector the executor reads the bytes off the request
        // frame; the embedding step is unused but we still need a
        // shard search. We carry an empty text placeholder so the
        // shape stays uniform; the executor will sidestep the embed
        // call when it sees the ByVector variant.
        PlanState::ByVector { .. } => String::new(),
    };

    let selectivity = 1.0_f32;
    let ef = cost::pick_ef(10, selectivity, ctx);
    let candidates = 10usize
        .max((10.0 * cost::over_factor(selectivity)) as usize)
        .min(ctx.config.max_candidates_per_search);

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
            final_top: 10,
            confidence_min: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::MemoryId;
    use brain_protocol::envelope::request::{PlanBudget, PlanState};

    fn base_request() -> PlanRequest {
        PlanRequest {
            start: PlanState::ByText("origin".into()),
            goal: PlanState::ByText("destination".into()),
            budget: PlanBudget {
                max_steps: 4,
                max_wall_time_ms: 100,
                max_branches_explored: 64,
            },
            strategy_hint: None,
            context_filter: None,
            request_id: None,
            txn_id: None,
        }
    }

    fn unwrap_path(plan: ExecutionPlan) -> PathPlan {
        match plan {
            ExecutionPlan::Plan(p) => p,
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[test]
    fn default_request_shape() {
        let plan = unwrap_path(plan_path(&base_request(), &PlannerContext::default()).unwrap());
        assert!(plan.starting_recall.is_some(), "ByText start → recall");
        assert!(plan.goal_recall.is_some(), "ByText goal → recall");
        assert_eq!(plan.traversal.max_depth, 4);
        assert!(plan.traversal.bidirectional);
        assert_eq!(
            plan.traversal.edge_kinds,
            default_plan_edge_kinds(),
            "default kinds = [Caused, FollowedBy]"
        );
        assert!(plan.estimated_cost_ms > 0.0);
    }

    #[test]
    fn by_memory_id_skips_recall() {
        let mut r = base_request();
        r.start = PlanState::ByMemoryId(MemoryId::from(7u128).raw());
        let plan = unwrap_path(plan_path(&r, &PlannerContext::default()).unwrap());
        assert!(plan.starting_recall.is_none());
        assert!(plan.goal_recall.is_some(), "goal still ByText");
    }

    #[test]
    fn zero_max_steps_rejected() {
        let mut r = base_request();
        r.budget.max_steps = 0;
        match plan_path(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, .. }) => {
                assert_eq!(field, "budget.max_steps");
            }
            other => panic!("expected InvalidParameters, got {other:?}"),
        }
    }

    #[test]
    fn depth_over_max_rejected() {
        let mut r = base_request();
        r.budget.max_steps = 11; // cap is 10
        match plan_path(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, reason }) => {
                assert_eq!(field, "budget.max_steps");
                assert!(reason.contains("max_traversal_depth"));
            }
            other => panic!("expected InvalidParameters, got {other:?}"),
        }
    }

    #[test]
    fn zero_branches_rejected() {
        let mut r = base_request();
        r.budget.max_branches_explored = 0;
        match plan_path(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, .. }) => {
                assert_eq!(field, "budget.max_branches_explored");
            }
            other => panic!("expected InvalidParameters, got {other:?}"),
        }
    }

}
