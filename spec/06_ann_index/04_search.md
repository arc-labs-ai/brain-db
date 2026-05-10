# 06.04 Search

The procedure for finding nearest neighbors of a query vector.

## 1. The search call

```rust
fn search(
    index: &HnswIndex,
    query: &[f32; 384],
    k: usize,
    ef: usize,
    filter: Option<AnnFilter>,
) -> Vec<(MemoryId, f32)> {
    let raw_results = index.hnsw.search(query, k_extended, ef);
    let results = raw_results.into_iter()
        .filter_map(|r| {
            let memory_id = index.id_map_reverse.get(&r.d_id)?;
            if let Some(filter) = &filter {
                if !filter.matches(memory_id) {
                    return None;
                }
            }
            Some((*memory_id, 1.0 - r.distance))  // distance → similarity
        })
        .take(k)
        .collect();
    results
}
```

Note `k_extended` — when filters are active, the substrate may request more raw results than k from HNSW because filtering may discard some. Discussed in [`09_filtering.md`](09_filtering.md).

## 2. The HNSW search algorithm

Internally, hnsw_rs implements:

1. Start at the entry point (top layer).
2. At each layer above 0, greedy-search: visit the closest neighbor, take it as the new starting point. Stop when no neighbor improves on the current node.
3. At layer 0, beam search with width `ef`: maintain a candidate set of size `ef`; iteratively expand the most promising candidate; add neighbors that beat the worst in the set.
4. Return the K closest from the candidate set.

The greedy walk through upper layers is fast (O(log N) hops); the beam search at layer 0 is the expensive part (O(ef × M) distance computations).

## 3. Distance computation

For 384-dim normalized vectors, the distance between query q and node v is:

```
distance = 1 - dot_product(q, v)
        = 1 - sum(q[i] * v[i] for i in 0..384)
```

The dot product uses SIMD (AVX2 on x86, NEON on ARM):

- AVX2: 8 floats per FMA instruction → 48 instructions per dot product.
- NEON: 4 floats per FMA → 96 instructions.

Per dot product, ~50 ns on modern x86 with AVX2. A search visiting 1000 nodes takes ~50 µs of pure distance computation, plus traversal overhead.

## 4. Search latency breakdown

For ef_search=64 on a 1M-node index:

| Phase | Cost |
|---|---|
| Top-layer greedy traversal | ~100 ns |
| Layer 1-N greedy traversal | ~5 µs (log N hops × distance) |
| Layer 0 beam search | ~1-2 ms (ef × M distance computations) |
| ID mapping and filtering | ~10 µs |
| Result sorting | ~1 µs |
| **Total** | **~1-2 ms** |

The bottom-layer beam search dominates. Reducing ef_search reduces this proportionally.

## 5. K and ef_search relationship

The search returns the K closest from the ef-wide candidate set:

- If `ef >= K`, search returns K results.
- If `ef < K`, search returns at most `ef` results.

The substrate enforces `ef >= K`, raising ef to K when needed. For typical RECALL with K=10 and ef_search=64, no adjustment is needed.

## 6. Concurrent searches

Multiple searches run concurrently against the same HNSW index. The HNSW data structure is read-only during search — reads don't modify the graph.

Searches are lock-free with respect to inserts, via the epoch-based publication protocol detailed in [`08_concurrency.md`](08_concurrency.md). A search sees the graph as-of the start of the search; concurrent inserts may add nodes that this search doesn't see.

## 7. Search and tombstones

Tombstoned memories (marked deleted but not yet removed from HNSW) may appear in search results. The substrate filters them out post-search via the filter mechanism.

If too many results are tombstoned, search may return fewer than K results. The substrate detects this and re-queries with a higher ef to gather more candidates. See [`05_deletion.md`](05_deletion.md).

## 8. Filtering during search

HNSW doesn't support filtering during traversal — filtering happens post-search.

The trade-off: post-search filtering is correct but inefficient when filters are very selective. If a filter excludes 99% of memories, search may need to gather 100× more candidates to return K filtered ones.

The substrate compensates by:
- Running search with a higher ef for selective filters.
- Caching filter results across queries.

Detailed in [`09_filtering.md`](09_filtering.md).

## 9. Returned similarity scores

Each result carries a similarity score in [-1, 1]:

- 1.0 = identical vectors.
- 0.0 = orthogonal.
- -1.0 = opposite.

For agent queries, scores below ~0.3 are typically too dissimilar to be useful. The substrate doesn't filter by score by default; the agent can filter on its end or use the `confidence_min` parameter.

## 10. The "exact" search fallback

For very small indexes (< 1000 nodes), brute-force exact search is faster than HNSW. The substrate uses brute force as a fallback:

- Iterate over all nodes.
- Compute distance to query.
- Sort and return top K.

Cost: O(N × dim). For N=1000, ~50 µs — comparable to HNSW search and exact (no recall loss).

The threshold is configurable; default 1000.

## 11. Search caching

Repeated identical queries could be cached. The substrate doesn't currently cache search results because:

- Results depend on the current state, which changes as memories are added or removed.
- Cache invalidation is complex.
- Search is fast enough that caching's benefit is marginal for typical workloads.

The cue cache (in the embedding layer, [04.05](../04_embedding_layer/05_caching.md)) is the only cache; it caches text→vector mappings, not search results.

## 12. The "search before commit" race

A subtle race: a memory has been encoded, the WAL is fsync'd, but the HNSW insert hasn't completed yet. A query that arrives in this narrow window won't find the new memory.

This is acceptable: encode is durable (WAL fsync'd), but ANN visibility is eventually-consistent (HNSW catch-up is async after the durability barrier).

For workloads that need strict read-your-writes, a `RECALL` after `ENCODE` may need to retry briefly if the encoded memory isn't yet in HNSW. The substrate doesn't enforce this; the SDK handles it for client convenience.

## 13. Result quality monitoring

The substrate exposes per-search metrics:

- Latency (p50, p99).
- Number of nodes visited.
- Recall@K (computed periodically against ground truth).
- Filter discard rate.

Operators monitor these to detect index quality regression (e.g., recall dropping after many deletions, see [`07_maintenance.md`](07_maintenance.md)).

## 14. The empty-index case

If a search runs against an empty HNSW (no nodes), it returns an empty result set. No error.

Newly-created shards start with an empty HNSW; queries against them simply return empty until memories are encoded.

---

*Continue to [`05_deletion.md`](05_deletion.md) for deletion.*
