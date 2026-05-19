# 17 — Hybrid retrieval

When a schema is declared, `recall` does more than vector
search. It runs *multiple retrievers in parallel*, takes
each one's ranking, and merges them into a single ranked
result. This is **hybrid retrieval**, and it's why a
knowledge-active deployment usually has higher recall
quality than substrate-only on the same data.

This chapter explains the three retrievers Brain runs, how
their results get merged with a technique called **RRF**,
and how the router decides which retrievers to use for a
given query.

---

## Why one retriever isn't enough

A single retrieval algorithm has blind spots. Concretely:

| Query | Vector search | Lexical (BM25) | Graph (relations) |
|---|---|---|---|
| "How does Priya feel about meetings?" | ✅ paraphrase-friendly | ⚠️ only if literal words match | ⚠️ if Priya is anchored |
| "ACME-1247" | ⚠️ embedding fuzzes IDs | ✅ exact match | ❌ no anchor |
| "Everyone who reports to Priya" | ❌ | ❌ | ✅ traverses `reports_to` |
| "Recent issues with deployment" | ✅ semantic match | ✅ "deployment" exact token | ⚠️ if entities mentioned |
| "What did she say yesterday?" | ⚠️ "yesterday" poorly embedded | ✅ temporal filter | ⚠️ if "she" resolves |

Each retriever covers some query types well and others
poorly. The fused result is usually better than any
individual one because the strong retriever for each query
type pulls its weight while the weak ones contribute
nothing harmful (they just rank irrelevant candidates
low, which the fusion ignores).

---

## The three retrievers

### Semantic retriever

What it does: embed the query and find the most-similar
*vectors* in the index.

Backing index:

- **Memory HNSW** — the main vector index over memories.
- **Statement HNSW** — a separate index over the embeddings
  of statement text representations.

The semantic retriever produces a ranked list of memories
(and optionally statements) sorted by cosine similarity
to the embedded query.

Strengths: paraphrase, intent, semantic similarity. Asking
"how does Priya feel about meetings?" finds the
"async meetings" memory even though the words don't match.

Weaknesses: exact terms, IDs, anything where the literal
characters matter.

> **What's HNSW again?**
>
> Hierarchical Navigable Small World — the approximate
> nearest-neighbour algorithm Brain uses to find similar
> vectors in `O(log N)` instead of `O(N)`. Chapter 20
> covers it.

### Lexical retriever

What it does: do a classical text search over the memory
and statement text indexes.

Backing index:

- **`memory_text.tantivy/`** — BM25 index over memory text.
- **`statements.tantivy/`** — BM25 index over statement text
  representations.

The lexical retriever produces a ranked list sorted by
BM25 score — a classical text-search algorithm that
weights matches by how often the query's terms appear,
normalised by how rare each term is across the corpus.

Strengths: exact terms ("ACME-1247"), specific phrases,
filter-style queries ("memories from agent alice in the
last week").

Weaknesses: paraphrase, semantic similarity. The lexical
retriever has no idea that "async" and "asynchronous"
mean the same thing.

Chapter 21 covers BM25 in more detail.

### Graph retriever

What it does: walk the typed relations graph starting from
an entity anchor.

Backing index:

- The `relations` tables in redb (one per direction).
- The `statements_by_subject` index for finding statements
  anchored on an entity.

The graph retriever produces a ranked list of entities,
statements, or relations reachable from the anchor entity,
ranked by **proximity** (one-hop > two-hops > three-hops),
with relation confidence and direction as tiebreakers.

Strengths: entity-anchored questions ("who reports to
Priya?"), structural queries ("Priya's network").

Weaknesses: anything without an entity anchor.

The graph retriever is **only** invoked when the query has
an entity anchor (either passed explicitly via the
`entity_anchor` field or detected heuristically by the
router from the query text).

---

## RRF: combining the three rankings

The three retrievers each produce a ranked list. The
fusion algorithm is **Reciprocal Rank Fusion** (RRF), a
score-free, rank-only method.

The formula:

```
RRF_score(item) = Σ_i  w_i / (k + rank_i(item))
```

where:

- `i` iterates over the retrievers that returned `item`.
- `w_i` is the per-retriever weight (default 1.0).
- `rank_i(item)` is `item`'s 1-indexed rank in retriever
  `i`'s output (rank 1 is the best).
- `k` is a smoothing constant (default 60).
- Items not present in retriever `i`'s output contribute 0.

The intuition: each retriever **votes** for items by
ranking them. Items at the top of multiple retrievers'
lists accumulate high scores. The formula's smoothing
factor `k = 60` keeps any one retriever from dominating —
the difference between rank 1 and rank 10 is small
(`1/61 ≈ 0.0164` vs `1/70 ≈ 0.0143`), so a retriever
ranking your right answer 5th doesn't get drowned out by
another retriever ranking it 25th.

### Three properties of RRF

1. **Score-scale invariance.** Cosine returns `[-1, 1]`;
   BM25 returns unbounded positives; graph proximity is in
   its own range. RRF only uses *ranks*, so it doesn't
   matter that the underlying scores aren't comparable.
2. **Stability under small perturbations.** If two
   candidates have nearly-tied raw scores in semantic, but
   they're ranked 5 and 6, RRF treats them as ranks 5 and
   6. No ε-thresholding to debate.
3. **Tail smoothing.** Rank 1 contributes only ~15% more
   than rank 10. One retriever wouldn't dominate fusion
   just because it's slightly more confident than the
   others.

Chapter 21 covers RRF math in more detail.

> **Why `k = 60`?**
>
> The original Cormack et al. 2009 paper that introduced
> RRF benchmarked across many retrieval tasks and converged
> on `k = 60` as a strong default. Brain ships that value;
> per-query overrides are accepted if you have evaluation
> data suggesting different.

---

## The router

The router is the piece that decides *which* retrievers to
run for a given query and *what weights* to give them.
It's rule-based — a small set of feature-detecting rules
that classify the query and pick retrievers accordingly.

### Features the router extracts

```
ClassificationFeatures {
    has_text:                    bool
    has_entity_anchor:           bool
    has_time_filter:             bool
    has_predicate_filter:        bool
    contains_exact_id:           bool   ← regex: [A-Z][A-Z0-9]+-\d+
    is_all_caps_tokens:          bool   ← e.g., "ACME GTM"
    is_short_and_noun_heavy:     bool   ← ≤4 tokens, no ?
    is_question:                 bool   ← starts with what/who/how, contains ?
    contains_entity_mention:     bool   ← title-case heuristic
    contains_temporal_expression: bool   ← "yesterday", "last week"
}
```

These are cheap checks — regex matches and field-presence
tests. Total feature-extraction cost is microseconds.

### The routing rules

Five rules, each fires independently and *adds* retrievers
with weights:

1. **Entity-anchored** (`has_entity_anchor` or `contains_entity_mention`):
   - Graph weight 2.0
   - Semantic weight 1.0
   - Lexical weight 0.5 (if `has_text`)
2. **Exact-term** (`contains_exact_id` or `is_all_caps_tokens`):
   - Lexical weight 2.0
   - Semantic weight 0.5
3. **Temporal** (`has_time_filter` or `contains_temporal_expression`):
   - No retrievers added. Sets `temporal_pushdown = true`
     so the temporal filter goes into each retriever.
4. **Type/predicate filter** (`has_type_filter` or
   `has_predicate_filter`):
   - No retrievers added. Filters apply post-fusion.
5. **Default** (no other rule fired AND `has_text`):
   - Semantic weight 1.0
   - Lexical weight 1.0

### Combining rules

A query may match multiple rules. The router takes the
**max** weight, not the sum, across rules. So a query that
hits both "entity-anchored" (graph 2.0) and "default"
(semantic 1.0) ends up with graph 2.0 and semantic 1.0, not
graph 2.0 + lexical 1.0 + semantic 1.0 + lexical 1.0.

The max-not-sum rule means a query tripping every rule
doesn't get an absurd 5x boost on one retriever.

### Capping at three retrievers

Brain caps fan-out at three retrievers per query
(`MAX_RETRIEVERS = 3`). This is the budget — three parallel
retriever invocations is the upper bound of work per
query. If a query's rules picked more than three, the top
three by weight win.

For three retrievers each timing out at 50 ms, the
worst-case latency budget for the retrieve+fuse phase is
~150 ms.

---

## Filter pushdown

Filters live in two places:

- **Pre-filter (pushed into the retriever).** The retriever
  uses its native index to skip non-qualifying rows
  *during retrieval*. Cheap.
- **Post-filter (after fusion).** The fused result is walked
  and rows that don't qualify get dropped. More expensive,
  but works for filters retrievers can't push down.

The planner decides per filter:

- **Temporal pushdown.** Time-range filters push into every
  retriever — tantivy's range query, the graph retriever's
  time-bounded walk, the semantic retriever's metadata-side
  filter. *The* most important push-down.
- **Predicate / kind pushdown.** Push into semantic and
  graph retrievers; the lexical retriever handles its own
  filters natively.
- **Confidence / tombstone / supersession.** Always
  post-fusion; the filter chain walks the fused candidates
  and trims.

The fewer items survive post-fusion, the fewer redb lookups
the filter chain has to do. Temporal pushdown is usually
the win that makes the difference between a
sub-100-ms query and a multi-second one.

---

## Top-K cap per retriever

Each retriever returns at most `top_n` candidates (default
~100-200, computed from the request's `limit`). Items
beyond `top_n` don't enter fusion.

The consequence: **a document ranked 250th in semantic
but 1st in lexical gets only the lexical contribution.**
Semantic doesn't push it onto the list because semantic
never considered it.

This bound matters mostly for "long tail" queries — when
the right answer is buried deep in one retriever and
visible in another. For most queries, `top_n = 100`
captures everything that matters.

You can tune `top_k` per query if needed; production
defaults work for >95% of workloads.

---

## What a client sees

The response shape is identical to substrate-only
`recall`. A `RecallResponse` with a ranked list of hits.

What changes:

- **Items may be of different types.** The same response
  can contain memories, statements, and entities — RRF
  doesn't care what type each candidate is, only how
  retrievers ranked it. (Each hit carries a type
  discriminator so the client knows what kind of object
  it's looking at.)
- **Rankings are better.** For queries that hit the
  hybrid path's strengths (entity anchor, mixed exact and
  fuzzy terms), the top hits are usually more relevant.
- **A trace is available.** Calling `query_trace` returns
  the same result plus per-retriever metadata: which
  retrievers ran, what each returned, where each candidate
  came from. Used for debugging "why didn't I get the
  result I expected."

The hybrid path is transparent. A client written for
substrate-only `recall` doesn't have to change to get the
hybrid behaviour — declaring a schema flips the gate
([chapter 02](02-two-layer-model.md)) and recall starts
using the hybrid path under the hood.

---

## QUERY_EXPLAIN and QUERY_TRACE

Two diagnostic verbs that surface the planner and executor's
behaviour:

- **`query_explain(request)`** runs the planner but *not*
  the executor. Returns the plan: which retrievers got
  picked, what features fired, what weights got assigned,
  what filters land where. No data is fetched.
- **`query_trace(request)`** runs the full query *and*
  attaches the explanation plus execution metadata:
  per-retriever timings, item counts, per-stage filter
  survivor counts. Heavyweight; use for debugging only.

When something looks wrong with a query's results, the
fastest path to diagnosis is `query_trace` — you can see
exactly which retriever surfaced which item, and where
the filter chain dropped candidates.

---

## Recap

- The hybrid path is what `recall` does when the schema
  gate is on.
- Three retrievers run in parallel — semantic (vector),
  lexical (BM25), graph (typed relations) — each strong on
  different query shapes.
- **RRF** fuses the three rankings into one by treating
  each retriever as a voter. `k = 60`. Score-scale
  invariant; tail-smoothed.
- The **router** picks retrievers and weights per query
  based on rule-based feature detection. Cap of three
  retrievers per query.
- **Filter pushdown** is the biggest performance win —
  temporal especially.
- The response shape is the same as substrate-only
  `recall`; the ranking is just better for queries that
  benefit from multiple signals.

---

## Where to go next

- **What HNSW is doing under the semantic retriever:**
  [chapter 20](20-indexes-exact-vs-approximate.md).
- **BM25 and the lexical side:** [chapter 21](21-lexical-and-fusion.md).
- **The verbs themselves:** [chapter 16](16-cognitive-operations.md).
- **The architecture-tier deep dive:**
  [`../architecture/11-hybrid-retrieval-rrf.md`](../architecture/11-hybrid-retrieval-rrf.md)
  and [`../architecture/12-query-router.md`](../architecture/12-query-router.md).
