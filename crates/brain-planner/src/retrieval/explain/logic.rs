//! EXPLAIN + TRACE renderers.
//!
//! The plan-structure diagnostic format.
//!
//! - [`render_plan`] takes a `QueryPlan` and returns a
//!   human-readable text report — no execution.
//! - [`render_trace`] takes the plan + a `QueryMetadata`
//!   and appends an EXECUTION block with per-retriever
//!   latency / status / result count, filter-chain survivor
//!   counts, and total wall-time.
//!
//! Format is plain text, monospace-friendly, designed for
//! `tracing`-style logs or wire-text frames. JSON serialisation
//! lives in the wire layer.

use std::fmt::Write;

use crate::retrieval::executor::{QueryMetadata, RetrieverStatus};
use crate::retrieval::filters::{FilterChain, FilterChainStats};
use crate::retrieval::fusion::FusionMethod;
use crate::retrieval::planner::{PreFilter, QueryPlan, RetrieverConfig};
use crate::retrieval::router::{OverrideKind, Retriever};

/// Render a `QueryPlan` as a human-readable EXPLAIN report.
///
/// EXPLAIN does not execute, so for RRF the effective smoothing constant
/// (derived adaptively from the candidate-pool size at execution) is not
/// yet known; the fusion line reports `k=adaptive`. TRACE, which executes,
/// renders the concrete effective k via [`render_trace`].
#[must_use]
pub fn render_plan(plan: &QueryPlan) -> String {
    render_plan_inner(plan, None)
}

/// Shared plan renderer. `effective_k` is the RRF constant execution
/// actually used (from [`QueryMetadata::effective_fusion_k`]); `None`
/// means the plan-only EXPLAIN path where no execution has happened.
fn render_plan_inner(plan: &QueryPlan, effective_k: Option<u32>) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "QUERY: <see request text in calling layer>");
    let _ = writeln!(s, "PLAN:");
    render_routing(&mut s, plan);
    render_pre_filters(&mut s, plan);
    render_retrievers(&mut s, plan);
    render_fusion(&mut s, plan, effective_k);
    render_post_filters(&mut s, &plan.post_filters);
    let _ = writeln!(s, "  LIMIT: {}", plan.limit);
    let _ = writeln!(s, "  ESTIMATED COST: {:.1}ms", plan.estimated_cost_ms);
    s
}

/// Render plan + execution metadata as a TRACE report.
///
/// The fusion line reports the effective k the engine fused at
/// ([`QueryMetadata::effective_fusion_k`]), so EXPLAIN/TRACE and execution
/// agree rather than always printing the plan's nominal k.
#[must_use]
pub fn render_trace(plan: &QueryPlan, metadata: &QueryMetadata) -> String {
    let mut s = render_plan_inner(plan, Some(metadata.effective_fusion_k));
    let _ = writeln!(s, "EXECUTION:");
    render_per_retriever(&mut s, metadata);
    render_filter_stats(&mut s, &metadata.filter_stats);
    let _ = writeln!(s, "  TOTAL LATENCY: {:.1}ms", metadata.total_latency_ms);
    s
}

// ---------------------------------------------------------------------------
// Section renderers.
// ---------------------------------------------------------------------------

fn render_routing(s: &mut String, plan: &QueryPlan) {
    let kind = match plan.routing.override_kind {
        OverrideKind::Auto => "auto",
        OverrideKind::Explicit => "explicit-override",
    };
    let mut features: Vec<&'static str> = Vec::new();
    let f = &plan.routing.features;
    if f.has_text {
        features.push("text");
    }
    if f.has_entity_anchor {
        features.push("entity-anchor");
    }
    if f.contains_exact_id {
        features.push("exact-id");
    }
    if f.is_all_caps_tokens {
        features.push("all-caps");
    }
    if f.is_question {
        features.push("question");
    }
    if f.contains_entity_mention_heuristic {
        features.push("entity-mention");
    }
    if f.contains_temporal_expression {
        features.push("temporal-expr");
    }
    if f.has_time_filter {
        features.push("time-filter");
    }
    if f.has_type_filter {
        features.push("type-filter");
    }
    if f.has_predicate_filter {
        features.push("predicate-filter");
    }
    let _ = writeln!(s, "  ROUTING: {kind}, features=[{}]", features.join(", "));
}

fn render_pre_filters(s: &mut String, plan: &QueryPlan) {
    let _ = writeln!(s, "  PRE_FILTERS:");
    for r in &plan.retrievers {
        let label = retriever_label(r.retriever);
        let pf = match &r.pre_filter {
            None => "none".to_string(),
            Some(PreFilter::Temporal(range)) => {
                format!(
                    "temporal({:?}..={:?})",
                    range.from_unix_ms, range.to_unix_ms,
                )
            }
            Some(PreFilter::SpaceIds(ids)) => format!("space_ids({} items)", ids.len()),
            Some(PreFilter::MemoryKind(ks)) => format!("memory_kind({ks:?})"),
            Some(PreFilter::StatementKind(ks)) => format!("statement_kind({ks:?})"),
            Some(PreFilter::PredicateId(ps)) => format!("predicate_id({ps:?})"),
        };
        let _ = writeln!(s, "    {label}: {pf}");
    }
}

fn render_retrievers(s: &mut String, plan: &QueryPlan) {
    let _ = writeln!(s, "  RETRIEVERS:");
    for r in &plan.retrievers {
        match &r.config {
            RetrieverConfig::Semantic {
                ef_search,
                similarity_threshold,
                timeout_ms,
            } => {
                let _ = writeln!(
                    s,
                    "    SemanticRetriever(weight={:.3}, top_n={}, ef_search={}, threshold={:.2}, timeout={}ms)",
                    r.weight, r.top_n, ef_search, similarity_threshold, timeout_ms,
                );
            }
            RetrieverConfig::Lexical {
                bm25_k1,
                bm25_b,
                min_score,
                timeout_ms,
            } => {
                let _ = writeln!(
                    s,
                    "    LexicalRetriever(weight={:.3}, top_n={}, bm25_k1={:.2}, bm25_b={:.2}, min_score={:?}, timeout={}ms)",
                    r.weight, r.top_n, bm25_k1, bm25_b, min_score, timeout_ms,
                );
            }
            RetrieverConfig::Graph {
                max_depth,
                max_branching,
                direction,
                include_statements,
                timeout_ms,
                ..
            } => {
                let _ = writeln!(
                    s,
                    "    GraphRetriever(weight={:.3}, top_n={}, depth={}, branching={}, direction={:?}, include_statements={}, timeout={}ms)",
                    r.weight,
                    r.top_n,
                    max_depth,
                    max_branching,
                    direction,
                    include_statements,
                    timeout_ms,
                );
            }
        }
    }
}

fn render_fusion(s: &mut String, plan: &QueryPlan, effective_k: Option<u32>) {
    let w = &plan.fusion.weights;
    // Report the k execution actually fused at. When known (TRACE), that is
    // the recorded effective k. When unknown (plan-only EXPLAIN): RRF picks
    // k adaptively from the pool size at run time, so no static value is
    // truthful — print `adaptive`; non-RRF methods do use `plan.fusion.k`.
    let k_display = match effective_k {
        Some(k) => k.to_string(),
        None => {
            if plan.fusion.method == FusionMethod::Rrf {
                "adaptive".to_string()
            } else {
                plan.fusion.k.to_string()
            }
        }
    };
    let _ = writeln!(
        s,
        "  FUSION: RRF(k={k_display}, weights={{sem={:.2}, lex={:.2}, gr={:.2}}})",
        w.semantic, w.lexical, w.graph,
    );
}

fn render_post_filters(s: &mut String, chain: &FilterChain) {
    let mut parts: Vec<String> = Vec::new();
    if !chain.kind_filter.is_empty() {
        parts.push(format!("kind in {:?}", chain.kind_filter));
    }
    if !chain.memory_kind_filter.is_empty() {
        parts.push(format!("memory_kind in {:?}", chain.memory_kind_filter));
    }
    if !chain.predicate_filter.is_empty() {
        parts.push(format!("predicate in {:?}", chain.predicate_filter));
    }
    if let Some(t) = chain.time_filter {
        parts.push(format!("time={:?}..={:?}", t.from_unix_ms, t.to_unix_ms));
    }
    if let Some(c) = chain.confidence_min {
        parts.push(format!("confidence >= {c:.2}"));
    }
    // !tombstoned / !superseded are implicit defaults — only
    // surface them when the request also carries an explicit
    // filter, so a no-filter EXPLAIN reads as `none` instead of
    // listing the always-on guards.
    let has_explicit_filter = !parts.is_empty();
    if has_explicit_filter && !chain.include_tombstoned {
        parts.push("!tombstoned".into());
    }
    if has_explicit_filter && !chain.include_superseded {
        parts.push("!superseded".into());
    }
    let summary = if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(", ")
    };
    let _ = writeln!(s, "  POST_FILTERS: {summary}");
}

fn render_per_retriever(s: &mut String, metadata: &QueryMetadata) {
    for outcome in &metadata.retriever_outcomes {
        let ms = metadata
            .retriever_latencies_ms
            .iter()
            .find(|(r, _)| *r == outcome.retriever)
            .map(|(_, ms)| *ms)
            .unwrap_or(0.0);
        let count = metadata
            .retriever_total_results
            .iter()
            .find(|(r, _)| *r == outcome.retriever)
            .map(|(_, c)| *c)
            .unwrap_or(0);
        let status = match &outcome.status {
            RetrieverStatus::Success => "ok".to_string(),
            RetrieverStatus::Skipped(reason) => format!("skipped({reason})"),
            RetrieverStatus::Timeout => "timeout".to_string(),
            RetrieverStatus::Failure(msg) => format!("failed({msg})"),
        };
        let _ = writeln!(
            s,
            "    {} latency={ms:.1}ms results={count} status={status}",
            retriever_label(outcome.retriever),
        );
    }
}

fn render_filter_stats(s: &mut String, stats: &FilterChainStats) {
    let _ = writeln!(
        s,
        "    Filter chain: {} → type {} → temporal {} → confidence {} → tombstone {} → supersession {} → limit {}",
        stats.before,
        stats.after_type,
        stats.after_temporal,
        stats.after_confidence,
        stats.after_tombstone,
        stats.after_supersession,
        stats.after_limit,
    );
}

fn retriever_label(r: Retriever) -> &'static str {
    match r {
        Retriever::Semantic => "Semantic",
        Retriever::Lexical => "Lexical",
        Retriever::Graph => "Graph",
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
