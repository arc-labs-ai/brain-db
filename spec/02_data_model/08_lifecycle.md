# 02.08 Memory Lifecycle

A memory is born, lives, may be forgotten, and eventually has its slot reclaimed. This file specifies the lifecycle states and transitions.

## 1. The state machine

```
              ┌─────────────────────────────────────────┐
              │                                         │
              ▼                                         │
            None        ENCODE              FORGET      │   reclaim
        (no memory)  ──────────►  Active  ─────────►  Tombstoned
                                    ▲   ▲                │
                                    │   │                │
                                    │   │ promotion      │
                                    │   │                │
                                    │  Semantic ←──── Consolidated
                                    │  /Cons.
                                    │
                            (kind transitions
                             — same Active state,
                             different kind)
              ┌──────────────────────────────────────────┘
              ▼
          Reclaimed (slot now holds a new memory with incremented version)
```

Three states: `Active`, `Tombstoned`, `Reclaimed`. The kind transitions ([`07_memory_kinds.md`](07_memory_kinds.md) §5) happen within the `Active` state.

## 2. State definitions

### 2.1 Active

The memory is queryable. All `RECALL`, `PLAN`, `REASON` operations may match it.

- The slot is occupied with the memory's vector.
- Metadata in redb is current.
- HNSW index includes the memory.
- Salience updates and decay apply.

### 2.2 Tombstoned

The memory has been forgotten but the slot has not yet been reclaimed.

- The slot is still occupied (vector and metadata unchanged from when it was forgotten).
- The memory is hidden from queries — operations skip tombstoned slots.
- HNSW index entries are removed.
- Edges are tagged for cleanup but may still be present.
- The slot is eligible for reuse on the next `ENCODE` allocation.

The tombstone state is brief in normal operation — typically a few seconds to a few minutes between forget and reclaim. It is observable to integrity-checking tools but never to client queries.

### 2.3 Reclaimed

The slot has been reused for a different memory. The previous memory's `MemoryId` is no longer valid for any active memory.

- The slot is occupied with a new memory's data.
- The new memory has an incremented `version` field.
- The old `MemoryId` (with the previous version) doesn't match any current memory.

`Reclaimed` is not a state of the *original* memory — it's a description of what happened to the slot. The original memory ceases to exist; the slot lives on, holding something new.

## 3. Lifecycle transitions

### 3.1 None → Active (creation)

Triggered by `ENCODE`. The transition involves:

1. Allocate a slot in the arena (or reuse a tombstoned slot, incrementing version).
2. Embed the text into a vector.
3. Compute initial salience.
4. Write a WAL record (the durability barrier).
5. fsync the WAL.
6. Write the vector to the arena slot.
7. Update metadata in redb.
8. Insert into HNSW.
9. Publish the new `MemoryId` to clients (epoch advance).

The memory is `Active` once step 9 completes.

The full sequence is detailed in [05. Storage: Arena & WAL](../05_storage_arena_wal/) §4.

### 3.2 Active → Tombstoned (FORGET)

Triggered by `FORGET`. The transition involves:

1. Validate that the `MemoryId` references an active memory owned by the requesting agent.
2. Write a `FORGET` record to the WAL.
3. fsync the WAL.
4. Mark the slot as tombstoned in metadata.
5. Remove the entry from the HNSW index.
6. Hide the memory from queries (epoch advance).
7. Set `forgot_at` timestamp.

For **soft forget**, the slot's data (vector and text) is preserved. The slot is eligible for reuse but the data is recoverable until reuse happens.

For **hard forget**, additional steps:

8. Overwrite the vector with zeros.
9. Clear the text.
10. Schedule the slot for immediate reclamation.

Hard forget makes the memory's content unrecoverable even via filesystem-level inspection. It's the right choice for compliance-driven removals (GDPR right-to-erasure, accidental encoding of secrets).

### 3.3 Tombstoned → Reclaimed (slot reuse)

Triggered when the next `ENCODE` allocates from the free list and picks this slot.

1. The new `ENCODE` operation finds the tombstoned slot in the per-shard free list.
2. The slot's `version` field is incremented (a 32-bit counter; at saturation the slot is permanently retired).
3. The slot is written with the new memory's vector and metadata.
4. The old memory's edges are cleaned up (incoming edges pointing at the old memory's ID are now stale; they're filtered out lazily during traversal).
5. The new memory is published.

After this, the original memory is gone; the slot holds the new memory.

The version increment ensures stale `MemoryId`s referencing the old memory cleanly mismatch — the lookup `MemoryId.version != slot.version` returns `MemoryNotFound` rather than silently returning the new memory.

### 3.4 Active → Active (kind transitions)

Several kind changes happen within the `Active` state:

- Episodic → Semantic (via `ADMIN_RECLASSIFY` or agent operation).
- Consolidated → Semantic (via promotion).

These are not lifecycle transitions in the sense of changing visibility or storage; they're metadata updates while the memory remains active and queryable. The mechanics are documented in [`07_memory_kinds.md`](07_memory_kinds.md) §5.

## 4. Background-driven transitions

### 4.1 Eviction (Active → Tombstoned)

The decay/consolidation worker may *evict* a memory whose salience has fallen below the eviction threshold. Eviction is functionally a `FORGET`:

- WAL record (with origin = "eviction").
- HNSW removal.
- Tombstone mark.

The eviction runs in soft-forget mode by default. The slot's data is preserved until reclaimed; this lets `ADMIN_RESTORE_RECENT` recover unintended evictions within a short window.

### 4.2 Consolidation (Active → Active, with creation)

The consolidation worker doesn't change a single memory's lifecycle; it creates new `Consolidated` memories from existing episodic ones. The original episodic memories remain `Active` (and may be evicted later based on salience).

Note: an aggressive consolidation policy could choose to evict the source episodic memories after consolidating them, freeing space. Brain's default policy is non-aggressive — episodic memories are kept unless their own salience falls below the eviction threshold.

## 5. State observability

What state is observable from outside:

- **`Active`** — fully visible. All fields readable, all queries return it.
- **`Tombstoned`** — invisible to client queries; visible to admin tools (`ADMIN_LIST_TOMBSTONED`).
- **`Reclaimed`** — the original memory is gone. The slot now holds a different memory; querying with the old `MemoryId` returns `MemoryNotFound`.

Clients should not rely on observing tombstoned state. The contract is "after `FORGET` returns, the memory is no longer queryable"; whether it's still on disk briefly is an implementation detail.

## 6. Slot version counter

The slot's `version` field is critical for lifecycle correctness.

### 6.1 Format

A 32-bit unsigned integer. Initial value: 1. Incremented each time the slot is reclaimed.

### 6.2 Saturation

When the version reaches `u32::MAX` (2^32 - 1, ≈ 4 billion), the slot is **permanently retired**. The next reclamation would wrap to 0, which we forbid (it would silently re-validate stale `MemoryId`s).

In practice, no real workload reaches this. A workload that reclaims a single slot 4 billion times is doing something pathological; we treat the saturation as the substrate's signal that something is wrong.

### 6.3 Why 32 bits

We chose 32 bits over 16 (which would saturate too quickly under churn) or 64 (which would expand `MemoryId` beyond 16 bytes). 32 bits gives ~4 billion reclamations per slot, which is more than enough.

## 7. The boundary cases

### 7.1 FORGET on a tombstoned memory

The original `MemoryId` references the tombstoned (or already-reclaimed) memory. Behavior:

- If tombstoned: idempotent. The `FORGET` returns success with `was_already_forgotten = true`.
- If reclaimed: the `MemoryId` is stale (version mismatch). Returns `MemoryNotFound`.

### 7.2 RECALL race with FORGET

A `RECALL` is in flight when `FORGET` arrives for one of the candidate memories. Behavior depends on timing:

- If the `RECALL` already returned the memory: the result is sent; the next `RECALL` won't include this memory (it's tombstoned).
- If the `RECALL` is still scoring candidates: the tombstoned memory is filtered out.

This is the epoch-based reclamation model; details are in [10. Concurrency + Epoch Model](../10_concurrency_epochs/).

### 7.3 Crash during ENCODE

If the server crashes between WAL fsync and full publication of the memory:

- WAL was durably written → recovery replays the encode → memory is published after recovery.
- WAL fsync hadn't completed → the encode is treated as never having happened.

The client, in either case, retries with the same `request_id`. If the encode succeeded pre-crash, the retry is deduplicated and returns the original `MemoryId`. If it didn't succeed, the retry creates the memory.

### 7.4 Crash during FORGET

Similar logic: if the WAL `FORGET` record was durable, recovery completes the forget. If not, the memory is still active after recovery; the client's retry will succeed.

## 8. Lifecycle timeline

A typical memory's timeline:

```
Time:  0s     0.01s    1s        100s        1d        90d         180d        365d
       │        │      │           │           │          │            │           │
       │        │      │           │           │          │            │           │
       │   ENCODE     RECALL    RECALL      decay      consolidation  forgotten?  evicted?
       │   begins  →  hits      hits         starts     creates new   if low      if no
       │              salience  salience     to bite    Consolidated  salience    activity
       │              boost     boost                    memory
       │
       (None state)
        Active state ──────────────────────────────────────────────────────►
                                                                                  Tombstoned ────►
                                                                                                  Reclaimed
```

The timeline above is illustrative — actual durations depend on the workload, the salience trajectory, and the agent's access patterns.

## 9. Memory edge cleanup on lifecycle change

When a memory transitions to `Tombstoned`:

- **Outgoing edges** (owned by the memory) are removed from the by-source index.
- **Incoming edges** (where this memory is the target) are tagged for lazy cleanup. They remain in the by-target index until the slot is reclaimed.

When the slot is `Reclaimed`:

- The by-target index entries pointing to this slot's `MemoryId` (with the old version) are scheduled for deletion in the next index-maintenance pass.
- The new memory's edges are added fresh.

The lazy-cleanup of incoming edges is a deliberate trade-off. Eager cleanup would require scanning every other memory's outgoing edges to find references — expensive. Lazy cleanup defers the cost until the slot is reused, when we have to rewrite the index anyway.

---

*Continue to [`09_schema_evolution.md`](09_schema_evolution.md) for schema evolution.*
