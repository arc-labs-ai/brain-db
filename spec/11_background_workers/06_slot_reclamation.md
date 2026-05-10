# 11.06 Slot Reclamation Worker

The slot reclamation worker reclaims arena slots from memories that have been forgotten beyond the grace period.

## 1. The lifecycle (recap)

From [02.08 Lifecycle](../02_data_model/08_lifecycle.md):

1. Memory is encoded → slot allocated.
2. Memory is forgotten (FORGET) → slot tombstoned, but slot still allocated.
3. Grace period elapses (default 7 days).
4. Slot is reclaimed → slot's data wiped, slot returned to free list.

Step 4 is this worker's job.

## 2. The cycle

Every 10 minutes (configurable):

1. Determine the cutoff: now - grace period.
2. Find tombstoned memories with `tombstoned_at < cutoff`.
3. For each, reclaim the slot.

## 3. Reclamation procedure

```rust
async fn reclaim_one(state: &ShardState, memory_id: MemoryId) -> Result<()> {
    let mut wtxn = state.metadata.begin_write()?;
    
    // 1. Verify the memory is still tombstoned
    let memories = wtxn.open_table(MEMORIES)?;
    let m = memories.get(&memory_id)?.unwrap();
    assert!(m.is_tombstoned() && m.tombstoned_at.unwrap() < cutoff);
    
    // 2. Delete from memories table
    let mut memories = wtxn.open_table(MEMORIES)?;
    memories.remove(&memory_id)?;
    
    // 3. Delete text
    let mut texts = wtxn.open_table(TEXTS)?;
    texts.remove(&memory_id)?;
    
    // 4. Delete edges (in both edges_out and edges_in)
    let mut edges_out = wtxn.open_table(EDGES_OUT)?;
    let mut edges_in = wtxn.open_table(EDGES_IN)?;
    delete_all_edges_for(memory_id, &mut edges_out, &mut edges_in)?;
    
    // 5. Increment slot version
    let mut versions = wtxn.open_table(SLOT_VERSIONS)?;
    let current = versions.get(&memory_id.slot_id())?.unwrap_or(0);
    versions.insert(&memory_id.slot_id(), &(current + 1))?;
    
    wtxn.commit()?;
    
    // 6. After commit, add slot to free list (in-memory)
    state.arena.free_list.push(memory_id.slot_id());
    
    Ok(())
}
```

The transaction handles the metadata side; the free list update is post-commit (in-memory operation, doesn't need to be transactional with metadata).

## 4. The slot_version increment

A reclaimed slot's version is incremented. This makes the old MemoryId (which encoded the previous version) no longer match the slot:

```
Before: slot 1234 has version 5; MemoryId M_5 = (slot=1234, version=5).
After reclaim: slot 1234 has version 6; M_5 still references version 5.
```

If anything (a stale reference, a buggy client) tries to use M_5 to access slot 1234, the substrate detects the mismatch:

```rust
fn validate_memory_id(id: MemoryId, slot: &Slot) -> bool {
    slot.metadata.slot_version == id.slot_version()
}
```

The mismatch causes the operation to return "not found" or similar.

## 5. The hard-forget special case

A hard-forgotten memory has had its vector and text zeroed at FORGET time. Reclamation just runs the normal procedure:

- Delete metadata, text, edges.
- Increment slot version.
- Free the slot.

The data is already gone (zeroed at FORGET); reclamation just frees the slot for reuse.

## 6. Edge cleanup

When a memory is reclaimed, its edges are deleted. This means edges that pointed to the reclaimed memory are dangling.

Wait — edges are bidirectional in the metadata (see [07.04](../07_metadata_graph/04_edge_storage.md)). For each `(source, kind, target)` edge:
- It exists in `edges_out[source, kind, target]`.
- It exists in `edges_in[target, kind, source]`.

Reclamation of memory M removes:
- All entries in `edges_out` where source=M.
- All entries in `edges_in` where target=M.

It does NOT remove:
- Entries in `edges_out` where target=M (the source's outgoing edges to M).
- Entries in `edges_in` where source=M (the target's incoming edges from M).

These dangling references are cleaned up by the edge scrub worker (see [`08_misc_workers.md`](08_misc_workers.md)).

## 7. The HNSW reference

When a memory is reclaimed, there's an old HNSW node referencing the (now-version-bumped) slot. The HNSW node has the old MemoryId.

This stale node is left in HNSW until the next maintenance rebuild. Searches that hit it will see the version mismatch (when reading the slot) and skip the result.

The maintenance worker eventually rebuilds the HNSW, removing all stale nodes.

## 8. Batch reclamation

Per cycle, the worker reclaims up to 1000 slots (configurable). Multiple cycles cover all eligible slots.

Each reclaim is its own transaction (single memory at a time). Could be batched into a single transaction for efficiency, but that increases lock duration. The single-memory approach is simpler.

## 9. The cost

Per slot:
- Metadata transaction: ~1 ms.
- Free list update: ~0.001 ms.
- Total: ~1 ms.

For 1000 slots/cycle: ~1 second of work, spread across the cycle's duration.

## 10. The "active" check

Before reclaiming, the worker re-checks that the memory is still tombstoned. A race could happen:

- Memory is tombstoned at time T.
- Grace period elapses at time T+7d.
- The worker schedules reclamation.
- Meanwhile, an operator runs `ADMIN_RESTORE` to undo the FORGET (a hypothetical operation).

The check ensures we don't reclaim something that was un-tombstoned. If un-tombstoned, the worker skips it.

(`ADMIN_RESTORE` isn't currently implemented; this is defensive.)

## 11. The free list

The free list (in the arena layer, see [05.05 Slot Lifecycle](../05_storage_arena_wal/05_slot_lifecycle.md)) is a concurrent data structure. The reclaim worker pushes; the encode path pops.

Free list operations are O(1) amortized via crossbeam-epoch.

## 12. The free list overflow

If the free list grows very large (many reclaimable slots all at once), it consumes memory. Each entry is a few bytes; a million entries are a few MB. Acceptable.

The list is bounded by the number of slots, which is bounded by the arena size.

## 13. The "no eligible work" path

If no slots are eligible for reclamation:

- The worker scans, finds none.
- Cycles end quickly.
- Sleep until next cycle.

For shards with little churn, this is the common case. The worker is mostly idle.

## 14. The grace period configuration

```toml
[memory]
forget_grace_period = "7d"
```

Shorter grace: faster reclamation, less recovery window.
Longer grace: more recovery window, slower reclamation.

For deployments wanting strict data retention (e.g., legal compliance), the grace period might be set to days or weeks.

For deployments wanting fast space recovery (e.g., high churn), grace might be shorter (minutes to hours).

## 15. The "bypass grace" flag

Hard FORGET still respects the grace period for slot reclamation. But a special flag (`force_reclaim_now=true`) bypasses it:

- Reclaim immediately after FORGET.
- No recovery window.

This is for sensitive data where the grace period is unacceptable. The substrate logs all uses of this flag for audit.

## 16. The cycle interactions

The reclaim worker and the edge scrub worker may operate on the same memory:

- Reclaim deletes the memory's outgoing edges.
- Edge scrub finds dangling edges that point to it (from other memories) and deletes them.

These are independent operations; each transaction is atomic. They don't conflict beyond redb's normal serialization.

## 17. Audit logging

For deployments that need audit trails, the reclaim worker emits a log entry per reclamation:

```
{
  event: "slot_reclaimed",
  memory_id: ...,
  slot_id: ...,
  forgot_at: ...,
  reclaimed_at: ...,
}
```

This is in addition to the WAL records (which capture the FORGET event but not the reclamation event explicitly).

---

*Continue to [`07_wal_retention.md`](07_wal_retention.md) for WAL retention.*
