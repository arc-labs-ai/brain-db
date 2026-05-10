# 06.02 HNSW Parameters

The three HNSW knobs and Brain's defaults.

## 1. The parameters

| Parameter | Default | Range | Effect |
|---|---|---|---|
| `M` | 16 | 4–64 | Max edges per node per layer (except bottom, which is 2M) |
| `ef_construction` | 200 | 50–500 | Search width during insertion |
| `ef_search` | 64 | 10–500 | Search width during query (per-call override possible) |

## 2. M = 16

`M` controls graph density:

- Higher M → more edges per node → better recall, slower build, more memory.
- Lower M → fewer edges → faster build, lower recall.

Empirically, M=16 is the sweet spot for 384-dim vectors. The original HNSW paper uses M=12-48 across benchmarks; 16 hits the middle.

Memory cost per node: 16 edges × 4 bytes per edge = 64 bytes per non-bottom layer × log2(N) layers + 32 edges × 4 bytes = 128 bytes for the bottom layer.

For N=1M nodes: ~150 MB. For N=10M: ~1.5 GB.

## 3. ef_construction = 200

Higher ef_construction produces a better-quality graph. The trade-off:

- ef_construction=100: faster build (~30 µs per insert), slightly lower-quality graph.
- ef_construction=200: balanced (~80 µs per insert), good-quality graph.
- ef_construction=500: slow build (~250 µs per insert), marginal quality gains.

200 is the standard recommendation. Brain uses it.

## 4. ef_search = 64 (per-query overridable)

`ef_search` is the search beam width. Higher values give better recall but slower queries:

| ef_search | recall@10 | typical latency |
|---|---|---|
| 16 | ~85% | ~0.5 ms |
| 32 | ~92% | ~1 ms |
| 64 | ~96% | ~2 ms |
| 128 | ~98% | ~4 ms |
| 256 | ~99% | ~8 ms |

These numbers assume a 1M-vector index. For smaller indexes, recall is higher at any given ef_search.

ef_search is overridable per query in `RECALL`, letting the agent trade latency for recall on demand.

## 5. The relationship to K

If a query asks for K results:

- `ef_search` must be >= K for HNSW to return K results.
- The convention is `ef_search = max(K, default_ef_search)`.

For K=100 with default ef_search=64, the substrate uses ef_search=100 for that query.

## 6. ef_construction and ef_search interaction

The two parameters are somewhat independent. A graph built with high ef_construction can be queried with low ef_search, but vice versa requires the graph to support fine search at the bottom layer. Brain's defaults (ef_construction=200, ef_search=64) are well-balanced.

## 7. Tuning

Tune ef_search per query for query-time quality. Tune ef_construction at deployment-config time for graph quality.

`M` is set at index creation and isn't easily changed; changing M requires rebuilding the entire index.

## 8. Configuration

```
[ann]
m = 16
ef_construction = 200
ef_search = 64
ef_search_max = 500           # cap on per-query overrides
```

## 9. Per-shard parameters

Each shard's HNSW uses these parameters. Different shards could in principle use different parameters, but in practice all shards use the cluster-wide defaults.

If an operator wants to experiment with different parameters on a shard, the procedure is to rebuild that shard's index with the new parameters (a heavy operation; see [`07_maintenance.md`](07_maintenance.md)).

## 10. The bottom-layer doubling

The bottom layer uses 2M edges per node by convention. This is the convention from the original HNSW paper; it produces denser local connectivity for fine-grained search. Brain follows this convention via hnsw_rs's defaults.

---

*Continue to [`03_insertion.md`](03_insertion.md) for the insertion algorithm.*
