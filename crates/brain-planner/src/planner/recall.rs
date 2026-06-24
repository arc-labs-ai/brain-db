//! Planner side for the `RECALL` cognitive operation.
//!
//! Takes a wire `RecallRequest` (from `brain-protocol`) and produces a
//! single-shard `RecallPlan`. Pure: no I/O, no async, no state.
//!
//! Ships single-shard today; the cross-shard branch later lights up
//! using the same `RecallPlan { shards: Vec<_> }` envelope.

use brain_core::ContextId;
use brain_protocol::envelope::request::RecallRequest;

use crate::config::PlannerConfig;
use crate::context::PlannerContext;
use crate::cost;
use crate::error::PlanError;
use crate::plan::{
    AnnSearchStep, EmbeddingStep, ExecutionPlan, FilterRule, FilterStage, FilterStep, MergeStep,
    MetadataLookupStep, RecallPlan, ResponseStep, ShardSearchStep, SortKey, TextFetchStep,
};

/// Build the execution plan for a RECALL request.
pub fn plan_recall(req: &RecallRequest, ctx: &PlannerContext) -> Result<ExecutionPlan, PlanError> {
    let plan = plan_recall_inner(req, ctx)?;
    Ok(ExecutionPlan::Recall(plan))
}

/// Same as [`plan_recall`] but returns the `RecallPlan` directly —
/// useful for tests that want to inspect the inner structure.
pub fn plan_recall_inner(
    req: &RecallRequest,
    ctx: &PlannerContext,
) -> Result<RecallPlan, PlanError> {
    validate(req, &ctx.config)?;

    let post_rules = build_filter_rules(req);
    let selectivity = cost::estimate_filter_selectivity(&post_rules);
    let k = req.max_results as usize;

    let ef = cost::pick_ef(k, selectivity, ctx).max(k); // ef ≥ k
    let factor = cost::over_factor(selectivity);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let candidates = ((k as f32 * factor) as usize)
        .max(k)
        .min(ctx.config.max_candidates_per_search);

    // Pessimistic about cache: the planner assumes a miss when
    // estimating uses the cache-miss mid-point.
    let estimated = cost::cost_recall(k, selectivity, /* cache_hit */ false, ctx);
    cost::check_budget(estimated, ctx)?;

    let confidence_min = if req.confidence_threshold > 0.0 {
        Some(req.confidence_threshold)
    } else {
        None
    };

    Ok(RecallPlan {
        embedding: EmbeddingStep {
            text: req.cue_text.clone(),
            cache_lookup: true,
        },
        shards: vec![ShardSearchStep {
            shard_id: 0,
            ann_search: AnnSearchStep {
                ef,
                candidates_to_request: candidates,
                pre_filter: Vec::new(),
            },
            metadata_lookup: MetadataLookupStep {
                include_extra: req.include_edges,
            },
            filter_apply: FilterStep {
                stage: FilterStage::PostFilter,
                rules: post_rules,
            },
        }],
        merge: MergeStep {
            sort_by: SortKey::Score,
            final_top: k,
            confidence_min,
        },
        // `memory_ids` is left empty here; the executor fills it from
        // the surviving post-filter hits before issuing the batched read.
        text_fetch: if req.include_text {
            Some(TextFetchStep {
                memory_ids: Vec::new(),
                parallel: true,
            })
        } else {
            None
        },
        response: ResponseStep {
            include_text: req.include_text,
            include_metadata: req.include_edges,
        },
        estimated_cost_ms: estimated,
    })
}

fn validate(req: &RecallRequest, config: &PlannerConfig) -> Result<(), PlanError> {
    if req.max_results == 0 {
        return Err(PlanError::InvalidParameters {
            field: "max_results",
            reason: "must be > 0".to_string(),
        });
    }
    let k = req.max_results as usize;
    if k > config.max_k {
        return Err(PlanError::InvalidParameters {
            field: "max_results",
            reason: format!("{k} exceeds max_k = {}", config.max_k),
        });
    }
    if !(0.0..=1.0).contains(&req.confidence_threshold) {
        return Err(PlanError::InvalidParameters {
            field: "confidence_threshold",
            reason: format!("{} must be in [0, 1]", req.confidence_threshold),
        });
    }
    if !(0.0..=1.0).contains(&req.salience_floor) {
        return Err(PlanError::InvalidParameters {
            field: "salience_floor",
            reason: format!("{} must be in [0, 1]", req.salience_floor),
        });
    }
    Ok(())
}

fn build_filter_rules(req: &RecallRequest) -> Vec<FilterRule> {
    let mut rules = Vec::new();

    if let Some(kinds) = &req.kind_filter {
        if !kinds.is_empty() {
            let mapped = kinds.iter().copied().map(Into::into).collect::<Vec<_>>();
            rules.push(FilterRule::KindIn(mapped));
        }
    }

    if let Some(contexts) = &req.context_filter {
        if !contexts.is_empty() {
            let mapped = contexts
                .iter()
                .copied()
                .map(ContextId::from)
                .collect::<Vec<_>>();
            rules.push(FilterRule::ContextIn(mapped));
        }
    }

    if req.salience_floor > 0.0 {
        rules.push(FilterRule::SalienceFloor(req.salience_floor));
    }

    if let Some(age_floor) = req.age_bound_unix_nanos {
        rules.push(FilterRule::AgeBound {
            not_older_than_unix_nanos: age_floor,
        });
    }

    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::envelope::request::{MemoryKindWire};

    fn base_request() -> RecallRequest {
        RecallRequest {
            cue_text: "hello".into(),
            subject_name: String::new(),
            max_results: 10,
            confidence_threshold: 0.0,
            context_filter: None,
            age_bound_unix_nanos: None,
            as_of_record_time_unix_nanos: None,
            kind_filter: None,
            salience_floor: 0.0,
            include_edges: false,
            include_graph: false,
            include_text: false,
            request_id: None,
            txn_id: None,
            agent_filter: Vec::new(),
            include_other_agents: false,
        }
    }

    fn ctx() -> PlannerContext {
        PlannerContext::default()
    }

    fn unwrap_recall(plan: ExecutionPlan) -> RecallPlan {
        match plan {
            ExecutionPlan::Recall(p) => p,
            other => panic!("expected Recall, got {other:?}"),
        }
    }

    #[test]
    fn default_request_yields_default_ef() {
        let plan = unwrap_recall(plan_recall(&base_request(), &ctx()).unwrap());
        let shard = &plan.shards[0];
        assert_eq!(shard.ann_search.ef, 64);
        // No filter ⇒ no rules.
        assert!(shard.filter_apply.rules.is_empty());
        // top_k=10 ⇒ final_top=10; over_factor at selectivity 1.0 = 1.
        assert_eq!(plan.merge.final_top, 10);
        assert!(plan.merge.confidence_min.is_none());
    }

    #[test]
    fn zero_k_is_rejected() {
        let mut r = base_request();
        r.max_results = 0;
        match plan_recall(&r, &ctx()) {
            Err(PlanError::InvalidParameters { field, .. }) => assert_eq!(field, "max_results"),
            other => panic!("expected InvalidParameters[max_results], got {other:?}"),
        }
    }

    #[test]
    fn k_over_max_is_rejected() {
        let mut r = base_request();
        r.max_results = 5000;
        match plan_recall(&r, &ctx()) {
            Err(PlanError::InvalidParameters { field, reason }) => {
                assert_eq!(field, "max_results");
                assert!(reason.contains("max_k"));
            }
            other => panic!("expected InvalidParameters[max_results], got {other:?}"),
        }
    }

    #[test]
    fn confidence_out_of_range_is_rejected() {
        let mut r = base_request();
        r.confidence_threshold = 2.0;
        match plan_recall(&r, &ctx()) {
            Err(PlanError::InvalidParameters { field, .. }) => {
                assert_eq!(field, "confidence_threshold");
            }
            other => panic!("expected InvalidParameters[confidence], got {other:?}"),
        }
    }

    #[test]
    fn confidence_threshold_maps_to_merge_filter() {
        let mut r = base_request();
        r.confidence_threshold = 0.7;
        let plan = unwrap_recall(plan_recall(&r, &ctx()).unwrap());
        assert_eq!(plan.merge.confidence_min, Some(0.7));
    }

    #[test]
    fn kind_filter_produces_filter_rule() {
        let mut r = base_request();
        r.kind_filter = Some(vec![MemoryKindWire::Episodic]);
        let plan = unwrap_recall(plan_recall(&r, &ctx()).unwrap());
        let rules = &plan.shards[0].filter_apply.rules;
        assert_eq!(rules.len(), 1);
        match &rules[0] {
            FilterRule::KindIn(kinds) => {
                assert_eq!(kinds.len(), 1);
            }
            other => panic!("expected KindIn, got {other:?}"),
        }
    }

    #[test]
    fn filter_pushes_ef_up() {
        let mut r = base_request();
        r.kind_filter = Some(vec![MemoryKindWire::Episodic]);
        r.salience_floor = 0.8;
        let plan = unwrap_recall(plan_recall(&r, &ctx()).unwrap());
        // Two filter rules — selectivity well below 1.0 — ef should be
        // above the 64 default.
        assert!(plan.shards[0].ann_search.ef > 64);
        // And there should be two post-filter rules.
        assert_eq!(plan.shards[0].filter_apply.rules.len(), 2);
    }

    #[test]
    fn candidates_are_capped() {
        let mut r = base_request();
        r.max_results = 100;
        let plan = unwrap_recall(plan_recall(&r, &ctx()).unwrap());
        assert!(
            plan.shards[0].ann_search.candidates_to_request
                <= ctx().config.max_candidates_per_search
        );
        assert!(plan.shards[0].ann_search.candidates_to_request >= 100);
    }

    #[test]
    fn include_text_false_omits_text_fetch_step() {
        let plan = unwrap_recall(plan_recall(&base_request(), &ctx()).unwrap());
        assert!(plan.text_fetch.is_none());
        assert!(!plan.response.include_text);
    }

    #[test]
    fn include_text_true_adds_text_fetch_step() {
        let mut r = base_request();
        r.include_text = true;
        let plan = unwrap_recall(plan_recall(&r, &ctx()).unwrap());
        let step = plan
            .text_fetch
            .as_ref()
            .expect("include_text=true must add a TextFetchStep");
        // memory_ids is filled in by the executor — empty at plan time.
        assert!(step.memory_ids.is_empty());
        assert!(step.parallel);
        assert!(plan.response.include_text);
    }
}
