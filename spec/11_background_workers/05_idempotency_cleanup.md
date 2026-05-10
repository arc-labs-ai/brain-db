# 11.05 Idempotency Cleanup Worker

The idempotency cleanup worker prunes expired entries from the idempotency table. Without it, the table would grow indefinitely.

## 1. The lifetime of an idempotency entry

From [07.06 Idempotency](../07_metadata_graph/06_idempotency.md):

- Created at: when a state-mutating request is processed.
- TTL: 24 hours (configurable).
- Deleted: by this worker, after TTL expires.

## 2. The cycle

Every hour (configurable), the worker:

1. Determines the cutoff time (now - TTL).
2. Scans the idempotency table for entries older than the cutoff.
3. Deletes them in batches.

## 3. Implementation

```rust
async fn cleanup_cycle(state: &ShardState) -> Result<usize> {
    let cutoff = Timestamp::now() - state.config.idempotency_ttl;
    let mut total_deleted = 0;

    loop {
        let mut deleted_in_batch = 0;
        let mut wtxn = state.metadata.begin_write()?;
        let mut idem = wtxn.open_table(IDEMPOTENCY)?;
        
        // Collect candidates first (can't delete while iterating)
        let to_delete: Vec<RequestId> = idem.iter()?
            .take_while(|(_, e)| deleted_in_batch < 1000)
            .filter(|(_, e)| e.created_at < cutoff)
            .map(|(k, _)| k.to_owned())
            .collect();
        
        for key in &to_delete {
            idem.remove(key)?;
            deleted_in_batch += 1;
        }
        
        wtxn.commit()?;
        total_deleted += deleted_in_batch;

        if deleted_in_batch < 1000 {
            break;    // No more to delete
        }
        
        glommio::yield_now().await;
    }

    Ok(total_deleted)
}
```

The cleanup is incremental — at most 1000 deletes per transaction. Multiple iterations cover all expired entries.

## 4. The size implications

For a shard processing 1000 mutations per second with 24-hour TTL:

- Steady state: ~86M entries.
- At ~50 bytes each: ~4 GB.
- The cleanup keeps the size from growing past this.

For lower mutation rates, the table is much smaller. For higher rates, it scales linearly.

## 5. The TTL choice

24 hours is the default. The trade-off:

- Shorter TTL: smaller table, less memory/disk usage.
  - Risk: a slow client retry might miss the idempotency window and produce a duplicate.
- Longer TTL: larger table, more storage.
  - Benefit: more retry tolerance.

For typical clients (which retry within seconds to minutes), 24 hours is more than enough. For unusual cases (clients that crash and restart hours later), 24 hours covers most.

## 6. The configuration

```toml
[idempotency]
ttl = "24h"

[workers.idempotency_cleanup]
enabled = true
interval = "1h"
batch_size = 1000
```

Cleanup interval is 1 hour by default. With TTL of 24 hours, this means at any time the table has 0-1 hour worth of "to-be-deleted" entries — the lag is bounded.

## 7. The "lazy deletion" alternative

Instead of a worker, the substrate could delete expired entries lazily — when a duplicate request hits an expired entry, treat it as a miss and proceed.

We use proactive deletion via the worker because:
- Lazy deletion doesn't bound table size.
- The cost of regular cleanup is small.

## 8. The cycle's cost

For a typical 4 GB table:

- One cycle (1000 deletes): ~5 ms.
- Cycles to clean up a 1-hour batch (~3.6M entries): 3,600 cycles → ~18 seconds total.
- Spread across the hour (between cleanup cycles): ~0.5% CPU.

Negligible overhead.

## 9. The "no work" path

If no entries are expired:

```
1. Open transaction.
2. Scan: find no expired entries.
3. Close transaction.
4. Sleep until next cycle.
```

The empty cycle takes ~5 ms total.

## 10. Concurrency with mutations

While the cleanup is running, new mutations are happening. Their idempotency entries are inserted (creating new "young" entries). The cleanup only deletes "old" entries.

The single-writer-per-shard discipline serializes the cleanup's writes with mutation writes. They don't conflict; redb's serialization is sufficient.

## 11. The order of deletion

The cleanup deletes in batch order — typically the order entries were inserted (because the table is sorted by RequestId, which is roughly time-ordered via UUIDv7).

The order doesn't matter for correctness. It does mean the oldest entries are deleted first — natural FIFO.

## 12. Monitoring

Per-cycle metrics:

- `idem_cleanup_entries_deleted` — counter.
- `idem_cleanup_table_size` — current size.
- `idem_cleanup_oldest_age` — age of the oldest entry.

If `oldest_age` grows beyond the TTL, the cleanup is failing or behind.

## 13. The "cleanup paused" risk

If the cleanup worker is paused (e.g., during heavy load), the table grows. Operators should watch the table size metric.

When the cleanup resumes, it catches up over a few cycles.

## 14. The "manual cleanup" override

`ADMIN_IDEMPOTENCY_PRUNE` triggers an immediate full cleanup, bypassing the timer-based scheduling. Useful when the table has grown unexpectedly.

## 15. The TTL change scenario

If an operator changes the TTL (e.g., from 24h to 1h):

- New entries are created normally.
- The cleanup worker uses the new TTL on its next cycle.
- The next cleanup cycle deletes everything older than 1h ago — potentially a large batch.

The worker handles this by spreading the large batch across multiple cycles. Eventually, the table converges to 1h-of-data steady state.

## 16. The deletion vs replay race

When a client retries a request just as the cleanup is deleting the entry:

- If the cleanup's transaction commits first: the lookup misses; the request is processed as new. May produce a duplicate.
- If the replay's lookup happens first: the cleanup's transaction is delayed; the lookup finds the entry; replay returns the cached response.

The race window is small (microseconds). For workloads where it matters, increase the TTL.

## 17. The "long-tail retry" caveat

A pathological client could retry days after the original request. By the time it retries, the idempotency entry is gone. The substrate processes the retry as a new request, possibly producing a duplicate.

For typical clients, this isn't a concern. The substrate's contract is "idempotency within the TTL window".

---

*Continue to [`06_slot_reclamation.md`](06_slot_reclamation.md) for slot reclamation.*
