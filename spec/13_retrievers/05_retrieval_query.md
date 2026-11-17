# 13.05 The Query Pipeline

## Purpose

The Query Pipeline is the **single** read pipeline every shard runs.
It plans and executes queries over Memories, Statements, Relations,
and Entities through one stack
(`crates/brain-planner/src/retrieval/executor/logic.rs:634-800`):

```
validate → embed cue → fan out to the three always-wired retrievers
(semantic + lexical + graph) → fusion → filter chain
→ [recency boost, if the query carries a temporal signal]
→ [cross-encoder rerank, if the model is loaded]
→ [merge / diversity, if the router detected list intent]
→ limit → wire response
```

The rerank sits **after** the filter chain, not between fusion and it:
re-scoring a candidate the filter chain is about to drop would be
cross-encoder time spent on a result nobody can receive. The rerank is
always-on whenever a cross-encoder is loaded on the shard, and loading is
governed by the deploy-time `[rerank] enabled` switch, which defaults to
`false`; there is no per-request toggle.

The Query Router classifies each incoming query to pick weights and pool
widths, **not** to switch between modes — every retriever lane is
available every time.

This section extends Brain's Query Planner (§12) with:
- Multi-retriever execution (semantic + lexical + graph in parallel).
- Filter chain (type, temporal, confidence, tombstone).
- RRF fusion.
- Query classification and routing (weight selection, not mode selection).

There is no single-retriever counterpart — the pipeline is the same
whether or not a user schema has been declared. Schema declarations
narrow what predicate-aware filters accept; they do not gate any stage
of this pipeline.

This pipeline is the **engine**, not a client verb. The sole client entry point
is `RECALL` (§05.03), which runs this engine and returns a membership answer
(Single / Many / None). The engine's raw fused output is exposed on the wire
only through the operator debug ops `QUERY_EXPLAIN` (plan, no execution) and
`QUERY_TRACE` (execute + per-retriever breakdown). There is no client-facing
bulk-query verb that returns the raw ranked list.

## Query shape

The engine's internal request. `RECALL` builds it from the cue; `QUERY_TRACE`
accepts it directly so an operator can drive the engine for diagnosis. It is
not a client query language.

```rust
struct QueryRequest {
    text: Option<String>,
    entity_anchor: Option<EntityId>,
    kind_filter: Vec<StatementKind>,
    predicate_filter: Vec<PredicateId>,
    time_filter: Option<TimeRange>,
    confidence_min: Option<f32>,
    include_tombstoned: bool,
    include_superseded: bool,
    limit: u32,                        // safety cap on returned members, NOT a
                                       // ranking top_k; RECALL feeds max_results
    retrievers: RetrieverSelection,    // Auto | Explicit (debug-override only)
    fusion_config: Option<FusionConfig>, // debug-override only
}
```

## Query router

The router classifies the query and decides retrievers/weights.

### Classification features

The router extracts features from the query:

- **Has text?** (semantic and lexical apply)
- **Has entity anchor?** (graph applies)
- **Has time filter?** (temporal applies)
- **Has type filter?** (narrowing applies)
- **Text contains entity names?** (NER on query)
- **Text contains exact IDs / proper nouns?** (lexical prefers)
- **Text is short and noun-heavy?** (lexical prefers)
- **Text is a question or phrase?** (semantic prefers)

### Routing rules

Rule-based router. For each rule, if the conditions match, retrievers are selected with the noted weights:

```
Rule 1: Entity-anchored query
    Conditions: query.entity_anchor.is_some() OR a Title-Case mention in text
    Retrievers: Graph (weight 2.0), Semantic (1.0), Lexical (0.7)

Rule 2: Exact-term query
    Conditions: text contains an exact identifier (e.g. ACME-1247)
                OR text is all-caps tokens
    Retrievers: Lexical (2.0), Semantic (1.0), Graph (0.7)

Rule 3: Time-filtered query
    Conditions: time_filter.is_some() OR text contains temporal expression
    Retrievers: + Temporal filter (no separate retriever; applied at filter stage)

Rule 4: Type-filtered query
    Conditions: kind_filter or predicate_filter present
    Effect: Filter chain narrows after retrieval

Rule 5: Free-text query (the "paraphrase-likely" class)
    Conditions: text present, no anchor and no exact-term signal
    Retrievers: Semantic (1.5), Lexical (1.0), Graph (0.5)

Rule 7: Default
    Conditions: no text and no anchor — a filter-only request
    Retrievers: Semantic (1.0), Lexical (1.0), Graph (1.0)
    Effect: no retriever signal is left to weight, so no lane is preferred

Rule 6: List / aggregation query  (internal — never client-selected)
    Conditions: free-text cue carries enumerative intent — "list", "every",
                "how many/much", "all of", "name all/every/the", "what are/were",
                "what kinds/types of" (precision-biased; bare "what"/"who" excluded)
    Effect:    sets an internal `list_intent` flag, orthogonal to the class
               so it also fires on entity-anchored list queries. Widens the
               per-retriever pool ceiling (200 → 400) so deeply-ranked set
               members reach fusion (coverage), and enables the merge /
               diversity stage (see [`./06_post_processing.md`](./06_post_processing.md)).
               Free-text list cues additionally pick the `ListAggregation`
               class (semantic-led, wide pool).
```

**The client never selects this.** Brain is a database: the caller asks
`recall(cue)` and the router decides — from the cue text alone — whether the
answer is one memory or a set, and quietly does the extra coverage + merge work
behind the scenes. There is no `diversity` / "list mode" knob on the wire; the
caller cannot know in advance whether their question has one answer or many, so
the DB owns that decision. There is no `top_k` either: the membership band over
the full candidate pool decides which memories belong to the answer, and
`max_results` is only a safety cap on the returned member count — list handling
changes *which* results the band admits, not a caller-chosen count.

**Classification picks exactly one class, in priority order**, and the class picks one weight-and-pool profile (`crates/brain-planner/src/retrieval/router/logic.rs:243-268`):

```
entity anchor or Title-Case mention  → EntityAnchored
else exact identifier or all-caps    → ExactTerm
else text, with enumerative intent   → ListAggregation
else text                            → Paraphrase
else (no text, no anchor)            → Default
```

The rules above are how those conditions are read; they are not independently-composable weight sources. Rules 3, 4 and 6 are different in kind: they set *orthogonal* flags rather than a class, so they can fire alongside any of the five. An entity-anchored question that also reads as a list keeps its graph-led profile and gains the list flag — the class decides whose rank wins, the flag decides how wide the pool is and whether the merge stage runs.

The full per-class weight and pool table is in [`./01_rrf_fusion.md`](./01_rrf_fusion.md) §"Adaptive top-K from the router".

### Limits and budgets

The router enforces:
- Max retrievers per query: 3 (all of semantic, lexical, graph if matched).
- Per-retriever pool width: set by the query class — 50, 100, 200 or 400 (§"Classification"). 400, on a list/aggregation query, is the widest any class plans.
- Per-retriever budget: 1 s, and **soft**. Exceeding it records the lane as `Timeout` and emits a warning; it does not cancel the lane, and a late result still reaches fusion (`crates/brain-planner/src/retrieval/planner/logic.rs:55-65`). A stuck retriever is therefore visible but not cut off.
- Cost estimate: computed per plan and surfaced on the plan (and through `EXPLAIN`) as `estimated_cost_ms` (`crates/brain-planner/src/retrieval/planner/logic.rs:207`). It is a **diagnostic, not an admission control**: nothing in the planner degrades a plan to fewer retrievers or a narrower pool on the strength of its own estimate. A query the planner expects to be slow is planned and run as asked, and the estimate is what lets an operator see that it was.

### Per-query override (debug only)

The router's decision can be overridden through the `retrievers` /
`fusion_config` fields of the internal `QueryRequest` — but only via the
operator debug ops (`QUERY_EXPLAIN` / `QUERY_TRACE`), never on the `RECALL`
client read:

```rust
.retrievers(Explicit(vec![Retriever::Semantic, Retriever::Graph]))
.fusion_config(FusionConfig { k: 30, ..default })
```

The override is logged for audit. A client RECALL cannot reach these knobs — the
router owns retriever/weight selection for the read path.

## Filter chain

After retrieval and fusion, the result list passes through filters:

```
fused candidates
  → Type filter (kind, predicate)
  → Temporal filter (event_at or valid_from/valid_to within range)
  → as_of(record_time) filter (see below; server-internal)
  → Confidence filter (confidence ≥ threshold)
  → Tombstone filter (exclude tombstoned unless explicitly included)
  → Supersession filter (exclude superseded unless explicitly included)
  → Membership band + max_results safety cap
```

### The as_of(record_time) filter

`as_of(record_time)` selects statements whose **record-time window** contains the supplied timestamp — i.e. statements Brain believed at that point in time. The window is `[extracted_at_unix_nanos, record_invalidated_at_unix_nanos.unwrap_or(u64::MAX))`. See [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) for the four-timestamp model.

The filter answers "what did Brain believe on date X" without resurrecting tombstoned rows — the row stays in the table with `record_invalidated_at` set, and this filter picks it up when the target time falls inside the record window.

Wire surface: exposed on the client wire as `as_of_record_time_unix_nanos` on `RECALL_REQ` (`None`/0 = current state); also present on the internal `QueryRequest` that the `QUERY_TRACE` debug op accepts. Besides driving this record-time filter, a set anchor becomes the reference point for the recency-ranking decay (see [`./01_rrf_fusion.md`](./01_rrf_fusion.md) §"Recency ranking"). It applies to statement/relation results; memory hits have no record-time axis and pass the filter unchanged.

Filters are applied in this order because:
- Type and confidence are cheap and aggressive (early dropout).
- Temporal requires field reads; medium cost.
- Tombstone and supersession need redb reads.

For typical queries, the filter chain removes 50-90% of fused candidates. The remaining are returned.

## Filter as retriever vs filter

Some filters could be implemented as retrievers (e.g., a "temporal retriever" that scans memories in a time range). The design here: only those that *produce* candidates from a corpus are retrievers; those that *narrow* a candidate set are filters.

This is a design call. The advantage of filter-only-after-fusion: filters are uniform and composable. The disadvantage: if a filter is very selective (e.g., "events in the last hour"), running it after fusion is wasteful — Brain would retrieve many candidates and throw most away.

For very selective filters, the planner *pushes them down* into the retrievers as pre-filters:

- Temporal: passed to retrievers as a pre-filter (HNSW's filter callback, tantivy's query AST).
- Type/predicate: passed as pre-filter to graph retriever (which can join through type indexes directly).

Push-down is handled by the planner per retriever.

## Plan structure

A query plan is a DAG:

```
QueryPlan {
    routing: RoutingDecision,
    pre_filters: Vec<PreFilter>,          // applied at retriever level
    retrievers: Vec<RetrieverInvocation>,
    fusion: FusionStep,
    post_filters: Vec<PostFilter>,        // applied after fusion
    limit: u32,
    estimated_cost: f32,
}

struct RetrieverInvocation {
    retriever: Retriever,
    config: RetrieverConfig,
    pre_filter: Option<PreFilter>,
    top_n: usize,
    weight: f32,
}
```

The plan is built by the planner from the request. EXPLAIN-style debug output is available:

```
QUERY: "what does Priya prefer about meetings"
PLAN:
  ROUTING: entity-anchored (Priya), text-bearing
  PRE_FILTERS: none
  CLASS: EntityAnchored  (weights 1.0 / 0.7 / 2.0, pool 50)
  RETRIEVERS:
    SemanticRetriever(weight=1.0, top_n=50, corpus=statements)
    LexicalRetriever(weight=0.7, top_n=50, query="meetings preferences")
    GraphRetriever(weight=2.0, top_n=50, anchor=priya, depth=1)
  FUSION: RRF(k=15)   # pool < 200 ⇒ k=15, not the canonical 60
  POST_FILTERS: kind in [Preference], confidence ≥ 0.5, !superseded
  LIMIT: 20
  ESTIMATED COST: 12ms   # diagnostic only; does not degrade the plan
```

## Execution

The executor runs retrievers in parallel (each on its own task on the shard's executor), gathers what they return, then fuses, filters, and runs the post-filter stages. The sketch below shows the gather-and-fuse shape; the budget it reads as a deadline is soft in the implementation — a lane that overruns is *recorded* as `Timeout` rather than cancelled, so its result can still arrive and be fused.

```rust
async fn execute(plan: &QueryPlan, ctx: &Ctx) -> QueryResult {
    let mut futures = FuturesUnordered::new();
    for inv in &plan.retrievers {
        futures.push(execute_retriever(inv, plan.pre_filters.clone(), ctx));
    }
    
    let mut retriever_outputs = Vec::new();
    let deadline = Instant::now() + plan.timeout;
    
    while let Some(result) = futures.next().with_timeout_at(deadline).await {
        match result {
            Ok(output) => retriever_outputs.push(output),
            Err(TimeoutError) => {
                // partial; proceed with the outputs already gathered
                break;
            }
        }
    }
    
    // `k` is derived from the gathered pool's width, not taken from a
    // constant — see 01_rrf_fusion.md §"Choice of k".
    let k = adaptive_k(candidate_pool_width(&retriever_outputs));
    let fused = fuse(retriever_outputs, k, &plan.fusion.weights, plan.fusion.method);
    let mut items = apply_filter_chain(fused, &plan.post_filters);

    // Post-filter stages, each conditional on a signal the router or the
    // operator supplied — never on a client request field.
    if plan.routing.temporal_pushdown || plan.post_filters.as_of.is_some() {
        apply_recency_boost(&mut items, plan.fusion.weights.temporal, k);
    }
    if ctx.cross_encoder.is_some() {
        items = rerank_stage(items, ctx).await;
    }
    if plan.routing.list_intent {
        apply_diversity(&mut items);
    }

    let limited = items.into_iter().take(plan.limit as usize).collect();
    QueryResult { items: limited, debug: ... }
}
```

`plan.limit` here is a **safety ceiling**, not a caller-chosen top_k. For the
`RECALL` read path the ceiling is set large (a full recall-candidate budget) so
the membership stage downstream sees the whole filtered pool and the relevance
band — not a pre-truncated head — decides the answer set. The `take` only guards
against a runaway pool; `max_results` caps the final member count after
membership shaping.

## Streaming results

For large result sets (`limit > 100`), the executor streams results to the client. Each fused-and-filtered item is emitted as it passes the limit boundary. The client can stop reading early.

Streaming uses the wire protocol's SUBSCRIBE mechanism (§04), with QueryRequest opcodes as event types.

## Result shape

```rust
struct QueryResult {
    items: Vec<ResultItem>,
    metadata: QueryMetadata,
}

struct ResultItem {
    item: ItemRef,                       // Memory | Statement | Relation | Entity
    fused_score: f64,
    contributing_retrievers: Vec<RetrieverContribution>,
}

struct RetrieverContribution {
    retriever: String,
    rank: usize,
    raw_score: f32,
}

struct QueryMetadata {
    plan_summary: String,
    retriever_latencies_ms: HashMap<String, f64>,
    total_latency_ms: f64,
    retriever_total_results: HashMap<String, usize>,
    filters_applied: Vec<FilterSummary>,
}
```

`contributing_retrievers` is per-result: clients can see which retrievers brought this item into the result and what its ranks were. This is the basis for explainability.

## Learned routing (future versions)

Brain uses rule-based routing. A future version (deferred) may support learned routing: a small classifier trained on labeled queries (`query_text → preferred_retrievers`). The classifier would be a feature; the rule-based fallback remains for cold start and ambiguous queries.

Labels come from:
- Click-through data (user picks a result; retrievers that surfaced it get credit).
- Explicit feedback (an explicit feedback call from the client).
- Synthetic labels from a teacher LLM.

Brain is rule-based by default. Documents the path to future learned routing.
