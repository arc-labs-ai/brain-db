# Plan: Phase 23 — Task 06, Query planner

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Implement the query planner per §24/00 §"Plan structure".
Takes a `QueryRequest` (from 23.3), produces a `QueryPlan` —
a self-contained, immutable description of what the executor
(23.7) will run: which retrievers, with what pre-filters,
what fusion, what post-filters, what limit, and a cost
estimate. EXPLAIN (23.8) consumes the plan without
executing.

Concrete deliverables:

1. New module `crates/brain-planner/src/knowledge/planner.rs`:
   - `QueryPlan` struct matching §24/00 §"Plan structure".
   - `RetrieverInvocation` (planner-internal, more detail
     than the router's by the same name — adds
     `top_n`, `pre_filter`, `config`).
   - `FusionStep { k: u32, weights: PerRetrieverWeights }`.
   - `PreFilter` enum (`AgentId(...)`, `MemoryKind(...)`,
     `StatementKind(...)`, `PredicateId(u32)`,
     `TimeRange(TimeRange)`).
   - `PostFilter` (alias for the 23.5 `FilterChain`).
   - `PlanError` taxonomy.
   - `plan(request: &QueryRequest) -> Result<QueryPlan, PlanError>`.
2. Per-retriever config produced from the routing decision:
   - Semantic: `top_n = 100` default (configurable),
     `ef_search = 64`, `similarity_threshold = 0.0`,
     `timeout_ms = 50`.
   - Lexical: `top_n = 100`, `bm25_k1 = 1.2`, `bm25_b = 0.75`,
     `min_score = None`, `timeout_ms = 50`.
   - Graph: `top_n = 64`, `max_depth = 3` for Star /
     `max_depth = 5` cap, `max_branching = 200`, `timeout_ms = 50`.
3. **Push-down decisions** (§24/00 §"Filter as retriever vs
   filter"):
   - **Temporal** push-down if `router.temporal_pushdown ==
     true`. Builds a `PreFilter::TimeRange(range)` attached to
     each retriever invocation. Stays in `post_filters` if
     not pushed (planner duplicates it post-fusion).
   - **Type / predicate** push-down: semantic + graph
     retrievers push them at the retriever level
     (`PreFilter::PredicateId / StatementKind`). Lexical
     retriever already handles them via its `LexicalFilters`
     (22.5); planner mirrors the config.
   - Push-down filters are **also** retained in
     `post_filters` per the §24/00 documented order — the
     filter chain (23.5) re-checks them so the final
     contract is unambiguous. (Idempotent: push-down hits
     the same rows; double-checking adds nanoseconds per
     candidate.)
4. **Fusion step**:
   - `k = req.fusion_config.k.unwrap_or(DEFAULT_K)`.
   - `weights = req.fusion_config.weights.unwrap_or_default()`,
     then OVERLAY router-derived weights on top (router
     gives Graph 2.0 / Semantic 1.0 / Lexical 0.5 for the
     entity-anchored rule etc.). Per-query `weights` win when
     non-default.
5. **Cost estimate** — coarse linear sum:
   - Semantic: 5 ms base + ef_search-related slope.
   - Lexical: 10 ms base + (top_n / 100) × 5 ms.
   - Graph: depth² × 5 ms (rough O(b^d) bound).
   - Fusion: 0.1 ms × N_retrievers.
   - Filter chain: 1 ms × top_k.
   The total is the planner's `estimated_cost: f32` (ms).
6. **Limit propagation**:
   - `req.limit` is the final post-filter limit.
   - Per-retriever `top_n` defaults to `max(req.limit × 3,
     100)` so retrievers fetch enough candidates for fusion
     to be meaningful even when one retriever's top-k overlaps
     poorly with another's.
7. Unit tests across all 5 routing rules + explicit override +
   k / weight precedence + push-down / no-push-down + cost
   estimate sanity bounds.

NOT in scope:
- Actually invoking retrievers — 23.7 owns execution.
- EXPLAIN/TRACE rendering — 23.8.
- Streaming plan execution — post-v1.
- Adaptive cost-based plan rewrite — post-v1.

## 2. Spec references

- `spec/24_hybrid_query/00_purpose.md` §"Plan structure" — the
  binding shape.
- `spec/24_hybrid_query/00_purpose.md` §"Filter as retriever
  vs filter" — push-down vs post-fusion criterion.
- `spec/23_retrievers/01_rrf_fusion.md` §"Choice of k" — k
  defaults + per-query override semantics.
- The three retriever specs (`§23/02 / §23/03 / §23/04 §8`)
  for the per-retriever default configs.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `QueryRequest` shape | `brain-planner::knowledge::router` (23.3) | Yes — has `text`, `entity_anchor`, `kind_filter`, `predicate_filter`, `time_filter`, `confidence_min`, `include_*`, `limit`, `retrievers`, `fusion_config`. |
| `RoutingDecision` shape | router (23.3) | `retrievers: Vec<RetrieverInvocation>` (router-local — note name collision with planner-local `RetrieverInvocation`; planner uses a richer type). |
| `FusionConfig` shape | router (23.3) | `k: u32`, `weights: PerRetrieverWeights`. |
| `FilterChain` shape | filters (23.5) | All fields default to pass-through; the planner builds an explicit post-filter chain from the request. |
| `DEFAULT_K` constant | fusion (23.4) | `pub const DEFAULT_K: u32 = 60`. |

## 4. Architecture sketch

### Types

```rust
// crates/brain-planner/src/knowledge/planner.rs

use brain_core::knowledge::StatementKind;
use brain_core::{AgentId, MemoryKind, PredicateId};

use super::filters::FilterChain;
use super::fusion::DEFAULT_K;
use super::router::{
    route, FusionConfig, PerRetrieverWeights, QueryRequest, Retriever, RetrieverSelection,
    RoutingDecision, TimeRange,
};

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub routing: RoutingDecision,
    pub retrievers: Vec<PlannedRetriever>,
    pub fusion: FusionStep,
    pub post_filters: FilterChain,
    pub limit: u32,
    pub estimated_cost_ms: f32,
}

#[derive(Debug, Clone)]
pub struct PlannedRetriever {
    pub retriever: Retriever,
    pub weight: f32,
    pub top_n: usize,
    pub config: RetrieverConfig,
    pub pre_filter: Option<PreFilter>,
}

#[derive(Debug, Clone)]
pub enum RetrieverConfig {
    Semantic {
        ef_search: usize,
        similarity_threshold: f32,
        timeout_ms: u32,
    },
    Lexical {
        bm25_k1: f32,
        bm25_b: f32,
        min_score: Option<f32>,
        timeout_ms: u32,
    },
    Graph {
        max_depth: u8,
        max_branching: u32,
        direction: GraphDirection,
        relation_types: Option<Vec<RelationTypeId>>,
        include_statements: bool,
        timeout_ms: u32,
    },
}

#[derive(Debug, Clone)]
pub enum PreFilter {
    AgentId(AgentId),
    MemoryKind(Vec<MemoryKind>),
    StatementKind(Vec<StatementKind>),
    PredicateId(Vec<PredicateId>),
    Temporal(TimeRange),
}

#[derive(Debug, Clone)]
pub struct FusionStep {
    pub k: u32,
    pub weights: PerRetrieverWeights,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("query has no retrievable signal (no text, no anchor, no filters)")]
    NoSignal,
    #[error("max_depth {got} exceeds spec cap 5")]
    MaxDepthExceeded { got: u8 },
}
```

### `plan` entry point

```rust
pub fn plan(req: &QueryRequest) -> Result<QueryPlan, PlanError> {
    let routing = route(req);

    if routing.retrievers.is_empty() {
        // §24/00 §"Routing rules" — no retriever match. This
        // happens e.g. when the request only has filters (no
        // text / no anchor); v1 returns an explicit error so
        // clients see a deterministic signal. A filter-only
        // mode lands post-v1.
        return Err(PlanError::NoSignal);
    }

    let limit = if req.limit == 0 { 20 } else { req.limit };
    let top_n_default = limit.saturating_mul(3).max(100) as usize;

    let retrievers = routing.retrievers.iter().map(|inv| {
        let pre_filter = pre_filter_for(req, inv.retriever, &routing);
        let config = retriever_config_for(inv.retriever, req);
        PlannedRetriever {
            retriever: inv.retriever,
            weight: inv.weight,
            top_n: top_n_default,
            config,
            pre_filter,
        }
    }).collect();

    let fusion = build_fusion_step(req);
    let post_filters = build_post_filters(req);
    let estimated_cost_ms = estimate_cost(&retrievers, &post_filters, limit);

    Ok(QueryPlan {
        routing,
        retrievers,
        fusion,
        post_filters,
        limit,
        estimated_cost_ms,
    })
}
```

### Push-down decisions

```rust
fn pre_filter_for(req: &QueryRequest, retriever: Retriever, routing: &RoutingDecision)
    -> Option<PreFilter>
{
    if routing.temporal_pushdown {
        if let Some(range) = req.time_filter {
            return Some(PreFilter::Temporal(range));
        }
    }
    if !req.predicate_filter.is_empty() && matches!(retriever, Retriever::Semantic | Retriever::Graph) {
        return Some(PreFilter::PredicateId(req.predicate_filter.clone()));
    }
    // ...etc. for kind_filter — graph + semantic only.
    None
}
```

A retriever invocation gets **at most one** PreFilter in v1
because `LexicalFilters` already bundles multiple filters and
the semantic / graph retrievers' filter callbacks handle one
condition per call. The planner emits the highest-impact
pre-filter (temporal > predicate > kind) per retriever; the
remaining filters apply post-fusion. Future polish can emit
multiple pre-filters per retriever.

### Cost estimate

```rust
fn estimate_cost(retrievers: &[PlannedRetriever], filters: &FilterChain, limit: u32) -> f32 {
    let mut cost = 0.0_f32;
    for r in retrievers {
        cost += match &r.config {
            RetrieverConfig::Semantic { ef_search, .. } => {
                5.0 + (*ef_search as f32) * 0.05
            }
            RetrieverConfig::Lexical { .. } => {
                10.0 + (r.top_n as f32) * 0.05
            }
            RetrieverConfig::Graph { max_depth, .. } => {
                let d = *max_depth as f32;
                5.0 * d * d
            }
        };
    }
    cost += 0.1 * retrievers.len() as f32;
    cost += 1.0 * (limit as f32);
    cost
}
```

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| `Option<PreFilter>` per retriever (this plan) | Simple shape; documented v1 limitation | One push-down per retriever; the rest land in post-filter | ✓ — explicit; future polish lifts the cap |
| `Vec<PreFilter>` per retriever | More expressive | Complicates the executor; the retriever traits don't all support multi-filter pre-filtering uniformly | rejected — v1 |
| Planner owns retriever invocation (executes too) | One module to rule them all | Mixes plan-time and exec-time concerns; breaks EXPLAIN-only path | rejected — §24/00 separates plan and execute |
| `PlanError::NoSignal` on filter-only requests | Explicit | Filter-only is a real use case ("find all Fact statements"); v1 says no, post-v1 says yes | ✓ for v1 — Document the limitation; phase 23.5's filter-only mode is the path |
| Linear-sum cost model (this plan) | Cheap; transparent | Doesn't model retriever overlap, cache effects | acceptable for v1 — EXPLAIN displays component costs for operator audit |
| Per-retriever timeout from `RouterConfig` | Centralised | Adds a struct; v1 defaults from the retriever specs are good enough | rejected for v1 |

## 6. Risks / open questions

- **Risk:** `req.fusion_config.weights` override vs router weights. **Resolution:** if `fusion_config.weights` is non-default (any weight ≠ 1.0), use it; otherwise fold in the router's per-retriever weights (e.g. Graph 2.0 in the entity-anchored rule).
- **Risk:** Per-retriever weights collide with router weights — router gives `RetrieverInvocation.weight` (per-retriever weight in the fusion sum) while `PerRetrieverWeights` is the per-retriever multiplier carried into RRF. v1 uses the router weights only at fusion time; `PerRetrieverWeights` is overlaid for explicit operator tuning. The planner picks max of (router weight, configured weight) per retriever.
- **Open question:** Is `top_n_default = max(limit × 3, 100)` the right shape? **Resolution:** matches §24/00 §"Limits and budgets" max-top-n cap of 200; v1 picks the upper bound at 100 and lets the cost model show the trade-off. Configurable in `req.fusion_config` later.
- **Open question:** Should the planner reject queries where the estimated cost > threshold? **Resolution:** v1 emits the cost; the executor (23.7) enforces budget. Planner is informational.

## 7. Test plan

Unit tests in `crates/brain-planner/src/knowledge/planner/tests.rs`:

- `default_free_text_plan` — only `text` → 2 retrievers (Semantic + Lexical) at equal weight; fusion k=60; post-filters empty.
- `entity_anchored_plan` — `entity_anchor` set → 3 retrievers (Graph 2.0, Semantic 1.0, Lexical 0.5 if text); cost reflects depth.
- `exact_id_plan` — text contains `ACME-1247` → Lexical 2.0, Semantic 0.5.
- `temporal_push_down_attaches_prefilter` — `time_filter` set, router signals push-down → every retriever has `Some(PreFilter::Temporal)`.
- `predicate_filter_pushes_to_semantic_and_graph_not_lexical` — `predicate_filter` set with entity anchor → Semantic + Graph have `PreFilter::PredicateId`; Lexical doesn't (handled via `LexicalFilters` natively).
- `request_fusion_config_overrides_k` — `req.fusion_config.k = Some(30)` → plan.fusion.k == 30.
- `request_weights_override_router_when_nondefault` — `fusion_config.weights.graph = 3.0` → fusion uses 3.0, even if the router said 2.0.
- `explicit_retrievers_skips_auto_weights` — `retrievers = Explicit([Semantic, Graph])` → exactly those two with weight 1.0.
- `limit_propagation` — `req.limit = 25` → plan.limit = 25; top_n_default = 75 (limit × 3) capped at min 100.
- `no_signal_returns_error` — request with no text, no anchor, no filters → `Err(PlanError::NoSignal)`.
- `filter_only_request_also_returns_no_signal` — `kind_filter` set but no text → `Err(PlanError::NoSignal)` (v1 limitation).
- `cost_estimate_monotonic_in_top_n` — larger top_n → larger cost.
- `cost_estimate_monotonic_in_max_depth` — Graph deeper → quadratic-ish cost growth.

## 8. Commit shape

Single commit:

```
feat(planner): 23.6 — query planner

- crates/brain-planner/src/knowledge/planner.rs (new):
  QueryPlan + PlannedRetriever + RetrieverConfig +
  PreFilter + FusionStep + PlanError. `plan(req)` calls
  the router (23.3), expands the routing decision into a
  concrete plan DAG, decides push-down per retriever
  (temporal > predicate > kind), folds per-query weights
  over router weights, builds the post-filter FilterChain
  (23.5), and emits a linear-sum cost estimate.
- crates/brain-planner/src/knowledge/planner/tests.rs (new):
  ~13 unit tests covering all 5 router rules, push-down
  decisions, weight precedence, limit propagation, no-signal
  error, and cost-monotonicity sanity checks.
- crates/brain-planner/src/knowledge/mod.rs: `pub mod planner`.
```

## 9. Confirmation

Please confirm:

1. **`Option<PreFilter>` per retriever** (one push-down each, v1) vs `Vec<PreFilter>` — future polish.
2. **Push-down precedence: temporal > predicate > kind** when multiple filters present.
3. **Push-down filters are ALSO retained in `post_filters`** — idempotent re-check after fusion for unambiguous final contract.
4. **`PlanError::NoSignal` on filter-only requests** — v1 rejects; filter-only mode is post-v1.
5. **Linear-sum cost model** — coarse, transparent, displayed in EXPLAIN for operator audit. No cost-based plan rewriting in v1.

After approval: implement + tests + commit.
