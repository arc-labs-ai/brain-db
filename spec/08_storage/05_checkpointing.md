# 08.05 Checkpointing and WAL Retention

A **checkpoint** is a marker indicating that all WAL records up to a specific LSN are reflected in the arena and metadata store. After a checkpoint, the WAL records before the checkpoint LSN are eligible for deletion (subject to SUBSCRIBE retention).

This file specifies:

- What a checkpoint is and why Brain requires it.
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
    durable_lsn: u64,                    // Highest contiguous prefix of records accounted for in metadata
    arena_capacity_at_checkpoint: u64,
    metadata_version_at_checkpoint: u64,
    started_at: u64,                     // unix nanoseconds
    completed_at: u64,
}
```

`durable_lsn` is the shard's published durable watermark, clamped to the WAL tail sampled at `CHECKPOINT_BEGIN`. It is *not* the WAL-appended tail. The watermark only advances past a record once that record's redb transaction has committed, so everything at or below `durable_lsn` is genuinely reflected in metadata and recovery may skip it; a record that is WAL-durable but not yet committed sits strictly above and is replayed.

The clamp is there because the watermark is read after `CHECKPOINT_BEGIN`: the watermark is always ≤ the true WAL tail, but a commit concurrent with the checkpoint could push it past the tail that was sampled, and `min` keeps the checkpoint consistent with the WAL bundled alongside it.

The metadata store has a singleton table holding the most recent checkpoint:

```
table: checkpoints
key: checkpoint_id (u64)
value: Checkpoint struct
```

Multiple checkpoints can exist; Brain keeps the most recent one as the recovery target.

## 3. The checkpoint procedure

A checkpoint is initiated by the snapshot worker, as the opening of a snapshot
(`crates/brain-server/src/shard/adapters.rs:541`). §4 says why there is no
separate checkpoint worker to initiate one.

1. **Begin.** Write a `CHECKPOINT_BEGIN` WAL record. Sample the WAL tail (`next_lsn - 1`) as `target_lsn_hint`.
2. **Sync arena.** Issue `msync(MS_SYNC)` on the arena. This ensures all dirty pages are written back. The arena borrow is held only across this synchronous call, never across an await.
3. **Read the watermark.** `durable_lsn = min(published_watermark, target_lsn_hint)`.
4. **End.** Write a `CHECKPOINT_END` WAL record carrying `durable_lsn` and the arena capacity observed at step 2. Recovery picks the new checkpoint up via `apply(CheckpointEnd)` rather than having it pushed at runtime, which avoids a window in which the sink and the WAL disagree.

Metadata needs no separate sync step: redb flushes on commit, so every write that the watermark counts is already durable in the metadata file.

The HNSW index is in-memory and is not dumped as part of this sequence — the `CHECKPOINT_BEGIN`/`CHECKPOINT_END` pair carries no index step at all. The dump happens after `CHECKPOINT_END`, as part of the same snapshot bundle; see [09.06 Index Persistence & Recovery](../09_indexing/06_persistence.md).

## 3a. Checkpoints do not pause writes

There is no drain and no write stall. The shard keeps accepting and committing writes throughout the sequence above.

The watermark is what makes a drain unnecessary. A write whose records are in the WAL but whose redb commit has not run sits strictly above the published frontier, so step 3 does not claim it and recovery replays it. A write that commits concurrently with the checkpoint either lands below the clamp (and is claimed, correctly, because it committed) or above it (and is replayed). Both outcomes are consistent with the WAL bundled alongside.

A drain would buy nothing the watermark does not already guarantee, and would cost a stall on every checkpoint.

If step 2 fails, `CHECKPOINT_END` is not written. The next recovery sees a `CHECKPOINT_BEGIN` with no matching `END` and ignores it; the previous checkpoint stays the recovery target.

## 4. Checkpoint frequency

There is no checkpoint worker and no checkpoint-only cadence. The
`CHECKPOINT_BEGIN` → arena `msync` → `CHECKPOINT_END` sequence of §3 runs only
as the opening of a snapshot, inlined in the snapshot path
(`crates/brain-server/src/shard/adapters.rs:541-620`, with the snapshot bundle
laid out from `:623`). A checkpoint therefore happens exactly when a snapshot
happens, and the snapshot worker's cadence is the whole of the answer.

Three things take a snapshot, and so three things write a checkpoint:

- **The snapshot worker, hourly.** `WorkerKind::Snapshot` carries a built-in
  interval of 3600 s (`crates/brain-workers/src/config.rs:168`), which the
  worker adopts at construction
  (`crates/brain-workers/src/workers/snapshot.rs:178`) and the scheduler loop
  reads once and sleeps on (`crates/brain-workers/src/scheduler.rs:377`,
  `:462`). The worker skips its first tick (`snapshot.rs:212`), so a shard's
  first checkpoint lands an hour after spawn rather than at spawn. A cycle
  whose vector index is empty returns without checkpointing
  (`snapshot.rs:236`), so a shard that has never taken an encode never writes
  a checkpoint record at all.
- **Graceful shutdown**, once, after the worker scheduler has drained and
  under the same non-empty-index condition
  (`crates/brain-server/src/shard/mod.rs:5167-5175`). This is the fast-restart
  path of §11.
- **An operator**, on demand, through `POST /v1/snapshots?shard=N`
  (`crates/brain-server/src/admin/handlers/snapshot/create.rs:21`) and the
  shard request it dispatches (`crates/brain-server/src/shard/mod.rs:1376`,
  `:4732`). The manual path does not apply the empty-index condition.

### The cadence is not configurable

Hourly is the only cadence a build runs. `workers.snapshot_interval_sec` is
parsed into the workers config
(`crates/brain-server/src/config/mod.rs:589`) and validated as non-zero at
boot (`:1914`), and then nothing reads it. The function that registers the
eleven maintenance workers takes no configuration argument at all and
constructs the snapshot worker bare
(`crates/brain-server/src/shard/mod.rs:2681-2711`); the only workers handed a
resolved cadence are the four with their own `[workers.*]` sub-section —
auto-edge, temporal-edge, extractor and causal-edge (`:4287`, `:4317`,
`:4451`, `:4558`). There is no `[workers.snapshot]` sub-section to fall back
on. The scheduler reads a worker's interval once when its loop starts, so the
admin surface cannot move it either: it offers pause, resume and run-now, and
nothing that rewrites a cadence
(`crates/brain-workers/src/scheduler.rs:185`, `:197`, `:214`).

The shipped configuration files set `snapshot_interval_sec = 21600`
(`config/dev.toml:85`, `config/docker.toml:145`). That six hours has no effect
on anything. A default build checkpoints hourly with those files in place,
exactly as it does without them, and an operator who edits the value will
observe no change. Shortening or lengthening the real cadence today means
driving `POST /v1/snapshots` on a schedule of your own, or changing the
built-in default and rebuilding.

### No size trigger

Nothing counts bytes. Neither the worker cycle
(`crates/brain-workers/src/workers/snapshot.rs:224-246`) nor the
checkpoint-and-bundle sequence it drives
(`crates/brain-server/src/shard/adapters.rs:541`) consults WAL size, segment
count or arena growth. A checkpoint is a function of elapsed time, of
shutdown, or of an operator, and of nothing else. WAL volume is the reason a
checkpoint matters — §6 — but it is not an input to when one is taken.

### The trade-off, at an hour

- **Frequent checkpoints:** low recovery time, more checkpoint overhead.
- **Infrequent checkpoints:** high recovery time on crash, low overhead.

An hour is the replay tail a crash exposes, and the restart cost scales with
whatever that hour accumulated. The snapshot bundle holds the cost down in
practice, because the vector graph loads from its dump instead of rebuilding
(see [09.06 Index Persistence & Recovery](../09_indexing/06_persistence.md)),
but the WAL written since the last checkpoint is replayed regardless. A
latency-critical deployment that wants a shorter window has no configuration
knob that delivers one.

## 5. Concurrency during checkpoint

Checkpoints are non-blocking:

- Writes in flight at `CHECKPOINT_BEGIN` keep running; nothing waits for them.
- Writes arriving during the sequence are appended and committed normally.
- Whether a given write is covered by this checkpoint is decided by the watermark at step 3, not by when it arrived.

The checkpoint's own two records compete for the group-commit window like any other append, and the arena `msync` is a synchronous call that holds the shard's core for its duration — so the shard is not *idle* during a checkpoint, but it is never closed to writes.

The checkpoint path appends its records directly rather than through the writer, and those records are deliberately never reserved against the watermark: they mutate no redb state, so a reservation nothing could resolve would hold the frontier down for the life of the shard.

## 6. WAL retention

After a checkpoint completes, WAL segments containing only records older than `durable_lsn` are eligible for deletion. The retention policy decides whether to delete or keep them.

Retention reasons:

1. **SUBSCRIBE clients.** A client subscribed from an old LSN needs the records still available. Brain tracks the oldest active SUBSCRIBE LSN and won't delete segments containing records newer than it.
2. **Backup window.** Operators may want a few minutes/hours of WAL retained to support point-in-time recovery from snapshots.
3. **Debugging.** Operators may want to retain WAL for forensics.

The retention horizon (oldest LSN Brain promises to keep) is the maximum of:

- The checkpoint's `durable_lsn` (Brain does not need anything older for recovery).
- The oldest active SUBSCRIBE's LSN.
- A configurable time-based floor (default: keep at least 1 hour of WAL).
- A configurable LSN-based floor (default: keep at least 1 GiB of WAL).

Brain periodically runs a retention sweep:

```rust
let oldest_kept_lsn = compute_retention_horizon();

for segment in wal_segments() {
    if segment.last_lsn < oldest_kept_lsn {
        delete_segment(segment);
    }
}
```

## 7. The retention horizon and SubscriptionLsnTooOld

When a SUBSCRIBE client requests `from_lsn` older than the retention horizon, the server returns `SubscriptionLsnTooOld`. The client must either:

- Restart from the current LSN (losing the gap).
- Restore from a snapshot at an older point and replay forward.

The retention horizon is visible through the WAL gauges on `GET /metrics` (`brain_wal_size_bytes`, `brain_wal_segments`) so operators can monitor it.

## 8. Checkpoint corruption

If the checkpoint marker in the metadata store is corrupted or missing, Brain falls back to replaying the WAL from the beginning of the oldest available segment. This is slow but correct.

In the worst case (oldest segment plus checkpoint both corrupted), Brain refuses to start. Restore from a backup.

## 9. The relationship between checkpoint and snapshot

A **checkpoint** is an internal consistency point; the WAL records before it are no longer needed for recovery.

A **snapshot** is a backup; the entire shard's state at a point in time, copied or referenced for offline use.

A snapshot includes the arena, metadata store, and recent WAL. Snapshots typically include the most recent checkpoint plus the WAL records since.

Detailed snapshot procedure is in [`06_snapshots.md`](06_snapshots.md).

## 10. Checkpoint observability

Brain exposes checkpoint metrics:

- `checkpoint_count` — total checkpoints since startup.
- `last_checkpoint_at` — timestamp of the most recent checkpoint.
- `last_checkpoint_duration_ms` — how long the most recent took.
- `wal_size_bytes` — current size of all WAL segments.
- `wal_oldest_lsn` — the LSN of the oldest record still on disk.
- `wal_retention_target_lsn` — the LSN below which Brain deletes on next sweep.

Available on `GET /metrics`, and per shard in `GET /v1/diagnostics/debug-snapshot`. Operators monitor these to understand recovery times and disk usage.

## 11. The "fast restart" path

For a graceful shutdown, Brain runs a checkpoint just before exit. This means:

- Recovery on next startup has minimal work.
- Shutdown takes a few hundred milliseconds longer due to the final checkpoint.

For ungraceful shutdown (kill -9, OOM), no final checkpoint happens. Recovery uses the most recent prior checkpoint.

## 12. Checkpoint failures

### 12.1 Disk full during checkpoint

The arena `msync` in step 2 and the two WAL appends sync data to disk. If the disk is full, a sync fails. The checkpoint aborts:

- The CHECKPOINT_BEGIN record is in the WAL.
- No CHECKPOINT_END record is written.
- The previous checkpoint remains the active recovery target.

When disk space is freed, a future checkpoint may complete. Brain logs the failure and retries on schedule.

### 12.2 Watermark behind the WAL tail

A checkpoint whose watermark lags far behind the WAL tail still succeeds; it simply claims less. Recovery then replays more records than strictly necessary, which costs startup time and nothing else.

This is the honest outcome of a long-running write or a stalled commit, and it is why the checkpoint reads the watermark rather than the tail: claiming the tail would be faster to recover from and wrong.

### 12.3 Metadata commit failure

If the metadata store fails to commit the checkpoint table update, the checkpoint is incomplete. The previous checkpoint stays valid; Brain retries.

In all failure cases, Brain is left with a valid (older) checkpoint as the recovery target. Forward progress on checkpoints is the goal; failure is non-catastrophic.

## 13. Checkpoint of a shard with many memories

For shards with many memories (10M+), the per-checkpoint work scales:

- Arena msync: proportional to the number of dirty pages. Most pages aren't dirty between checkpoints (only pages of recently-modified slots), so this is bounded.
- The two WAL appends and the watermark read: constant.

The checkpoint sequence does not scale with memory count. Index persistence does — a graph snapshot for 10M memories is ~30-60 seconds — but it falls outside the `CHECKPOINT_BEGIN`/`CHECKPOINT_END` pair, running after `CHECKPOINT_END` as part of the same snapshot bundle ([09.06](../09_indexing/06_persistence.md)).

Operators of large shards may schedule index snapshots during low-traffic windows, or accept the full-rebuild fallback and its longer recovery time.

## 14. The reverse: very small shards

For very small shards (a few hundred memories), checkpointing is fast (milliseconds). Frequent checkpoints (every minute) become viable, ensuring near-zero recovery time.

This is a workload-specific tuning. The default 10 min / 1 GiB works for typical mid-size shards.

---

*Continue to [`06_snapshots.md`](06_snapshots.md) for snapshots.*
