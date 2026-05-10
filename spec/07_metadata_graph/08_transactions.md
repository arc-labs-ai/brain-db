# 07.08 Transaction Semantics

The metadata store provides ACID transactions through redb. This file specifies how Brain uses them.

## 1. Two transaction kinds

redb provides:

- **Read transaction** (`db.begin_read()`) — sees a consistent snapshot. Many can be concurrent. No locking.
- **Write transaction** (`db.begin_write()`) — at most one active at a time. Serialized; second `begin_write()` blocks until the first commits or aborts.

Brain uses both.

## 2. Read transactions

Used for:

- Lookups during request handling.
- Iterations over tables (e.g., listing context memories).
- Snapshot views for SUBSCRIBE.

A read transaction sees the database as-of when it began. Modifications by concurrent write transactions are invisible until the read transaction is dropped and a new one is begun.

This is **MVCC** — multi-version concurrency control. Reads don't block writes; writes don't block reads.

## 3. Write transactions

Used for:

- The actual mutation in ENCODE, FORGET, LINK, etc.
- Salience updates (batched).
- Consolidation worker writes.
- Bookkeeping updates (checkpoints, model fingerprints, etc.).

The single-writer-per-shard discipline means there's only one writer per shard, naturally serializing redb's write transactions. No contention; no waiting.

## 4. Transaction granularity

Brain's writes are typically:

- One transaction per state-mutating operation (ENCODE = 1 txn).
- One transaction per batch of related updates (decay worker batches many salience updates per txn).

Smaller transactions: more commits, more fsyncs, slower.
Larger transactions: fewer commits, less observability granularity, larger memory footprint during the transaction.

We tune for "one transaction per request, with batching where natural".

## 5. The encode transaction

A typical ENCODE transaction:

```rust
let mut wtxn = db.begin_write()?;
{
    let mut memories = wtxn.open_table(MEMORIES)?;
    let mut texts = wtxn.open_table(TEXTS)?;
    let mut idem = wtxn.open_table(IDEMPOTENCY)?;
    let mut edges_out = wtxn.open_table(EDGES_OUT)?;
    let mut edges_in = wtxn.open_table(EDGES_IN)?;
    let mut model_fps = wtxn.open_table(MODEL_FINGERPRINTS)?;

    memories.insert(&memory_id, &metadata)?;
    texts.insert(&memory_id, &text)?;
    idem.insert(&request_id, &idem_entry)?;

    for edge in &edges {
        edges_out.insert(&edge.out_key(), &edge.data())?;
        edges_in.insert(&edge.in_key(), &edge.data())?;
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

Steps 4-6 happen after the durability barrier. If the substrate crashes between 4 and 6, recovery replays the WAL record, redoing steps 4-6.

The redb commit (step 5) has its own internal sync. This means we have two layers of durability:

- The Brain WAL fsync (step 3) — for substrate-level durability.
- The redb commit fsync (step 5) — for redb's internal consistency.

The redb sync isn't strictly necessary for substrate-level durability (the WAL is the source of truth). But it ensures redb's own state is consistent across restarts. Removing redb's sync would risk redb internal corruption.

## 9. The cost of the double sync

For each ENCODE: WAL fsync (~0.3 ms) + redb commit (~0.5 ms) = ~0.8 ms of fsync overhead. Adding HNSW insertion and other costs, the total per-encode is ~1-2 ms (excluding embedding).

We've considered "redb without sync" mode, where redb relies on the OS for eventual durability and trusts the Brain WAL for actual durability. Rejected because:

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
- The `edges_out` and `edges_in` tables.
- The `idempotency` table.

After commit, all tables reflect the changes. Before commit, none do (from a read transaction's perspective).

## 12. Transaction scope

We don't use redb transactions for the entire request handling. The request handler does:

1. Read transaction for lookups (fast, lock-free).
2. Drop the read transaction.
3. Compute/embed/etc.
4. Write transaction for the actual mutation.
5. Commit the write transaction.
6. Acknowledge.

Long-held write transactions would block other writes (single-writer). We keep them brief.

## 13. The Brain-level transaction

Brain's wire protocol exposes TXN_BEGIN/TXN_COMMIT operations ([03.07 Request Frames](../03_wire_protocol/07_request_frames.md)). These are at a different level than redb transactions:

- A Brain transaction may span multiple operations (ENCODE + LINK + LINK + ...).
- Each operation has its own redb transaction.
- The Brain transaction is reflected in the WAL via TXN_BEGIN/TXN_COMMIT records.
- Recovery applies all-or-nothing across the operations within a Brain transaction.

So Brain transactions provide "logical atomicity" across operations even though each operation has its own underlying redb commit. This works because:

- The WAL is the source of truth.
- Recovery sees the TXN_BEGIN/TXN_COMMIT brackets and applies records atomically.
- If the substrate crashes mid-Brain-transaction, recovery sees an unmatched TXN_BEGIN and discards subsequent records.

## 14. The "open table" cost

Each `wtxn.open_table(TABLE_DEF)?` has a small cost (~1-2 µs). For transactions touching many tables, this adds up.

For very-hot paths, the substrate caches table handles within a writer task. The cache is invalidated when the schema changes (rare).

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

*Continue to [`09_concurrency.md`](09_concurrency.md) for concurrency.*
