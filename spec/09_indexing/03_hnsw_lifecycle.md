# 09.03 HNSW Lifecycle

Persistence and maintenance. The HNSW index is derived state; this file specifies how it is rebuilt on startup, optionally snapshotted for fast restart, and maintained against degradation over time.

## Persistence

The HNSW index is **not persisted** as a primary on-disk structure. It's rebuilt at startup from the arena and metadata. This section specifies the rebuild path and an optional persistence mechanism for fast restart.

### 1. Why no primary persistence

The HNSW graph is derived state — given the vectors (in the arena) and which memories are active (in the metadata), the graph can be rebuilt deterministically.

The index could be persisted for fast startup. It is not, by default, because:

- **The graph is large** — ~150 MB per million memories.
- **The graph is fragile** — a partial write or corruption corrupts the entire structure; partial recovery is hard.
- **The rebuild is fast enough** — 5–30 seconds for 1M memories with parallel insertion.
- **The arena and metadata are the source of truth** — persisting derived state risks divergence.

Restart cost is the main downside. For deployments where minute-scale restart is acceptable, no-persistence is simpler. For deployments where every second matters, the optional persistence (§ 5) helps.

### 2. The rebuild procedure

At startup, after WAL replay completes ([08.04 Recovery](../08_storage/04_recovery.md)):

```
1. Initialize an empty HNSW with configured parameters.
2. Iterate over all active memories in the metadata store.
3. For each memory:
   a. Read the vector from the arena.
   b. Insert into HNSW.
   c. Update id maps.
4. The HNSW is now consistent with the arena and metadata.
5. The shard is marked ready.
```

The iteration is parallelized:
- The metadata store is read sequentially (it's a B-tree; sequential read is fast).
- Vectors are batched and inserted in parallel batches.

For 1M memories on commodity hardware:
- Single-threaded: ~30 seconds.
- 4-thread: ~10 seconds.
- 16-thread: ~5 seconds.

Up to `nproc` parallelism is used by default.

### 3. Active memories only

Only active (non-tombstoned) memories are inserted into the rebuilt HNSW. Tombstoned memories are skipped — the rebuild is also a "compaction" in the sense that it strips out tombstones.

### 4. Memory order

The order of insertion during rebuild affects HNSW quality slightly. Metadata-store order is used, which is roughly insertion order (B-tree is keyed by MemoryId, and MemoryIds are roughly time-ordered via UUIDv7).

For pathological inputs (e.g., memories that lie exactly on a 1D manifold), insertion order matters more. For typical workloads, the order is effectively random.

### 5. Optional fast-restart persistence

For deployments wanting faster restart, a serialized HNSW can be written to disk during checkpointing. This is the **HNSW snapshot**.

#### 5.1 Procedure

During checkpointing ([08.05 Checkpointing](../08_storage/05_checkpointing.md)):

1. Pause writes briefly (the checkpoint drain).
2. Serialize the HNSW state to a file: `data/<shard>/hnsw_snapshot.bin`.
3. Include the durable LSN at which the snapshot was taken.
4. Resume writes.

The HNSW snapshot file format:

```
[header: 64 bytes]
  magic: "BHN0"  (Brain HNSW v0)
  format_version: u32
  shard_uuid: [u8; 16]
  taken_at_lsn: u64
  graph_size: u64
  parameters: { M, ef_construction }
  header_crc32c: u32
[graph data: serialized via hnsw_rs's built-in serialization]
[id_map data: serialized HashMaps]
[footer: 8 bytes — full-file BLAKE3 hash truncated to u64]
```

#### 5.2 Restore

At startup, if a snapshot exists and is valid:

1. Read the header; verify magic, version, shard_uuid, CRC.
2. Deserialize the graph and id maps.
3. Replay WAL records since `taken_at_lsn` (these add memories that came after the snapshot).
4. The HNSW is now current.

For a snapshot covering 1M memories plus 1000 post-snapshot WAL records:
- Snapshot deserialize: ~1-2 seconds.
- WAL replay for HNSW: ~1 second.
- Total: ~3 seconds.

Compared to ~5 seconds rebuild from scratch, the gain is modest at this size. For larger shards (10M+) or slower hardware, the gain is more meaningful.

#### 5.3 Failure modes

If the snapshot is corrupted (CRC fails, deserialize errors), the system falls back to full rebuild. No data loss; just slower startup.

If the snapshot is older than the metadata (some checkpointing failure), the LSN comparison detects this and the system rebuilds rather than using a stale snapshot.

### 6. The choice between persistence options

| Option | Restart time | Disk overhead | Complexity |
|---|---|---|---|
| No persistence (default) | 5-30 s for 1M | 0 | Lowest |
| Periodic snapshot | 3-5 s for 1M | ~150 MB per snapshot | Medium |

For most deployments, no persistence is fine. For large shards or restart-sensitive deployments, snapshots help.

The configuration knob:

```
[ann.persistence]
mode = "rebuild"     # or "snapshot"
snapshot_interval = "10m"
```

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

The HNSW snapshot is a fast-restart artifact, not a backup. A backup of the shard ([08.06 Snapshots](../08_storage/06_snapshots.md)) doesn't need to include the HNSW snapshot; the arena and metadata are sufficient to reconstruct everything.

If a backup includes the HNSW snapshot, restore can use it for faster shard ready time. If not, rebuild from arena + metadata.

### 10. Cross-version persistence

The HNSW snapshot's format version protects against incompatible loads. If the binary is upgraded and the snapshot's format is older, the system falls back to rebuild.

This means a Brain upgrade can mean a slower first restart (for the rebuild) but no data loss.

### 11. The "warm" rebuild

For large shards where rebuild takes a meaningful fraction of a minute, a "warm" path could expose: respond to read queries against the partially-built index (with degraded recall) while rebuild completes.

This is not currently implemented. The shard isn't marked ready until rebuild completes; queries return `ShardNotReady` until then.

A future enhancement (open question, [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md)): partial-readiness, where the shard accepts queries during rebuild with a "best effort" recall caveat.

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

1. **Build new index in the background.** Allocate a new HNSW; iterate over active memories; insert each in parallel.
2. **Wait for catch-up.** Once the new index is built, apply any inserts that happened during the build (via WAL replay from the build-start LSN).
3. **Atomic swap.** Replace the active HNSW with the new one. Old HNSW is freed after no readers reference it (via Arc/epoch).
4. **Cleanup.** Free the old graph's memory.

The rebuild is non-blocking — reads and writes continue against the old index during the build. The atomic swap is a brief moment (microseconds).

### 17. Rebuild duration

For 1M active memories with parallel insertion:

- Build phase: 5-30 seconds.
- Catch-up phase: typically < 1 second (only inserts during build need re-application).
- Swap: microseconds.

For larger shards (10M), build phase scales linearly. Very large rebuilds may be spread across multiple cycles to avoid using too much memory at once.

### 18. Memory pressure during rebuild

During rebuild, both the old and new HNSW are in memory: ~300 MB for 1M memories. For larger shards, rebuild memory peaks proportionally.

A configuration knob `ann.rebuild_max_memory_gb` bounds this. If a rebuild would exceed the limit, it's split into multiple phases (each phase rebuilds a subset of nodes; not currently implemented, tracked as future work).

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

The technique is well-documented but implementation-heavy. Brain sticks with the simpler full-rebuild approach. Continuous cleanup is tracked in [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

---

*Continue to [`04_concurrency.md`](04_concurrency.md) for concurrency.*
