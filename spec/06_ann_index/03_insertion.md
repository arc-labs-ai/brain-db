# 06.03 Insertion

The procedure for inserting a memory's vector into the HNSW index. This file specifies how the substrate calls hnsw_rs and what additional bookkeeping it does.

## 1. The insert call

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

The insert is in-memory and doesn't directly touch disk. The associated WAL record (ENCODE) was already fsync'd before this call ([05.07](../05_storage_arena_wal/07_write_path.md)).

## 2. The internal ID

hnsw_rs uses sequential u32 IDs for nodes internally. Brain maps these to the public 16-byte MemoryId via two HashMaps:

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

## 3. Insertion order

The substrate inserts memories into HNSW after the WAL fsync and metadata commit. So the order of HNSW inserts within a shard is the order of confirmed encodes.

Different shards' HNSWs are independent; their insertion orders don't coordinate.

## 4. Insertion latency

Per-insert latency depends on the index size:

| N (nodes) | Latency |
|---|---|
| 1K | ~50 µs |
| 10K | ~100 µs |
| 100K | ~300 µs |
| 1M | ~1 ms |
| 10M | ~3 ms |

The growth is roughly O(M log N) — logarithmic in N.

For Brain's targets (sub-25 ms p99 on encode), HNSW insertion at 1M scale (~1 ms) leaves room for embedding (5-10 ms) and other steps. At 10M, the picture is tighter.

## 5. Concurrent inserts

The substrate has a **single-writer-per-shard** discipline. Within a shard, inserts are serialized through the writer task. There's no concurrent-insert path within a shard.

Across shards, inserts run in parallel (each shard has its own writer).

The hnsw_rs crate itself supports multi-threaded inserts via internal locking, but Brain doesn't use that mode — the single-writer discipline avoids the lock contention while still giving us cross-shard parallelism.

## 6. Layer assignment

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

## 7. The neighbor selection

Inserting at layer L:

1. From the current entry point, greedy-search to find the closest node at layer L.
2. From that node, run an `ef_construction`-wide beam search at layer L to find candidates.
3. Among the candidates, select the M closest as the new node's neighbors.
4. Add bidirectional edges between the new node and each selected neighbor.
5. If a neighbor's edge count now exceeds M (or 2M for the bottom layer), prune that neighbor's edge list — keep the M most useful edges.

Edge pruning uses a heuristic: prefer edges that diversify connectivity (avoid keeping only the closest neighbors, which can fragment the graph).

The hnsw_rs crate handles all this internally.

## 8. The entry point update

If the new node is at a layer higher than the current entry point's layer, the entry point is updated to the new node. This is rare (only ~0.1% of nodes go above layer 3 with M=16), but it's important for navigation through the upper layers.

The entry point update is atomic — readers see either the old or the new entry, never an inconsistent state.

## 9. Batched inserts

For high-throughput insert workloads, single-insert calls have overhead (mostly from the entry-point lookup and the per-insert allocations). Batched inserts can amortize.

The substrate batches HNSW inserts when:
- Multiple WAL records are committed in a single group commit, AND
- They're all ENCODE records on the same shard.

The batch is inserted into HNSW after the metadata commits. Internally, hnsw_rs's parallel insert mode is used for the batch, which can interleave inserts across multiple cores.

For typical agent workloads (low to moderate write rate), batching has minimal effect. For high-throughput (bulk import, migration), it can 2-3× the throughput.

## 10. Insert failures

HNSW inserts can fail for:

- **Out of memory.** The insert needs to allocate edge lists; if the substrate is OOM, the allocation fails.
- **Internal HNSW error.** Very rare; would indicate a bug.
- **Duplicate MemoryId.** If the same MemoryId is inserted twice, the second insert overwrites the first internally. Brain treats this as a bug — the writer should never re-insert an existing memory.

On insert failure, the substrate:

1. Logs the error with the offending MemoryId.
2. Marks the encode as partially-completed (WAL durable, HNSW failed).
3. Returns a degraded-state response to the client (the encode completed durably; ANN search may not include this memory until repair).

A maintenance worker repairs partially-completed encodes by retrying the HNSW insert.

## 11. Inserting with a stale arena pointer

The HNSW node references the vector through the slot ID, not through a direct pointer. So if the arena's mmap pointer changes (during arena growth, see [05.03](../05_storage_arena_wal/03_arena_growth.md)), HNSW search continues to work — it computes the vector pointer fresh on each access via `arena_base + slot_offset(slot_id)`.

This means the HNSW doesn't need to be updated when the arena grows. The decoupling is via the slot ID.

## 12. The vector copy question

When inserting, does HNSW store its own copy of the vector, or does it reference the arena?

hnsw_rs stores its own copy (a Vec<f32>) per node. This is duplicate data: 1.5 KB per memory, 1.5 GB at 1M nodes.

We considered modifying hnsw_rs to reference the arena directly, avoiding the duplicate. Rejected:

- Significant fork of hnsw_rs.
- The arena's mmap pages may be evicted under memory pressure; HNSW search would then take page faults during distance computations, hurting tail latency.
- The duplicate is only ~10% of total RAM at typical sizes (HNSW graph + duplicate vectors + arena mmap).

We accept the duplication. It's the simpler choice that gives more predictable performance.

## 13. Insertion in a near-full arena

The arena and HNSW grow together. When the arena's slot_count_in_use approaches the arena's slot_count_capacity, growth happens (see [05.03](../05_storage_arena_wal/03_arena_growth.md)).

HNSW doesn't have an analogous capacity limit; it grows incrementally with each insert. Internal hnsw_rs structures may resize occasionally (similar to a Vec growing), which is amortized O(1).

So an arena growth event is independent of HNSW growth. Each just handles its own resize.

## 14. The "first node" special case

The very first node inserted into an empty HNSW becomes the entry point. Subsequent inserts use this entry point as their starting traversal node.

For the first-insert path, hnsw_rs handles this internally; from Brain's perspective, the API call is the same as any insert.

---

*Continue to [`04_search.md`](04_search.md) for the search algorithm.*
