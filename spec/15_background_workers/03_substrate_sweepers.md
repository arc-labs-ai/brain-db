# 15.03 Substrate Sweepers

The three substrate cleanup workers: the idempotency table sweeper, the slot reclamation worker (arena GC), and the WAL retention worker.

## Idempotency Cleanup Worker

The idempotency sweep worker prunes expired entries from the idempotency table. Without it, the table would grow indefinitely.

### 1. The lifetime of an idempotency entry

From [10. Metadata + Graph Store — substrate tables](../10_metadata/03_substrate_tables.md):

- Created at: when a state-mutating request is processed.
- TTL: 24 hours (configurable).
- Deleted: by this worker, after TTL expires.

### 2. The cycle

Every hour (configurable), the worker:

1. Determines the cutoff time (now - TTL).
2. Scans the idempotency table for entries older than the cutoff.
3. Deletes them in batches.

### 3. Implementation

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

### 4. The size implications

For a shard processing 1000 mutations per second with 24-hour TTL:

- Steady state: ~86M entries.
- At ~50 bytes each: ~4 GB.
- The cleanup keeps the size from growing past this.

For lower mutation rates, the table is much smaller. For higher rates, it scales linearly.

### 5. The TTL choice

24 hours is the default. The trade-off:

- Shorter TTL: smaller table, less memory/disk usage.
  - Risk: a slow client retry might miss the idempotency window and produce a duplicate.
- Longer TTL: larger table, more storage.
  - Benefit: more retry tolerance.

For typical clients (which retry within seconds to minutes), 24 hours is more than enough. For unusual cases (clients that crash and restart hours later), 24 hours covers most.

### 6. The configuration

```toml
[idempotency]
ttl = "24h"

[workers.idempotency_cleanup]
enabled = true
interval = "1h"
batch_size = 1000
```

Cleanup interval is 1 hour by default. With TTL of 24 hours, this means at any time the table has 0-1 hour worth of "to-be-deleted" entries — the lag is bounded.

### 7. The "lazy deletion" alternative

Instead of a worker, expired entries could be deleted lazily — when a duplicate request hits an expired entry, treat it as a miss and proceed.

Proactive deletion via the worker is used because:
- Lazy deletion doesn't bound table size.
- The cost of regular cleanup is small.

### 8. The cycle's cost

For a typical 4 GB table:

- One cycle (1000 deletes): ~5 ms.
- Cycles to clean up a 1-hour batch (~3.6M entries): 3,600 cycles → ~18 seconds total.
- Spread across the hour (between cleanup cycles): ~0.5% CPU.

Negligible overhead.

### 9. The "no work" path

If no entries are expired:

```
1. Open transaction.
2. Scan: find no expired entries.
3. Close transaction.
4. Sleep until next cycle.
```

The empty cycle takes ~5 ms total.

### 10. Concurrency with mutations

While the cleanup is running, new mutations are happening. Their idempotency entries are inserted (creating new "young" entries). The cleanup only deletes "old" entries.

The single-writer-per-shard discipline serializes the cleanup's writes with mutation writes. They don't conflict; redb's serialization is sufficient.

### 11. The order of deletion

The cleanup deletes in batch order — typically the order entries were inserted (because the table is sorted by RequestId, which is roughly time-ordered via UUIDv7).

The order doesn't matter for correctness. It does mean the oldest entries are deleted first — natural FIFO.

### 12. Monitoring

Per-cycle metrics:

- `idem_cleanup_entries_deleted` — counter.
- `idem_cleanup_table_size` — current size.
- `idem_cleanup_oldest_age` — age of the oldest entry.

If `oldest_age` grows beyond the TTL, the cleanup is failing or behind.

### 13. The "cleanup paused" risk

If the cleanup worker is paused (e.g., during heavy load), the table grows. Operators should watch the table size metric.

When the cleanup resumes, it catches up over a few cycles.

### 14. The "manual cleanup" override

`ADMIN_IDEMPOTENCY_PRUNE` triggers an immediate full cleanup, bypassing the timer-based scheduling. Useful when the table has grown unexpectedly.

### 15. The TTL change scenario

If an operator changes the TTL (e.g., from 24h to 1h):

- New entries are created normally.
- The cleanup worker uses the new TTL on its next cycle.
- The next cleanup cycle deletes everything older than 1h ago — potentially a large batch.

The worker handles this by spreading the large batch across multiple cycles. Eventually, the table converges to 1h-of-data steady state.

### 16. The deletion vs replay race

When a client retries a request just as the cleanup is deleting the entry:

- If the cleanup's transaction commits first: the lookup misses; the request is processed as new. May produce a duplicate.
- If the replay's lookup happens first: the cleanup's transaction is delayed; the lookup finds the entry; replay returns the cached response.

The race window is small (microseconds). For workloads where it matters, increase the TTL.

### 17. The "long-tail retry" caveat

A pathological client could retry days after the original request. By the time it retries, the idempotency entry is gone. The retry is processed as a new request, possibly producing a duplicate.

For typical clients, this isn't a concern. The contract is "idempotency within the TTL window".

## Slot Reclamation Worker

The arena GC worker reclaims arena slots from memories that have been forgotten beyond the grace period.

### 18. The lifecycle (recap)

From [02. Data Model — Memory lifecycle](../02_data_model/02_memory.md):

1. Memory is encoded → slot allocated.
2. Memory is forgotten (FORGET) → slot tombstoned, but slot still allocated.
3. Grace period elapses (default 7 days).
4. Slot is reclaimed → slot's data wiped, slot returned to free list.

Step 4 is this worker's job.

### 19. The cycle

Every 10 minutes (configurable):

1. Determine the cutoff: now - grace period.
2. Find tombstoned memories with `tombstoned_at < cutoff`.
3. For each, reclaim the slot.

### 20. Reclamation procedure

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
    
    // 4. Delete edges (in both edges and edges_reverse)
    let mut edges = wtxn.open_table(EDGES_TABLE)?;
    let mut edges_reverse = wtxn.open_table(EDGES_REVERSE_TABLE)?;
    delete_all_edges_for(memory_id, &mut edges, &mut edges_reverse)?;
    
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

### 21. The slot_version increment

A reclaimed slot's version is incremented. This makes the old MemoryId (which encoded the previous version) no longer match the slot:

```
Before: slot 1234 has version 5; MemoryId M_5 = (slot=1234, version=5).
After reclaim: slot 1234 has version 6; M_5 still references version 5.
```

If anything (a stale reference, a buggy client) tries to use M_5 to access slot 1234, the mismatch is detected:

```rust
fn validate_memory_id(id: MemoryId, slot: &Slot) -> bool {
    slot.metadata.slot_version == id.slot_version()
}
```

The mismatch causes the operation to return "not found" or similar.

### 22. The hard-forget special case

A hard-forgotten memory has had its vector and text zeroed at FORGET time. Reclamation just runs the normal procedure:

- Delete metadata, text, edges.
- Increment slot version.
- Free the slot.

The data is already gone (zeroed at FORGET); reclamation just frees the slot for reuse.

### 23. Edge cleanup

When a memory is reclaimed, its edges are deleted. This means edges that pointed to the reclaimed memory are dangling.

Edges are bidirectional in the metadata (see [10. Metadata + Graph Store — substrate tables](../10_metadata/03_substrate_tables.md)). For each `(source, kind, target)` edge:
- It exists in `edges[source, kind, target]`.
- It exists in `edges_reverse[target, kind, source]`.

Reclamation of memory M removes:
- All entries in `edges` where source=M.
- All entries in `edges_reverse` where target=M.

It does NOT remove:
- Entries in `edges` where target=M (the source's outgoing edges to M).
- Entries in `edges_reverse` where source=M (the target's incoming edges from M).

These dangling references are cleaned up by the edge scrub worker (see [`04_misc_workers.md`](04_misc_workers.md)).

### 24. The HNSW reference

When a memory is reclaimed, there's an old HNSW node referencing the (now-version-bumped) slot. The HNSW node has the old MemoryId.

This stale node is left in HNSW until the next maintenance rebuild. Searches that hit it will see the version mismatch (when reading the slot) and skip the result.

The maintenance worker eventually rebuilds the HNSW, removing all stale nodes.

### 25. Batch reclamation

Per cycle, the worker reclaims up to 1000 slots (configurable). Multiple cycles cover all eligible slots.

Each reclaim is its own transaction (single memory at a time). Could be batched into a single transaction for efficiency, but that increases lock duration. The single-memory approach is simpler.

### 26. The cost

Per slot:
- Metadata transaction: ~1 ms.
- Free list update: ~0.001 ms.
- Total: ~1 ms.

For 1000 slots/cycle: ~1 second of work, spread across the cycle's duration.

### 27. The "active" check

Before reclaiming, the worker re-checks that the memory is still tombstoned. A race could happen:

- Memory is tombstoned at time T.
- Grace period elapses at time T+7d.
- The worker schedules reclamation.
- Meanwhile, an operator runs `ADMIN_RESTORE` to undo the FORGET (a hypothetical operation).

The check ensures the system does not reclaim something that was un-tombstoned. If un-tombstoned, the worker skips it.

(`ADMIN_RESTORE` isn't currently implemented; this is defensive.)

### 28. The free list

The free list (in the arena layer, see [08. Storage — Arena](../08_storage/01_arena.md)) is a concurrent data structure. The reclaim worker pushes; the encode path pops.

Free list operations are O(1) amortized via crossbeam-epoch.

### 29. The free list overflow

If the free list grows very large (many reclaimable slots all at once), it consumes memory. Each entry is a few bytes; a million entries are a few MB. Acceptable.

The list is bounded by the number of slots, which is bounded by the arena size.

### 30. The "no eligible work" path

If no slots are eligible for reclamation:

- The worker scans, finds none.
- Cycles end quickly.
- Sleep until next cycle.

For shards with little churn, this is the common case. The worker is mostly idle.

### 31. The grace period configuration

```toml
[memory]
forget_grace_period = "7d"
```

Shorter grace: faster reclamation, less recovery window.
Longer grace: more recovery window, slower reclamation.

For deployments wanting strict data retention (e.g., legal compliance), the grace period might be set to days or weeks.

For deployments wanting fast space recovery (e.g., high churn), grace might be shorter (minutes to hours).

### 32. The "bypass grace" flag

Hard FORGET still respects the grace period for arena GC. But a special flag (`force_reclaim_now=true`) bypasses it:

- Reclaim immediately after FORGET.
- No recovery window.

This is for sensitive data where the grace period is unacceptable. All uses of this flag are logged for audit.

### 33. The cycle interactions

The reclaim worker and the edge scrub worker may operate on the same memory:

- Reclaim deletes the memory's outgoing edges.
- Edge scrub finds dangling edges that point to it (from other memories) and deletes them.

These are independent operations; each transaction is atomic. They don't conflict beyond redb's normal serialization.

### 34. Audit logging

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

## WAL Retention Worker

The WAL retention worker deletes old WAL segments after a checkpoint has covered them.

### 35. The WAL segment lifecycle

From [08. Storage — WAL](../08_storage/02_wal.md):

- WAL is split into segments (256 MiB each by default).
- New writes append to the current segment.
- When a segment fills, it's closed; a new segment is started.
- Closed segments are kept until they're "covered" by a checkpoint.

A segment is **covered** when:
- Its highest LSN is less than the latest checkpoint's `durable_lsn`.
- Equivalently: the metadata reflects all changes from this segment.

A covered segment is no longer needed for recovery.

### 36. The cycle

Every 1 minute (configurable):

1. Read the latest checkpoint.
2. List segments older than the checkpoint's LSN coverage.
3. Delete those segments (and add to deletion log).

### 37. Implementation

```rust
async fn cycle(state: &ShardState) -> Result<usize> {
    let checkpoint = state.metadata.read_latest_checkpoint().await?;
    let cutoff_lsn = checkpoint.durable_lsn;
    let retention_extra = state.config.wal_retention_extra;  // For safety
    let safe_cutoff = cutoff_lsn.saturating_sub(retention_extra);

    let mut deleted = 0;
    let segments = state.wal.list_segments().await?;
    
    for segment in segments {
        if segment.last_lsn < safe_cutoff {
            state.wal.delete_segment(segment.id).await?;
            deleted += 1;
            audit_log("wal_segment_deleted", &segment);
        }
    }

    Ok(deleted)
}
```

Segments below the safe cutoff are deleted. The `retention_extra` (default: 1 segment's worth of LSNs) provides a safety buffer.

### 38. Why retain a buffer

Without the buffer:

- Checkpoint at LSN 1,000,000.
- Latest WAL segment ends at LSN 1,000,500.
- The worker deletes segments ending below 1,000,000.

But the checkpoint may not actually cover everything up to 1,000,000 — it's a snapshot of metadata at the time, and there may be in-flight writes still being applied to metadata when the checkpoint was taken.

The retention buffer protects against this: keep an extra segment's worth, just in case.

In practice, the buffer is rarely needed; checkpointing is conservative. But it's cheap and adds robustness.

### 39. The recovery dependency

WAL retention is critical for recovery: if a needed segment is deleted, recovery fails.

The worker is conservative about deletion:
- Only delete after a confirmed checkpoint covers the segment.
- Apply the retention buffer.
- Verify the segment's LSN range against the checkpoint.

### 40. The disk usage tradeoff

Retention period determines WAL disk usage:

- Default checkpoint cadence: every ~1 hour or 256 MiB worth of WAL.
- Retention: until checkpointed.
- Steady state: ~512 MiB - 1 GiB of WAL on disk per shard.

For deployments with longer checkpoint cadence, more WAL retained. For very short cadence, very little.

### 41. The configuration

```toml
[wal]
retention_extra = "256MiB"        # The buffer
segment_size = "256MiB"

[workers.wal_retention]
enabled = true
interval = "1m"
```

Per-segment cleanup happens promptly (within the next cycle after coverage).

### 42. The "audit"

Each segment deletion is logged:

```
{
  event: "wal_segment_deleted",
  segment_id: 12345,
  first_lsn: 800000,
  last_lsn: 850000,
  size_bytes: 268435456,
}
```

For audit trails, these logs document data lifecycle. If a segment is deleted in error, the log is the first place to look.

### 43. The safety check

Before deleting, the worker double-checks:

- The metadata's `next_lsn` is greater than the segment's last LSN (the metadata has progressed past this segment).
- The shard isn't currently recovering.
- The checkpoint is recent (not stale).

If any check fails, the deletion is skipped. The worker tries again next cycle.

### 44. The "rare failure" case

If a segment is somehow deleted prematurely (a bug), recovery fails:

- Recovery sees a gap in the WAL (LSN X exists in metadata but the WAL for LSN X-1 is missing).
- The system refuses to start.
- An operator must restore from backup.

Such failures are bugs and should be caught in testing. The retention buffer provides defense-in-depth.

### 45. The interaction with snapshots

When a snapshot is taken (`ADMIN_SNAPSHOT_CREATE`):

- The snapshot includes the current state up to a known LSN.
- The snapshot is not a substitute for the WAL.

WAL retention is independent of snapshots. A snapshot may exist that includes the deleted WAL data, but recovery uses snapshots only via explicit `ADMIN_RESTORE`.

### 46. The cleanup cost

Per segment deletion: ~10-50 ms (for Linux's unlink + filesystem metadata update).

For a 1-minute cycle deleting 1-2 segments: negligible.

### 47. The "no deletions" path

If no segments are eligible for deletion:

- The worker checks, finds none.
- Sleep until next cycle.

For low-write shards, most cycles have nothing to delete.

### 48. The disk full prevention

If the disk is filling, the system doesn't aggressively delete WAL — that would risk recoverability. Instead:

- The system sheds load (rejects new writes).
- The operator must address disk space.

WAL retention is conservative; it prefers data safety over disk reclamation.

### 49. The audit log itself

The deletion logs are in the structured log stream. They can be exported or stored separately for compliance.

For high-compliance deployments, the logs may include cryptographic hashes of segments before deletion (so an auditor can verify what was deleted).

For now, simple structured logs are enough. Cryptographic logging is a future enhancement.

### 50. The "manual delete" override

`ADMIN_WAL_PRUNE <up_to_lsn>` triggers an immediate retention pass with the specified LSN cutoff. Useful for:

- Reclaiming space after operator confirmation.
- Cleaning up after a known recovery completion.

The operation respects the safety checks; if the cutoff is unsafe (would delete uncheckpointed data), it's rejected.

---

*Continue to [`04_misc_workers.md`](04_misc_workers.md) for the remaining workers.*
