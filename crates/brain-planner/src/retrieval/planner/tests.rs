//! Unit tests for the query planner.

use brain_core::StatementKind;
use brain_core::{AgentId, EntityId, PredicateId};

use super::{
    plan, PlanError, PreFilter, QueryPlan, Retriever, RetrieverConfig, MAX_TOP_N, MIN_TOP_N,
};
use crate::retrieval::fusion::DEFAULT_K;
use crate::retrieval::router::{
    FusionConfig, PerRetrieverWeights, QueryRequest, RetrieverSelection, TimeRange,
};

fn req_with_text(text: &str) -> QueryRequest {
    QueryRequest {
        text: Some(text.into()),
        ..Default::default()
    }
}

fn weight_of(plan: &QueryPlan, r: Retriever) -> Option<f32> {
    plan.retrievers
        .iter()
        .find(|p| p.retriever == r)
        .map(|p| p.weight)
}

fn has_retriever(plan: &QueryPlan, r: Retriever) -> bool {
    plan.retrievers.iter().any(|p| p.retriever == r)
}

// ---------------------------------------------------------------------------
// Routing rule coverage.
// ---------------------------------------------------------------------------

#[test]
fn default_free_text_plan() {
    let p = plan(&req_with_text("budget pushback")).expect("plan");
    // Default plan is retrieval for everyone: semantic + lexical
    // always, plus graph at half weight using top semantic hits
    // as memory anchors (the path that lights up the substrate
    // edge graph even without a declared schema).
    assert_eq!(p.retrievers.len(), 3);
    assert_eq!(weight_of(&p, Retriever::Semantic), Some(1.0));
    assert_eq!(weight_of(&p, Retriever::Lexical), Some(1.0));
    assert_eq!(weight_of(&p, Retriever::Graph), Some(0.5));
    assert_eq!(p.fusion.k, DEFAULT_K);
}

#[test]
fn entity_anchored_plan() {
    let req = QueryRequest {
        text: Some("meeting notes".into()),
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    assert_eq!(p.retrievers.len(), 3);
    assert_eq!(weight_of(&p, Retriever::Graph), Some(2.0));
    assert_eq!(weight_of(&p, Retriever::Semantic), Some(1.0));
    assert_eq!(weight_of(&p, Retriever::Lexical), Some(0.5));
}

#[test]
fn exact_id_plan_promotes_lexical() {
    let p = plan(&req_with_text("ticket ACME-1247 broke prod")).expect("plan");
    assert_eq!(weight_of(&p, Retriever::Lexical), Some(2.0));
    assert_eq!(weight_of(&p, Retriever::Semantic), Some(0.5));
}

#[test]
fn no_signal_returns_error() {
    let err = plan(&QueryRequest::default()).expect_err("no signal");
    assert!(matches!(err, PlanError::NoSignal));
}

#[test]
fn filter_only_request_also_returns_no_signal() {
    // Rule 4: type-filter alone adds no retriever; v1 plan rejects.
    let req = QueryRequest {
        kind_filter: vec![StatementKind::Fact],
        ..Default::default()
    };
    let err = plan(&req).expect_err("filter-only is no-signal in v1");
    assert!(matches!(err, PlanError::NoSignal));
}

// ---------------------------------------------------------------------------
// Push-down decisions.
// ---------------------------------------------------------------------------

#[test]
fn temporal_filter_pushes_down_to_every_retriever() {
    let req = QueryRequest {
        text: Some("budget".into()),
        time_filter: Some(TimeRange {
            from_unix_ms: Some(0),
            to_unix_ms: Some(1000),
        }),
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    assert!(p.temporal_pushed_down());
    for r in &p.retrievers {
        match &r.pre_filter {
            Some(PreFilter::Temporal(_)) => {}
            other => panic!("expected temporal pre-filter, got {other:?}"),
        }
    }
}

#[test]
fn predicate_filter_pushes_to_semantic_and_graph_only() {
    let req = QueryRequest {
        text: Some("note".into()),
        entity_anchor: Some(EntityId::new()),
        predicate_filter: vec![PredicateId::from(7)],
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    for r in &p.retrievers {
        match (r.retriever, &r.pre_filter) {
            (Retriever::Semantic | Retriever::Graph, Some(PreFilter::PredicateId(ps))) => {
                assert_eq!(ps.len(), 1);
            }
            (Retriever::Lexical, None) => {}
            (other_r, other_f) => {
                panic!("unexpected pre-filter for {other_r:?}: {other_f:?}")
            }
        }
    }
}

#[test]
fn temporal_pushdown_takes_precedence_over_predicate() {
    let req = QueryRequest {
        text: Some("note".into()),
        entity_anchor: Some(EntityId::new()),
        predicate_filter: vec![PredicateId::from(7)],
        time_filter: Some(TimeRange {
            from_unix_ms: Some(0),
            to_unix_ms: Some(1000),
        }),
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    // All retrievers should carry the temporal pre-filter
    // (highest precedence). Predicate falls through to the
    // post-fusion filter chain.
    for r in &p.retrievers {
        assert!(matches!(r.pre_filter, Some(PreFilter::Temporal(_))));
    }
    // Predicate still present in post_filters.
    assert_eq!(p.post_filters.predicate_filter.len(), 1);
}

#[test]
fn agent_filter_emits_pre_filter() {
    let agent = AgentId::new();
    let req = QueryRequest {
        text: Some("budget".into()),
        agent_filter: vec![agent],
        ..Default::default()
    };
    let p = plan(&req).expect("plan");

    let semantic = p
        .retrievers
        .iter()
        .find(|r| r.retriever == Retriever::Semantic)
        .expect("semantic present");
    match &semantic.pre_filter {
        Some(PreFilter::AgentIds(ids)) => assert_eq!(ids.as_slice(), &[agent]),
        other => panic!("expected AgentIds pre-filter on semantic, got {other:?}"),
    }

    let lexical = p
        .retrievers
        .iter()
        .find(|r| r.retriever == Retriever::Lexical)
        .expect("lexical present");
    match &lexical.pre_filter {
        Some(PreFilter::AgentIds(ids)) => assert_eq!(ids.as_slice(), &[agent]),
        other => panic!("expected AgentIds pre-filter on lexical, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Post-filter materialisation.
// ---------------------------------------------------------------------------

#[test]
fn post_filters_carry_request_fields() {
    let req = QueryRequest {
        text: Some("x".into()),
        kind_filter: vec![StatementKind::Fact],
        confidence_min: Some(0.5),
        include_tombstoned: true,
        include_superseded: false,
        time_filter: Some(TimeRange {
            from_unix_ms: Some(100),
            to_unix_ms: None,
        }),
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    assert_eq!(p.post_filters.kind_filter, vec![StatementKind::Fact]);
    assert_eq!(p.post_filters.confidence_min, Some(0.5));
    assert!(p.post_filters.include_tombstoned);
    assert!(!p.post_filters.include_superseded);
    assert!(p.post_filters.time_filter.is_some());
}

// ---------------------------------------------------------------------------
// Fusion config precedence.
// ---------------------------------------------------------------------------

#[test]
fn request_fusion_k_overrides_default() {
    let req = QueryRequest {
        text: Some("x".into()),
        fusion_config: Some(FusionConfig {
            k: 30,
            weights: PerRetrieverWeights::default(),
        }),
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    assert_eq!(p.fusion.k, 30);
}

#[test]
fn fusion_weights_are_max_of_request_and_router() {
    // Request says graph weight = 3.0; router will say 2.0
    // (entity-anchored rule). Max wins → 3.0.
    let req = QueryRequest {
        text: Some("x".into()),
        entity_anchor: Some(EntityId::new()),
        fusion_config: Some(FusionConfig {
            k: DEFAULT_K,
            weights: PerRetrieverWeights {
                semantic: 1.0,
                lexical: 1.0,
                graph: 3.0,
                temporal: 0.5,
            },
        }),
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    assert!((p.fusion.weights.graph - 3.0).abs() < 1e-6);
}

#[test]
fn fusion_weights_take_router_value_when_request_default() {
    // Request leaves weights at defaults (1.0 each); router
    // gives Graph 2.0 → fusion uses 2.0.
    let req = QueryRequest {
        text: Some("x".into()),
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    assert!((p.fusion.weights.graph - 2.0).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// Explicit override.
// ---------------------------------------------------------------------------

#[test]
fn explicit_retrievers_carry_through_to_plan() {
    let req = QueryRequest {
        text: Some("x".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic, Retriever::Graph]),
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    assert!(p.is_explicit());
    assert!(has_retriever(&p, Retriever::Semantic));
    assert!(has_retriever(&p, Retriever::Graph));
    assert!(!has_retriever(&p, Retriever::Lexical));
}

// ---------------------------------------------------------------------------
// Limit + top_n.
// ---------------------------------------------------------------------------

#[test]
fn limit_zero_defaults_to_20() {
    let p = plan(&req_with_text("x")).expect("plan");
    assert_eq!(p.limit, super::DEFAULT_RESULT_LIMIT);
}

#[test]
fn limit_propagation_and_top_n_floor() {
    // limit = 10 → top_n_default = max(30, 100) = 100.
    let req = QueryRequest {
        text: Some("x".into()),
        limit: 10,
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    assert_eq!(p.limit, 10);
    for r in &p.retrievers {
        assert!(r.top_n >= MIN_TOP_N);
    }
}

#[test]
fn top_n_capped_at_max() {
    // limit = 500 → top_n = max(1500, 100) = 1500, capped to 200.
    let req = QueryRequest {
        text: Some("x".into()),
        limit: 500,
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    for r in &p.retrievers {
        assert!(r.top_n <= MAX_TOP_N);
    }
}

// ---------------------------------------------------------------------------
// Per-retriever config defaults.
// ---------------------------------------------------------------------------

#[test]
fn semantic_config_uses_spec_defaults() {
    let p = plan(&req_with_text("x")).expect("plan");
    let sem = p
        .retrievers
        .iter()
        .find(|r| r.retriever == Retriever::Semantic)
        .expect("semantic present");
    match sem.config {
        RetrieverConfig::Semantic {
            ef_search,
            similarity_threshold,
            ..
        } => {
            assert_eq!(ef_search, 64);
            assert!((similarity_threshold - 0.0).abs() < 1e-6);
        }
        ref other => panic!("expected Semantic config, got {other:?}"),
    }
}

#[test]
fn lexical_config_uses_spec_defaults() {
    let p = plan(&req_with_text("x")).expect("plan");
    let lex = p
        .retrievers
        .iter()
        .find(|r| r.retriever == Retriever::Lexical)
        .expect("lexical present");
    match lex.config {
        RetrieverConfig::Lexical {
            bm25_k1, bm25_b, ..
        } => {
            assert!((bm25_k1 - 1.2).abs() < 1e-6);
            assert!((bm25_b - 0.75).abs() < 1e-6);
        }
        ref other => panic!("expected Lexical config, got {other:?}"),
    }
}

#[test]
fn graph_config_uses_spec_defaults() {
    let req = QueryRequest {
        text: Some("x".into()),
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    };
    let p = plan(&req).expect("plan");
    let gr = p
        .retrievers
        .iter()
        .find(|r| r.retriever == Retriever::Graph)
        .expect("graph present");
    match gr.config {
        RetrieverConfig::Graph {
            max_depth,
            max_branching,
            ..
        } => {
            assert_eq!(max_depth, 3);
            assert_eq!(max_branching, 200);
        }
        ref other => panic!("expected Graph config, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Cost estimate.
// ---------------------------------------------------------------------------

#[test]
fn cost_estimate_is_positive() {
    let p = plan(&req_with_text("x")).expect("plan");
    assert!(p.estimated_cost_ms > 0.0);
}

#[test]
fn cost_estimate_increases_with_retrievers() {
    let one_retriever = plan(&QueryRequest {
        text: Some("x".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic]),
        ..Default::default()
    })
    .expect("plan");
    let three_retrievers = plan(&QueryRequest {
        text: Some("x".into()),
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    })
    .expect("plan");
    assert!(
        three_retrievers.estimated_cost_ms > one_retriever.estimated_cost_ms,
        "{} vs {}",
        three_retrievers.estimated_cost_ms,
        one_retriever.estimated_cost_ms
    );
}
