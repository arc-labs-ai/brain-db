# Query Engine and Router

## Purpose

The Query Engine plans and executes hybrid queries over Memories, Statements, Relations, and Entities. The Query Router classifies incoming queries and decides which retrievers and filters to invoke.

This section extends the substrate's Query Planner (Section 08 in the substrate) with:
- Multi-retriever execution (semantic + lexical + graph in parallel).
- Filter chain (type, temporal, confidence, tombstone).
- RRF fusion.
- Query classification and routing.

## Query shape

Two surfaces, same engine:

### Fluent API (primary, Rust SDK)

```rust
let results = brain.query()
    .recall("budget pushback")                              // text query
    .with_entity::<Person>("Priya")                         // entity anchor
    .of_kind(StatementKind::Preference)                     // type filter
    .where_time(TimeRange::last(Duration::days(30)))        // temporal filter
    .with_min_confidence(0.7)                               // confidence filter
    .limit(20)
    .execute()
    .await?;
```

### Structured request (wire protocol)

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
    limit: u32,
    retrievers: RetrieverSelection,    // Auto | Explicit
    fusion_config: Option<FusionConfig>,
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

### Routing rules (the knowledge layer)

Rule-based router. For each rule, if the conditions match, retrievers are selected with the noted weights:

```
Rule 1: Entity-anchored query
    Conditions: query.entity_anchor.is_some() OR NER finds entity in text
    Retrievers: Graph (weight 2.0), Semantic (1.0)
    + Lexical (0.5) if text is also present

Rule 2: Exact-term query
    Conditions: text matches /[A-Z0-9-]{2,}/ (IDs, codes)
                OR text is all-caps tokens
    Retrievers: Lexical (2.0), Semantic (0.5)

Rule 3: Time-filtered query
    Conditions: time_filter.is_some() OR text contains temporal expression
    Retrievers: + Temporal filter (no separate retriever; applied at filter stage)

Rule 4: Type-filtered query
    Conditions: kind_filter or predicate_filter present
    Effect: Filter chain narrows after retrieval

Rule 5: Default (free-text query)
    Conditions: text present, no other signals
    Retrievers: Semantic (1.0), Lexical (1.0)
```

Rules are applied non-exclusively: a query can match multiple rules. The router unions selected retrievers and uses the maximum weight per retriever across matching rules.

### Limits and budgets

The router enforces:
- Max retrievers per query: 3 (all of semantic, lexical, graph if matched).
- Max top_n per retriever: 200 (configurable).
- Query timeout: default 1 second; cancellable.
- Cost estimate: if estimated cost exceeds threshold, query is degraded (smaller top_n or fewer retrievers).

### Per-query override

The client can override the router's decision:

```rust
.retrievers(Explicit(vec![Retriever::Semantic, Retriever::Graph]))
.fusion_config(FusionConfig { k: 30, ..default })
```

The override is logged for audit.

## Filter chain

After retrieval and fusion, the result list passes through filters:

```
fused candidates
  → Type filter (kind, predicate)
  → Temporal filter (event_at or valid_from/valid_to within range)
  → Confidence filter (confidence ≥ threshold)
  → Tombstone filter (exclude tombstoned unless explicitly included)
  → Supersession filter (exclude superseded unless explicitly included)
  → Limit
```

Filters are applied in this order because:
- Type and confidence are cheap and aggressive (early dropout).
- Temporal requires field reads; medium cost.
- Tombstone and supersession need redb reads.

For typical queries, the filter chain removes 50-90% of fused candidates. The remaining are returned.

## Filter as retriever vs filter

Some filters could be implemented as retrievers (e.g., a "temporal retriever" that scans memories in a time range). the design here: only those that *produce* candidates from a corpus are retrievers; those that *narrow* a candidate set are filters.

This is a design call. The advantage of filter-only-after-fusion: filters are uniform and composable. The disadvantage: if a filter is very selective (e.g., "events in the last hour"), running it after fusion is wasteful — we'd retrieve many candidates and throw most away.

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
  RETRIEVERS:
    SemanticRetriever(weight=1.0, top_n=100, corpus=statements)
    LexicalRetriever(weight=0.7, top_n=100, query="meetings preferences")
    GraphRetriever(weight=2.0, top_n=50, anchor=priya, depth=1)
  FUSION: RRF(k=60)
  POST_FILTERS: kind in [Preference], confidence ≥ 0.5, !superseded
  LIMIT: 20
  ESTIMATED COST: 12ms
```

## Execution

The executor runs retrievers in parallel (each on its own task on the shard's executor), waits for all to complete (or timeout), then fuses and filters.

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
                // partial; proceed with what we have
                break;
            }
        }
    }
    
    let fused = fuse_rrf(retriever_outputs, plan.fusion.k, &plan.fusion.weights);
    let filtered = apply_filters(fused, &plan.post_filters);
    let limited = filtered.into_iter().take(plan.limit as usize).collect();
    
    QueryResult { items: limited, debug: ... }
}
```

## Streaming results

For large result sets (`limit > 100`), the executor streams results to the client. Each fused-and-filtered item is emitted as it passes the limit boundary. The client can stop reading early.

Streaming uses the wire protocol's SUBSCRIBE mechanism (section 03), with QueryRequest opcodes as event types.

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

the knowledge layer uses rule-based routing. future versions (deferred) will support learned routing: a small classifier trained on labeled queries (`query_text → preferred_retrievers`). The classifier is a feature; the rule-based fallback remains for cold start and ambiguous queries.

Labels come from:
- Click-through data (user picks a result; retrievers that surfaced it get credit).
- Explicit feedback (`/feedback` slash command in the SDK).
- Synthetic labels from a teacher LLM.

For the knowledge layer: rule-based. Document a clear path to future versions.
