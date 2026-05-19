# 11 — Hybrid retrieval (RRF)

**Audience:** anyone tuning recall quality, debugging "why
didn't this query return X," weighing retriever weights, or
adding a new retriever.

**Goal:** by the end you should know what each retriever does,
how RRF fuses their outputs into one ranking, why the fusion
constant `k = 60`, and how the per-query weights and `top_k`
caps interact.

This chapter assumes [04 — HNSW](04-hnsw-index.md) (the
semantic retriever's index), [09 — Knowledge layer](09-knowledge-layer.md)
(the graph retriever's tables, the statement tantivy index for
lexical), and [10 — Extractors](10-extractors.md) (what
populates them). The router lives in
[chapter 12](12-query-router.md); this chapter is the
*retrievers + fusion* half.

---

## What hybrid retrieval is

A single retriever has blind spots. A semantic search over
embeddings is good at paraphrase and intent ("how does she feel
about meetings") and bad at exact terms ("ACME-1247"). A
lexical search over BM25 is the inverse. A graph traversal over
typed relations is good at "everyone who reports to Priya" and
useless on free text.

Hybrid retrieval runs *multiple retrievers in parallel*, takes
each one's ranked output, and fuses them into a single ranking
that draws on all three strengths.

```
        QUERY
          │
          ├──────────────► Semantic retriever ──► Vec<RankedItem>
          │                  (HNSW over embeddings)
          │
          ├──────────────► Lexical retriever ───► Vec<RankedItem>
          │                  (tantivy BM25)
          │
          └──────────────► Graph retriever ─────► Vec<RankedItem>
                            (typed relation walk)

                                  │
                                  ▼
                        ┌─────────────────────┐
                        │   RRF fusion        │
                        │   k = 60            │
                        │   weights per       │
                        │   retriever         │
                        └──────────┬──────────┘
                                   │
                                   ▼
                              fused ranking
                              (Vec<FusedItem>)
```

The router ([chapter 12](12-query-router.md)) picks which
retrievers to invoke and what weights to give them. The
retrievers run in parallel. The fusion step is deterministic
and lock-free.

---

## The three retrievers

All three are object-safe traits living in `brain-index`:

| Trait | What it queries | Backing index |
|---|---|---|
| `SemanticRetriever` | Embedding similarity. | Memory HNSW + (optionally) statement HNSW. |
| `LexicalRetriever` | Free-text and phrase queries with filters. | Two tantivy indexes (`memory_text.tantivy/`, `statements.tantivy/`). |
| `GraphRetriever` | Entity-anchored relation traversal. | `relations_*` and `statements_by_subject` tables in redb. |

Each trait method returns a `Vec<RankedItem>` — the same shape
across all three retrievers. That's what makes fusion uniform.

A `RankedItem`
(`crates/brain-index/src/tantivy_shard/retriever.rs:81`):

```rust
pub struct RankedItem {
    pub id: RankedItemId,
    pub rank: u32,
    pub score: f32,
    pub snippet: Option<String>,
}
```

`RankedItemId` is an enum across `Memory`, `Statement`,
`Entity`, `Relation` — fusion can mix types if multiple
retrievers return different shapes for the same query.

### Semantic retriever

`SemanticRetriever`
(`crates/brain-index/src/semantic_retriever.rs:40`):

```rust
pub trait SemanticRetriever: Send + Sync {
    fn retrieve(
        &self,
        query: &SemanticQuery,
        scope: SemanticScope,
        config: &SemanticRetrieverConfig,
    ) -> Result<Vec<RankedItem>, SemanticError>;
}
```

The production implementation (`BrainSemanticRetriever`) wraps:

- The embedder (from `OpsContext::executor.dispatcher`), to
  embed the query text on demand.
- The memory HNSW ([chapter 04](04-hnsw-index.md)) for the
  default `memory` scope.
- Optionally the statement HNSW ([chapter 09](09-knowledge-layer.md))
  for the `statement` scope.
- A reference to the `MetadataDb` for joining HNSW hits back to
  metadata rows (filters operate on this side).

Defaults
(`crates/brain-index/src/semantic_retriever.rs:33`):
`top_k = 64`, `ef_search = 64` (from
[chapter 04](04-hnsw-index.md)), 50 ms timeout.

The scope picks *which* HNSW to query. A semantic query for
memories goes to the memory HNSW; one for statements goes to
the statement HNSW. A "both" scope queries both indexes and
merges results — useful when the query is genuinely
type-agnostic ("anything about Priya's preferences").

### Lexical retriever

`LexicalRetriever`
(`crates/brain-index/src/tantivy_shard/retriever.rs:29`):

```rust
pub trait LexicalRetriever: Send + Sync {
    fn retrieve(
        &self,
        query: &LexicalQuery,
        scope: LexicalScope,
        config: &LexicalRetrieverConfig,
    ) -> Result<Vec<RankedItem>, LexicalError>;
}
```

Backed by tantivy. A `LexicalQuery`
(`crates/brain-index/src/tantivy_shard/retriever.rs:38`)
carries:

- **`terms`** — free-text tokens, OR-combined, ranked by BM25.
- **`phrase_clauses`** — exact-adjacency phrases, AND-ed
  against the term set.
- **`filters`** — agent, memory kind, statement kind,
  predicate, confidence bucket, time range.

Defaults
(`crates/brain-index/src/tantivy_shard/retriever.rs:69`):
`top_k = 64`, `bm25_k1 = 1.2`, `bm25_b = 0.75`, no min-score
floor, 50 ms timeout.

BM25's parameters are tantivy's standard defaults. We don't
tune them per deployment — they sit at the values the IR
literature has settled on for English over decades. If you
*do* want to tune them, the config struct exposes both — most
operators shouldn't.

The scope picks `memory_text.tantivy/` vs
`statements.tantivy/`. Same two-index distinction as the
semantic retriever, same merge-results-in-both pattern.

### Graph retriever

`GraphRetriever`
(`crates/brain-index/src/graph_retriever.rs:29`):

```rust
pub trait GraphRetriever: Send + Sync {
    fn retrieve(
        &self,
        query: &GraphQuery,
        scope: GraphScope,
        config: &GraphRetrieverConfig,
    ) -> Result<Vec<RankedItem>, GraphError>;
}
```

Walks relations and statement-by-subject indexes anchored on an
`EntityId`. A `GraphQuery` specifies:

- An anchor `EntityId` ("Priya").
- A `Direction` (`Outgoing`, `Incoming`, `Both`).
- An optional set of relation types to filter to (e.g., only
  `reports_to`).
- A `depth` cap (default 1 — direct neighbours only).
- An optional `max_branching` per hop.

Defaults
(`crates/brain-index/src/graph_retriever.rs:13` + `:100`):
`top_k = 64`, `max_depth = 4` hard cap (with a configurable
soft cap), 50 ms timeout.

The scoring is **proximity-based** — entities one hop from the
anchor rank higher than two hops, weighted by relation type's
confidence and direction. Pure structural; no embeddings, no
BM25.

This is the retriever that *only* makes sense in the
knowledge-active mode — the substrate's memory-to-memory edges
in `EDGES_OUT_TABLE` aren't typed relations between entities.
Substrate-only queries never invoke this retriever.

### Why three retrievers, not one

Each retriever's blind spots are the next one's strengths.
The table:

| Query | Semantic | Lexical | Graph |
|---|---|---|---|
| "How does Priya feel about meetings?" | ✅ paraphrase | ⚠️ matches if literal | ⚠️ only "Priya" anchor, not feeling |
| "ACME-1247" | ⚠️ embedding is fuzzy | ✅ exact token | ❌ no anchor |
| "Everyone who reports to Priya" | ❌ | ❌ | ✅ traverse `reports_to` |
| "Recent issues with deployment" | ✅ semantic match | ✅ "deployment" token | ⚠️ if "issue" entity is anchored |
| "What did she say yesterday?" | ⚠️ "yesterday" is poorly embedded | ✅ temporal filter | ⚠️ if entity resolution finds "she" |

A fused query covers all three patterns at once.

---

## Reciprocal Rank Fusion

RRF is the fusion algorithm. The full formula:

```
RRF_score(d) = Σ_i  w_i / (k + rank_i(d))
```

- `d` is a candidate item (memory, statement, entity, relation).
- `i` iterates over retrievers that returned `d` in their top
  `top_k`.
- `w_i` is the per-retriever weight (default 1.0; router or
  config overrides).
- `rank_i(d)` is `d`'s 1-indexed rank in retriever `i`'s output.
- `k` is the smoothing constant (default 60).

Items not present in retriever `i`'s top `top_k` contribute 0
from that retriever.

### Three properties

RRF earns its keep because of three properties:

**1. Score-scale invariance.** Cosine similarity returns
`[-1, 1]`; BM25 returns unbounded positives; the graph
retriever's proximity score is whatever bounded range we pick.
RRF doesn't care — it only uses *ranks*. There is no
normalisation step, no per-retriever calibration, no
deployment-specific scaling.

**2. Stable under small score perturbations.** If documents A
and B have cosine 0.812 and 0.811 — practically a tie — but
ranked 3rd and 4th, RRF treats them as ranks 3 and 4 and never
worries about the ε. The retriever's job is to order; the
fusion's job is to combine orderings, not scores.

**3. Smooths the tail.** With `k = 60`, the top result
contributes `1/(60+1) = 1/61 ≈ 0.0164`. The 10th contributes
`1/(60+10) = 1/70 ≈ 0.0143`. Ratio ~1.15 — rank 1 is only
marginally more valuable than rank 10. **One retriever can't
dominate fusion** just by being slightly more confident; the
others get equal-ish voice in the top results.

### Why `k = 60`

The Cormack et al. 2009 paper introducing RRF benchmarked
across many retrieval tasks and converged on `k = 60` as the
canonical default. We ship that
(`crates/brain-planner/src/knowledge/fusion.rs:23`):

```rust
pub const DEFAULT_K: u32 = 60;
```

Larger `k` (e.g., 120) flattens the curve further — better when
retrievers are individually noisy. Smaller `k` (e.g., 30) makes
the top contribute more — better when retrievers are
well-calibrated. Per-query overrides exist; per-deployment
overrides are a config switch. Most deployments stick with 60.

### The implementation

`fuse_rrf`
(`crates/brain-planner/src/knowledge/fusion.rs:62`):

```rust
pub fn fuse_rrf(
    outputs: &[(Retriever, Vec<RankedItem>)],
    k: u32,
    weights: &PerRetrieverWeights,
) -> Vec<FusedItem> {
    let k_f = f64::from(k);
    let mut accum: HashMap<RankedItemId, FusedItem> = HashMap::new();

    for (retriever, items) in outputs {
        let w = f64::from(weight_for(*retriever, weights));
        for item in items {
            let rank = f64::from(item.rank);
            let contribution = w / (k_f + rank);
            let entry = accum.entry(item.id).or_insert_with(|| FusedItem {
                id: item.id,
                fused_score: 0.0,
                contributing: Vec::new(),
            });
            entry.fused_score += contribution;
            entry.contributing.push(RetrieverContribution {
                retriever: *retriever,
                rank: item.rank,
                raw_score: item.score,
            });
        }
    }

    let mut out: Vec<FusedItem> = accum.into_values().collect();
    out.sort_by(|a, b| {
        b.fused_score
            .partial_cmp(&a.fused_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| id_sort_key(&a.id).cmp(&id_sort_key(&b.id)))
    });
    out
}
```

Four notes:

- **`HashMap<RankedItemId, FusedItem>`** — the dedup happens
  here. Two retrievers ranking the same item merge into one
  `FusedItem` with both contributions summed.
- **`FusedItem.contributing`**
  (`crates/brain-planner/src/knowledge/fusion.rs:27`) — every
  fused item records *which* retrievers contributed and at what
  rank/score. This is what `EXPLAIN` and `TRACE`
  ([chapter 12](12-query-router.md)) surface to operators.
- **Deterministic tie-break**
  (`crates/brain-planner/src/knowledge/fusion.rs:111`) — when
  two items have the same fused score (rare but possible at
  small `top_k` with symmetric weights), the 17-byte
  `id_sort_key` breaks ties by `(type tag, id bytes)`. Same
  query → same response, byte-for-byte.
- **Pure, sync function.** No I/O. The async work is in the
  retrievers; fusion is a hot loop over already-collected
  results.

---

## Per-retriever weights

Default weights are equal at 1.0
(`crates/brain-planner/src/knowledge/router.rs:96`):

```rust
pub struct PerRetrieverWeights {
    pub semantic: f32,  // 1.0
    pub lexical: f32,   // 1.0
    pub graph: f32,     // 1.0
}
```

The router ([chapter 12](12-query-router.md)) adjusts weights
per query based on classification rules:

| Query type | Semantic | Lexical | Graph |
|---|---|---|---|
| Entity-anchored ("about Priya") | 1.0 | 0.5 | 2.0 |
| Exact-term ("ACME-1247") | 0.5 | 2.0 | 0.5 |
| Default (ambiguous) | 1.0 | 1.0 | 1.0 |

The full rule set lives in
`crates/brain-planner/src/knowledge/router.rs:165`. The weight
chosen by a rule is *maxed*, not summed, across rules — a query
that matches two rules gets the higher weight per retriever,
not the sum.

Operators can override weights per-deployment in the schema's
fusion config, or per-query in a `RECALL_HYBRID` request. The
defaults are reasonable; tuning weights requires labelled
evaluation data and is rarely worth the work below 10K queries
a day.

---

## The `top_k` cap at each retriever

To bound fusion cost, each retriever returns at most `top_k`
candidates (default 64). Items beyond rank `top_k` don't enter
fusion at all.

This has a real consequence: **a document ranked 250th in
semantic but 1st in lexical gets only the lexical contribution
in the fused score** — semantic's contribution is 0 because
the item wasn't in semantic's top 64.

Tuning `top_k`:

- **High-precision queries** (single-result expected,
  ID-shaped) — `top_k = 20` is plenty. The right answer is
  ranked 1st by *some* retriever; you don't need the long
  tail.
- **Exploratory queries** ("anything related to X") —
  `top_k = 100` or `200` is reasonable. Longer tails let
  retrievers with partial coverage still contribute.

The router doesn't dynamically tune `top_k` in v1 — operators
set the default in `*RetrieverConfig` and per-query overrides
are accepted via the wire.

---

## What gets returned

A `FusedItem`
(`crates/brain-planner/src/knowledge/fusion.rs:27`):

```rust
pub struct FusedItem {
    pub id: RankedItemId,
    pub fused_score: f64,
    pub contributing: Vec<RetrieverContribution>,
}

pub struct RetrieverContribution {
    pub retriever: Retriever,
    pub rank: u32,
    pub raw_score: f32,
}
```

The `contributing` list is what makes the fusion *observable*.
A debug-mode response (or `QUERY_TRACE` opcode) shows each
fused result with:

```
top result: statement_xyz
  contributing:
    semantic   rank 5  raw_score 0.812
    graph      rank 1  raw_score 0.94
  fused_score: 0.0318
```

Operators reading a trace can answer "which retriever brought
this in" without instrumenting the system. Same data is
aggregated into the per-retriever metrics (next section).

---

## When the hybrid path runs

Three triggers, in increasing specificity:

- **`RECALL` (substrate verb) with a schema declared.** The
  RECALL handler checks `SchemaGate::is_declared()`
  ([chapter 09](09-knowledge-layer.md)). If true, it fans out
  through the hybrid path; if false, pure-substrate HNSW
  search.
- **`RECALL_HYBRID` (explicit knowledge opcode).** Always uses
  the hybrid path; fails with `SchemaNotDeclared` if no schema
  has been declared on this shard.
- **`QUERY` (knowledge-shaped structured query).** Full
  request shape with `entity_anchor`, `kind_filter`,
  `predicate_filter`, etc. Routes through the router, fans out
  to selected retrievers, fuses.

`QUERY_EXPLAIN` and `QUERY_TRACE` are the diagnostic variants
— same plan, with extra metadata in the response. Covered in
[chapter 12](12-query-router.md).

---

## Failure modes

**One retriever errors.** The planner drops that retriever's
output and runs fusion with what's left. The audit response
records which retrievers failed and why. A failed retriever
isn't a request failure unless *every* retriever fails.

**One retriever times out** (default 50 ms per retriever). Same
handling as error — the partial output is dropped, fusion runs
on the others. A timeout at the retriever layer is intentional:
we'd rather return a partial fused result than block the whole
query.

**Empty fused result.** No retriever returned anything. The
response is a valid empty list — not an error. The
`contributing` view shows that each retriever returned 0 items.

**Score collision.** Two items have the same `fused_score`.
The deterministic tie-break (17-byte `id_sort_key`) decides
the order. Same query, same response, byte-for-byte.

**Semantic and lexical disagree heavily.** Common, not a bug.
The `top_k = 64` cap per retriever may exclude some items from
fusion; per-retriever metrics show whether one retriever is
consistently shut out, which is the signal to widen `top_k` or
tune weights.

**Graph retriever invoked without an entity anchor.** Returns
empty (no items). The router shouldn't pick the graph
retriever in this case
([chapter 12](12-query-router.md)); if it does, fusion just
ignores the empty output.

**No retrievers run at all.** Router decided every rule failed.
Returns an empty result with a `NoRetrieversSelected` reason in
the trace. Rare — the default rule fires for any query with
text.

---

## Configuration & tuning

| Knob | Where | Default | Notes |
|---|---|---|---|
| `DEFAULT_K` (RRF smoothing) | `crates/brain-planner/src/knowledge/fusion.rs:23` | 60 | Per-query override accepted. |
| Per-retriever `top_k` | `*RetrieverConfig` | 64 | Larger = more fusion candidates, slower fusion. |
| Per-retriever weights | `PerRetrieverWeights` | 1.0 each | Router adjusts per-query; operator overrides per-deployment. |
| Per-retriever timeout | `*RetrieverConfig.timeout_ms` | 50 ms | Partial-result on timeout. |
| BM25 `k1`/`b` | `LexicalRetrieverConfig` | 1.2 / 0.75 | Tantivy / IR-literature defaults. |
| Graph `max_depth` cap | `GraphRetrieverConfig.max_depth` | 4 hard cap | Soft cap configurable below. |
| HNSW `ef_search` | `SemanticRetrieverConfig.ef_search` | 64 | Same as the underlying HNSW; per-query overridable. |

Operational rules:

- **Default weights are fine for most deployments.** The
  router's per-query weighting handles common patterns
  (entity-anchored / exact-term / default). Tune weights only
  with labelled query traffic.
- **Raise `top_k` for exploratory workloads, not for "more
  results."** A `top_k = 200` doesn't make the top-10 more
  accurate; it just makes more items eligible to enter the
  fused tail.
- **`k = 60` is rarely worth changing.** Larger flattens
  responsiveness to retriever confidence; smaller risks one
  retriever dominating.
- **Watch the per-retriever contribution metrics.** A
  retriever consistently providing < 5% of top-10 results is a
  signal that it's mis-weighted or its index is empty.
- **Per-retriever timeout floors the query budget.** Three
  retrievers × 50 ms = 150 ms worst-case parallel fan-out, plus
  fusion (~1 ms) and filters (~ms). Lower the timeout if your
  end-to-end budget is tighter.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| `SemanticRetriever` trait + config | `crates/brain-index/src/semantic_retriever.rs` |
| `LexicalRetriever` trait + config, tantivy backend | `crates/brain-index/src/tantivy_shard/retriever.rs` |
| `GraphRetriever` trait + config | `crates/brain-index/src/graph_retriever.rs` |
| `RankedItem`, `RankedItemId` | `crates/brain-index/src/tantivy_shard/retriever.rs` |
| Production semantic retriever (`BrainSemanticRetriever`) | `crates/brain-ops/src/ops/semantic_retriever.rs` |
| Production graph retriever (`BrainGraphRetriever`) | `crates/brain-ops/src/ops/graph_retriever.rs` |
| Tantivy retriever impl | `crates/brain-index/src/tantivy_shard/` |
| RRF fusion (`fuse_rrf`, `DEFAULT_K`, `FusedItem`) | `crates/brain-planner/src/knowledge/fusion.rs` |
| Router (`PerRetrieverWeights`, `route`) | `crates/brain-planner/src/knowledge/router.rs` |
| Filter chain (applied post-fusion or pushed down) | `crates/brain-planner/src/knowledge/filters.rs` |
| Hybrid planner / executor (DAG build, retriever fan-out) | `crates/brain-planner/src/knowledge/planner.rs`, `executor.rs` |
| EXPLAIN / TRACE output | `crates/brain-planner/src/knowledge/explain.rs` |

---

## Further reading

- [04 — HNSW index](04-hnsw-index.md) for the semantic
  retriever's backing index.
- [09 — Knowledge layer](09-knowledge-layer.md) for the
  redb tables the graph retriever walks and the tantivy
  indexes the lexical retriever queries.
- [10 — Extractors](10-extractors.md) for what populates all
  three retrievers' source data.
- [12 — Query router](12-query-router.md) for how the planner
  picks retrievers and weights per query, plus filter
  pushdown and the EXPLAIN/TRACE diagnostic verbs.
