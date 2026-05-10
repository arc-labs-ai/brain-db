# 06.01 HNSW Primer

A condensed introduction to HNSW for the reader who isn't already familiar with the algorithm. The full paper is [Malkov & Yashunin, "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs"](https://arxiv.org/abs/1603.09320).

## 1. The high-level idea

HNSW builds a hierarchical graph where:

- Vectors are nodes.
- Edges connect nodes to their nearby neighbors (in vector space).
- The graph has multiple **layers**: a small top layer with few nodes, larger middle layers, and a bottom layer with all nodes.

To search for nearest neighbors of a query:

- Start at the top layer's entry point.
- Greedily walk toward the query — visit neighbors, take the closest one.
- When you can't get closer in the current layer, descend to the next layer.
- Repeat until the bottom layer.
- At the bottom, do a more exhaustive search to find the actual top-K.

This combines:

- **Coarse navigation** in the upper layers (rapid traversal across the space).
- **Fine search** in the bottom layer (high-recall local exploration).

## 2. Why "hierarchical"

Without layers, navigating to the right region of the space takes many steps in a flat graph. Adding upper layers lets the search skip across the space quickly.

The number of layers is logarithmic in the index size. For 1M nodes, ~20 layers; for 10M, ~25.

The number of nodes per layer decreases exponentially upward. The bottom layer has all N nodes; the next has ~N/M; the next has ~N/M²; etc.

## 3. Why "small world"

A "small-world graph" is one with:

- Sparse connections (each node has few neighbors).
- Short path lengths (any two nodes are reachable in O(log N) hops).

HNSW's graph has these properties. It's specifically designed so that greedy search (always move to the closest neighbor) tends to converge on the true nearest neighbor.

The graph is also "navigable", meaning greedy search reliably finds nearby points. Not all small-world graphs are navigable; HNSW's construction algorithm ensures navigability.

## 4. Key parameters

Three parameters govern HNSW behavior:

- **`M`**: max edges per node per layer. Higher M = more memory, better recall, slower build.
- **`ef_construction`**: search width during insertion (how many candidates to consider when selecting neighbors for a new node). Higher = better-quality graph, slower build.
- **`ef_search`**: search width during query. Higher = better recall, slower query.

Brain's defaults: `M=16, ef_construction=200, ef_search=64`. Discussed in [`02_parameters.md`](02_parameters.md).

## 5. Insertion algorithm

To insert a new node:

1. Choose a random target layer L (using a probability distribution that decreases exponentially with L).
2. Greedy-search from the top layer's entry down to layer L+1, finding the closest node.
3. Starting at layer L, perform an `ef_construction`-wide search to find the new node's neighbors.
4. Add edges to the M closest neighbors.
5. Repeat for layers L-1, L-2, ..., 0 (the bottom).
6. If the new node is at a higher layer than the current top, update the entry point.

The cost of insertion is roughly `O(M × log(N) × distance_computations)`. For N=1M and default parameters, ~1 ms per insert.

## 6. Search algorithm

To find K nearest neighbors of query q:

1. Start at the top layer's entry point.
2. Greedy-search down through the layers: at each layer, find the closest node to q in this layer (starting from the entry point of this layer).
3. At layer 0 (bottom), perform an `ef_search`-wide beam search:
   a. Maintain a candidate set of size `ef_search`.
   b. Iteratively: pop the closest unvisited candidate, examine its neighbors, add to candidates if better than the current worst.
   c. Continue until no improvement.
4. Return the K closest nodes from the candidate set.

The cost of search is roughly `O(ef_search × log(N) × distance_computations)`. For N=1M and `ef_search=64`, ~1-3 ms per search.

## 7. Distance metric: cosine (dot product)

Brain's vectors are L2-normalized; cosine similarity equals the dot product:

```
similarity(a, b) = a · b   (when ||a|| = ||b|| = 1)
```

Higher similarity = closer. HNSW operates on similarity (or, equivalently, distance = 1 - similarity).

The hnsw_rs crate supports cosine distance directly.

## 8. The "Skip List" analogy

HNSW's hierarchical structure is similar in spirit to a [skip list](https://en.wikipedia.org/wiki/Skip_list):

- Skip list: a sorted linked list with random "express" links at higher levels.
- HNSW: a navigable graph with random "express" connectivity at higher layers.

The randomization ensures the structure is balanced on average without requiring explicit rebalancing.

## 9. The bottom layer

The bottom layer (layer 0) contains all N nodes. A search that reaches this layer with a good entry point can find true nearest neighbors quickly.

The bottom layer is the "ground truth"; upper layers exist only to find a good entry into the bottom layer.

## 10. Memory layout

Each HNSW node holds:

- A reference to the vector (in our case, a slot ID into the arena).
- For each layer the node is in, a list of edges (neighbor node IDs).

The hnsw_rs implementation stores:

- A flat array of nodes (indexed by HNSW-internal ID, not Brain's MemoryId).
- A mapping from HNSW-internal ID to Brain's MemoryId (and vice versa).
- For each layer, an array of edge lists.

For an N=1M index with `M=16` and ~20 layers, total HNSW memory is ~150 MB. Plus the per-node ID-mapping table (~16 MB).

## 11. Insertion order matters (a little)

The HNSW graph's quality depends slightly on insertion order. Random insertion gives a reasonable graph; sequential insertion (e.g., always inserting nodes that lie on a 1D manifold) can produce a less-navigable graph.

For Brain's typical workload, insertion order is effectively random — agents encode memories in a wide variety of "topics", so the vectors don't form pathological sequences.

## 12. Robustness

HNSW is robust to outliers and skewed distributions. Adding a few nodes very far from the rest doesn't break the index.

It's less robust to:

- Highly clustered data (many near-duplicates) — the M-edge cap may cause the graph to "underconnect" within clusters.
- Very low-dimensional embeddings (where most points are near-equidistant).

Brain's 384-dim vectors from BGE are well-distributed; HNSW handles them well.

## 13. Update support

HNSW supports:

- **Insert** as detailed above. O(M log N) per insert.
- **Delete** via tombstones (marking nodes as deleted; periodically rebuilding to actually remove). See [`05_deletion.md`](05_deletion.md).
- **Update** is not directly supported; updates are usually done as delete + insert.

## 14. Beyond the basics

The full HNSW paper covers nuances we don't dive into here:

- "Heuristic edge selection" — how to choose the best M edges for a new node.
- "Layer-0 over-connection" — the bottom layer typically has 2× M edges (denser graph for fine-grained search).
- "Pruning" — removing edges that aren't useful.

The hnsw_rs implementation handles these for us. We use sensible defaults; we don't tune the algorithm internals.

---

*Continue to [`02_parameters.md`](02_parameters.md) for parameter selection.*
