# 09.01 HNSW Basics

A condensed introduction to HNSW and the three knobs that govern its behavior.

## HNSW Primer

A condensed introduction to HNSW for the reader who isn't already familiar with the algorithm. The full paper is [Malkov & Yashunin, "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs"](https://arxiv.org/abs/1603.09320).

### 1. The high-level idea

HNSW builds a hierarchical graph where:

- Vectors are nodes.
- Edges connect nodes to their nearby neighbors (in vector space).
- The graph has multiple **layers**: a small top layer with few nodes, larger middle layers, and a bottom layer with all nodes.

To search for nearest neighbors of a query:

- Start at the top layer's entry point.
- Greedily walk toward the query — visit neighbors, take the closest one.
- When no closer neighbor is reachable in the current layer, descend to the next layer.
- Repeat until the bottom layer.
- At the bottom, do a more exhaustive search to find the actual top-K.

This combines:

- **Coarse navigation** in the upper layers (rapid traversal across the space).
- **Fine search** in the bottom layer (high-recall local exploration).

### 2. Why "hierarchical"

Without layers, navigating to the right region of the space takes many steps in a flat graph. Adding upper layers lets the search skip across the space quickly.

The number of layers is logarithmic in the index size. For 1M nodes, ~20 layers; for 10M, ~25.

The number of nodes per layer decreases exponentially upward. The bottom layer has all N nodes; the next has ~N/M; the next has ~N/M²; etc.

### 3. Why "small world"

A "small-world graph" is one with:

- Sparse connections (each node has few neighbors).
- Short path lengths (any two nodes are reachable in O(log N) hops).

HNSW's graph has these properties. It's specifically designed so that greedy search (always move to the closest neighbor) tends to converge on the true nearest neighbor.

The graph is also "navigable", meaning greedy search reliably finds nearby points. Not all small-world graphs are navigable; HNSW's construction algorithm ensures navigability.

### 4. Key parameters

Three parameters govern HNSW behavior:

- **`M`**: max edges per node per layer. Higher M = more memory, better recall, slower build.
- **`ef_construction`**: search width during insertion (how many candidates to consider when selecting neighbors for a new node). Higher = better-quality graph, slower build.
- **`ef_search`**: search width during query. Higher = better recall, slower query.

Defaults: `M=16, ef_construction=200, ef_search=64`. Discussed in the Parameters section below.

### 5. Insertion algorithm

To insert a new node:

1. Choose a random target layer L (using a probability distribution that decreases exponentially with L).
2. Greedy-search from the top layer's entry down to layer L+1, finding the closest node.
3. Starting at layer L, perform an `ef_construction`-wide search to find the new node's neighbors.
4. Add edges to the M closest neighbors.
5. Repeat for layers L-1, L-2, ..., 0 (the bottom).
6. If the new node is at a higher layer than the current top, update the entry point.

The cost of insertion is roughly `O(M × log(N) × distance_computations)`. For N=1M and default parameters, ~1 ms per insert.

### 6. Search algorithm

To find K nearest neighbors of query q:

1. Start at the top layer's entry point.
2. Greedy-search down through the layers: at each layer, find the closest node to q in this layer (starting from the entry point of this layer).
3. At layer 0 (bottom), perform an `ef_search`-wide beam search:
   a. Maintain a candidate set of size `ef_search`.
   b. Iteratively: pop the closest unvisited candidate, examine its neighbors, add to candidates if better than the current worst.
   c. Continue until no improvement.
4. Return the K closest nodes from the candidate set.

The cost of search is roughly `O(ef_search × log(N) × distance_computations)`. For N=1M and `ef_search=64`, ~1-3 ms per search.

### 7. Distance metric: cosine (dot product)

Vectors are L2-normalized; cosine similarity equals the dot product:

```
similarity(a, b) = a · b   (when ||a|| = ||b|| = 1)
```

Higher similarity = closer. HNSW operates on similarity (or, equivalently, distance = 1 - similarity).

The hnsw_rs crate supports cosine distance directly.

### 8. The "Skip List" analogy

HNSW's hierarchical structure is similar in spirit to a [skip list](https://en.wikipedia.org/wiki/Skip_list):

- Skip list: a sorted linked list with random "express" links at higher levels.
- HNSW: a navigable graph with random "express" connectivity at higher layers.

The randomization ensures the structure is balanced on average without requiring explicit rebalancing.

### 9. The bottom layer

The bottom layer (layer 0) contains all N nodes. A search that reaches this layer with a good entry point can find true nearest neighbors quickly.

The bottom layer is the "ground truth"; upper layers exist only to find a good entry into the bottom layer.

### 10. Memory layout

Each HNSW node holds:

- A reference to the vector (in this case, a slot ID into the arena).
- For each layer the node is in, a list of edges (neighbor node IDs).

The hnsw_rs implementation stores:

- A flat array of nodes (indexed by HNSW-internal ID, not the public MemoryId).
- A mapping from HNSW-internal ID to MemoryId (and vice versa).
- For each layer, an array of edge lists.

For an N=1M index with `M=16` and ~20 layers, total HNSW memory is ~150 MB. Plus the per-node ID-mapping table (~16 MB).

### 11. Insertion order matters (a little)

The HNSW graph's quality depends slightly on insertion order. Random insertion gives a reasonable graph; sequential insertion (e.g., always inserting nodes that lie on a 1D manifold) can produce a less-navigable graph.

For the typical workload, insertion order is effectively random — agents encode memories in a wide variety of "topics", so the vectors don't form pathological sequences.

### 12. Robustness

HNSW is robust to outliers and skewed distributions. Adding a few nodes very far from the rest doesn't break the index.

It's less robust to:

- Highly clustered data (many near-duplicates) — the M-edge cap may cause the graph to "underconnect" within clusters.
- Very low-dimensional embeddings (where most points are near-equidistant).

The 384-dim vectors from BGE are well-distributed; HNSW handles them well.

### 13. Update support

HNSW supports:

- **Insert** as detailed above. O(M log N) per insert.
- **Delete** via tombstones (marking nodes as deleted; periodically rebuilding to actually remove). See [`02_hnsw_operations.md`](02_hnsw_operations.md).
- **Update** is not directly supported; updates are usually done as delete + insert.

### 14. Beyond the basics

The full HNSW paper covers nuances not detailed here:

- "Heuristic edge selection" — how to choose the best M edges for a new node.
- "Layer-0 over-connection" — the bottom layer typically has 2× M edges (denser graph for fine-grained search).
- "Pruning" — removing edges that aren't useful.

The hnsw_rs implementation handles these. Sensible defaults are used; the algorithm internals are not tuned.

## Parameters

The three HNSW knobs and their defaults.

### 15. The parameters

| Parameter | Default | Range | Effect |
|---|---|---|---|
| `M` | 16 | 4–64 | Max edges per node per layer (except bottom, which is 2M) |
| `ef_construction` | 200 | 50–500 | Search width during insertion |
| `ef_search` | 64 | 10–500 | Default search width per query |
| `ef_search_max` | 500 | ≥ `ef_search` | Ceiling on any derived or overridden beam |
| `ef_factor` | 1 | 1–16 | Multiplier deriving the beam from the requested `k` when no explicit `ef` is given |

### 16. M = 16

`M` controls graph density:

- Higher M → more edges per node → better recall, slower build, more memory.
- Lower M → fewer edges → faster build, lower recall.

Empirically, M=16 is the sweet spot for 384-dim vectors. The original HNSW paper uses M=12-48 across benchmarks; 16 hits the middle.

Memory cost per node: 16 edges × 4 bytes per edge = 64 bytes per non-bottom layer × log2(N) layers + 32 edges × 4 bytes = 128 bytes for the bottom layer.

For N=1M nodes: ~150 MB. For N=10M: ~1.5 GB.

### 17. ef_construction = 200

Higher ef_construction produces a better-quality graph. The trade-off:

- ef_construction=100: faster build (~30 µs per insert), slightly lower-quality graph.
- ef_construction=200: balanced (~80 µs per insert), good-quality graph.
- ef_construction=500: slow build (~250 µs per insert), marginal quality gains.

200 is the standard recommendation. The default.

### 18. ef_search = 64 (per-query overridable)

`ef_search` is the search beam width. Higher values give better recall but slower queries:

| ef_search | recall@10 | typical latency |
|---|---|---|
| 16 | ~85% | ~0.5 ms |
| 32 | ~92% | ~1 ms |
| 64 | ~96% | ~2 ms |
| 128 | ~98% | ~4 ms |
| 256 | ~99% | ~8 ms |

These numbers assume a 1M-vector index. For smaller indexes, recall is higher at any given ef_search.

`RECALL` carries no per-query `ef`. The beam is a deployment-tuned property
(`ef_search`, `ef_factor`, `ef_search_max`), not a knob a client turns: a caller
cannot know the corpus size or the index's occupancy, which are what decide
whether a wider beam buys anything, and exposing it would make the same cue
return different results depending on which client sent it. The planner derives
the beam from `k` and the configured factor.

### 19. The relationship to K, and `ef_factor`

HNSW recall depends on the beam being meaningfully **wider** than the result
set, not merely as wide. At Brain's shipped defaults `ef_search` and the default
top-K are **both 64**, so `ef == k` exactly — the degenerate configuration where
the search barely explores past the results it is about to return.

`ef_factor` exists to fix that, by deriving the beam from the requested `k` when
a query gives no explicit `ef`:

```text
ef = clamp(max(ef_search, k * ef_factor), k, ef_search_max)
```

**The default is `1`, which reproduces the prior behaviour exactly** — the beam
is whatever `ef_search` says and `k` does not widen it. It is a knob first and a
default change second: the factor raises read cost in proportion to `k`, so the
recall it buys has to be measured on a real corpus before the default moves. A
small-corpus A/B moved one hand-authored set by two supporting contexts out of
twelve and did nothing for a larger generated one, which is not enough to
justify changing it.

#### Most of the accepted range does nothing at the shipped defaults

The derived beam is clamped to `ef_search_max` (500), so the factor stops having
any effect once `k × ef_factor` crosses it. At the default `k = 64` that happens
at **8**:

```text
  k=64, ef_factor=7  ->  448   distinct
  k=64, ef_factor=8  ->  512 -> clamped to 500
  k=64, ef_factor=16 ->       clamped to 500   (identical to 8)
```

Validation nevertheless accepts `1..=16`, because the useful range depends on
`k` — a per-query value the validator cannot see, and at `k = 10` every factor
up to 16 is still distinct. The consequence is worth stating rather than
narrowing away: **at the default `k`, nine of the sixteen accepted values are
the same setting**, and raising the factor from 8 to 16 reads as a tuning change
while changing nothing. A narrower validated range would be wrong for small-`k`
callers; silently flattening half the range is the part worth knowing.

The floor still holds in every case: `ef >= k`, or HNSW cannot return `k`
results at all.

### 20. ef_construction and ef_search interaction

The two parameters are somewhat independent. A graph built with high ef_construction can be queried with low ef_search, but vice versa requires the graph to support fine search at the bottom layer. The defaults (ef_construction=200, ef_search=64) are well-balanced.

### 21. Tuning

Tune `ef_search` and `ef_factor` at deployment-config time for query-time
quality, and `ef_construction` for graph quality. Measure on your own corpus
before moving either: the published recall/latency curve is an ordering, not a
promise about your data.

`M` is set at index creation and isn't easily changed; changing M requires rebuilding the entire index.

### 22. Configuration

```toml
[hnsw]
m = 16
ef_construction = 200
ef_search = 64
# ef_factor = 1               # derive the beam from k: max(ef_search, k * ef_factor)
```

The section is named `[hnsw]`, not `[ann]`. `ef_search_max` (500) is fixed in
the binary rather than exposed: it is the ceiling the derived beam clamps to,
and letting a deployment raise it would let a large `k` turn one read into an
unbounded scan.

### 23. Per-shard parameters

Each shard's HNSW uses these parameters. Different shards could in principle use different parameters, but in practice all shards use the cluster-wide defaults.

#### 23.1 Statement HNSW

The typed graph's statement index is a second per-shard HNSW, populated by the `statement_embed` background worker (see [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md)). Statements are typically shorter and more thematically clustered than raw memory text, so the graph is built denser with a wider construction beam:

| Parameter | Statement HNSW | Memory HNSW (above) |
|---|---|---|
| `M` | 32 | 16 |
| `ef_construction` | 200 | 200 |
| `ef_search` | 128 | 64 |

The wider `ef_search` reflects that statement-corpus queries usually want higher recall — fewer candidates, but each one carries a typed payload the planner can join on. Memory cost per node scales with `M`, so a per-shard statement corpus of 1M rows runs ~300 MB for the graph alone.

The parameters live alongside the memory HNSW config block:

```
[ann.statements]
m = 32
ef_construction = 200
ef_search = 128
```

To experiment with different parameters on a shard, the procedure is to rebuild that shard's index with the new parameters (a heavy operation; see [`03_hnsw_lifecycle.md`](03_hnsw_lifecycle.md)).

### 24. The bottom-layer doubling

The bottom layer uses 2M edges per node by convention. This is the convention from the original HNSW paper; it produces denser local connectivity for fine-grained search. This implementation follows the convention via hnsw_rs's defaults.

### 25. Per-corpus parameters

Brain runs three HNSW corpora per shard — memory, entity, and statement — from byte zero, not on schema upload. Each corpus has different size, query frequency, and recall sensitivity, so the parameters differ by design rather than sharing a single default.

| Corpus | M | ef_construction | ef_search | Rationale |
|---|---|---|---|---|
| Memory | 16 | 200 | 64 | Default; largest corpus, balanced cost/quality |
| Entity | 16 | 100 | 64 | Smaller corpus; cheaper build, same search budget |
| Statement | 32 | 200 | 128 | Hot path for typed retrieval; pays for higher M and ef_search |

The entity corpus is typically 100–1000× smaller than the memory corpus, so the cheaper `ef_construction=100` keeps build cost low without meaningfully hurting recall on a sparse graph. The statement corpus carries typed payloads the planner joins on and serves the typed-retrieval hot path; the higher `M=32` and `ef_search=128` buy recall at a memory cost (~300 MB per 1M statements vs ~150 MB for the memory graph at M=16).

Per-corpus storage layout — file names, redb sibling tables, rebuild procedures — is documented in [`10. Metadata + Graph Store`](../10_metadata/02_table_layout.md) § 16.2.

---

*Continue to [`02_hnsw_operations.md`](02_hnsw_operations.md) for insertion, search, and deletion.*
