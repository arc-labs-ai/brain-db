# 09.03 HNSW Lifecycle

Persistence and maintenance. The HNSW index is derived state; this file specifies how it is rebuilt on startup, optionally snapshotted for fast restart, and maintained against degradation over time.

## Persistence

The HNSW index is never a primary on-disk structure. A graph dump is written on every snapshot cycle and loaded at startup when it verifies, and the rebuild from durable state is always there underneath it. This section specifies both paths and how a restart chooses between them.

### 1. Why no primary persistence

The HNSW graph is derived state — given the vectors (in the arena) and which memories are active (in the metadata), the graph can be rebuilt deterministically.

The graph is never the primary copy of anything, and the rebuild path is always
available:

- **The graph is large** — ~150 MB per million memories.
- **The graph is fragile** — a partial write or corruption corrupts the entire structure; partial recovery is hard.
- **The rebuild is affordable** — ~30 seconds for 1M memories.
- **redb and the arena are the source of truth** — the graph is derived state, so a dump is an optimisation and never an authority.

That is why a dump is refused rather than trusted whenever anything about it
fails to verify (§5.2), and why a restored backup rebuilds instead of loading
one (§9). What the dump buys is restart time, which is the whole reason the
snapshot worker writes one on every cycle (§5).

### 2. The rebuild procedure

At startup, after WAL replay completes ([08.04 Recovery](../08_storage/04_recovery.md)):

```
1. Initialize an empty HNSW with configured parameters.
2. Snapshot every occupied, non-tombstoned arena slot as (MemoryId, vector).
   At this point the arena is the freshly-replayed image of every durable
   memory, which is what makes it authoritative here — and only here
   ([06_persistence.md](06_persistence.md) §2a).
3. For each pair, in order:
   a. Insert into HNSW.
   b. Update the id map.
4. Publish the result, folding in the pending buffer so a boot-time
   insert cannot be lost.
5. The shard is marked ready.
```

Step 3 is **sequential**. The insert loop runs on the shard's own executor, one
vector at a time, which keeps the build deterministic and keeps the ordering
that graph quality depends on (§4).

For 1M memories on commodity hardware the sequential build is ~30 seconds. The
`hnsw_rs` parallel insert path would bring that to ~5 seconds on 16 threads;
Brain does not use it, so that figure is headroom rather than a measurement of
this path. The snapshot path (§5) is what makes the 30 seconds rare, not
parallelism.

### 3. Active memories only

Only active (non-tombstoned) memories are inserted into the rebuilt HNSW. Tombstoned memories are skipped — the rebuild is also a "compaction" in the sense that it strips out tombstones.

### 4. Memory order

The order of insertion during rebuild affects HNSW quality slightly. Metadata-store order is used, which is roughly insertion order (B-tree is keyed by MemoryId, and MemoryIds are roughly time-ordered via UUIDv7).

For pathological inputs (e.g., memories that lie exactly on a 1D manifold), insertion order matters more. For typical workloads, the order is effectively random.

### 5. Fast-restart persistence

For deployments wanting faster restart, a serialized HNSW can be written to disk during checkpointing. This is the **HNSW snapshot**.

#### 5.1 Procedure

Taking a snapshot brackets a checkpoint and then lays out a bundle. The graph dump belongs to the **bundle**, not to the checkpoint sequence: CHECKPOINT_BEGIN → msync the arena → CHECKPOINT_END carries no index step at all. After CHECKPOINT_END, alongside the reflinked arena, metadata and WAL segments:

1. Write the published graph as three files under the bundle directory, at the basename `hnsw`.
2. Stamp the checkpoint's durable LSN into the wrapper as `taken_at_lsn`.

Writes are never paused — not for the checkpoint and not for the dump. There is no drain anywhere in the path, and no stall to budget for. The whole sequence runs as a task on the shard's own executor, so a write that interleaves with it simply lands above `taken_at_lsn`, which is exactly what the restore's tail-replay is for. See [08.05 Checkpointing](../08_storage/05_checkpointing.md) §3a for why the durable watermark makes a drain unnecessary.

A snapshot is a **triple**, because hnsw_rs's own dump is two files and Brain's wrapper is the third:

```
<dir>/hnsw.hnsw.graph     hnsw_rs graph dump
<dir>/hnsw.hnsw.data      hnsw_rs data dump
<dir>/hnsw.brain          Brain's wrapper, written LAST
```

The wrapper is written last, so its presence is the marker for "snapshot complete", and it carries the BLAKE3 of each sibling so cross-file integrity is verifiable from the wrapper alone:

```
[header: 64 bytes]
  magic: "BHN0"
  format_version: u32            (= 3)
  shard_uuid: [u8; 16]
  taken_at_lsn: u64
  graph_node_count: u64
  parameters: { m, ef_construction, ef_search, ef_search_max }
  vector_dim: u32
  header_crc32c: u32             (CRC32C over bytes [0..60])
[id_map: count, then (memory_id, internal_id) pairs, then next_internal_id]
[tombstones: bitmap words + set count]
[graph_hash: BLAKE3 of hnsw.hnsw.graph]
[data_hash:  BLAKE3 of hnsw.hnsw.data]
[footer: 8 bytes — BLAKE3 of everything before it, truncated to u64]
```

#### 5.2 Restore

At startup, the shard takes the most recent snapshot directory and, if its triple loads and verifies:

1. Read the wrapper header; verify magic, format version, shard_uuid, header CRC, and both sibling hashes.
2. Deserialize the graph, the id map and the tombstone bitmap, and publish it as the main graph.
3. Tail-replay: scan the arena and push every memory the loaded graph doesn't hold into the pending buffer. Those are the writes that landed after `taken_at_lsn`.
4. Reconcile deletes: for every id the snapshot holds, re-tombstone the ones redb now marks inactive. The arena tail carries only active memories, so a FORGET that landed after `taken_at_lsn` is invisible to step 3 and would otherwise leave a ghost node — a live top-k slot diverging from redb indefinitely.
5. The HNSW active set now matches redb's.

Any failure along the way — no snapshot, wrong magic, wrong format version, wrong shard_uuid, failed CRC or hash, hnsw_rs deserialization error — logs and falls through to the full rebuild. The rebuild is correct on its own; the snapshot only makes it faster.

For a snapshot covering 1M memories plus 1000 post-snapshot WAL records:
- Snapshot deserialize: ~1-2 seconds.
- WAL replay for HNSW: ~1 second.
- Total: ~3 seconds.

Compared to ~5 seconds rebuild from scratch, the gain is modest at this size. For larger shards (10M+) or slower hardware, the gain is more meaningful.

#### 5.3 Failure modes

If the snapshot is corrupted (CRC fails, deserialize errors), the system falls back to full rebuild. No data loss; just slower startup.

If the snapshot is older than the metadata (some checkpointing failure), the LSN comparison detects this and the system rebuilds rather than using a stale snapshot.

### 6. What a restart actually costs

These are not two modes to choose between. The dump is always written; which path a restart takes depends only on whether a dump verifies.

| Path | Restart time (1M) | Disk overhead |
|---|---|---|
| Dump loads | 3-5 s | ~150 MB per snapshot |
| Dump missing or refused | ~30 s | — |

The only knob is how often the snapshot worker runs:

```
[workers]
snapshot_interval_sec = 3600   # code default; the shipped configs set 21600
```

Lengthening it widens the WAL tail a restart must replay past `taken_at_lsn`; it does not turn persistence off.

### 7. The metadata-only rebuild

A subtle case: what if the arena is fine but the metadata store is restored from an older backup?

- The metadata says memory M exists.
- The arena slot for M is the right vector (assuming arena wasn't restored).
- HNSW rebuild finds M in metadata, reads its vector from the arena, inserts it.

Result: the rebuilt HNSW is consistent with both the arena and metadata. No issue.

The opposite case (arena restored but metadata current) is more problematic — the metadata might reference vectors that aren't in the older arena. This is detected during rebuild and logged as warnings. Affected memories are skipped.

### 8. Recovery integration

HNSW rebuild happens after WAL replay during startup recovery:

```
1. Open metadata store.
2. Open arena.
3. Replay WAL.
4. (Optional: deserialize HNSW snapshot if present and valid.)
5. Rebuild HNSW from active memories (or replay-from-snapshot path).
6. Mark shard ready.
```

Steps 1-5 are sequential within a shard. Across shards, they happen in parallel.

### 9. Snapshot vs full backup

The HNSW snapshot is a fast-restart artifact, not a backup. The arena and metadata are sufficient to reconstruct everything, so the snapshot bundle's manifest ([08.06 Snapshots](../08_storage/06_snapshots.md)) deliberately leaves the HNSW triple out of the files it hashes: a restored bundle rebuilds the index rather than trusting a graph from it.

What the triple does serve is this shard's own restart, where the directory it was written into is still the one the shard reads.

### 10. Cross-version persistence

The wrapper's `format_version` protects against incompatible loads: a version this binary doesn't write is refused and the shard rebuilds instead.

So a restart on a binary whose snapshot format moved is a slower first start, never a wrong index and never data loss — the graph is derived state, and the arena and metadata it derives from are untouched.

### 11. The "warm" rebuild

For large shards where rebuild takes a meaningful fraction of a minute, a "warm" path could expose: respond to read queries against the partially-built index (with degraded recall) while rebuild completes.

This is not currently implemented. The shard isn't marked ready until rebuild completes; queries return `ShardNotReady` until then.

A future enhancement: partial-readiness, where the shard accepts queries during rebuild with a "best effort" recall caveat.

## Maintenance

The HNSW index degrades over time as memories are added and removed. The maintenance worker monitors quality and rebuilds when needed.

### 12. Why maintenance

Two kinds of degradation:

#### 12.1 Tombstone accumulation

Each FORGET adds a tombstone. Tombstones consume graph edges without contributing to results. Above ~30%, search recall and latency suffer noticeably.

#### 12.2 Topological drift

The HNSW graph's quality depends on the order of insertions. Over time, with many inserts and pseudo-deletes (tombstones), the graph's edge structure becomes suboptimal — the M edges per node may not be the best M edges for current navigability.

Rebuilding from scratch produces a graph that's optimal for the current memory set.

### 13. The maintenance worker

A background task per shard:

```
loop {
    sleep(maintenance_interval);  // default 5 min

    let stats = collect_index_stats(shard);
    let action = decide_action(stats);

    match action {
        Action::None => continue,
        Action::PartialRebuild => partial_rebuild(shard),
        Action::FullRebuild => full_rebuild(shard),
        Action::ScheduleRebuildSoon => schedule(shard),
    }
}
```

The worker is rate-limited; one maintenance action per shard at a time.

### 14. Decision criteria

```rust
fn decide_action(stats: IndexStats) -> Action {
    if stats.tombstone_ratio > 0.30 {
        return Action::FullRebuild;
    }
    if stats.recall_estimate < 0.90 {
        return Action::FullRebuild;
    }
    if stats.tombstone_ratio > 0.15 || stats.recall_estimate < 0.93 {
        return Action::ScheduleRebuildSoon;
    }
    Action::None
}
```

Thresholds are configurable. Defaults are conservative — most workloads never hit them.

### 15. The recall estimate

The maintenance worker estimates current recall by:

1. Selecting a random sample of recent queries (logged with their results).
2. Re-running them with a much higher ef_search (say, 500) to get a "near-truth" result set.
3. Comparing the original result set's overlap with the higher-ef set.

The overlap fraction approximates recall@K. If it falls below the configured threshold, rebuild is triggered.

This is an approximation — not exact ground-truth — but adequate for detecting drift. Exact ground truth is not computed (would require brute-force search over all vectors, expensive).

### 16. Full rebuild

Procedure:

1. **Read the live set.** Ask redb for every active memory's write-time vector. Not the arena — at runtime the arena is stale by this run's writes ([06_persistence.md](06_persistence.md) §2a).
2. **Build.** Allocate a new HNSW and insert each vector sequentially, with no lock held.
3. **Fold and publish, as one step.** `flush_with_rebuild` takes the pending buffer's entries, lets them shadow the redb snapshot for any id in both (the buffered vector is the later write), builds the graph from the union, then stores the new epoch and drains the folded entries under the same guard.

Step 3 is the catch-up, and it is why no WAL replay is needed between
build-start and swap: anything written while the build ran is in the pending
buffer, and the buffer is read inside the publishing call rather than before
it. The old graph is freed once the last reader holding its `Arc` drops the
reference — there is no separate cleanup step.

The rebuild is non-blocking — reads and writes continue against the old index during the build. The atomic swap is a brief moment (microseconds).

### 17. Rebuild duration

For 1M active memories:

- Build phase: ~30 seconds sequential (5 seconds is what the unused parallel insert path would give).
- Fold: proportional to the pending buffer, which is bounded by the writes taken since the last publish — typically well under a second.
- Swap: microseconds.

For larger shards (10M), build phase scales linearly. Very large rebuilds may be spread across multiple cycles to avoid using too much memory at once.

### 18. Memory pressure during rebuild

During rebuild, both the old and new HNSW are in memory: ~300 MB for 1M memories. For larger shards, rebuild memory peaks proportionally.

Nothing bounds this today: there is no memory cap on a rebuild and no phase-splitting, so a shard sized such that two copies of its graph do not fit is a shard whose rebuild cannot run. Sizing is the operator's lever — the 0.30 tombstone threshold is what decides when the second copy appears.

### 19. Partial rebuild

A partial rebuild repairs only sections of the graph that are degraded:

- Identify regions with high tombstone density.
- Re-insert the active memories in those regions.
- Don't touch the rest.

This is faster than a full rebuild but more complex. Brain doesn't currently implement partial rebuild; full rebuild is the only mechanism. Partial rebuild is an open question.

### 20. The maintenance worker schedule

The worker runs:
- On a regular interval (default 5 minutes) to check stats.
- After bulk operations (consolidation, migration, large FORGETs).
- On-demand via `ADMIN_REBUILD_ANN`.

The interval is conservative; most shards won't need maintenance most of the time. The worker's check itself is cheap (just reading stats); only the rebuild action is expensive.

### 21. Maintenance and snapshots

When a snapshot is taken (`ADMIN_SNAPSHOT_CREATE`), the snapshot includes the HNSW snapshot file (if persistence is enabled) reflecting the current state.

Maintenance shouldn't run concurrently with snapshot creation. The two are serialized: if a snapshot is being taken, maintenance defers; if maintenance is running, snapshot waits briefly.

### 22. Manual rebuild

`ADMIN_REBUILD_ANN` triggers an immediate full rebuild. Use cases:

- After a known event that degraded the index (mass deletion, model migration).
- Before a benchmark, to ensure fresh graph quality.
- For debugging.

The operation:
1. Rejects if a rebuild is already in progress.
2. Triggers the rebuild.
3. Returns immediately (rebuild is async).
4. Status is queryable via `ADMIN_STATS`.

### 23. Failure handling

A rebuild can fail due to:

- **OOM** during build. The build is aborted; the old HNSW remains active.
- **Crash during build.** Same as OOM — the old HNSW is what remains at startup.
- **Inconsistency detected** (e.g., a vector fails norm validation during insertion). The corrupted memory is logged; rebuild continues with valid ones; the corrupted memories are flagged for repair.

A failed rebuild doesn't degrade the running state; the old index continues to serve.

### 24. Monitoring

Metrics exposed by the maintenance worker:

- `last_check_at`, `last_check_decision` — last decision and timestamp.
- `last_rebuild_at`, `last_rebuild_duration_ms` — last successful rebuild.
- `current_recall_estimate` — most recent recall estimate.
- `pending_rebuild_eta` — if a rebuild is scheduled, when it will start.

Operators monitor these to ensure the worker is functioning.

### 25. Maintenance and write throughput

During a rebuild, write throughput may dip slightly because:

- The build phase consumes CPU.
- The catch-up phase blocks the writer briefly (to apply pending inserts).

The dip is typically 10-20% during the build phase, recovering after. For most workloads, this is acceptable. For latency-critical workloads, schedule rebuilds during low-traffic windows.

### 26. Future: continuous incremental cleanup

A more sophisticated maintenance approach: as inserts happen, periodically clean up nearby tombstoned nodes. This avoids the "stop the world" feel of full rebuilds.

The technique is well-documented but implementation-heavy. Brain sticks with the simpler full-rebuild approach.

---

*Continue to [`04_concurrency.md`](04_concurrency.md) for concurrency.*
