# 09.02 HNSW Operations

Insertion, search, and deletion. The three operations the index supports and how each works.

## Insertion

The procedure for inserting a memory's vector into the HNSW index.

### 1. The insert call

```rust
fn insert(
    index: &mut HnswIndex,
    memory_id: MemoryId,
    vector: &[f32; 384],
) -> Result<(), AnnError> {
    let internal_id = index.next_internal_id.fetch_add(1, Ordering::Relaxed);
    index.id_map_forward.insert(memory_id, internal_id);
    index.id_map_reverse.insert(internal_id, memory_id);
    index.hnsw.insert((vector, internal_id));
    Ok(())
}
```

The insert is in-memory and doesn't directly touch disk. The associated WAL record (ENCODE) was already fsync'd before this call ([08.03 Write Path](../08_storage/03_write_path.md)).

### 2. The internal ID

hnsw_rs uses sequential u32 IDs for nodes internally. These are mapped to the public 16-byte MemoryId via two HashMaps:

```rust
struct HnswIndex {
    hnsw: hnsw_rs::Hnsw<f32, DistCosine>,
    id_map_forward: HashMap<MemoryId, u32>,
    id_map_reverse: HashMap<u32, MemoryId>,
    next_internal_id: AtomicU32,
}
```

The maps are kept in sync. Insert appends to both; remove deletes from both.

The maps live in memory only; they're rebuilt from the metadata store on startup (along with the HNSW graph itself).

### 3. Insertion order

Memories are inserted into HNSW after the WAL fsync and metadata commit. So the order of HNSW inserts within a shard is the order of confirmed encodes.

Different shards' HNSWs are independent; their insertion orders don't coordinate.

### 4. Insertion latency

Per-insert latency depends on the index size:

| N (nodes) | Latency |
|---|---|
| 1K | ~50 µs |
| 10K | ~100 µs |
| 100K | ~300 µs |
| 1M | ~1 ms |
| 10M | ~3 ms |

The growth is roughly O(M log N) — logarithmic in N.

For sub-25 ms p99 on encode, HNSW insertion at 1M scale (~1 ms) leaves room for embedding (5-10 ms) and other steps. At 10M, the picture is tighter.

### 5. Concurrent inserts

A **single-writer-per-shard** discipline applies. Within a shard, inserts are serialized through the writer task. There's no concurrent-insert path within a shard.

Across shards, inserts run in parallel (each shard has its own writer).

The hnsw_rs crate itself supports multi-threaded inserts via internal locking, but that mode is not used — the single-writer discipline avoids the lock contention while still giving cross-shard parallelism.

### 6. Layer assignment

When inserting, hnsw_rs randomly assigns the new node to a target layer using an exponential distribution:

```
P(layer = L) = exp(-L / mL) × (1 - exp(-1/mL))
```

where `mL = 1 / ln(M)` ≈ 0.36 for M=16.

So:
- ~70% of nodes go to layer 0 only.
- ~25% go up to layer 1.
- ~5% go up to layer 2.
- ~0.1% go up to layer 3.

The distribution gives an approximately balanced multi-layer structure.

### 7. The neighbor selection

Inserting at layer L:

1. From the current entry point, greedy-search to find the closest node at layer L.
2. From that node, run an `ef_construction`-wide beam search at layer L to find candidates.
3. Among the candidates, select the M closest as the new node's neighbors.
4. Add bidirectional edges between the new node and each selected neighbor.
5. If a neighbor's edge count now exceeds M (or 2M for the bottom layer), prune that neighbor's edge list — keep the M most useful edges.

Edge pruning uses a heuristic: prefer edges that diversify connectivity (avoid keeping only the closest neighbors, which can fragment the graph).

The hnsw_rs crate handles all this internally.

### 8. The entry point update

If the new node is at a layer higher than the current entry point's layer, the entry point is updated to the new node. This is rare (only ~0.1% of nodes go above layer 3 with M=16), but it's important for navigation through the upper layers.

The entry point update is atomic — readers see either the old or the new entry, never an inconsistent state.

### 9. Batched inserts

For high-throughput insert workloads, single-insert calls have overhead (mostly from the entry-point lookup and the per-insert allocations). Batched inserts can amortize.

HNSW inserts are batched when:
- Multiple WAL records are committed in a single group commit, AND
- They're all ENCODE records on the same shard.

The batch is inserted into HNSW after the metadata commits. Internally, hnsw_rs's parallel insert mode is used for the batch, which can interleave inserts across multiple cores.

For typical agent workloads (low to moderate write rate), batching has minimal effect. For high-throughput (bulk import, migration), it can 2-3× the throughput.

### 10. Insert failures

HNSW inserts can fail for:

- **Out of memory.** The insert needs to allocate edge lists; if the process is OOM, the allocation fails.
- **Internal HNSW error.** Very rare; would indicate a bug.
- **Duplicate MemoryId.** If the same MemoryId is inserted twice, the second insert overwrites the first internally. This is treated as a bug — the writer should never re-insert an existing memory.

On insert failure:

1. The error is logged with the offending MemoryId.
2. The encode is marked as partially-completed (WAL durable, HNSW failed).
3. A degraded-state response is returned to the client (the encode completed durably; ANN search may not include this memory until repair).

A maintenance worker repairs partially-completed encodes by retrying the HNSW insert.

### 11. Inserting with a stale arena pointer

The HNSW node references the vector through the slot ID, not through a direct pointer. So if the arena's mmap pointer changes (during arena growth, see [08.01 Arena](../08_storage/01_arena.md)), HNSW search continues to work — it computes the vector pointer fresh on each access via `arena_base + slot_offset(slot_id)`.

This means the HNSW doesn't need to be updated when the arena grows. The decoupling is via the slot ID.

### 12. The vector copy question

When inserting, does HNSW store its own copy of the vector, or does it reference the arena?

hnsw_rs stores its own copy (a Vec<f32>) per node. This is duplicate data: 1.5 KB per memory, 1.5 GB at 1M nodes.

Modifying hnsw_rs to reference the arena directly was considered and rejected:

- Significant fork of hnsw_rs.
- The arena's mmap pages may be evicted under memory pressure; HNSW search would then take page faults during distance computations, hurting tail latency.
- The duplicate is only ~10% of total RAM at typical sizes (HNSW graph + duplicate vectors + arena mmap).

The duplication is accepted. It's the simpler choice that gives more predictable performance.

### 13. Insertion in a near-full arena

The arena and HNSW grow together. When the arena's slot_count_in_use approaches the arena's slot_count_capacity, growth happens (see [08.01 Arena](../08_storage/01_arena.md)).

HNSW doesn't have an analogous capacity limit; it grows incrementally with each insert. Internal hnsw_rs structures may resize occasionally (similar to a Vec growing), which is amortized O(1).

So an arena growth event is independent of HNSW growth. Each just handles its own resize.

### 14. The "first node" special case

The very first node inserted into an empty HNSW becomes the entry point. Subsequent inserts use this entry point as their starting traversal node.

For the first-insert path, hnsw_rs handles this internally; the API call is the same as any insert.

## Search

The procedure for finding nearest neighbors of a query vector.

### 15. The search call

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

Note `k_extended` — when filters are active, more raw results may be requested than k from HNSW because filtering may discard some. Discussed in [`05_filtering.md`](05_filtering.md).

### 16. The HNSW search algorithm

Internally, hnsw_rs implements:

1. Start at the entry point (top layer).
2. At each layer above 0, greedy-search: visit the closest neighbor, take it as the new starting point. Stop when no neighbor improves on the current node.
3. At layer 0, beam search with width `ef`: maintain a candidate set of size `ef`; iteratively expand the most promising candidate; add neighbors that beat the worst in the set.
4. Return the K closest from the candidate set.

The greedy walk through upper layers is fast (O(log N) hops); the beam search at layer 0 is the expensive part (O(ef × M) distance computations).

### 17. Distance computation

For 384-dim normalized vectors, the distance between query q and node v is:

```
distance = 1 - dot_product(q, v)
        = 1 - sum(q[i] * v[i] for i in 0..384)
```

The dot product uses SIMD (AVX2 on x86, NEON on ARM):

- AVX2: 8 floats per FMA instruction → 48 instructions per dot product.
- NEON: 4 floats per FMA → 96 instructions.

Per dot product, ~50 ns on modern x86 with AVX2. A search visiting 1000 nodes takes ~50 µs of pure distance computation, plus traversal overhead.

### 18. Search latency breakdown

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

### 19. K and ef_search relationship

The search returns the K closest from the ef-wide candidate set:

- If `ef >= K`, search returns K results.
- If `ef < K`, search returns at most `ef` results.

The implementation enforces `ef >= K`, raising ef to K when needed. For typical RECALL with K=10 and ef_search=64, no adjustment is needed.

### 20. Concurrent searches

Multiple searches run concurrently against the same HNSW index. The HNSW data structure is read-only during search — reads don't modify the graph.

Searches are lock-free with respect to inserts, via the epoch-based publication protocol detailed in [`04_concurrency.md`](04_concurrency.md). A search sees the graph as-of the start of the search; concurrent inserts may add nodes that this search doesn't see.

### 21. Search and tombstones

Tombstoned memories (marked deleted but not yet removed from HNSW) may appear in search results. Tombstones are filtered out post-search via the filter mechanism.

If too many results are tombstoned, search may return fewer than K results. This is detected and search re-queries with a higher ef to gather more candidates. See the Deletion section below.

### 22. Filtering during search

HNSW doesn't support filtering during traversal — filtering happens post-search.

The trade-off: post-search filtering is correct but inefficient when filters are very selective. If a filter excludes 99% of memories, search may need to gather 100× more candidates to return K filtered ones.

The implementation compensates by:
- Running search with a higher ef for selective filters.
- Caching filter results across queries.

Detailed in [`05_filtering.md`](05_filtering.md).

### 23. Returned similarity scores

Each result carries a similarity score in [-1, 1]:

- 1.0 = identical vectors.
- 0.0 = orthogonal.
- -1.0 = opposite.

For agent queries, scores below ~0.3 are typically too dissimilar to be useful. There is no default filter by score; the agent can filter on its end or use the `confidence_min` parameter.

### 24. The "exact" search fallback

For very small indexes (< 1000 nodes), brute-force exact search is faster than HNSW. Brute force is used as a fallback:

- Iterate over all nodes.
- Compute distance to query.
- Sort and return top K.

Cost: O(N × dim). For N=1000, ~50 µs — comparable to HNSW search and exact (no recall loss).

The threshold is configurable; default 1000.

### 25. Search caching

Repeated identical queries could be cached. The implementation does not currently cache search results because:

- Results depend on the current state, which changes as memories are added or removed.
- Cache invalidation is complex.
- Search is fast enough that caching's benefit is marginal for typical workloads.

The cue cache (in the embedding layer, [07.03 Caching](../07_embedding/03_caching.md)) is the only cache; it caches text→vector mappings, not search results.

### 26. The "search before commit" race

A subtle race: a memory has been encoded, the WAL is fsync'd, but the HNSW insert hasn't completed yet. A query that arrives in this narrow window won't find the new memory.

This is acceptable: encode is durable (WAL fsync'd), but ANN visibility is eventually-consistent (HNSW catch-up is async after the durability barrier).

For workloads that need strict read-your-writes, a `RECALL` after `ENCODE` may need to retry briefly if the encoded memory isn't yet in HNSW. Brain doesn't enforce this; clients handle the brief retry.

### 27. Result quality monitoring

Per-search metrics:

- Latency (p50, p99).
- Number of nodes visited.
- Recall@K (computed periodically against ground truth).
- Filter discard rate.

Operators monitor these to detect index quality regression (e.g., recall dropping after many deletions, see [`03_hnsw_lifecycle.md`](03_hnsw_lifecycle.md)).

### 28. The empty-index case

If a search runs against an empty HNSW (no nodes), it returns an empty result set. No error.

Newly-created shards start with an empty HNSW; queries against them simply return empty until memories are encoded.

## Deletion

HNSW doesn't support efficient direct deletion. This section covers how forgotten memories are handled — tombstones, lazy cleanup, and rebuild triggers.

### 29. Why deletion is hard

To delete a node from an HNSW graph, the algorithm would need to:

1. Remove the node's edges (easy).
2. Repair its neighbors' edge lists (the deleted node was their neighbor; now they have one fewer connection).
3. Possibly add new edges to maintain navigability.

Step 3 is expensive. In the worst case, removing one node requires re-evaluating many other nodes' connectivity. For many deletions, the index degrades — the average path length grows, search recall drops.

The standard solution: **don't delete eagerly**. Mark nodes as deleted but keep them in the graph. Periodically rebuild the affected sections to actually remove them.

### 30. Tombstones

When a memory is forgotten:

1. The arena slot is tombstoned (flag bit 1 = 1).
2. The metadata is updated (memory's `forgot_at` set, `tombstoned_at` set).
3. The HNSW node remains in the graph.

The HNSW node still has its edges; navigation through it works. Search may return the tombstoned node as a candidate.

Tombstoned candidates are filtered from search results:

```rust
fn search_results_filter(candidates: Vec<...>, k: usize) -> Vec<...> {
    candidates.into_iter()
        .filter(|c| !memory_is_tombstoned(c.memory_id))
        .take(k)
        .collect()
}
```

The filter is implicit in every search.

### 31. Tombstone overhead

Each tombstoned node is "graph noise":
- It still consumes graph edges.
- Searches visit it but discard it.
- It contributes to navigation but produces no results.

For a small fraction of tombstones (< 5%), the overhead is negligible. For large fractions (> 30%), search quality degrades — too many candidates need to be gathered to return K results.

`tombstone_ratio` is tracked per shard. When it exceeds a threshold, the maintenance worker schedules rebuild. See [`03_hnsw_lifecycle.md`](03_hnsw_lifecycle.md).

### 32. Soft vs hard FORGET

From the data model ([02.02 Memory](../02_data_model/02_memory.md)):

- **Soft FORGET** marks tombstone, retains data for grace period. Default grace: 7 days.
- **Hard FORGET** zeros the slot's vector and text immediately, then tombstones.

Both result in the same HNSW state: the node remains in the graph, marked tombstoned in metadata.

The difference is that hard-forgotten nodes have zeroed vectors. If a search somehow reads such a vector (via a bug), it gets noise — a zero vector. But the filter discards tombstoned nodes before this happens, so the zeroed vector is never returned to clients.

### 33. Slot reclamation

After the grace period, a tombstoned slot is **reclaimed**:

1. The slot's flags are cleared (back to free).
2. The slot's metadata is wiped.
3. The slot is added to the free list.
4. A new encode can use this slot.

The reclaimed slot's HNSW node is still in the graph at this point. It's a "ghost node" — references a slot that no longer holds the original vector.

The maintenance worker handles this: ghost nodes are detected by checking the slot's metadata vs the HNSW's expected memory ID (via the version field in the MemoryId). If there's a mismatch, the HNSW node is removed during the next maintenance cycle.

### 34. Removing a node from HNSW

When the maintenance worker removes a node:

```rust
fn remove_node(index: &mut HnswIndex, internal_id: u32) {
    // 1. Remove from id maps
    let memory_id = index.id_map_reverse.remove(&internal_id).unwrap();
    index.id_map_forward.remove(&memory_id);

    // 2. Mark in HNSW (hnsw_rs supports this via internal flag)
    index.hnsw.mark_removed(internal_id);

    // 3. Defer actual graph repair to rebuild
}
```

`mark_removed` sets a flag on the node; subsequent searches skip it. The actual graph structure isn't repaired — that's deferred to a rebuild.

### 35. Rebuild triggers

The maintenance worker rebuilds the HNSW when:

- Tombstone ratio > 30% (configurable threshold).
- Recall has degraded measurably (sampling-based detection).
- Operator runs `ADMIN_REBUILD_ANN`.

Rebuild is described in [`03_hnsw_lifecycle.md`](03_hnsw_lifecycle.md). Briefly: a new HNSW is built from the current set of active memories, and atomically swapped in.

### 36. Rebuild cost

For N active memories:

- ~1 ms per insert × N memories = N ms.
- For N=1M: ~17 minutes single-threaded; ~3 minutes with parallel inserts.
- Memory: 2× HNSW size during rebuild (old + new).

Rebuild runs as a background task. Doesn't block reads or writes. The new index is swapped in atomically once complete.

### 37. The "delete then re-insert" pattern

Users sometimes want to update a memory's vector (e.g., re-embedding with a new model). The pattern:

1. New encode with the same text → new MemoryId.
2. Old memory FORGET (soft).
3. Optional: copy edges from old to new.

The MIGRATE_EMBEDDING workflow ([07.06 Migration](../07_embedding/06_migration.md)) does this transparently for model upgrades. Users don't see the temporary mid-state.

### 38. Tombstone in id map

The id_map_forward / id_map_reverse retain entries for tombstoned memories. They're cleaned up when the maintenance worker removes the HNSW node.

For very long-lived shards with many tombstones, the id maps can grow. This is bounded by `tombstone_ratio_threshold × N`; once the threshold is hit, rebuild empties the maps of tombstones.

### 39. Cleanup and the version field

The MemoryId includes a `slot_version`. When a slot is reclaimed:

- The slot's stored `slot_version` is incremented.
- The old MemoryId (with the old version) can never match the slot anymore.
- Any HNSW reference to the old MemoryId is now stale.

The maintenance worker detects stale HNSW nodes by comparing the HNSW's recorded MemoryId against the slot's current state:

```rust
fn is_stale(memory_id: MemoryId, slot: &Slot) -> bool {
    let current_version = slot.metadata.slot_version;
    memory_id.slot_version() != current_version
}
```

Stale HNSW nodes are removed during maintenance.

### 40. The deletion path latency

For a single FORGET:

- WAL append + fsync: ~0.3 ms.
- Metadata update: ~0.5 ms.
- Tombstone the slot: ~0.001 ms (memcpy a flag byte).
- Remove from HNSW: ~0.1 ms (set the flag).
- Total: ~0.9 ms.

Hard FORGET adds: vector zeroing (~0.001 ms). Negligible.

### 41. Deletion observability

Per-shard metrics:

- `tombstone_count` — current tombstoned memories.
- `tombstone_ratio` — `tombstone_count / total_memory_count`.
- `last_rebuild_at` — when the last full rebuild completed.

These metrics drive the maintenance worker's scheduling decisions and are exposed via `ADMIN_STATS`.

### 42. Bulk deletion

For workloads with bulk deletes (e.g., "forget everything in this context", "evict all memories with salience < threshold"):

1. The deletes are processed one by one (each gets its own WAL record).
2. Tombstone counts rise sharply.
3. The rebuild trigger fires once the threshold is reached.

For very large bulk deletes, the operator can run `ADMIN_REBUILD_ANN` to force an immediate rebuild and bypass the threshold-based scheduling.

---

*Continue to [`03_hnsw_lifecycle.md`](03_hnsw_lifecycle.md) for persistence and maintenance.*
