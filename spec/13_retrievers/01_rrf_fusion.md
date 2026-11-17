# 13.01 RRF Fusion

## Reciprocal Rank Fusion

Brain uses RRF to combine ranked lists from multiple retrievers into a single ranked output.

The formula:

```
RRF_score(d) = Σ_i  w_i / (k + rank_i(d))
```

Where:
- `d` is a candidate document.
- `i` iterates over retrievers that returned `d`.
- `w_i` is the weight of retriever `i` (default 1.0; tunable per-retriever or per-query).
- `rank_i(d)` is `d`'s 1-indexed rank in retriever `i`'s output.
- `k` is a smoothing constant, sized per query from the candidate-pool width (see §"Choice of `k`").

Documents not present in retriever `i`'s output contribute 0 to the sum.

## Why this formula

RRF has three properties that make it the production-default for retrieval:

1. **Score-scale invariance.** It doesn't matter that cosine returns [0, 1] while BM25 returns unbounded positives. Only ranks are used.

2. **Stable under small score perturbations.** If document A is ranked 3rd and B is ranked 4th in semantic, but the underlying cosines are 0.812 and 0.811, RRF treats them as ranks 3 and 4 (not scores within ε of each other). This is robust to noise in scores.

3. **Smooths the tail.** With `k = 60`, the top result contributes `1/61 ≈ 0.0164`, the 10th contributes `1/70 ≈ 0.0143`. The ratio is ~1.15: rank 1 is only marginally more valuable than rank 10. This prevents one retriever from dominating fusion.

## Choice of `k`

`k = 60` is the canonical constant (from the original Cormack et al. 2009 paper), and it is the value Brain uses at full pool width. Increasing `k` flattens the curve — better when retrievers have noisy individual rankings. Decreasing `k` makes top results count more — better when the pool is small enough that rank 1 really is distinguishable from rank 10.

Brain sizes `k` **per query from the width of the candidate pool**, not from the query class (`crates/brain-planner/src/retrieval/fusion/logic.rs:29-43`):

| Candidate pool | `k` |
|---|---|
| < 200 | 15 |
| 200 – 1 999 | 30 |
| ≥ 2 000 | 60 |

The reason is the smoothing itself. At `k = 60` the ratio between a rank-1 and a rank-10 contribution is ~1.15, which is the desired flattening across a wide pool but is near-total erasure of ordering across a pool of twenty. Scaling `k` down with the pool keeps the smoothing proportionate to how many ranks there are to smooth.

The pool-derived value is what fusion actually runs with (`crates/brain-planner/src/retrieval/executor/logic.rs:634`), and it is reported per query as the `brain_query_fusion_k` histogram. A `fusion_config.k` supplied through the operator debug ops overrides it; a client `RECALL` cannot reach it.

Every downstream term that is expressed in RRF units — the recency boost in particular — must be sized with the **same** `k` fusion used, not with 60. One top-rank vote is `1/(k+1)`: ≈ 0.0164 at `k = 60` but 0.0625 at `k = 15`, so sizing a tie-breaker against 60 while fusion ran at 15 would make it four times too weak to break anything (`crates/brain-planner/src/retrieval/executor/logic.rs:739-751`).

## Cross-shard merge (namespace-wide RECALL)

RRF fuses per-retriever lanes on one shard. A **namespace-wide** RECALL
(`scope = Namespace`, see [`../05_operations/03_read_pipeline.md`](../05_operations/03_read_pipeline.md)
§"Recall scope") fans out to every shard and applies RRF a **second time to merge
the per-shard candidate pools**. Raw fused scores are normalized per shard and are
not comparable across shards, so the cross-shard merge keys on each hit's
**within-shard rank** — the one cross-comparable signal — with a fixed `k = 60`:
a rank-`r` hit contributes `1/(60 + r)`
(`crates/brain-ops/src/handlers/recall.rs:409-412,570-590`). The merge constant is
fixed rather than pool-derived because the per-shard pools it merges were each fused
at their own `k`, so no one shard's value is the right one for the merge. A memory is owned by exactly one shard, so
the pools' ids are disjoint; the merge dedups defensively (keeping the higher RRF
score), orders by the cross-shard RRF score (ties broken on memory id for
determinism), and truncates to the candidate-pool budget so the single downstream
shaping pass stays bounded regardless of shard count. Membership/precision shaping
then runs **once** over this merged pool, so `Single`/`Many`/`None` is decided
globally, never per shard.

## Per-retriever weights

Weights let the router express relative trust per retriever. The weight set is a four-field struct — the three lanes plus a `temporal` term that is not a lane (`crates/brain-planner/src/retrieval/router/logic.rs:123-138`):

```rust
PerRetrieverWeights {
    semantic: 1.0,
    lexical:  1.0,
    graph:    1.0,
    temporal: 0.5,   // post-fusion recency boost, not a retriever lane
}
```

Those are the **baseline** weights, used for the `Default` query class — all three lanes equally trusted. The per-class profiles below are where the bias lives; the baseline deliberately expresses no preference, because the class is what carries the evidence for one.

Tuning weights requires evaluation data. Brain emits per-retriever invocation, candidate-count and latency series to inform it; see §"Observability" for exactly what is and is not available.

## Recency ranking (the `temporal` weight)

`PerRetrieverWeights` carries a fourth, `temporal` weight (default `0.5`)
that is **not** a retriever lane — there is no temporal retriever — but a
post-fusion **event-time recency boost** folded into `fused_score` after
the filter chain and before rerank
(`crates/brain-planner/src/retrieval/executor/logic.rs:736-768`,
`crates/brain-planner/src/retrieval/recency.rs:11-44`).

It is a soft, additive, RRF-scale term applied to memory hits only:

```
boost(d) = temporal_weight · (1 / (k + 1)) · 0.5^(age(d) / half_life)
age(d)   = reference_time − event_time(d)        # saturates at 0 for future-dated events
event_time(d) = occurred_at(d)  ?? created_at(d)  # client event time, else write time
```

- **RRF-scale.** `1/(k+1)` is one top-rank retriever vote, so a brand-new
  memory at the default weight earns at most half of one such vote — a
  tie-breaker that re-orders comparably-relevant hits, never one that
  overrides genuine relevance. This is why `0.5` is the default
  (half-strength) and why the term is capped at the RRF unit. `k` here is
  the pool-derived value fusion actually used, not a fixed 60 (see
  §"Choice of `k`"); the boost scales with the smoothing it has to
  tie-break against.
- **Exponential decay** on event time with a default half-life of **90
  days**. Statements/relations are untouched (their bi-temporal validity
  is the filter chain's job).
- **Gated on a temporal signal.** The boost runs only when the query
  carries one — a temporal expression detected by the router, an explicit
  time filter, or an `as_of_record_time_unix_nanos` anchor — so timeless
  facts ("what's my wife's name") are never penalised for being old. The
  reference point is the `as_of` anchor when set, otherwise wall-clock now.

## Adaptive top-K from the router

The query router (see [`./05_retrieval_query.md`](./05_retrieval_query.md) §"Query router") classifies the incoming query and the planner turns that class into a **retrieval profile**: one weight set plus one per-retriever pool width, applied together. Weights decide whose rank-1 hit dominates; the pool width decides how deep each lane looks before fusion narrows it back down. The two move together because they answer the same question from opposite ends (`crates/brain-planner/src/retrieval/router/logic.rs:180-241`).

| Query class | Semantic | Lexical | Graph | Pool per retriever |
|---|---|---|---|---|
| Entity-anchored ("about Priya") | 1.0 | 0.7 | 2.0 | 50 |
| Exact-term ("ACME-1247") | 1.0 | 2.0 | 0.7 | 100 |
| Paraphrase-likely ("how does she feel about") | 1.5 | 1.0 | 0.5 | 200 |
| List / aggregation ("list every X") | 1.5 | 1.0 | 0.5 | 400 |
| Default (no text, filter-only) | 1.0 | 1.0 | 1.0 | 100 |

The `temporal` weight is `0.5` in every profile; no class changes it.

Two pairings carry the whole design and are worth reading as such:

- **Entity-anchored narrows the pool while raising graph.** When the anchor is known, the graph lane is the precision driver and a wide semantic pool only adds noise to fuse against — so the pool is the narrowest of any class at 50.
- **List/aggregation widens the pool without touching the weights.** It is the Paraphrase weight set at double the depth, because a set question fails from *coverage*, not from ranking: diversity cannot spread a member that never reached fusion. Hence 400.

De-rating the off-class lane rather than dropping it (0.7, 0.5 — never 0) is deliberate: a misclassified query degrades to a weaker ranking rather than to a missing lane.

Pool width is advisory in the sense that the operator debug ops can override it per query via `QueryRequest.fusion_config`; a client `RECALL` cannot.

## Top-N cut at each retriever

To bound fusion cost, each retriever returns at most `top_n` candidates, where `top_n` is the pool width of the query's profile — 50 to 400 across the classes above, 100 for `Default`. Items beyond that rank don't enter fusion at all.

This is the one cut in the pipeline that cannot be recovered downstream. A document ranked 250th in semantic but 1st in lexical enters fusion on the lexical rank alone if the pool is 200; if it ranked outside every lane's pool it is simply absent, and no later stage — filter chain, recency, rerank, diversity — can reintroduce it. Rerank re-orders the pool; it does not refill it. That asymmetry is why the list/aggregation class spends its budget on pool width rather than on weights.

The cap is where coverage is bought: a narrow pool is cheaper and less noisy, a wide pool is the only way a true answer that ranks deep in every individual lane can reach the answer set.

## Implementation sketch

```rust
fn fuse_rrf(
    retriever_outputs: Vec<RetrieverOutput>,
    k: f64,
    weights: &HashMap<&str, f64>,
) -> Vec<FusedItem> {
    let mut scores: HashMap<ItemId, f64> = HashMap::new();
    
    for output in &retriever_outputs {
        let w = weights.get(output.retriever_name).copied().unwrap_or(1.0);
        for (rank_minus_1, item) in output.items.iter().enumerate() {
            let rank = rank_minus_1 + 1;
            let contribution = w / (k + rank as f64);
            *scores.entry(item.id).or_insert(0.0) += contribution;
        }
    }
    
    let mut fused: Vec<_> = scores.into_iter()
        .map(|(id, score)| FusedItem { id, score, contributors: lookup_contributors(...) })
        .collect();
    fused.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    fused
}
```

`contributors` tracks which retrievers contributed to each fused result. Useful for debugging and observability ("this result came from semantic + graph, not lexical").

## Score-aware fusion methods

RRF is the shipped method, and it throws score magnitude away. That is its strength — and it has one pathology, which is why two score-aware methods ship beside it (`crates/brain-planner/src/retrieval/fusion/logic.rs:45-63`):

| Method | How it combines | Reads magnitude |
|---|---|---|
| `rrf` (shipped default) | `Σ w_i / (k + rank_i)` | No — rank only |
| `relative` | per-query min-max normalization per lane, then a weighted convex sum | Yes |
| `zscore` | per-query mean/σ normalization per lane, then a weighted convex sum | Yes |

**The pathology RRF has.** Because only rank counts, a near-perfect match found by one retriever and absent from the other two loses to a document that is middling in all three. One lane's rank 1 is worth `w/(k+1)`; three lanes' rank 5 is worth `3w/(k+5)`, which is larger. This is the coverage bias, and on a corpus where one lane is the only one that can see the answer — an exact identifier, or a paraphrase no lexical index will match — it is a wrong answer, not a slightly worse ordering.

**What the score-aware methods trade for fixing it.** Both normalize per query, inside each lane, before summing, so a strong single signal carries its magnitude through. `zscore` is the more robust of the two when score scales differ widely or carry outliers, since min-max pins to the single best and worst candidate in the batch. The cost is that cosine and BM25 distributions are not Gaussian and not bounded alike, so a normalization computed over one query's candidate set is only as stable as that set — a query returning three candidates normalizes across three points.

RRF stays the default because its failure mode is a worse ordering over a correct pool, while an unstable normalization's failure mode is an arbitrary one. Selection is deploy-time, not per-query: `[retrieval] fusion_method = "rrf" | "relative" | "zscore"`, process-wide, installed once at boot, with an unrecognized value falling back to `rrf` (`crates/brain-core/src/retrieval_config.rs:22-46`, `crates/brain-planner/src/retrieval/planner/logic.rs:392-401`). `k` applies only to `rrf`; the other two ignore it.

Three further knobs share that section and are all off by default: `hype_rrf` (fold HyPE hits in as their own rank list rather than a non-displacing append), `ef_occupancy_scaling` (scale `ef_search` by index occupancy on the memory probe) and `autocut` (relative-drop truncation of the ranked tail).

## Alternative: learned fusion

A learned fusion model (e.g., logistic regression or a small neural net) takes per-retriever scores + features → fused score. Better in some benchmarks; requires training data per deployment.

Brain ships RRF. After labeled query traffic accumulates, a future version can offer a learned fusion option behind a config flag.

## Observability

What `/metrics` carries, per shard and labelled by retriever (`crates/brain-server/src/metrics/format.rs:1098-1120`):

- `brain_retriever_invocations_total{retriever}` — how often each lane ran.
- `brain_retriever_candidates_total{retriever}` — how many candidates each lane produced.
- `brain_retriever_latency_ms{retriever}` — per-lane wall-time histogram.
- `brain_query_fusion_k` — the pool-derived `k` histogram, so an operator can see which smoothing band real traffic lands in.

What it does **not** carry: any per-retriever *contribution* series. There is no "items this lane contributed to the fused top-10", no mean `w_i / (k + rank_i)`, and no average fused rank per lane. The question those would answer — "the graph lane drove 70% of top results last week, is its weight too high?" — is not answerable from the scrape surface.

It is answerable per query. Every result carries `contributing_retrievers` with each lane's rank and raw score, and `QUERY_TRACE` returns the same breakdown with per-stage costs. That shape fits weight tuning, which is an investigation over a sampled query set, better than a continuously-scraped aggregate would: a contribution counter tells an operator that a weight moved the mix, while the trace tells them which documents moved and why.

Per-query trace (debug/admin only):
```
query: "what does Priya prefer"
retrievers invoked: semantic, graph
top result: statement_xyz
  semantic rank: 5  → contributes 1/(60+5) = 0.0154
  graph rank: 1     → contributes 1/(60+1) = 0.0164
  fused score: 0.0318
```
