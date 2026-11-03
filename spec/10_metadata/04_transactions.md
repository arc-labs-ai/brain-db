# 10.04 Transactions and Concurrency

> **TL;DR.** redb provides ACID transactions and MVCC concurrency. This section covers transaction semantics (read vs write, isolation, lifetime, retry) and the concurrency model (single writer per shard, snapshot reads, no read-write conflicts in the steady state).

## Transactions

The metadata store provides ACID transactions through redb. This subsection specifies how Brain uses them.


## 1. Two transaction kinds

redb provides:

- **Read transaction** (`db.begin_read()`) — sees a consistent snapshot. Many can be concurrent. No locking.
- **Write transaction** (`db.begin_write()`) — at most one active at a time. Serialized; second `begin_write()` blocks until the first commits or aborts.

Brain uses both.

## 2. Read transactions

Used for:

- Lookups during request handling.
- Iterations over tables (e.g., listing session memories).
- Snapshot views for SUBSCRIBE.

A read transaction sees the database as-of when it began. Modifications by concurrent write transactions are invisible until the read transaction is dropped and a new one is begun.

This is **MVCC** — multi-version concurrency control. Reads don't block writes; writes don't block reads.

## 3. Write transactions

Used for:

- The actual mutation in ENCODE, FORGET, LINK, etc.
- Salience updates (batched).
- Consolidation worker writes.
- Bookkeeping updates (checkpoints, model fingerprints, etc.).

The single-writer-per-shard discipline ([14. Concurrency](../14_concurrency/00_purpose.md)) means there's only one writer per shard, naturally serializing redb's write transactions. No contention; no waiting.

## 4. Transaction granularity

Brain's writes are typically:

- One transaction per state-mutating operation (ENCODE = 1 txn).
- One transaction per batch of related updates (decay worker batches many salience updates per txn).

Smaller transactions: more commits, more fsyncs, slower.
Larger transactions: fewer commits, less observability granularity, larger memory footprint during the transaction.

Brain tunes for "one transaction per request, with batching where natural".

## 5. The encode transaction

A typical ENCODE transaction:

```rust
let mut wtxn = db.begin_write()?;
{
    let mut memories = wtxn.open_table(MEMORIES)?;
    let mut texts = wtxn.open_table(TEXTS)?;
    let mut idem = wtxn.open_table(IDEMPOTENCY)?;
    let mut edges = wtxn.open_table(EDGES_TABLE)?;
    let mut edges_reverse = wtxn.open_table(EDGES_REVERSE_TABLE)?;
    let mut model_fps = wtxn.open_table(MODEL_FINGERPRINTS)?;

    memories.insert(&memory_id, &metadata)?;
    texts.insert(&memory_id, &text)?;
    idem.insert(&request_id, &idem_entry)?;

    for edge in &edges {
        edges.insert(&edge.out_key(), &edge.data())?;
        edges_reverse.insert(&edge.in_key(), &edge.data())?;
    }

    if !model_fps.contains(&fingerprint)? {
        model_fps.insert(&fingerprint, &model_info)?;
    }
}
wtxn.commit()?;
```

All writes in one atomic unit. If commit fails, none happen.

## 6. Commit cost

A redb commit:

- Serializes B-tree changes.
- Writes pages to disk.
- Calls fsync (defaulted to sync-on-commit).

Cost: 0.1-1 ms typically on NVMe. The fsync is the main contributor.

For Brain's per-shard writer pacing: ~10K commits/sec sustainable. Higher with batching.

## 7. Transaction abort

A transaction aborts if:

- It's dropped without committing (e.g., panic, early return).
- An explicit `txn.abort()` is called.
- A commit fails (rare; would indicate disk error or similar).

On abort, no changes are applied. The database returns to its pre-transaction state.

## 8. The commit-vs-WAL ordering

Brain's writes go through:

1. Allocate slot (in-memory).
2. Append WAL record.
3. fsync WAL.  ← durability barrier
4. Apply to arena (memcpy).
5. Apply to redb (begin txn, insert, commit).
6. Apply to HNSW.
7. Acknowledge.

Steps 4-6 happen after the durability barrier. If Brain crashes between 4 and 6, recovery replays the WAL record, redoing steps 4-6.

The redb commit (step 5) has its own internal sync. This means Brain has two layers of durability:

- The Brain WAL fsync (step 3) — for substrate-level durability.
- The redb commit fsync (step 5) — for redb's internal consistency.

The redb sync isn't strictly necessary for substrate-level durability (the WAL is the source of truth). But it ensures redb's own state is consistent across restarts. Removing redb's sync would risk redb internal corruption.

## 9. The cost of the double sync

For each ENCODE: WAL fsync (~0.3 ms) + redb commit (~0.5 ms) = ~0.8 ms of fsync overhead. Adding HNSW insertion and other costs, the total per-encode is ~1-2 ms (excluding embedding).

A "redb without sync" mode (where redb relies on the OS for eventual durability and trusts the Brain WAL for actual durability) was considered and rejected:

- redb's internal consistency depends on its own sync; without it, redb may corrupt on crash.
- The cost saving is small (~0.5 ms).
- Operational complexity increases (a custom redb mode).

## 10. Read-after-write within a transaction

A write transaction sees its own changes:

```rust
let mut wtxn = db.begin_write()?;
{
    let mut t = wtxn.open_table(...)?;
    t.insert(&key, &value1)?;
    let v = t.get(&key)?;  // Returns value1
    t.insert(&key, &value2)?;
    let v = t.get(&key)?;  // Returns value2
}
wtxn.commit()?;
```

Concurrent read transactions don't see these intermediate states.

## 11. Multi-table consistency

A single write transaction can update multiple tables atomically:

- The `memories` table.
- The `texts` table.
- The `edges` and `edges_reverse` tables.
- The `idempotency` table.

After commit, all tables reflect the changes. Before commit, none do (from a read transaction's perspective).

## 12. Transaction scope

Brain does not use redb transactions for the entire request handling. The request handler does:

1. Read transaction for lookups (fast, lock-free).
2. Drop the read transaction.
3. Compute/embed/etc.
4. Write transaction for the actual mutation.
5. Commit the write transaction.
6. Acknowledge.

Long-held write transactions would block other writes (single-writer). Brain keeps them brief.

## 13. The Brain-level transaction

Brain's wire protocol exposes TXN_BEGIN/TXN_COMMIT operations ([04.05 Frame Layouts](../04_wire_protocol/05_frame_layouts.md)). These are at a different level than redb transactions:

- A Brain transaction may span multiple operations (ENCODE + LINK + LINK + ...).
- Each operation has its own redb transaction.
- The Brain transaction is reflected in the WAL via TXN_BEGIN/TXN_COMMIT records.
- Recovery applies all-or-nothing across the operations within a Brain transaction.

So Brain transactions provide "logical atomicity" across operations even though each operation has its own underlying redb commit. This works because:

- The WAL is the source of truth.
- Recovery sees the TXN_BEGIN/TXN_COMMIT brackets and applies records atomically.
- If Brain crashes mid-Brain-transaction, recovery sees an unmatched TXN_BEGIN and discards subsequent records.

## 14. The "open table" cost

Each `wtxn.open_table(TABLE_DEF)?` has a small cost (~1-2 µs). For transactions touching many tables, this adds up.

For very-hot paths, Brain caches table handles within a writer task. The cache is invalidated when the schema changes (rare).

## 15. Snapshot reads

`SUBSCRIBE` (and other long-running readers) use a stable read transaction across many records. The read transaction doesn't see updates made after it began.

For SUBSCRIBE, this is correct: the client wants a stable view. New records after the snapshot LSN are delivered via WAL replay; the redb read transaction is for the post-snapshot lookups.

## 16. The "no-progress" risk

If a write transaction is held open for a long time (a bug or misuse), other writers wait. Brain mitigates this:

- The single-writer task discipline ensures only the writer holds write transactions.
- The writer task uses brief transactions; long-running work isn't done within a transaction.
- A timeout (default 30 sec) aborts a write transaction held too long.

## 17. Best practices

For Brain's code:

- Open write transactions briefly. Don't do I/O or compute within them.
- Use read transactions for lookups; drop them before doing anything heavy.
- Batch related writes into a single transaction when natural (e.g., all edges of an ENCODE).
- Don't share transactions across async tasks.

---

## Concurrency

How the metadata store coexists with the rest of Brain's concurrent operations.

## 1. The single-writer-per-shard

Within a shard, only one task writes to the metadata store: the writer task. This matches the broader single-writer-per-shard discipline ([14. Concurrency](../14_concurrency/00_purpose.md)).

The writer's redb transactions don't contend with other writers (because there are none on this shard). The serialization redb provides via "at most one write transaction" is effectively a no-op given the single-writer discipline.

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

For multi-writer architectures (which Brain doesn't have), this read-modify-write pattern would need explicit conflict detection. Brain does not deal with that.

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

Brain limits read-transaction duration:

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

For operations that span shards (very rare, e.g., a hypothetical cross-space edge), Brain doesn't support them. Each shard's writes are independent.

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

Brain sometimes caches results across multiple read transactions for the same request, but this introduces consistency questions (the cache may be stale). Generally, a single read transaction is enough for one request.

## 12. The "writer pause" pattern

A long maintenance operation (e.g., session deletion of a large session) might want to pause normal writes briefly. Brain doesn't have a built-in mechanism for this; instead, the maintenance worker does the work in chunks:

- Open a write transaction.
- Process N records.
- Commit.
- Yield to other writes.
- Repeat.

Each chunk is a brief writer-task occupancy; other writes (in progress or pending) get a turn between chunks.

## 13. The cooperative-yield discipline

The writer task on each shard runs on a single Glommio executor. Long-running work blocks other tasks on the same executor. Brain's writer task yields cooperatively:

- Between requests.
- During large transactions.
- During large iterations.

Yielding lets other tasks (request handlers, the embedder, etc.) run on the same core. This is essential for fairness.

## 14. The "one writer at a time" assumption

Brain assumes a single writer per shard. If, due to a bug, two writer tasks tried to commit transactions, redb would serialize them — but Brain's WAL ordering would be broken (the LSN sequence assumes one writer).

The codebase has assertions to catch this; the architecture intentionally creates only one writer per shard. The single-writer discipline is invariant.

## 15. The "writer is idle" optimization

When the writer task has no pending work, it doesn't hold any transaction. This means redb's internal locks (which are minimal, but exist) are released; reads run without any contention.

In practice, the writer is rarely fully idle on busy shards. But the design doesn't add overhead for idle periods.

---

*Continue to [`05_failure_and_audit.md`](05_failure_and_audit.md) for failure modes.*
