# 05.09 Checkpointing and WAL Retention

A **checkpoint** is a marker indicating that all WAL records up to a specific LSN are reflected in the arena and metadata store. After a checkpoint, the WAL records before the checkpoint LSN are eligible for deletion (subject to SUBSCRIBE retention).

This file specifies:

- What a checkpoint is and why we need it.
- How checkpoints are created.
- How WAL retention works.
- How recovery uses checkpoints.

## 1. Why checkpoint

Without checkpointing, recovery would always replay the entire WAL from the very first record. As the WAL grows, recovery time grows with it, eventually becoming prohibitive.

Checkpointing bounds recovery time. After a checkpoint, recovery can skip everything before the checkpoint's LSN and start from there.

## 2. The checkpoint marker

A checkpoint is a record in the metadata store:

```rust
struct Checkpoint {
    checkpoint_id: u64,                  // Monotonic counter
    durable_lsn: u64,                    // All records up to and including this LSN are durable in arena+metadata
    arena_capacity_at_checkpoint: u64,
    metadata_version_at_checkpoint: u64,
    started_at: u64,                     // unix nanoseconds
    completed_at: u64,
}
```

The metadata store has a singleton table holding the most recent checkpoint:

```
table: checkpoints
key: checkpoint_id (u64)
value: Checkpoint struct
```

Multiple checkpoints can exist; the substrate keeps the most recent one as the recovery target.

## 3. The checkpoint procedure

A checkpoint is initiated by the checkpoint worker (a background task):

1. **Begin.** Write a `CHECKPOINT_BEGIN` WAL record. Note the current LSN as `target_lsn`.
2. **Drain.** Wait for all in-flight writes to complete. The substrate stops accepting new writes briefly (or buffers them in a "pending" queue).
3. **Sync arena.** Issue `msync(MS_SYNC)` on the arena. This ensures all dirty pages are written back.
4. **Sync metadata.** redb's flush-on-commit means metadata is durable record-by-record; the checkpoint just verifies the redb log is fully synced.
5. **Sync HNSW state.** The HNSW index is in-memory; "syncing" means dumping a serialized form of it to a checkpoint file. Optional in v1; if not done, recovery rebuilds from arena + metadata.
6. **End.** Write a `CHECKPOINT_END` WAL record with `durable_lsn = target_lsn`. Update the checkpoint table in the metadata store.
7. **Resume.** Resume accepting normal writes.

Steps 2–6 are typically completed in tens of milliseconds — fast enough that operators don't notice the brief drain.

## 4. Checkpoint frequency

The checkpoint worker runs on a schedule:

- **Time-based:** every 10 minutes by default.
- **Size-based:** after 1 GiB of new WAL has been written.

Whichever comes first triggers a checkpoint. These intervals are configurable.

The trade-off:

- **Frequent checkpoints:** low recovery time, more checkpoint overhead.
- **Infrequent checkpoints:** high recovery time on crash, low overhead.

10 min / 1 GiB is conservative — recovery from such a checkpoint should take under a minute. For latency-critical deployments, more frequent checkpoints reduce worst-case recovery time.

## 5. Concurrency during checkpoint

The substrate aims for checkpoints to be non-blocking:

- The drain in step 2 waits for in-flight writes to complete (a few hundred microseconds typically).
- New writes after the drain are buffered briefly (during steps 3–6).
- The buffered writes are appended to the WAL after the checkpoint completes, with LSNs starting from `target_lsn + 1`.

Total stall time: typically 10–50 ms. For workloads that can tolerate a brief pause, this is fine. For workloads that can't, the substrate offers a "non-blocking checkpoint" mode (more complex; uses snapshot semantics).

In v1, the brief stall is the default. Non-blocking is an open question ([`12_open_questions.md`](12_open_questions.md)).

## 6. WAL retention

After a checkpoint completes, WAL segments containing only records older than `durable_lsn` are eligible for deletion. The retention policy decides whether to delete or keep them.

Retention reasons:

1. **SUBSCRIBE clients.** A client subscribed from an old LSN needs the records still available. The substrate tracks the oldest active SUBSCRIBE LSN and won't delete segments containing records newer than it.
2. **Backup window.** Operators may want a few minutes/hours of WAL retained to support point-in-time recovery from snapshots.
3. **Debugging.** Operators may want to retain WAL for forensics.

The retention horizon (oldest LSN we promise to keep) is the maximum of:

- The checkpoint's `durable_lsn` (we don't need anything older for recovery).
- The oldest active SUBSCRIBE's LSN.
- A configurable time-based floor (default: keep at least 1 hour of WAL).
- A configurable LSN-based floor (default: keep at least 1 GiB of WAL).

The substrate periodically runs a retention sweep:

```rust
let oldest_kept_lsn = compute_retention_horizon();

for segment in wal_segments() {
    if segment.last_lsn < oldest_kept_lsn {
        delete_segment(segment);
    }
}
```

## 7. The retention horizon and LsnTooOld

When a SUBSCRIBE client requests `from_lsn` older than the retention horizon, the server returns `LsnTooOld`. The client must either:

- Restart from the current LSN (losing the gap).
- Restore from a snapshot at an older point and replay forward.

The retention horizon is reported in `ADMIN_STATS` so operators can monitor it.

## 8. Checkpoint corruption

If the checkpoint marker in the metadata store is corrupted or missing, the substrate falls back to replaying the WAL from the beginning of the oldest available segment. This is slow but correct.

In the worst case (oldest segment plus checkpoint both corrupted), the substrate refuses to start. Restore from a backup.

## 9. The relationship between checkpoint and snapshot

A **checkpoint** is an internal consistency point; the WAL records before it are no longer needed for recovery.

A **snapshot** is a backup; the entire shard's state at a point in time, copied or referenced for offline use.

A snapshot includes the arena, metadata store, and recent WAL. Snapshots typically include the most recent checkpoint plus the WAL records since.

Detailed snapshot procedure is in [`10_snapshots.md`](10_snapshots.md).

## 10. Checkpoint observability

The substrate exposes checkpoint metrics:

- `checkpoint_count` — total checkpoints since startup.
- `last_checkpoint_at` — timestamp of the most recent checkpoint.
- `last_checkpoint_duration_ms` — how long the most recent took.
- `wal_size_bytes` — current size of all WAL segments.
- `wal_oldest_lsn` — the LSN of the oldest record still on disk.
- `wal_retention_target_lsn` — the LSN below which we will delete on next sweep.

Available via `ADMIN_STATS`. Operators monitor these to understand recovery times and disk usage.

## 11. The "fast restart" path

For a graceful shutdown, the substrate runs a checkpoint just before exit. This means:

- Recovery on next startup has minimal work.
- Shutdown takes a few hundred milliseconds longer due to the final checkpoint.

For ungraceful shutdown (kill -9, OOM), no final checkpoint happens. Recovery uses the most recent prior checkpoint.

## 12. Checkpoint failures

### 12.1 Disk full during checkpoint

Steps 3–4 sync data to disk. If the disk is full, sync fails. The checkpoint aborts:

- The CHECKPOINT_BEGIN record is in the WAL.
- No CHECKPOINT_END record is written.
- The previous checkpoint remains the active recovery target.

When disk space is freed, a future checkpoint may complete. The substrate logs the failure and retries on schedule.

### 12.2 Drain timeout

If in-flight writes take too long to complete (a slow disk, an unusual workload), the drain in step 2 may time out. The substrate logs the warning, aborts the checkpoint, and retries on schedule.

### 12.3 Metadata commit failure

If the metadata store fails to commit the checkpoint table update, the checkpoint is incomplete. The previous checkpoint stays valid; the substrate retries.

In all failure cases, the substrate is left with a valid (older) checkpoint as the recovery target. Forward progress on checkpoints is the goal; failure is non-catastrophic.

## 13. Checkpoint of a shard with many memories

For shards with many memories (10M+), the per-checkpoint work scales:

- Arena msync: proportional to the number of dirty pages. Most pages aren't dirty between checkpoints (only pages of recently-modified slots), so this is bounded.
- HNSW dump (optional): proportional to the number of memories. For 10M memories, ~30-60 seconds.

Operators of large shards may disable HNSW dumps and accept a longer recovery time, or schedule checkpoints during low-traffic windows.

## 14. The reverse: very small shards

For very small shards (a few hundred memories), checkpointing is fast (milliseconds). Frequent checkpoints (every minute) become viable, ensuring near-zero recovery time.

This is a workload-specific tuning. The default 10 min / 1 GiB works for typical mid-size shards.

---

*Continue to [`10_snapshots.md`](10_snapshots.md) for snapshots.*
