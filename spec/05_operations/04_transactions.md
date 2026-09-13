# 05.04 Transactions

The TXN_BEGIN / TXN_COMMIT / TXN_ABORT primitives bracket multiple operations atomically.

## 1. Why transactions

Without transactions, multi-operation workflows have failure-mode complexity:

```
brain.encode("step 1")        // succeeds
brain.encode("step 2")        // ENCODE 2 fails — step 1 is now dangling
```

With transactions:

```
txn = brain.txn_begin()
brain.encode("step 1", txn=txn)
brain.encode("step 2", txn=txn)
brain.txn_commit(txn)         // both succeed, or neither
```

Atomicity across multiple operations.

## 2. TXN_BEGIN

```
TXN_BEGIN(agent_id, request_id) → TxnHandle
```

Brain:

1. Allocates a TransactionId.
2. Creates a transaction context on the shard.
3. Returns a handle that subsequent operations carry.

```rust
struct TxnHandle {
    txn_id: TransactionId,
    shard_id: ShardId,
    started_at: u64,
    expires_at: u64,
}
```

The transaction is single-shard (the agent's primary shard). Cross-shard transactions are not supported.

## 3. TXN_COMMIT

```
TXN_COMMIT(txn_id, request_id) → CommitResponse
```

Brain:

1. Verifies the transaction is still alive.
2. Writes a TXN_COMMIT marker to the WAL.
3. Fsyncs the WAL.
4. Applies all transaction operations atomically.
5. Releases the transaction.

After COMMIT, all operations are durable.

## 4. TXN_ABORT

```
TXN_ABORT(txn_id, request_id) → AbortResponse
```

Brain:

1. Discards the transaction's pending operations.
2. Writes a TXN_ABORT marker to the WAL (for audit).
3. Releases the transaction.

After ABORT, no operations from the transaction take effect.

## 5. Operation semantics within a transaction

When an operation carries a `txn` parameter:

- ENCODE, LINK, UNLINK, FORGET: applied to the transaction; not durable until COMMIT.
- RECALL, PLAN, REASON: read against a snapshot that includes the transaction's pending writes.
- Other operations: as documented per-operation.

Within the transaction, the operations see their own pending writes:

```
txn = brain.txn_begin()
m1 = brain.encode("x", txn=txn)
results = brain.recall("x", txn=txn)         // includes m1
brain.txn_commit(txn)
```

Without `txn`, the recall would only see committed memories.

## 6. The transaction's WAL records

A transaction is represented in the WAL as:

```
TXN_BEGIN(txn_id)
ENCODE(memory_1, txn_id)
LINK(edge_1, txn_id)
... 
TXN_COMMIT(txn_id) | TXN_ABORT(txn_id)
```

On recovery, Brain sees the records:

- Records between TXN_BEGIN and TXN_COMMIT are applied.
- Records between TXN_BEGIN and TXN_ABORT are skipped.
- An unmatched TXN_BEGIN at the end (no COMMIT or ABORT before crash) is treated as an implicit ABORT.

## 7. Isolation

Other clients don't see in-flight transaction writes. They see the database as-of the last committed state.

This is **read committed** isolation. Within a transaction, the client sees its own writes (read-your-writes semantics).

Brain doesn't provide stricter isolation (e.g., serializable). For most agent workloads, read-committed is sufficient.

## 8. Concurrent transactions

Multiple transactions can be open simultaneously on the same shard:

- Each is per-client (identified by the TxnHandle).
- They're isolated from each other.
- The writer task processes their commits sequentially.

Within a shard, transactions effectively serialize at commit time. No conflict detection beyond that — the single-writer serialization makes conflicts impossible.

## 9. Transaction expiration

Transactions have a max duration (default 30 sec):

- After the duration, Brain auto-aborts.
- The client gets `TransactionExpired` if it tries to commit.

This prevents resource leaks from clients that forget to commit/abort.

For long-running workflows that genuinely need more time, the duration is configurable per-transaction (up to a max of 5 min).

## 10. Transaction size limits

A transaction is limited:

- Max operations: 1000 per transaction.
- Max payload size: 100 MB.

Beyond these, TXN_COMMIT fails with `TransactionTooLarge`. The client should split into multiple transactions.

These limits keep Brain's per-transaction memory bounded.

## 11. Failure modes

### TransactionNotFound

The TxnHandle is invalid or expired. Error.

### TransactionExpired

The transaction lived too long. Auto-aborted. Error on subsequent operations.

### TransactionTooLarge

The transaction has too many operations or too much payload. Error on COMMIT.

### CommitConflict

Rare; reserved for future use if cross-transaction conflicts become possible. Currently, single-writer-per-shard means no conflicts.

## 12. The "implicit single-op transaction"

Each non-transactional operation is, internally, an implicit transaction:

- TXN_BEGIN.
- The operation.
- TXN_COMMIT.

This is invisible to the client (and isn't logged as a separate WAL record — the operation's WAL record is sufficient). It just means every operation has the same atomicity guarantees as transactional ones.

## 13. The "no nested transactions" rule

A transaction can't contain another TXN_BEGIN. Brain rejects nested transactions.

For composability, agents typically wrap workflows in transactions at one level — the outermost level — and don't try to nest.

## 14. The "transaction can't span shards" limitation

A transaction is always single-shard (the agent's primary). Operations carrying a `txn` parameter are routed to that shard, even if their data would naturally live elsewhere.

For workflows that touch multiple shards atomically, Brain doesn't help. Agents that need this:

1. Decompose into single-shard transactions.
2. Use a saga pattern with compensating actions on failure.

Cross-shard transactions are an open question; not currently supported.

## 15. The transaction commit latency

Commit cost:

- Apply all pending operations (varies with size).
- Write a TXN_COMMIT marker to the WAL.
- Fsync.
- Update in-memory state for all operations.

For a small transaction (5 ops): ~5-10 ms.

For a large transaction (500 ops): ~50-100 ms.

The commit dominates the transaction's latency; individual operations within are buffered in memory until commit.

## 16. The transaction abort cost

Abort is cheap:

- Discard the in-memory pending writes.
- Write an abort marker to the WAL.

~1 ms typical. Much faster than commit.

## 17. The "agent transaction" pattern

A typical agent workflow:

```
def do_thing():
    txn = brain.txn_begin()
    try:
        m1 = brain.encode("...", txn=txn)
        m2 = brain.encode("...", txn=txn)
        brain.link(m1, m2, FOLLOWED_BY, txn=txn)
        brain.txn_commit(txn)
    except Exception:
        brain.txn_abort(txn)
        raise
```

Clients can wrap this in a scope/closure abstraction:

```python
with brain.transaction() as txn:
    m1 = txn.encode("...")
    m2 = txn.encode("...")
    txn.link(m1, m2, FOLLOWED_BY)
    # Auto-commit on context exit; auto-abort on exception
```

## 18. Idempotency within transactions

Each operation within a transaction has its own RequestId. The transaction itself doesn't have a top-level RequestId for replay (the operations within do).

If the client retries TXN_COMMIT (e.g., network drop after commit but before ack), Brain sees the same TXN_COMMIT request_id and replays the response. No double-commit.

## 19. Visibility timing

After TXN_COMMIT succeeds:

- Future RECALL within the same shard sees the committed memories (subject to publication lag, ~10 ms).
- Cross-shard RECALL is unaffected (the transaction was single-shard).
- SUBSCRIBE clients receive the WAL records.

For read-after-write within the same transaction (operations following the commit on the same client), Brain ensures visibility — the next RECALL sees the writes immediately.

## 20. The "best practice" for transactions

Use transactions when:
- Multiple operations must succeed or fail together.
- The outcome of one operation affects the next (read-your-writes within transaction).
- Audit/log shows a coherent group.

Don't use transactions when:
- Individual operations are independent.
- The cost of holding a transaction is meaningful.
- The workflow can tolerate partial completion.

For most agent operations (a single ENCODE, a single RECALL), transactions add no value. Use them deliberately.

---

*Continue to [`05_subscribe.md`](05_subscribe.md) for SUBSCRIBE.*
