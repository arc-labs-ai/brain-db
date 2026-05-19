//! Unit tests for the rule-based query router (phase 23.3).

use brain_core::knowledge::StatementKind;
use brain_core::EntityId;

use super::{
    route, GraphAnchorMode, OverrideKind, QueryRequest, Retriever, RetrieverSelection, TimeRange,
    MAX_RETRIEVERS,
};

fn req_with_text(text: &str) -> QueryRequest {
    QueryRequest {
        text: Some(text.into()),
        ..Default::default()
    }
}

fn retriever_weight(decision: &super::RoutingDecision, r: Retriever) -> Option<f32> {
    decision
        .retrievers
        .iter()
        .find(|inv| inv.retriever == r)
        .map(|inv| inv.weight)
}

// ---------------------------------------------------------------------------
// Rule 1 — entity-anchored.
// ---------------------------------------------------------------------------

#[test]
fn rule_1_entity_anchor_selects_graph_and_semantic() {
    let req = QueryRequest {
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    };
    let d = route(&req);
    assert_eq!(retriever_weight(&d, Retriever::Graph), Some(2.0));
    assert_eq!(retriever_weight(&d, Retriever::Semantic), Some(1.0));
    assert_eq!(retriever_weight(&d, Retriever::Lexical), None);
    assert_eq!(d.override_kind, OverrideKind::Auto);
}

#[test]
fn rule_1_with_text_also_adds_lexical() {
    let req = QueryRequest {
        text: Some("meeting notes".into()),
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    };
    let d = route(&req);
    assert_eq!(retriever_weight(&d, Retriever::Graph), Some(2.0));
    assert_eq!(retriever_weight(&d, Retriever::Semantic), Some(1.0));
    assert_eq!(retriever_weight(&d, Retriever::Lexical), Some(0.5));
}

// ---------------------------------------------------------------------------
// Rule 2 — exact-term.
// ---------------------------------------------------------------------------

#[test]
fn rule_2_exact_id_promotes_lexical() {
    let d = route(&req_with_text("ticket ACME-1247 broke prod"));
    assert_eq!(retriever_weight(&d, Retriever::Lexical), Some(2.0));
    assert_eq!(retriever_weight(&d, Retriever::Semantic), Some(0.5));
    assert!(d.features.contains_exact_id);
}

#[test]
fn rule_2_all_caps_tokens_promotes_lexical() {
    let d = route(&req_with_text("ACME XYZ"));
    assert!(d.features.is_all_caps_tokens);
    assert_eq!(retriever_weight(&d, Retriever::Lexical), Some(2.0));
}

#[test]
fn lowercase_text_doesnt_trigger_all_caps_path() {
    let d = route(&req_with_text("acme xyz"));
    assert!(!d.features.is_all_caps_tokens);
}

// ---------------------------------------------------------------------------
// Rule 5 — default free-text.
// ---------------------------------------------------------------------------

#[test]
fn rule_5_default_free_text_picks_semantic_lexical_and_graph() {
    let d = route(&req_with_text("budget pushback"));
    assert_eq!(retriever_weight(&d, Retriever::Semantic), Some(1.0));
    assert_eq!(retriever_weight(&d, Retriever::Lexical), Some(1.0));
    // Graph rides along anchored at the semantic top-K — keeps
    // substrate edges contributing on schemaless deployments.
    assert_eq!(retriever_weight(&d, Retriever::Graph), Some(0.5));
    assert_eq!(
        d.graph_anchor_mode,
        Some(GraphAnchorMode::MemoryFromSemantic)
    );
}

#[test]
fn entity_anchor_picks_entity_graph_mode() {
    let req = QueryRequest {
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    };
    let d = route(&req);
    assert_eq!(d.graph_anchor_mode, Some(GraphAnchorMode::Entity));
}

#[test]
fn no_text_no_anchor_leaves_graph_anchor_unset() {
    // The empty-text path can't select graph (no retrievers
    // matched any rule); the router should report None.
    let req = QueryRequest::default();
    let d = route(&req);
    assert_eq!(d.graph_anchor_mode, None);
}

// ---------------------------------------------------------------------------
// Rule 3 + 4 — filter signals.
// ---------------------------------------------------------------------------

#[test]
fn rule_3_time_filter_sets_temporal_pushdown() {
    let req = QueryRequest {
        text: Some("budget".into()),
        time_filter: Some(TimeRange {
            from_unix_ms: Some(0),
            to_unix_ms: Some(1_000),
        }),
        ..Default::default()
    };
    let d = route(&req);
    assert!(d.temporal_pushdown);
    assert!(d.features.has_time_filter);
}

#[test]
fn rule_3_temporal_expression_in_text_also_sets_pushdown() {
    let d = route(&req_with_text("meeting last week"));
    assert!(d.features.contains_temporal_expression);
    assert!(d.temporal_pushdown);
}

#[test]
fn rule_4_type_filter_only_returns_empty_retrievers() {
    let req = QueryRequest {
        kind_filter: vec![StatementKind::Fact],
        ..Default::default()
    };
    let d = route(&req);
    assert!(d.retrievers.is_empty());
    assert!(d.features.has_type_filter);
}

// ---------------------------------------------------------------------------
// Union semantics + 3-retriever cap.
// ---------------------------------------------------------------------------

#[test]
fn union_takes_max_weight_per_retriever() {
    // Anchor → Rule 1: Graph=2.0, Semantic=1.0, Lexical=0.5.
    // ACME-1247 → Rule 2: Lexical=2.0, Semantic=0.5.
    // Max-weight union should yield: Graph=2.0, Semantic=1.0 (Rule 1 wins), Lexical=2.0.
    let req = QueryRequest {
        text: Some("see ACME-1247 in detail".into()),
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    };
    let d = route(&req);
    assert_eq!(retriever_weight(&d, Retriever::Graph), Some(2.0));
    assert_eq!(retriever_weight(&d, Retriever::Semantic), Some(1.0));
    assert_eq!(retriever_weight(&d, Retriever::Lexical), Some(2.0));
}

#[test]
fn at_most_three_retrievers() {
    // Trigger Rule 1 (3 retrievers) + Rule 2 (2 retrievers).
    let req = QueryRequest {
        text: Some("Alice and Bob on ACME-1247".into()),
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    };
    let d = route(&req);
    assert!(d.retrievers.len() <= MAX_RETRIEVERS);
}

// ---------------------------------------------------------------------------
// Explicit override.
// ---------------------------------------------------------------------------

#[test]
fn explicit_override_skips_rules() {
    let req = QueryRequest {
        text: Some("ACME-1247".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic, Retriever::Graph]),
        ..Default::default()
    };
    let d = route(&req);
    assert_eq!(d.override_kind, OverrideKind::Explicit);
    assert_eq!(d.retrievers.len(), 2);
    for inv in &d.retrievers {
        assert!((inv.weight - 1.0).abs() < 1e-6);
        assert!(matches!(
            inv.retriever,
            Retriever::Semantic | Retriever::Graph
        ));
    }
}

#[test]
fn explicit_override_dedupes_duplicates() {
    let req = QueryRequest {
        text: None,
        retrievers: RetrieverSelection::Explicit(vec![
            Retriever::Semantic,
            Retriever::Semantic,
            Retriever::Lexical,
        ]),
        ..Default::default()
    };
    let d = route(&req);
    assert_eq!(d.retrievers.len(), 2);
}

#[test]
fn explicit_override_caps_at_max_retrievers() {
    // Spec §24/00 §"Limits and budgets": cap is uniform — the
    // explicit override does NOT bypass MAX_RETRIEVERS. A raw
    // wire caller submitting 5 distinct retrievers (only 3 exist
    // but a future variant could push past the cap; simulate by
    // duplicating the unique ones) must see the result truncated
    // at MAX_RETRIEVERS.
    let req = QueryRequest {
        text: None,
        retrievers: RetrieverSelection::Explicit(vec![
            Retriever::Semantic,
            Retriever::Lexical,
            Retriever::Graph,
            Retriever::Semantic, // dedup'd — doesn't count toward the cap.
            Retriever::Lexical,  // dedup'd.
        ]),
        ..Default::default()
    };
    let d = route(&req);
    assert_eq!(
        d.retrievers.len(),
        super::MAX_RETRIEVERS,
        "explicit override must be capped at MAX_RETRIEVERS post-dedup",
    );
}

#[test]
fn explicit_override_preserves_caller_order_on_dedup() {
    // Lexical first, then Semantic — caller order should survive
    // the dedup pass (it's a `Vec`-based contains-check, not a
    // HashSet).
    let req = QueryRequest {
        text: None,
        retrievers: RetrieverSelection::Explicit(vec![
            Retriever::Lexical,
            Retriever::Semantic,
            Retriever::Lexical,
        ]),
        ..Default::default()
    };
    let d = route(&req);
    let order: Vec<_> = d.retrievers.iter().map(|i| i.retriever).collect();
    assert_eq!(order, vec![Retriever::Lexical, Retriever::Semantic]);
}

// ---------------------------------------------------------------------------
// Empty / edge cases.
// ---------------------------------------------------------------------------

#[test]
fn empty_request_returns_empty_decision() {
    let d = route(&QueryRequest::default());
    assert!(d.retrievers.is_empty());
    assert!(!d.features.has_text);
    assert!(!d.features.has_entity_anchor);
}

#[test]
fn question_text_is_detected() {
    let d = route(&req_with_text("who is Priya?"));
    assert!(d.features.is_question);
}

#[test]
fn title_case_triggers_entity_mention_heuristic() {
    let d = route(&req_with_text("Alice met Bob in Paris"));
    assert!(d.features.contains_entity_mention_heuristic);
    // Without an explicit anchor, Title-Case alone triggers Rule 1.
    assert_eq!(retriever_weight(&d, Retriever::Graph), Some(2.0));
}

#[test]
fn iso_date_triggers_temporal_expression() {
    let d = route(&req_with_text("budget on 2024-03-15"));
    assert!(d.features.contains_temporal_expression);
    assert!(d.temporal_pushdown);
}

#[test]
fn last_n_days_triggers_temporal_expression() {
    let d = route(&req_with_text("meetings in the last 7 days"));
    assert!(d.features.contains_temporal_expression);
}

// ---------------------------------------------------------------------------
// E1 — Empty cue text + entity anchor only.
//
// "Text + anchor" picks all three retrievers; "anchor only" must
// drop lexical entirely because there's no text signal to lex on,
// and skip semantic's text-dependent weight too.
// ---------------------------------------------------------------------------

#[test]
fn entity_anchor_without_text_selects_graph_and_semantic_only() {
    let req = QueryRequest {
        text: None,
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    };
    let d = route(&req);

    assert_eq!(
        retriever_weight(&d, Retriever::Graph),
        Some(2.0),
        "graph weight unchanged when text is absent",
    );
    assert_eq!(
        retriever_weight(&d, Retriever::Semantic),
        Some(1.0),
        "semantic still rides along on the anchor",
    );
    assert_eq!(
        retriever_weight(&d, Retriever::Lexical),
        None,
        "lexical requires text — must not be selected on anchor-only",
    );
    assert_eq!(
        d.graph_anchor_mode,
        Some(GraphAnchorMode::Entity),
        "anchor present → entity graph mode (no semantic anchoring)",
    );
}
