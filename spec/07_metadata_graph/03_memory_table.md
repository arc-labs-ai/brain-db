# 07.03 The Memory Metadata Table

The `memories` table is the central index of the substrate. Every memory has exactly one row here. This file specifies the row's fields and access patterns.

## 1. The row layout

```rust
struct MemoryMetadata {
    // Identity
    memory_id: MemoryId,                  // 16 bytes (also the key)
    agent_id: AgentId,                    // 16 bytes
    context_id: ContextId,                // 8 bytes
    slot_id: u64,                         // 8 bytes (effective 48-bit)
    slot_version: u32,                    // 4 bytes

    // Type and content
    kind: MemoryKind,                     // 1 byte (Episodic/Semantic/Consolidated)
    text_size: u32,                       // 4 bytes (in `texts` table)

    // Temporal
    created_at: u64,                      // 8 bytes (unix nanoseconds)
    last_accessed_at: u64,                // 8 bytes
    forgot_at: Option<u64>,               // 8+1 bytes
    tombstoned_at: Option<u64>,           // 8+1 bytes
    consolidated_at: Option<u64>,         // 8+1 bytes (when promoted to Consolidated)

    // Salience
    salience: f32,                        // 4 bytes (current)
    salience_initial: f32,                // 4 bytes (initial baseline)
    access_count: u32,                    // 4 bytes (lifetime)

    // Embedding
    embedding_model_fp: ModelFingerprint, // 16 bytes
    
    // Status flags
    flags: u32,                           // 4 bytes (bit-packed)

    // Counters
    edges_out_count: u32,                 // 4 bytes (denormalized; updated on edge changes)
    edges_in_count: u32,                  // 4 bytes
}
```

Total: ~140 bytes per row. With redb's per-row overhead, ~150 bytes effective.

For 1M memories: ~150 MB.

## 2. Field semantics

### 2.1 Identity

- `memory_id` — primary key. Repeated as a row field for convenience (rkyv decoders can return the row including the key).
- `agent_id` — owner. Searches typically filter by agent.
- `context_id` — bucket the memory belongs to (one of an agent's contexts).
- `slot_id` and `slot_version` — locate the vector in the arena. Version disambiguates reused slots.

### 2.2 Kind

One of `Episodic`, `Semantic`, `Consolidated`. Set at creation; can be changed via `UPDATE_KIND` operation.

### 2.3 Text size

Cached size of the text (in the `texts` table). Lets us quickly answer "how big is this memory's text?" without a separate read.

### 2.4 Temporal fields

- `created_at` — when the memory was first encoded.
- `last_accessed_at` — when the memory was last returned in a RECALL response.
- `forgot_at` — set on FORGET; null otherwise.
- `tombstoned_at` — set on the same FORGET event; redundant with `forgot_at` in v1, distinguished for future fine-grained handling.
- `consolidated_at` — set when an Episodic memory is promoted to Consolidated.

All times are unix nanoseconds; 64-bit handles dates well past year 2200.

### 2.5 Salience

- `salience` — current salience after decay and access boosts. Range [0, 1].
- `salience_initial` — baseline at creation time (before decay).
- `access_count` — total number of times this memory has been returned.

Salience is recomputed periodically by the decay worker (see [11. Background Workers](../11_background_workers/) §Decay).

### 2.6 Embedding model fingerprint

The fingerprint of the model that produced this memory's vector. Used for cross-model exclusion in queries.

### 2.7 Flags (bit-packed)

| Bit | Meaning |
|---|---|
| 0 | Active (1) vs tombstoned (0) |
| 1 | Hard-forgotten (vector zeroed) |
| 2 | Pinned (won't be auto-evicted) |
| 3 | Reserved for staleness flag (set if vector hasn't been re-embedded after model change) |
| 4-31 | Reserved |

### 2.8 Edge counts

- `edges_out_count` and `edges_in_count` — denormalized counts.
- Updated during LINK/UNLINK operations.
- Avoid range scans of the edge tables when callers just want a count.

## 3. Access patterns

### 3.1 By MemoryId

The most common access. O(log N) lookup.

### 3.2 By agent

To list an agent's memories: a range scan of `memories` by MemoryId range corresponding to the agent. Since MemoryIds are agent-clustered (the high bits encode agent), this is a tight range.

Actually — looking at our MemoryId layout ([02.03](../02_data_model/03_identifiers.md)) — MemoryIds are not strictly agent-clustered. The shard_id_runtime is in the high bits, then slot_id, then slot_version. So MemoryIds within a shard are slot-id-ordered (which is roughly creation-time-ordered).

To list an agent's memories, we need an auxiliary index. We don't currently have one in the table layout above; we'd need to add `(AgentId, MemoryId) → ()` as another index table. This is a [`11_open_questions.md`](11_open_questions.md) item.

In v1, listing an agent's memories means scanning all memories in the shard and filtering by agent_id. For shards with thousands of agents, this is wasteful. For shards with one or few agents per shard, it's fine.

### 3.3 By context

`(AgentId, ContextId, MemoryId) → ()` index would enable this efficiently. Same open question as agent index.

For v1, `RECALL` with a context filter applies the filter post-search; no metadata-side index is used for it.

### 3.4 Range scans by time

UUIDv7's time-ordered prefix means range scans by MemoryId approximate range scans by creation time. "Memories created since X" is a tight range scan starting from a synthetic MemoryId derived from X.

## 4. Updates

### 4.1 Common updates

Most metadata updates are read-modify-write:

```
let mut metadata = memories.get(&memory_id)?;
metadata.last_accessed_at = now();
metadata.access_count += 1;
memories.insert(&memory_id, &metadata);
```

These are coalesced when possible (multiple memories' updates in a single transaction).

### 4.2 The salience update path

Salience updates are the most frequent. They happen:
- Immediately on access (boost).
- Periodically (decay).

The substrate batches salience updates: the decay worker processes many memories in a single transaction; the access boost is buffered until the next transaction commit.

### 4.3 Edge count updates

Edge counts are updated whenever LINK or UNLINK happens. The update is in the same transaction as the edge insert/delete. This keeps the count accurate.

If the count gets out of sync (due to a bug or partial recovery), a maintenance worker periodically recomputes it.

## 5. Tombstoning

A FORGET operation sets the appropriate flags and timestamps; the row remains in the table:

```
metadata.flags &= !ACTIVE_BIT;  // clear active bit
metadata.forgot_at = Some(now());
metadata.tombstoned_at = Some(now());
```

The row stays for the grace period. After grace, the slot is reclaimed and the row is deleted.

## 6. Reclamation

When a slot is reclaimed:

1. The row is deleted from `memories`.
2. The text is deleted from `texts`.
3. Edges referencing the memory are deleted from `edges_out` and `edges_in`.
4. The slot's version in `slot_versions` is incremented.

This is a single redb transaction. After commit, the memory is fully gone.

## 7. The "active" filter

Most queries filter for active memories (flag bit 0 = 1). The substrate either:

- Adds a filter expression to the query (and pays the cost of reading inactive rows).
- Maintains a separate index of active memory IDs. Not currently done.

For v1, post-filter is the approach. The cost is minor — most rows are active.

## 8. Sizing analysis

Per-row size: ~150 bytes.

For 10M memories: ~1.5 GB just for the memory table. redb's overhead adds ~20-30%, so plan for 2 GB at this scale.

For comparison, the vector arena at 10M memories is 15 GB. The metadata table is ~10% of arena size.

## 9. Cross-version compatibility

The row's binary layout is rkyv-encoded. Adding fields requires:
- Bumping the table's schema version.
- A migration that rewrites existing rows in the new format.

Migrations are run lazily (per-row on access) or eagerly (full-table scan on startup), depending on the migration type. See [02.09](../02_data_model/09_schema_evolution.md).

## 10. The "fresh memory" lifecycle

The lifecycle of a row in this table:

1. **Created** by ENCODE: row inserted, flags = ACTIVE.
2. **Updated** by accesses: salience and access_count update.
3. **Updated** by edge changes: edges_out_count, edges_in_count.
4. **Maybe consolidated**: kind changes to Consolidated, consolidated_at set.
5. **Maybe forgotten**: flags clear ACTIVE, forgot_at set.
6. **Eventually reclaimed**: row deleted.

The active lifetime ranges from minutes (transient memories) to years (persistent semantic memories). The substrate doesn't impose a maximum lifetime.

---

*Continue to [`04_edge_storage.md`](04_edge_storage.md) for edges.*
