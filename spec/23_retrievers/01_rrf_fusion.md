# RRF Fusion

## Reciprocal Rank Fusion

the knowledge layer uses RRF to combine ranked lists from multiple retrievers into a single ranked output.

The formula:

```
RRF_score(d) = Σ_i  w_i / (k + rank_i(d))
```

Where:
- `d` is a candidate document.
- `i` iterates over retrievers that returned `d`.
- `w_i` is the weight of retriever `i` (default 1.0; tunable per-retriever or per-query).
- `rank_i(d)` is `d`'s 1-indexed rank in retriever `i`'s output.
- `k` is a smoothing constant (default 60).

Documents not present in retriever `i`'s output contribute 0 to the sum.

## Why this formula

RRF has three properties that make it the production-default for hybrid retrieval:

1. **Score-scale invariance.** It doesn't matter that cosine returns [0, 1] while BM25 returns unbounded positives. Only ranks are used.

2. **Stable under small score perturbations.** If document A is ranked 3rd and B is ranked 4th in semantic, but the underlying cosines are 0.812 and 0.811, RRF treats them as ranks 3 and 4 (not scores within ε of each other). This is robust to noise in scores.

3. **Smooths the tail.** With `k = 60`, the top result contributes `1/61 ≈ 0.0164`, the 10th contributes `1/70 ≈ 0.0143`. The ratio is ~1.15: rank 1 is only marginally more valuable than rank 10. This prevents one retriever from dominating fusion.

## Choice of `k`

`k = 60` is the canonical default (from the original Cormack et al. 2009 paper).

Increasing `k` (e.g., 120) flattens the curve further — better when retrievers have noisy individual rankings.

Decreasing `k` (e.g., 30) makes top results count more — better when retrievers are individually well-calibrated.

For the knowledge layer:
- `k = 60` is the default.
- Per-query override is allowed.
- The query router may select `k` based on query class (e.g., higher `k` for ambiguous queries where no single retriever is trusted; lower `k` for entity-anchored queries where graph is trusted).

## Per-retriever weights

Weights let operators tune the relative trust of retrievers:

```
RRF_config {
    semantic_weight: 1.0,
    lexical_weight: 1.0,
    graph_weight: 1.2,        # slightly trusted more
}
```

Equal weights are the default. Tuning weights requires evaluation data; the substrate provides metrics on per-retriever contribution to fused results to inform tuning.

## Per-query weights (query router)

The router can adjust weights based on query classification:

| Query type | Semantic | Lexical | Graph |
|---|---|---|---|
| Entity-anchored ("about Priya") | 0.8 | 0.5 | 2.0 |
| Exact-term ("ACME-1247") | 0.5 | 2.0 | 0.5 |
| Paraphrase-likely ("how does she feel about") | 1.5 | 0.5 | 1.0 |
| Default (ambiguous) | 1.0 | 1.0 | 1.0 |

These are starting points; deployments tune them on real query distributions.

## Top-N cut at each retriever

To bound fusion cost, each retriever returns at most `top_n` candidates (default 100). Items beyond rank 100 don't enter fusion.

This means a document ranked 250th in semantic but 1st in lexical will not get the lexical-rank-1 boost contributed alone. The cap matters; tuning `top_n` is a tradeoff between coverage and fusion cost.

For high-precision queries (single-result expected), `top_n = 20` is enough. For exploratory queries, `top_n = 200` is reasonable.

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

## Alternative: weighted-sum-after-normalization

Considered and rejected. Reasons:

- Cosine and BM25 distributions are not Gaussian; min-max normalization is unstable.
- Per-retriever calibration requires labeled data per deployment.
- RRF is simpler and benchmarks equivalent or better in published hybrid-retrieval evaluations.

We may revisit in future versions if specific use cases demand learned fusion. For the knowledge layer, RRF.

## Alternative: learned fusion

A learned fusion model (e.g., logistic regression or a small neural net) takes per-retriever scores + features → fused score. Better in some benchmarks; requires training data per deployment.

For the knowledge layer, RRF. For future versions (after we have labeled query traffic), we can ship a learned fusion option behind a config flag.

## Observability

Per-retriever metrics:
- Items contributed to top-10 of fused output (count).
- Average rank in fused output.
- Mean contribution (`w_i / (k + rank_i)`) to top-10.

These let operators see "the graph retriever contributed 70% of top results last week" — a signal that the weight may be too high if precision dropped, or just right if precision held.

Per-query trace (debug/admin only):
```
query: "what does Priya prefer"
retrievers invoked: semantic, graph
top result: statement_xyz
  semantic rank: 5  → contributes 1/(60+5) = 0.0154
  graph rank: 1     → contributes 1/(60+1) = 0.0164
  fused score: 0.0318
```
