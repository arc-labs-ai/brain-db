//! Unit tests for the EXPLAIN + TRACE renderers (phase 23.8).

use brain_core::knowledge::StatementKind;
use brain_core::EntityId;

use super::{render_plan, render_trace};
use crate::knowledge::executor::{QueryMetadata, RetrieverOutcome, RetrieverStatus};
use crate::knowledge::filters::FilterChainStats;
use crate::knowledge::planner::plan;
use crate::knowledge::router::{QueryRequest, Retriever, RetrieverSelection, TimeRange};

fn req_with_text(text: &str) -> QueryRequest {
    QueryRequest {
        text: Some(text.into()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// EXPLAIN section coverage.
// ---------------------------------------------------------------------------

#[test]
fn render_plan_includes_routing_section() {
    let req = QueryRequest {
        text: Some("budget".into()),
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let out = render_plan(&qp);
    assert!(out.contains("ROUTING:"), "missing ROUTING line: {out}");
    assert!(out.contains("entity-anchor"));
    assert!(out.contains("text"));
}

#[test]
fn render_plan_includes_each_retriever_label() {
    let req = QueryRequest {
        text: Some("topic".into()),
        entity_anchor: Some(EntityId::new()),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let out = render_plan(&qp);
    assert!(out.contains("SemanticRetriever"));
    assert!(out.contains("LexicalRetriever"));
    assert!(out.contains("GraphRetriever"));
}

#[test]
fn render_plan_fusion_line_format() {
    let qp = plan(&req_with_text("budget")).expect("plan");
    let out = render_plan(&qp);
    assert!(out.contains("FUSION: RRF(k=60"), "got {out}");
    assert!(out.contains("weights={sem="));
}

#[test]
fn render_plan_post_filters_none_when_empty() {
    let qp = plan(&req_with_text("topic")).expect("plan");
    let out = render_plan(&qp);
    assert!(out.contains("POST_FILTERS: none"), "{out}");
}

#[test]
fn render_plan_post_filters_summarises_active() {
    let req = QueryRequest {
        text: Some("topic".into()),
        kind_filter: vec![StatementKind::Fact],
        confidence_min: Some(0.5),
        include_tombstoned: false,
        include_superseded: false,
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let out = render_plan(&qp);
    assert!(out.contains("kind in"));
    assert!(out.contains("confidence >= 0.50"));
    assert!(out.contains("!tombstoned"));
    assert!(out.contains("!superseded"));
}

#[test]
fn render_plan_limit_and_cost_lines() {
    let req = QueryRequest {
        text: Some("topic".into()),
        limit: 42,
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let out = render_plan(&qp);
    assert!(out.contains("LIMIT: 42"));
    assert!(out.contains("ESTIMATED COST:"));
    assert!(out.contains("ms"));
}

#[test]
fn render_plan_pre_filter_temporal_when_pushed_down() {
    let req = QueryRequest {
        text: Some("topic".into()),
        time_filter: Some(TimeRange {
            from_unix_ms: Some(100),
            to_unix_ms: Some(900),
        }),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let out = render_plan(&qp);
    assert!(
        out.contains("temporal("),
        "expected temporal pre-filter line; got\n{out}"
    );
}

#[test]
fn render_plan_explicit_override_label() {
    let req = QueryRequest {
        text: Some("topic".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic]),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let out = render_plan(&qp);
    assert!(out.contains("explicit-override"), "{out}");
}

// ---------------------------------------------------------------------------
// TRACE.
// ---------------------------------------------------------------------------

fn sample_metadata() -> QueryMetadata {
    QueryMetadata {
        retriever_latencies_ms: vec![(Retriever::Semantic, 7.5), (Retriever::Lexical, 12.0)],
        retriever_outcomes: vec![
            RetrieverOutcome {
                retriever: Retriever::Semantic,
                status: RetrieverStatus::Success,
            },
            RetrieverOutcome {
                retriever: Retriever::Lexical,
                status: RetrieverStatus::Skipped("no query text"),
            },
        ],
        retriever_total_results: vec![(Retriever::Semantic, 10), (Retriever::Lexical, 0)],
        filter_stats: FilterChainStats {
            before: 10,
            after_type: 8,
            after_temporal: 7,
            after_confidence: 6,
            after_tombstone: 6,
            after_supersession: 5,
            after_limit: 3,
        },
        total_latency_ms: 22.4,
    }
}

#[test]
fn render_trace_appends_execution_block() {
    let qp = plan(&req_with_text("topic")).expect("plan");
    let trace = render_trace(&qp, &sample_metadata());
    let plan_only = render_plan(&qp);
    assert!(trace.starts_with(&plan_only));
    assert!(trace.contains("EXECUTION:"));
}

#[test]
fn render_trace_per_retriever_status_lines() {
    let qp = plan(&req_with_text("topic")).expect("plan");
    let trace = render_trace(&qp, &sample_metadata());
    assert!(trace.contains("Semantic latency=7.5ms results=10 status=ok"));
    assert!(trace.contains("Lexical latency=12.0ms results=0 status=skipped(no query text)"));
}

#[test]
fn render_trace_includes_failure_and_timeout_status() {
    let mut meta = sample_metadata();
    meta.retriever_outcomes[0].status = RetrieverStatus::Timeout;
    meta.retriever_outcomes[1].status = RetrieverStatus::Failure("io".into());
    let qp = plan(&req_with_text("topic")).expect("plan");
    let trace = render_trace(&qp, &meta);
    assert!(trace.contains("status=timeout"));
    assert!(trace.contains("status=failed(io)"));
}

#[test]
fn render_trace_filter_stats_arrow_format() {
    let qp = plan(&req_with_text("topic")).expect("plan");
    let trace = render_trace(&qp, &sample_metadata());
    assert!(
        trace.contains("Filter chain: 10 → type 8 → temporal 7 → confidence 6 → tombstone 6 → supersession 5 → limit 3"),
        "trace did not contain expected arrow line:\n{trace}",
    );
}

#[test]
fn render_trace_total_latency_line() {
    let qp = plan(&req_with_text("topic")).expect("plan");
    let trace = render_trace(&qp, &sample_metadata());
    assert!(trace.contains("TOTAL LATENCY: 22.4ms"));
}

// ---------------------------------------------------------------------------
// Determinism.
// ---------------------------------------------------------------------------

#[test]
fn render_is_deterministic() {
    let qp = plan(&req_with_text("topic")).expect("plan");
    let a = render_plan(&qp);
    let b = render_plan(&qp);
    assert_eq!(a, b);
    let meta = sample_metadata();
    let ta = render_trace(&qp, &meta);
    let tb = render_trace(&qp, &meta);
    assert_eq!(ta, tb);
}
