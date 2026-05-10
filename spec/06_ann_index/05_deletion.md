# 06.05 Deletion

HNSW doesn't support efficient direct deletion. This file specifies how Brain handles forgotten memories — tombstones, lazy cleanup, and rebuild triggers.

## 1. Why deletion is hard

To delete a node from an HNSW graph, we'd need to:

1. Remove the node's edges (easy).
2. Repair its neighbors' edge lists (the deleted node was their neighbor; now they have one fewer connection).
3. Possibly add new edges to maintain navigability.

Step 3 is expensive. In the worst case, removing one node requires re-evaluating many other nodes' connectivity. For many deletions, the index degrades — the average path length grows, search recall drops.

The standard solution: **don't delete eagerly**. Mark nodes as deleted but keep them in the graph. Periodically rebuild the affected sections to actually remove them.

## 2. Tombstones

When a memory is forgotten:

1. The arena slot is tombstoned (flag bit 1 = 1).
2. The metadata is updated (memory's `forgot_at` set, `tombstoned_at` set).
3. The HNSW node remains in the graph.

The HNSW node still has its edges; navigation through it works. Search may return the tombstoned node as a candidate.

The substrate filters tombstoned candidates from search results:

```rust
fn search_results_filter(candidates: Vec<...>, k: usize) -> Vec<...> {
    candidates.into_iter()
        .filter(|c| !memory_is_tombstoned(c.memory_id))
        .take(k)
        .collect()
}
```

The filter is implicit in every search.

## 3. Tombstone overhead

Each tombstoned node is "graph noise":
- It still consumes graph edges.
- Searches visit it but discard it.
- It contributes to navigation but produces no results.

For a small fraction of tombstones (< 5%), the overhead is negligible. For large fractions (> 30%), search quality degrades — too many candidates need to be gathered to return K results.

The substrate tracks `tombstone_ratio` per shard. When it exceeds a threshold, the maintenance worker schedules rebuild. See [`07_maintenance.md`](07_maintenance.md).

## 4. Soft vs hard FORGET

From the data model ([02.08 Lifecycle](../02_data_model/08_lifecycle.md)):

- **Soft FORGET** marks tombstone, retains data for grace period. Default grace: 7 days.
- **Hard FORGET** zeros the slot's vector and text immediately, then tombstones.

Both result in the same HNSW state: the node remains in the graph, marked tombstoned in metadata.

The difference is that hard-forgotten nodes have zeroed vectors. If a search somehow reads such a vector (via a bug), it gets noise — a zero vector. But the filter discards tombstoned nodes before this happens, so the zeroed vector is never returned to clients.

## 5. Slot reclamation

After the grace period, a tombstoned slot is **reclaimed**:

1. The slot's flags are cleared (back to free).
2. The slot's metadata is wiped.
3. The slot is added to the free list.
4. A new encode can use this slot.

The reclaimed slot's HNSW node is still in the graph at this point. It's a "ghost node" — references a slot that no longer holds the original vector.

The maintenance worker handles this: ghost nodes are detected by checking the slot's metadata vs the HNSW's expected memory ID (via the version field in the MemoryId). If there's a mismatch, the HNSW node is removed during the next maintenance cycle.

## 6. Removing a node from HNSW

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

## 7. Rebuild triggers

The maintenance worker rebuilds the HNSW when:

- Tombstone ratio > 30% (configurable threshold).
- Recall has degraded measurably (sampling-based detection).
- Operator runs `ADMIN_REBUILD_ANN`.

Rebuild is described in [`07_maintenance.md`](07_maintenance.md). Briefly: a new HNSW is built from the current set of active memories, and atomically swapped in.

## 8. Rebuild cost

For N active memories:

- ~1 ms per insert × N memories = N ms.
- For N=1M: ~17 minutes single-threaded; ~3 minutes with parallel inserts.
- Memory: 2× HNSW size during rebuild (old + new).

Rebuild runs as a background task. Doesn't block reads or writes. The new index is swapped in atomically once complete.

## 9. The "delete then re-insert" pattern

Users sometimes want to update a memory's vector (e.g., re-embedding with a new model). The pattern:

1. New encode with the same text → new MemoryId.
2. Old memory FORGET (soft).
3. Optional: copy edges from old to new.

The substrate's MIGRATE_EMBEDDING workflow ([04.08](../04_embedding_layer/08_migration.md)) does this transparently for model upgrades. Users don't see the temporary mid-state.

## 10. Tombstone in id map

The id_map_forward / id_map_reverse retain entries for tombstoned memories. They're cleaned up when the maintenance worker removes the HNSW node.

For very long-lived shards with many tombstones, the id maps can grow. This is bounded by `tombstone_ratio_threshold × N`; once the threshold is hit, rebuild empties the maps of tombstones.

## 11. Cleanup and the version field

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

## 12. The deletion path latency

For a single FORGET:

- WAL append + fsync: ~0.3 ms.
- Metadata update: ~0.5 ms.
- Tombstone the slot: ~0.001 ms (memcpy a flag byte).
- Remove from HNSW: ~0.1 ms (set the flag).
- Total: ~0.9 ms.

Hard FORGET adds: vector zeroing (~0.001 ms). Negligible.

## 13. Deletion observability

Per-shard metrics:

- `tombstone_count` — current tombstoned memories.
- `tombstone_ratio` — `tombstone_count / total_memory_count`.
- `last_rebuild_at` — when the last full rebuild completed.

These metrics drive the maintenance worker's scheduling decisions and are exposed via `ADMIN_STATS`.

## 14. Bulk deletion

For workloads with bulk deletes (e.g., "forget everything in this context", "evict all memories with salience < threshold"):

1. The substrate processes the deletes one by one (each gets its own WAL record).
2. Tombstone counts rise sharply.
3. The rebuild trigger fires once the threshold is reached.

For very large bulk deletes, the operator can run `ADMIN_REBUILD_ANN` to force an immediate rebuild and bypass the threshold-based scheduling.

---

*Continue to [`06_persistence.md`](06_persistence.md) for HNSW persistence.*
