# 07.09 Concurrency Model

How the metadata store coexists with the rest of Brain's concurrent operations.

## 1. The single-writer-per-shard

Within a shard, only one task writes to the metadata store: the writer task. This matches the broader single-writer-per-shard discipline ([10. Concurrency + Epoch Model](../10_concurrency_epochs/)).

The writer's redb transactions don't contend with other writers (because there are none on this shard). The serialization redb provides via "at most one write transaction" is effectively a no-op given our discipline.

## 2. Many concurrent readers

Many tasks read the metadata store concurrently:

- Request handlers looking up memory metadata for RECALL.
- The query planner reading edges.
- Background workers doing maintenance scans.
- SUBSCRIBE clients tailing the WAL with metadata-driven filters.

Each gets its own read transaction; redb's MVCC keeps them isolated.

## 3. Read-modify-write under MVCC

A common pattern: read a row, modify, write back.

```rust
// Read transaction
let metadata = {
    let rtxn = db.begin_read()?;
    let memories = rtxn.open_table(MEMORIES)?;
    memories.get(&memory_id)?.cloned()
};

// Compute new state
let mut new_metadata = metadata;
new_metadata.salience = 0.95;

// Write transaction
let mut wtxn = db.begin_write()?;
{
    let mut memories = wtxn.open_table(MEMORIES)?;
    memories.insert(&memory_id, &new_metadata)?;
}
wtxn.commit()?;
```

Between the read and the write, another writer could have modified the row. Brain's single-writer-per-shard discipline means this can't happen — only one writer per shard, and it executes serially.

For multi-writer architectures (which Brain doesn't have), this read-modify-write pattern would need explicit conflict detection. We don't deal with that.

## 4. Reads during a write

While a write transaction is in progress (between begin_write and commit), reads see the pre-write state. After commit, new reads see the post-write state. In-flight reads (those that began before commit) continue to see pre-write state.

This is standard MVCC. redb implements it correctly.

## 5. Transaction lifetime and Arc

redb's transactions are scoped (Rust lifetimes). Brain doesn't store transactions across async-await boundaries; transactions are completed within one logical "step" of the writer or reader.

For writes: the write transaction is opened, modifications are applied, commit, drop. All within one synchronous block of the writer task.

For reads: a read transaction is opened for one logical operation (e.g., handling a single RECALL). It's dropped after the operation completes.

## 6. Long-running read transactions

Some operations need long-running reads:

- SUBSCRIBE — a long-lived read view of the metadata.
- Maintenance scans — iterating over many rows.

For these, the read transaction is held for the duration. redb's MVCC ensures these don't block writes.

The cost of holding a read transaction: redb retains the snapshot pages, increasing on-disk space until the transaction is dropped. For short reads, this is irrelevant. For very long reads (hours), it can grow.

The substrate limits read-transaction duration:

- Default max: 1 hour.
- Long-running readers (SUBSCRIBE) periodically refresh by dropping the old transaction and opening a new one.

## 7. The "stale read" semantics

A read transaction sees the database as-of when it began. If an ENCODE happens during the read transaction, the new memory isn't visible until the read transaction is replaced.

For RECALL, this is fine — the user gets a consistent view, even if a few microseconds stale. For SUBSCRIBE, the WAL stream provides the missing updates.

## 8. The HNSW vs metadata interaction

A search:

1. Calls HNSW for candidate IDs.
2. Looks up each candidate's metadata.

These two steps may use different consistency views: HNSW may have a candidate that's tombstoned in the metadata (if the tombstone was set after HNSW's last publication). The metadata read sees this and the candidate is filtered out.

Inverse: HNSW doesn't have a candidate that's been added to the metadata. The new memory isn't returned. The next search (after HNSW catches up) will include it.

These transient inconsistencies are bounded by the publication interval (~10 ms typical). Acceptable for typical workloads.

## 9. The arena vs metadata interaction

When a search reads a vector from the arena, it does so via the slot ID stored in the metadata. The metadata says "memory M is at slot 1234, version 5"; the arena's slot 1234 should have version 5 in its metadata.

If they disagree (e.g., the slot was reclaimed and now hosts a different memory), the search detects the version mismatch and skips the candidate. The version field in the slot's metadata is the integrity check.

In practice, this should rarely happen — reclamation happens after FORGET + grace, and the HNSW would have removed the old node by then. But the version check is defensive.

## 10. Cross-shard

Different shards have independent metadata stores. No cross-shard transactions.

For operations that span shards (very rare, e.g., a hypothetical cross-agent edge), the substrate doesn't support them. Each shard's writes are independent.

## 11. The reader-cache pattern

Within a request handler:

```rust
let rtxn = db.begin_read()?;
let memories = rtxn.open_table(MEMORIES)?;
let m1 = memories.get(&id1)?.cloned();
let m2 = memories.get(&id2)?.cloned();
let m3 = memories.get(&id3)?.cloned();
```

All lookups in one read transaction; consistent with each other.

The substrate sometimes caches results across multiple read transactions for the same request, but this introduces consistency questions (the cache may be stale). Generally, a single read transaction is enough for one request.

## 12. The "writer pause" pattern

A long maintenance operation (e.g., context deletion of a large context) might want to pause normal writes briefly. The substrate doesn't have a built-in mechanism for this; instead, the maintenance worker does the work in chunks:

- Open a write transaction.
- Process N records.
- Commit.
- Yield to other writes.
- Repeat.

Each chunk is a brief writer-task occupancy; other writes (in progress or pending) get a turn between chunks.

## 13. The cooperative-yield discipline

The writer task on each shard runs on a single Glommio executor. Long-running work blocks other tasks on the same executor. The substrate's writer task yields cooperatively:

- Between requests.
- During large transactions.
- During large iterations.

Yielding lets other tasks (request handlers, the embedder, etc.) run on the same core. This is essential for fairness.

## 14. The "one writer at a time" assumption

The substrate assumes a single writer per shard. If, due to a bug, two writer tasks tried to commit transactions, redb would serialize them — but the substrate's WAL ordering would be broken (the LSN sequence assumes one writer).

The codebase has assertions to catch this; the architecture intentionally creates only one writer per shard. The single-writer discipline is invariant.

## 15. The "writer is idle" optimization

When the writer task has no pending work, it doesn't hold any transaction. This means redb's internal locks (which are minimal, but exist) are released; reads run without any contention.

In practice, the writer is rarely fully idle on busy shards. But the design doesn't add overhead for idle periods.

---

*Continue to [`10_failure_modes.md`](10_failure_modes.md) for failure modes.*
