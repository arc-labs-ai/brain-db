# 12 — Query router

**Audience:** anyone touching the planner, adding a routing
rule, debugging "why didn't this query use the graph
retriever," or making sense of `QUERY_EXPLAIN` / `QUERY_TRACE`
output.

**Goal:** by the end you should know how a `QueryRequest` turns
into an executable plan, what each filter does and which ones
get pushed down into retrievers, what the executor surfaces back
to clients in EXPLAIN/TRACE, and what's in the cost estimate.

This chapter is the planning-and-execution half of the
knowledge-layer query path. The retrievers + RRF fusion are in
[chapter 11](11-hybrid-retrieval-rrf.md); this chapter is what
*decides* which retrievers run and *what* they're handed.

---

## What the router is

The router turns a structured `QueryRequest` into a
`RoutingDecision`: which retrievers to invoke, with what
weights. It's rule-based, deterministic, and runs in
microseconds — no model, no learning, no async.

Why rule-based? Three reasons:

1. **It's explainable.** A trace can say "matched rule 1 +
   rule 2; selected graph + semantic with weights 2.0 + 1.0."
   A learned router can't.
2. **It's fast.** 5–10 µs per query. A learned router would
   add a forward pass.
3. **Rules are the right level of abstraction.** "Entity
   anchor present → trust graph more" is a domain insight, not
   a ML problem. Patterns in queries are not subtle.

The router is one function:
`route(req) -> RoutingDecision`
(`crates/brain-planner/src/knowledge/router.rs:165`). The
planner calls it once per query.

---

## Classifying a query

Before picking rules, the router extracts features from the
request. `ClassificationFeatures`
(`crates/brain-planner/src/knowledge/router.rs:144`):

```rust
pub struct ClassificationFeatures {
    pub has_text: bool,
    pub has_entity_anchor: bool,
    pub has_time_filter: bool,
    pub has_type_filter: bool,
    pub has_predicate_filter: bool,
    pub contains_exact_id: bool,
    pub is_all_caps_tokens: bool,
    pub is_short_and_noun_heavy: bool,
    pub is_question: bool,
    pub contains_entity_mention_heuristic: bool,
    pub contains_temporal_expression: bool,
}
```

Each flag is set by a cheap text inspection or by checking
which request fields are populated. The regexes used
(`EXACT_ID_RE`, `TITLE_CASE_RE`, `TEMPORAL_RE`) are
static-compiled via `LazyLock`
(`crates/brain-planner/src/knowledge/router.rs:32`) so the
per-query routing cost is dominated by hash lookups, not regex
compilation.

Features that come from the request structure (no text
inspection needed):

- `has_text` — `text` field is `Some`.
- `has_entity_anchor` — `entity_anchor` is `Some(EntityId)`.
- `has_time_filter` — `time_filter` is `Some(TimeRange)`.
- `has_type_filter`, `has_predicate_filter` — non-empty vec.

Features that come from inspecting the text:

- `contains_exact_id` — pattern `[A-Z][A-Z0-9]+-\d+` matched
  (e.g. `ACME-1247`, `BUG-42`).
- `is_all_caps_tokens` — every whitespace-separated token is
  uppercase + digits + `-_`.
- `is_short_and_noun_heavy` — ≤4 tokens, no `?`.
- `is_question` — contains `?` or starts with `what`/`who`/`how`/etc.
- `contains_entity_mention_heuristic` — title-case pattern
  matched.
- `contains_temporal_expression` — `yesterday`, `last week`,
  `2024-03-12`, etc.

The features get surfaced to the operator in
`QUERY_EXPLAIN` — a query routed unexpectedly often turns out
to be missing one of the structural flags.

### Why heuristics, not NER

A real NER pipeline would classify "Priya" as a `Person`
entity. We don't need that here — the router's job is to *bias
the retriever choice*, not to resolve. If the title-case
heuristic triggers a graph weight bump and the graph retriever
later returns nothing because the anchor isn't a known entity,
the fusion still works (graph contributes 0). The heuristic is
cheap to get wrong.

---

## Routing rules

The auto-router applies a small set of rules
(`crates/brain-planner/src/knowledge/router.rs:222`). Each rule
either adds retrievers with weights or flags a behaviour
(like temporal pushdown).

### Rule 1: entity-anchored

```
if has_entity_anchor OR contains_entity_mention_heuristic:
    graph    weight 2.0
    semantic weight 1.0
    if has_text:
        lexical weight 0.5
```

The graph retriever is *trusted most* when there's a
recognisable entity to anchor on. Semantic and lexical still
run as a sanity check — graph can return weird stuff if the
anchor resolution is off, and lexical/semantic ground the
result in the actual query text.

### Rule 2: exact-term

```
if contains_exact_id OR is_all_caps_tokens:
    lexical  weight 2.0
    semantic weight 0.5
```

When the query looks like an ID (`ACME-1247`) or an
all-caps acronym, lexical's BM25 is exactly right. Semantic
embedding might lose the exact-token signal in a fuzzy
neighbourhood; the half-weight keeps it as a backstop.

### Rules 3 & 4: temporal & filter

These don't *add retrievers*; they flag the planner to push
the filter down into the retrievers as a pre-filter.
`temporal_pushdown = has_time_filter || contains_temporal_expression`
is what the planner reads.

### Rule 5: default

```
if no other rule matched AND has_text:
    semantic weight 1.0
    lexical  weight 1.0
```

Equal weights, both retrievers, no graph. The fallback for
free-text queries that don't trigger anything specific.

### Combining rules

A query can match multiple rules. The weights *max*, not sum
(`crates/brain-planner/src/knowledge/router.rs:313`). So a
query that matches rule 1 (graph 2.0) and rule 5 (semantic 1.0)
just keeps both at the higher value. This stops a query that
trips many rules from getting weight 5.0 on one retriever and
overpowering fusion.

The selected retrievers are then **capped at
`MAX_RETRIEVERS = 3`**
(`crates/brain-planner/src/knowledge/router.rs:22`) — the
budget for parallel fan-out. Below the cap nothing changes; at
or above it, the top-3 by weight wins with a deterministic
tie-break by discriminant order.

---

## Explicit overrides

A client can bypass the router entirely
(`crates/brain-planner/src/knowledge/router.rs:181`):

```rust
RetrieverSelection::Explicit(vec![Retriever::Lexical, Retriever::Graph])
```

The router honours the list verbatim, dedups, truncates to
`MAX_RETRIEVERS`, and gives every selected retriever weight
1.0. The fusion weights still apply (the request's
`fusion_config.weights` override the flat 1.0s).

The `MAX_RETRIEVERS` cap is enforced even on explicit
overrides — a malicious or buggy client can't ask for 50
retrievers and tie up the executor. The SDK enforces it at
request construction; the router enforces it again at routing
time as defence in depth.

Explicit overrides are mainly for testing and for
operator-driven debugging. Most production queries use auto.

---

## From routing to plan

The router produces a `RoutingDecision`. The planner expands it
into a `QueryPlan`
(`crates/brain-planner/src/knowledge/planner.rs:64`):

```rust
pub struct QueryPlan {
    pub routing: RoutingDecision,
    pub retrievers: Vec<PlannedRetriever>,
    pub fusion: FusionStep,
    pub post_filters: FilterChain,
    pub limit: u32,
    pub estimated_cost_ms: f32,
}
```

Each `PlannedRetriever`
(`crates/brain-planner/src/knowledge/planner.rs:78`) holds:

- The retriever variant + weight (from the routing decision).
- A `top_n` cap (computed from the request `limit`).
- A `RetrieverConfig` for this retriever's per-query knobs.
- An *optional* `PreFilter` pushed down into this retriever.

`plan` is one function
(`crates/brain-planner/src/knowledge/planner.rs:146`):

```rust
pub fn plan(req: &QueryRequest) -> Result<QueryPlan, PlanError> {
    let routing = route(req);
    if routing.retrievers.is_empty() {
        return Err(PlanError::NoSignal);
    }
    let limit = if req.limit == 0 { DEFAULT_RESULT_LIMIT } else { req.limit };
    let top_n = top_n_for(limit);
    …
}
```

`NoSignal` is the "no text, no entity anchor" case — there's
literally nothing to query against. v1 doesn't support
filter-only queries ("return every Fact created in 2024"); the
planner errors out and the SDK surfaces it as a
`Validation::NoSignal` wire error.

`DEFAULT_RESULT_LIMIT = 20`
(`crates/brain-planner/src/knowledge/planner.rs:35`). A request
that doesn't specify `limit` gets 20.

### `top_n` per retriever

`top_n_for(limit)`
(`crates/brain-planner/src/knowledge/planner.rs:190`):

```rust
let from_limit = (limit as usize).saturating_mul(3);
from_limit.max(MIN_TOP_N).min(MAX_TOP_N)
```

So:

- **`MIN_TOP_N = 100`** — at least 100 candidates per retriever,
  regardless of `limit`. RRF needs candidates to fuse;
  starving it produces noisy top results.
- **`MAX_TOP_N = 200`** — never more than 200 per retriever.
  Larger only adds fusion cost without measurably improving
  the top-K of the result.
- **`limit × 3`** — between the floor and ceiling, scale with
  the request: `limit = 50` → `top_n = 150`.

This is what guarantees fusion has *enough* tail to work with
no matter the request shape. Spec [§11](11-hybrid-retrieval-rrf.md)'s
"document ranked 250th in semantic but 1st in lexical" case
exists; this is what bounds how often it bites.

---

## Filter pushdown

Filters apply in two places:

1. **Pre-filter (pushed down into a retriever).** The retriever
   reads the filter and uses its native index to skip rows
   that don't qualify. Cheap; the work happens at the index
   level.
2. **Post-filter (applied after fusion).** The filter chain
   walks the fused result and drops items that don't qualify.
   More expensive — every survivor needs a metadata lookup —
   but works for filters retrievers can't push down.

The planner decides per filter, per retriever. The rule
(`crates/brain-planner/src/knowledge/planner.rs:225`):

```
Push-down precedence (v1 emits at most one pre-filter per retriever):
  1. Temporal — push down into every retriever.
  2. Predicate — push down into Semantic + Graph
                 (Lexical handles it natively via its filters).
  3. Statement kind — push down into Semantic + Graph.

Anything not pushed down stays in the post-fusion filter chain.
```

The "at most one pre-filter per retriever" rule is v1's
limitation. Most queries only have one filter that's worth
pushing down anyway; multi-pre-filter support would mean the
retrievers' configs grow more complex without buying much.

### What goes in the post-filter chain

`FilterChain`
(`crates/brain-planner/src/knowledge/filters.rs:41`):

```rust
pub struct FilterChain {
    pub kind_filter: Vec<StatementKind>,
    pub memory_kind_filter: Vec<MemoryKind>,
    pub predicate_filter: Vec<PredicateId>,
    pub time_filter: Option<TimeRange>,
    pub confidence_min: Option<f32>,
    pub include_tombstoned: bool,
    pub include_superseded: bool,
}
```

The chain applies in fixed order
(`crates/brain-planner/src/knowledge/filters.rs:81`):

```
fused items
    │
    ▼ filter_type           (kind / memory_kind / predicate)
    ▼ filter_temporal       (time_filter, in case it wasn't pushed down)
    ▼ filter_confidence     (confidence_min)
    ▼ filter_tombstone      (drop tombstoned unless include_tombstoned)
    ▼ filter_supersession   (drop superseded unless include_superseded)
    ▼ truncate to limit
```

Each step records a *survivor count* into `FilterChainStats`
(`crates/brain-planner/src/knowledge/filters.rs:63`). The
stats surface in `EXPLAIN/TRACE` so operators can see exactly
which filter dropped the candidates:

```
filter chain:
  before:            150
  after_type:        120     (-30 dropped by kind_filter)
  after_temporal:    115
  after_confidence:   80
  after_tombstone:    78
  after_supersession: 60
  after_limit:        20
```

A query that returns nothing usually has a survivor count
that bottomed out at one specific filter — the trace pinpoints
which.

### Why temporal pushdown matters

Temporal filters are *the* push-down win. Without it, an "any
result from last week" query has to fetch a large fused list,
look up each item's timestamp in redb, and drop the old ones.
With pushdown, the retrievers query their respective indexes
directly with a date-range constraint — tantivy's range query,
the graph retriever's time-bounded traversal, the semantic
retriever's metadata-side filter.

The router flag is `temporal_pushdown`
(`crates/brain-planner/src/knowledge/router.rs:124`); the
planner reads it and threads the filter into each
`PlannedRetriever.pre_filter`. **A temporally-filtered query
runs measurably faster** than the equivalent post-filter
query at any meaningful result count.

---

## Fusion step

The planner also builds the `FusionStep`
(`crates/brain-planner/src/knowledge/planner.rs:255`):

```rust
let k = req.fusion_config.as_ref().map(|c| c.k).unwrap_or(DEFAULT_K);
let request_weights = ...;
let router_weights = router_weights_from(retrievers);
let weights = PerRetrieverWeights {
    semantic: request_weights.semantic.max(router_weights.semantic),
    lexical:  request_weights.lexical.max(router_weights.lexical),
    graph:    request_weights.graph.max(router_weights.graph),
};
```

Two sources of weights:
- The client's `fusion_config.weights` (default 1.0 each if
  not set).
- The router's per-retriever weight from the rule that
  matched.

The planner takes the **max** of the two — the strongest
signal wins. A client that explicitly sets `graph = 0.5` while
the router said `graph = 2.0` ends up with `graph = 2.0`. The
intent is "the router knows the query class; let it speak."

For deployments that want client-side weights to override the
router, the SDK exposes a "force weights" flag that
short-circuits this max. v1 reserves that for advanced use.

---

## Cost estimate

`estimate_cost`
(`crates/brain-planner/src/knowledge/planner.rs:311`) gives a
rough wall-clock prediction in milliseconds:

```rust
for each retriever:
    cost += match config:
        Semantic { ef_search }       => 5 + ef_search * 0.05
        Lexical  { top_n }           => 10 + top_n * 0.05
        Graph    { max_depth }       => 5 * max_depth² 

cost += 0.1 * retrievers.len()       // fusion
cost += limit                         // filter chain (~1 ms per candidate)
```

A typical 3-retriever query with `limit = 20` estimates around
30–60 ms. The estimate is a *budget* — the executor's
per-retriever timeout (50 ms default) caps the actual
worst-case. Queries that the cost model predicts will run hot
can be reshaped client-side before submission, or rejected
with `BudgetExceeded` if the deployment configures a budget
ceiling.

The cost number is *also* in the EXPLAIN output, which is
where it's most useful — operators tuning queries can see the
per-retriever contribution.

---

## Executing the plan

`execute`
(`crates/brain-planner/src/knowledge/executor.rs:106`) is the
plan-to-result function:

```rust
for each PlannedRetriever:
    invoke retriever with config + pre_filter
    record (latency, outcome)
fuse with plan.fusion.k, plan.fusion.weights
apply post_filters
truncate to limit
return QueryResult
```

The executor runs retrievers in order; production wiring
typically spawns each on its own task for parallel fan-out,
joining results before fusion. Each retriever's invocation
returns one of:

- **`Ok(items)`** — got results within the timeout.
- **`Err(Skipped(reason))`** — retriever decided not to run
  (e.g., empty query, no anchor for graph). The reason is
  recorded.
- **`Err(Failure(msg))`** — retriever errored. The executor
  logs and continues with partial results.
- **`Err(Missing)`** — retriever isn't wired (deployment
  misconfiguration). This *does* halt the query —
  `MissingRetriever` is a bug, not a runtime condition.

Soft-timeout: if `elapsed_ms > timeout_ms`, the retriever's
outcome is marked `Timeout` (status only — the partial result is
still used).

### What comes back

`QueryResult`
(`crates/brain-planner/src/knowledge/executor.rs:52`):

```rust
pub struct QueryResult {
    pub items: Vec<FusedItem>,         // post-filter, post-limit
    pub retriever_outcomes: Vec<RetrieverOutcome>,
    pub filter_stats: FilterChainStats,
    pub retriever_latencies_ms: Vec<(Retriever, f64)>,
    pub retriever_totals: Vec<(Retriever, usize)>,
    pub fusion_k: u32,
    pub total_latency_ms: f64,
}
```

The "outcomes + latencies + totals" triple is what
TRACE returns to clients — full transparency into what each
retriever did, how long it took, how many items it produced.

---

## EXPLAIN and TRACE

Two diagnostic verbs.

**`QUERY_EXPLAIN`** — runs the planner but **not** the
executor. Returns the plan as a structured response, including:

- The classification features (which flags fired).
- The routing decision (which retrievers, what weights, why).
- Each retriever's config + pre-filter.
- The fusion config (k, final weights).
- The filter chain (in order).
- The estimated cost.

This is what an SDK user sees when they want to know *why* a
query routed a certain way. No data is fetched; no cost is
spent.

The text rendering lives in `render_plan`
(`crates/brain-planner/src/knowledge/explain.rs:25`); the
typed structure goes through the wire response unchanged.

**`QUERY_TRACE`** — runs the full query *and* attaches the
explanation + execution metadata. The response carries:

- Everything `EXPLAIN` would return.
- Each retriever's `RetrieverOutcome`, latency, item count.
- The filter chain stats (per-stage survivor counts).
- The fused result with per-item `contributing` list
  ([chapter 11](11-hybrid-retrieval-rrf.md)).
- The total latency and end-to-end timing.

`QUERY_TRACE` is one of the most useful tools in the system
for debugging "why didn't I get the result I expected." It's
also expensive — full execution plus a heavyweight response —
so production code paths should rarely call it. Operator
dashboards and SDK debug modes do.

`render_trace`
(`crates/brain-planner/src/knowledge/explain.rs:41`) formats the
trace as a human-readable string.

---

## Failure modes

**`PlanError::NoSignal`.** Request had no text and no entity
anchor. The wire surfaces this as `Validation::NoSignal`. The
client needs to add text, an entity anchor, or both.

**`ExecutionError::MissingRetriever(R)`.** The plan referenced
a retriever the deployment hasn't wired. Always a
configuration bug — operator should investigate. The query
fails hard.

**Every retriever fails.** The post-filter chain runs over an
empty list; the result is empty. Outcomes contain the per-
retriever failure reasons.

**Filter chain reads metadata redb and errors.** Surfaced as
`FilterError::Metadata`. Usually transient (write lock held
elsewhere); retry succeeds.

**Soft timeout exceeded.** Retriever returned in time but past
the soft budget. Status `Timeout`; result is still used.
Operator metric — frequent timeouts are a tuning signal.

**Hard timeout exceeded** (the per-retriever future actually
takes longer than the wrapping Tokio timeout). Treated as
`Failure`; partial fusion proceeds. Should be rare with
50 ms soft timeouts.

**Explicit override requests a retriever that isn't wired.**
Same as missing-retriever — the executor's `MissingRetriever`
error fires before fusion.

---

## Configuration & tuning

| Knob | Where | Default | Notes |
|---|---|---|---|
| `MAX_RETRIEVERS` | `crates/brain-planner/src/knowledge/router.rs:22` | 3 | Hard cap; cannot be raised without code changes. |
| `DEFAULT_RESULT_LIMIT` | `crates/brain-planner/src/knowledge/planner.rs:35` | 20 | When request omits `limit`. |
| `MIN_TOP_N` / `MAX_TOP_N` per retriever | `crates/brain-planner/src/knowledge/planner.rs:40` | 100 / 200 | Bounds the fusion candidate pool. |
| `PER_RETRIEVER_TIMEOUT_MS` | `crates/brain-planner/src/knowledge/planner.rs:55` | 50 ms | Soft cap on each retriever. |
| `DEFAULT_K` (RRF smoothing) | `crates/brain-planner/src/knowledge/fusion.rs` | 60 | Per-query override accepted. |
| Routing rule weights | `crates/brain-planner/src/knowledge/router.rs:222` | (rule-specific) | Domain heuristics. Code-level change to retune. |
| `GRAPH_DEFAULT_MAX_DEPTH` | `crates/brain-planner/src/knowledge/planner.rs:53` | 3 | The graph retriever's depth cap when invoked from the planner. |

Operational rules:

- **Trust the auto-router for production queries.** The
  default rules are reasonable; explicit overrides are for
  testing and debugging.
- **Use `QUERY_EXPLAIN` before tuning weights.** Look at
  which features fired, what the cost estimate is, and which
  retrievers were chosen — that tells you what to change.
- **Use `QUERY_TRACE` to debug "wrong answer" queries.** The
  per-retriever rank + filter-chain survivor counts pinpoint
  where the right answer dropped out.
- **Raise `top_n` cautiously.** Above 200 the fusion cost
  starts mattering; below 100 fusion gets noisy.
- **Watch `retriever_latencies_ms` over time.** A retriever
  consistently saturating the 50 ms timeout is a tuning
  signal (its index is too large, or its config is too
  aggressive).
- **Filter pushdown matters most for temporal queries.** If
  your workload has many time-bounded queries, the temporal
  pushdown is doing real work; if you disable it (by stripping
  the heuristic regex), throughput drops measurably.
- **`MAX_RETRIEVERS = 3` is the contract.** Future retriever
  types (e.g., a future external-search retriever) would still
  fit within this cap by replacing one of the current three
  per query.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| `route`, classification features, rules | `crates/brain-planner/src/knowledge/router.rs` |
| `plan`, `QueryPlan`, `PlannedRetriever` | `crates/brain-planner/src/knowledge/planner.rs` |
| Pre-filter pushdown logic | `crates/brain-planner/src/knowledge/planner.rs` |
| `FilterChain`, post-filter chain order | `crates/brain-planner/src/knowledge/filters.rs` |
| Per-stage `FilterChainStats` | `crates/brain-planner/src/knowledge/filters.rs` |
| `execute`, `QueryResult`, `RetrieverOutcome` | `crates/brain-planner/src/knowledge/executor.rs` |
| RRF fusion entry from the executor | `crates/brain-planner/src/knowledge/executor.rs` |
| `render_plan` (EXPLAIN text) | `crates/brain-planner/src/knowledge/explain.rs` |
| `render_trace` (TRACE text) | `crates/brain-planner/src/knowledge/explain.rs` |
| Wire opcodes `QUERY`, `RECALL_HYBRID`, `QUERY_EXPLAIN`, `QUERY_TRACE` | `crates/brain-protocol/src/opcode.rs` |

---

## Further reading

- [11 — Hybrid retrieval (RRF)](11-hybrid-retrieval-rrf.md) for
  the retrievers + fusion this chapter feeds into.
- [09 — Knowledge layer](09-knowledge-layer.md) for the tables
  the filter chain looks up.
- [02 — Wire protocol](02-wire-protocol.md) for the `QUERY*`
  family of opcodes that surface this chapter's output to
  clients.
- [01 — System architecture](01-system-architecture.md) for
  where the planner sits relative to the shard executor and the
  per-shard `OpsContext`.
