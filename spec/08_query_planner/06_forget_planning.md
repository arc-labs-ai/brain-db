# 08.06 FORGET Planning

How the planner builds an execution plan for a FORGET request.

## 1. The FORGET request shape

```rust
struct ForgetRequest {
    target: ForgetTarget,         // What to forget
    agent_id: AgentId,
    mode: ForgetMode,             // Soft / Hard
    request_id: RequestId,
}

enum ForgetTarget {
    Memory(MemoryId),
    Memories(Vec<MemoryId>),
    Filter(ForgetFilter),         // E.g., all memories in a context with salience < 0.1
}

enum ForgetMode {
    Soft,    // Tombstone; reclaim after grace
    Hard,    // Tombstone, zero vector and text immediately
}
```

## 2. Two planning paths

### 2.1 Forget by ID (or list of IDs)

```rust
struct ForgetByIdPlan {
    memory_ids: Vec<MemoryId>,
    mode: ForgetMode,
    shard_routes: Vec<(ShardId, Vec<MemoryId>)>,    // Group by shard
    per_shard: Vec<ForgetShardStep>,
}
```

The planner groups memory IDs by shard and produces a sub-plan per shard. Each shard processes its memories independently.

### 2.2 Forget by filter

```rust
struct ForgetByFilterPlan {
    filter: ForgetFilter,
    mode: ForgetMode,
    discovery_step: DiscoveryStep,    // First, list matching memories
    forget_step: ForgetByIdStep,      // Then, forget them
}
```

The plan first discovers matching memories (a query-like step) then forgets them.

## 3. The per-shard forget step

```rust
struct ForgetShardStep {
    shard_id: ShardId,
    memory_ids: Vec<MemoryId>,
    wal_records: Vec<ForgetRecord>,    // One per memory
    metadata_updates: Vec<MetadataUpdate>,
    arena_updates: Vec<ArenaUpdate>,   // Tombstone flags
    hnsw_updates: Vec<HnswUpdate>,     // Mark removed
}
```

The shard processes the IDs in a single batch:
- Single WAL group commit.
- Single metadata write transaction.
- Batched arena tombstones.
- Batched HNSW marks.

For a batch of 1000 IDs: ~5-10 ms total.

## 4. Hard forget specifics

Hard forget zeros the vector and text:

```rust
struct HardForgetStep {
    arena_zero_vectors: Vec<u64>,    // Slot IDs to zero
    text_zero: Vec<MemoryId>,        // Texts to zero out
}
```

Zeroing happens in addition to tombstoning. Performed before the WAL fsync — the WAL record indicates "hard forget" so recovery knows to apply zeroing too.

## 5. The "forget by filter" two-phase

Phase 1: discovery.

```rust
struct DiscoveryStep {
    filter: ForgetFilter,
    list_max: usize,                 // Cap to avoid runaway
    use_metadata_iteration: bool,    // Iterate metadata table; not HNSW
}
```

Listing matching memories is a metadata-table operation. The substrate iterates the relevant table (e.g., scan `memories` and apply the filter). For large shards, this is expensive.

Phase 2: forget.

After discovery, the substrate has a list of memory IDs. It runs the "forget by ID" path on them.

## 6. The bulk-forget cap

`ForgetFilter` is bounded:

- A single FORGET request can affect at most 100,000 memories.
- Beyond this, the request fails with `TooManyMemories`.

For larger bulk operations, the operator uses `ADMIN_CONTEXT_DELETE` (which does its own staged processing) or scripts a sequence of capped FORGETs.

## 7. The idempotency check

Before doing work, check the idempotency table:

```rust
fn idempotency_check(req: &ForgetRequest) -> Either<ForgetResponse, ()> {
    if let Some(prior) = idempotency.get(&req.request_id) {
        return Left(prior.cached_response);
    }
    Right(())
}
```

Same as for ENCODE. If duplicate, replay; else proceed.

## 8. The shard routing

Each memory ID is routed to its shard:

```rust
fn route(memory_ids: Vec<MemoryId>) -> Vec<(ShardId, Vec<MemoryId>)> {
    let mut grouped = HashMap::new();
    for id in memory_ids {
        let shard = router.shard_for_memory(id);
        grouped.entry(shard).or_insert_with(Vec::new).push(id);
    }
    grouped.into_iter().collect()
}
```

The MemoryId encodes the shard ([02.03 Identifiers](../02_data_model/03_identifiers.md)), so routing is O(1) per ID.

## 9. The cross-shard fan-out

For a forget across shards, the planner produces parallel sub-plans. The executor runs them in parallel.

If one shard fails (write error), the other shards' forgets still proceed. The response indicates per-memory-ID success/failure.

## 10. The per-memory error tolerance

If a memory-ID isn't found (already forgotten or never existed), the substrate logs and continues:

```rust
match metadata.get(&memory_id) {
    Some(m) if m.is_active() => proceed_with_forget(m),
    Some(m) => log_warning("Memory already tombstoned"),    // No-op
    None => log_warning("Memory not found"),                // No-op
}
```

The response indicates which IDs were processed.

This makes FORGET idempotent at the per-ID level: re-forgetting a tombstoned memory is a no-op.

## 11. Cascading edge handling

When a memory is forgotten, what happens to its edges?

In v1:
- Outgoing edges from the forgotten memory: tombstoned (the source is gone).
- Incoming edges to the forgotten memory: tombstoned.

The maintenance worker eventually cleans up tombstoned edges. Until then, queries that traverse these edges see them as "leading to a tombstoned memory" and skip.

## 12. Cascade options

A future option (not in v1):

- **Cascade forget**: forgetting memory M also forgets memories that DERIVED_FROM M.
- **Restrict forget**: if memory M has incoming DERIVED_FROM edges, forgetting M is rejected (would orphan derived memories).

Both are nuanced semantics that need careful design. v1 doesn't impose them; deletes are unconstrained from the substrate's perspective.

## 13. The arena tombstone vs metadata

The arena tombstone is set first (in-memory; before the WAL). This prevents new searches from returning the slot during the brief window before the metadata is updated.

If the arena tombstone is set but the WAL fsync fails, recovery rolls back the in-memory state (the WAL record was never committed).

## 14. The HNSW update timing

HNSW node removal (the `mark_removed` flag) happens after the metadata commit. Ordering:

```
1. WAL fsync of FORGET record.
2. Arena slot tombstoned.
3. Metadata commit (memory's flags updated, forgot_at set).
4. HNSW node marked removed.
5. Acknowledge.
```

If a crash happens between 3 and 4, recovery re-applies step 4 by replaying from the WAL.

## 15. The grace period and reclaim

Soft FORGET starts a grace period. After the grace (default 7 days), the maintenance worker reclaims the slot:

```rust
fn reclaim(memory_id: MemoryId) {
    let mut wtxn = db.begin_write()?;
    delete_from_memories(memory_id);
    delete_from_texts(memory_id);
    delete_associated_edges(memory_id);
    increment_slot_version(memory_id.slot_id());
    wtxn.commit()?;

    add_to_arena_free_list(memory_id.slot_id());
}
```

The reclaim is a separate operation, not part of the original FORGET's plan.

## 16. The forget latency

For a single FORGET:

| Phase | Latency |
|---|---|
| Idempotency check | 5-10 µs |
| WAL append + fsync | 0.3 ms (group commit) |
| Arena tombstone | 0.001 ms |
| Metadata commit | 0.5 ms |
| HNSW mark removed | 0.1 ms |
| Response | 50 µs |
| **Total** | **~1 ms** |

Hard forget adds ~0.001 ms (zeroing). Negligible.

For a batched forget of 100 IDs: ~3-5 ms total (single WAL commit; single metadata transaction).

## 17. The plan size

A typical FORGET plan is ~200 bytes for a single ID; ~10 KB for 1000 IDs (mostly the ID list).

## 18. Plan validation

The planner checks:

- Memory IDs are well-formed.
- The agent owns the memories (from the agent_id in the IDs vs the request).
- The request's RequestId is set.
- For filter mode: the filter is well-formed.

Invalid → error response immediately.

---

*Continue to [`07_cost_estimation.md`](07_cost_estimation.md) for cost estimation.*
