# Plan: Phase 23 — Task 08, EXPLAIN + TRACE

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Implement the human-readable EXPLAIN / TRACE renderers per
§24/00 §"Plan structure". EXPLAIN takes a `QueryPlan` (23.6)
and returns a text report — no execution. TRACE takes the
plan + the `QueryMetadata` from one execution and produces a
report that includes per-retriever latencies + outcomes +
result counts + filter-chain survivor counts.

Concrete deliverables:

1. New module `crates/brain-planner/src/knowledge/explain.rs`:
   - `render_plan(&QueryPlan) -> String`.
   - `render_trace(&QueryPlan, &QueryMetadata) -> String`.
   - Output matches the §24/00 EXPLAIN example shape:
     ```
     QUERY: <one-line summary>
     PLAN:
       ROUTING: <override-kind>, features=[<list>]
       PRE_FILTERS:
         Semantic: <pre-filter or "none">
         Lexical:  <pre-filter or "none">
         Graph:    <pre-filter or "none">
       RETRIEVERS:
         SemanticRetriever(weight=<w>, top_n=<n>, ef_search=<e>,
                           threshold=<s>, timeout=<t>ms)
         LexicalRetriever(weight=<w>, top_n=<n>, bm25_k1=<k>,
                          bm25_b=<b>, timeout=<t>ms)
         GraphRetriever(weight=<w>, top_n=<n>, depth=<d>,
                        branching=<b>, direction=<d>, timeout=<t>ms)
       FUSION: RRF(k=<k>, weights={sem=<s>, lex=<l>, gr=<g>})
       POST_FILTERS: <filter chain summary or "none">
       LIMIT: <n>
       ESTIMATED COST: <c>ms
     ```
   - TRACE appends an `EXECUTION:` block with per-retriever
     latency / outcome / result-count + filter stats +
     `TOTAL LATENCY: <ms>`.
2. Format is **plain text, monospace-friendly**. Indentation
   = 2 spaces. Numbers rendered with sensible precision (3 dp
   for weights, 2 dp for scores, integer ms for latency).
3. The renderers don't take `QueryRequest` — `QueryPlan`
   carries enough (the request's text, anchor, filters are
   already encoded in `routing.features` + `post_filters`).
4. Unit tests cover:
   - EXPLAIN snapshot for the 5 routing rules' canonical
     shapes.
   - TRACE includes execution block with each retriever's
     status (Success / Skipped / Timeout / Failure).
   - Render is deterministic (no HashMap-key-order surprises).

NOT in scope:
- JSON-formatted EXPLAIN/TRACE — wire layer (23.9) will
  serialise the `QueryPlan` + `QueryMetadata` to whatever
  shape clients expect; the text format here is for human
  inspection.
- DOT / Graphviz plan visualisation — post-v1 polish.
- Cost-breakdown rendering (per-retriever cost contribution)
  — v1 surfaces only total estimated_cost_ms; per-retriever
  breakdown is post-v1.

## 2. Spec references

- `spec/24_hybrid_query/00_purpose.md` §"Plan structure" —
  the EXPLAIN example block we mirror.
- `spec/24_hybrid_query/00_purpose.md` §"Result shape" —
  defines the `QueryMetadata` fields TRACE renders.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `QueryPlan` fields | 23.6 `planner.rs` | All public; readable. |
| `QueryMetadata` fields | 23.7 `executor.rs` | Public; readable. |
| `ClassificationFeatures` | 23.3 `router.rs` | All bools; we render the set of `true` ones. |
| `FilterChain` rendering | 23.5 | Need a `format_filter_chain` helper that omits unset filters. |

## 4. Architecture sketch

```rust
// crates/brain-planner/src/knowledge/explain.rs

use std::fmt::Write;

use super::executor::{QueryMetadata, RetrieverOutcome, RetrieverStatus};
use super::filters::{FilterChain, FilterChainStats};
use super::planner::{PreFilter, QueryPlan, Retriever, RetrieverConfig};
use super::router::{ClassificationFeatures, OverrideKind, PerRetrieverWeights};

/// Render a `QueryPlan` as a human-readable EXPLAIN report.
#[must_use]
pub fn render_plan(plan: &QueryPlan) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "QUERY: <see request text in calling layer>");
    let _ = writeln!(s, "PLAN:");
    render_routing(&mut s, plan);
    render_pre_filters(&mut s, plan);
    render_retrievers(&mut s, plan);
    render_fusion(&mut s, plan);
    render_post_filters(&mut s, &plan.post_filters);
    let _ = writeln!(s, "  LIMIT: {}", plan.limit);
    let _ = writeln!(s, "  ESTIMATED COST: {:.1}ms", plan.estimated_cost_ms);
    s
}

/// Render plan + execution metadata as a TRACE report.
pub fn render_trace(plan: &QueryPlan, metadata: &QueryMetadata) -> String {
    let mut s = render_plan(plan);
    let _ = writeln!(s, "EXECUTION:");
    render_per_retriever(&mut s, metadata);
    render_filter_stats(&mut s, &metadata.filter_stats);
    let _ = writeln!(s, "  TOTAL LATENCY: {:.1}ms", metadata.total_latency_ms);
    s
}

fn render_routing(s: &mut String, plan: &QueryPlan) {
    let kind = match plan.routing.override_kind {
        OverrideKind::Auto => "auto",
        OverrideKind::Explicit => "explicit-override",
    };
    let mut features: Vec<&'static str> = Vec::new();
    let f = &plan.routing.features;
    if f.has_text { features.push("text"); }
    if f.has_entity_anchor { features.push("entity-anchor"); }
    if f.contains_exact_id { features.push("exact-id"); }
    if f.is_all_caps_tokens { features.push("all-caps"); }
    if f.is_question { features.push("question"); }
    if f.contains_entity_mention_heuristic { features.push("entity-mention"); }
    if f.contains_temporal_expression { features.push("temporal-expr"); }
    if f.has_time_filter { features.push("time-filter"); }
    if f.has_type_filter { features.push("type-filter"); }
    if f.has_predicate_filter { features.push("predicate-filter"); }
    let _ = writeln!(s, "  ROUTING: {kind}, features=[{}]", features.join(", "));
}

fn render_pre_filters(s: &mut String, plan: &QueryPlan) {
    let _ = writeln!(s, "  PRE_FILTERS:");
    for r in &plan.retrievers {
        let label = retriever_label(r.retriever);
        let pf = match &r.pre_filter {
            None => "none".to_string(),
            Some(PreFilter::Temporal(range)) => format!(
                "temporal({:?}..={:?})",
                range.from_unix_ms, range.to_unix_ms,
            ),
            Some(PreFilter::AgentId(_)) => "agent_id(...)".to_string(),
            Some(PreFilter::MemoryKind(ks)) => format!("memory_kind({:?})", ks),
            Some(PreFilter::StatementKind(ks)) => format!("statement_kind({:?})", ks),
            Some(PreFilter::PredicateId(ps)) => format!("predicate_id({:?})", ps),
        };
        let _ = writeln!(s, "    {label}: {pf}");
    }
}

fn render_retrievers(s: &mut String, plan: &QueryPlan) {
    let _ = writeln!(s, "  RETRIEVERS:");
    for r in &plan.retrievers {
        match &r.config {
            RetrieverConfig::Semantic { ef_search, similarity_threshold, timeout_ms } => {
                let _ = writeln!(
                    s,
                    "    SemanticRetriever(weight={:.3}, top_n={}, ef_search={}, threshold={:.2}, timeout={}ms)",
                    r.weight, r.top_n, ef_search, similarity_threshold, timeout_ms,
                );
            }
            RetrieverConfig::Lexical { bm25_k1, bm25_b, min_score, timeout_ms } => {
                let _ = writeln!(
                    s,
                    "    LexicalRetriever(weight={:.3}, top_n={}, bm25_k1={:.2}, bm25_b={:.2}, min_score={:?}, timeout={}ms)",
                    r.weight, r.top_n, bm25_k1, bm25_b, min_score, timeout_ms,
                );
            }
            RetrieverConfig::Graph { max_depth, max_branching, direction, include_statements, timeout_ms, .. } => {
                let _ = writeln!(
                    s,
                    "    GraphRetriever(weight={:.3}, top_n={}, depth={}, branching={}, direction={:?}, include_statements={}, timeout={}ms)",
                    r.weight, r.top_n, max_depth, max_branching, direction, include_statements, timeout_ms,
                );
            }
        }
    }
}

fn render_fusion(s: &mut String, plan: &QueryPlan) {
    let w = &plan.fusion.weights;
    let _ = writeln!(
        s,
        "  FUSION: RRF(k={}, weights={{sem={:.2}, lex={:.2}, gr={:.2}}})",
        plan.fusion.k, w.semantic, w.lexical, w.graph,
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
    if !chain.include_tombstoned { parts.push("!tombstoned".into()); }
    if !chain.include_superseded { parts.push("!superseded".into()); }
    let summary = if parts.is_empty() { "none".to_string() } else { parts.join(", ") };
    let _ = writeln!(s, "  POST_FILTERS: {summary}");
}

fn render_per_retriever(s: &mut String, metadata: &QueryMetadata) {
    for (i, outcome) in metadata.retriever_outcomes.iter().enumerate() {
        let ms = metadata.retriever_latencies_ms
            .iter()
            .find(|(r, _)| *r == outcome.retriever)
            .map(|(_, ms)| *ms)
            .unwrap_or(0.0);
        let count = metadata.retriever_total_results
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
        let _ = i; // index unused; loop iterates outcomes
        let _ = writeln!(
            s,
            "    {} latency={:.1}ms results={} status={status}",
            retriever_label(outcome.retriever),
            ms,
            count,
        );
    }
}

fn render_filter_stats(s: &mut String, stats: &FilterChainStats) {
    let _ = writeln!(
        s,
        "    Filter chain: {} → type {} → temporal {} → confidence {} → tombstone {} → supersession {} → limit {}",
        stats.before, stats.after_type, stats.after_temporal,
        stats.after_confidence, stats.after_tombstone,
        stats.after_supersession, stats.after_limit,
    );
}

fn retriever_label(r: Retriever) -> &'static str {
    match r {
        Retriever::Semantic => "Semantic",
        Retriever::Lexical => "Lexical",
        Retriever::Graph => "Graph",
    }
}
```

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Plain text formatter (this plan) | Easy to read; matches §24/00 example | Not machine-parseable | ✓ — wire serialiser owns JSON form |
| JSON struct + serde Serialize | Machine-friendly | Requires a serde derive on every type in the plan/metadata DAG; v1 wire layer (23.9) will build a wire-shaped struct from these | rejected for v1 — separation of concerns |
| `Display` impl on `QueryPlan` | Idiomatic | Forces a single format; v1 wants `render_plan` as a free fn that's easy to extend (e.g. for verbose mode) | rejected |
| Verbose vs terse modes | More flexibility | YAGNI for v1 | rejected — single format |
| Include `QueryRequest.text` in EXPLAIN | More informative | Couples renderer to request; the wire layer can prepend it | rejected — keep renderer pure-plan |

## 6. Risks / open questions

- **Risk:** Floating-point formatting drift (different locales? rust's default always uses `.`). **Mitigation:** `:.2` / `:.3` formatters are locale-independent in Rust.
- **Risk:** `Vec<StatementKind>` and similar debug-printed via `{:?}` may include the variant name format (`Fact`, `Preference`). **Mitigation:** that's the readable form; if operators want a specific format we can add one later.
- **Open question:** Should EXPLAIN include `QUERY: <text>`? **Resolution:** v1 emits the placeholder `<see request text in calling layer>`. The wire handler prepends the actual text when assembling the response. Keeps the renderer pure (no QueryRequest dep).
- **Open question:** Renderer return type — `String` (this plan) vs `impl Display`? **Resolution:** `String` for simplicity; wire layer wraps in JSON or sends as text frame. Future polish can add a `Display` newtype.

## 7. Test plan

Unit tests in `crates/brain-planner/src/knowledge/explain/tests.rs`:

- `render_plan_includes_routing_features` — entity-anchored
  plan → output contains `entity-anchor` token.
- `render_plan_includes_retrievers_section` — output contains
  `Semantic`, `Lexical`, `Graph` lines as appropriate.
- `render_plan_fusion_line` — output contains `RRF(k=60,
  weights={sem=1.00, lex=1.00, gr=2.00})` for the
  entity-anchored case.
- `render_plan_post_filters_none_when_empty` — empty
  FilterChain → `POST_FILTERS: none`.
- `render_plan_post_filters_summarises_active` — chain with
  `confidence_min=0.5` + `!tombstoned` → output contains
  both tokens.
- `render_plan_limit_and_cost` — `LIMIT: 20` + `ESTIMATED
  COST: <some-positive>ms` lines present.
- `render_trace_appends_execution_block` — TRACE output ⊇
  EXPLAIN output + contains `EXECUTION:` token.
- `render_trace_per_retriever_status_lines` — for each
  RetrieverStatus variant (Success / Skipped / Timeout /
  Failure), the line carries the right status token.
- `render_trace_filter_stats_arrow_format` — line matches
  `<before> → type <n> → temporal <n> → ...`.
- `render_is_deterministic` — same input → same output across
  two calls (sanity guard against HashMap key iteration).

## 8. Commit shape

Single commit:

```
feat(planner): 23.8 — EXPLAIN + TRACE renderers

- crates/brain-planner/src/knowledge/explain.rs (new):
  render_plan(&QueryPlan) -> String produces a §24/00
  §"Plan structure"-shaped text report (ROUTING / PRE_FILTERS /
  RETRIEVERS / FUSION / POST_FILTERS / LIMIT / ESTIMATED COST).
  render_trace(&QueryPlan, &QueryMetadata) -> String appends an
  EXECUTION: block (per-retriever latency + outcome + result
  count + filter-chain survivor counts + total latency).
- crates/brain-planner/src/knowledge/explain/tests.rs (new):
  ~10 unit tests over each section + status variants +
  determinism.
- crates/brain-planner/src/knowledge/mod.rs: pub mod explain.
```

## 9. Confirmation

Please confirm:

1. **Plain-text renderer** (vs JSON) — wire serialiser (23.9) owns JSON.
2. **EXPLAIN doesn't include `QueryRequest.text`** — calling layer prepends. Keeps renderer pure-plan.
3. **TRACE = EXPLAIN + EXECUTION block** — appended, not interleaved.
4. **Single format, no verbose mode** — YAGNI for v1.
5. **Per-retriever lines per status variant** — Success / Skipped(reason) / Timeout / Failure(msg).

After approval: implement + tests + commit.
