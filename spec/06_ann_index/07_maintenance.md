# 06.07 Maintenance

The HNSW index degrades over time as memories are added and removed. The maintenance worker monitors quality and rebuilds when needed.

## 1. Why maintenance

Two kinds of degradation:

### 1.1 Tombstone accumulation

Each FORGET adds a tombstone. Tombstones consume graph edges without contributing to results. Above ~30%, search recall and latency suffer noticeably.

### 1.2 Topological drift

The HNSW graph's quality depends on the order of insertions. Over time, with many inserts and pseudo-deletes (tombstones), the graph's edge structure becomes suboptimal — the M edges per node may not be the best M edges for current navigability.

Rebuilding from scratch produces a graph that's optimal for the current memory set.

## 2. The maintenance worker

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

## 3. Decision criteria

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

## 4. The recall estimate

The maintenance worker estimates current recall by:

1. Selecting a random sample of recent queries (logged with their results).
2. Re-running them with a much higher ef_search (say, 500) to get a "near-truth" result set.
3. Comparing the original result set's overlap with the higher-ef set.

The overlap fraction approximates recall@K. If it falls below the configured threshold, rebuild is triggered.

This is an approximation — not exact ground-truth — but adequate for detecting drift. The substrate doesn't compute exact ground truth (would require brute-force search over all vectors, expensive).

## 5. Full rebuild

Procedure:

1. **Build new index in the background.** Allocate a new HNSW; iterate over active memories; insert each in parallel.
2. **Wait for catch-up.** Once the new index is built, apply any inserts that happened during the build (via WAL replay from the build-start LSN).
3. **Atomic swap.** Replace the active HNSW with the new one. Old HNSW is freed after no readers reference it (via Arc/epoch).
4. **Cleanup.** Free the old graph's memory.

The rebuild is non-blocking — reads and writes continue against the old index during the build. The atomic swap is a brief moment (microseconds).

## 6. Rebuild duration

For 1M active memories with parallel insertion:

- Build phase: 5-30 seconds.
- Catch-up phase: typically < 1 second (only inserts during build need re-application).
- Swap: microseconds.

For larger shards (10M), build phase scales linearly. The substrate may spread very large rebuilds across multiple cycles to avoid using too much memory at once.

## 7. Memory pressure during rebuild

During rebuild, both the old and new HNSW are in memory: ~300 MB for 1M memories. For larger shards, rebuild memory peaks proportionally.

The substrate has a configuration knob `ann.rebuild_max_memory_gb` to bound this. If a rebuild would exceed the limit, it's split into multiple phases (each phase rebuilds a subset of nodes; not implemented in v1, tracked as future work).

## 8. Partial rebuild

A partial rebuild repairs only sections of the graph that are degraded:

- Identify regions with high tombstone density.
- Re-insert the active memories in those regions.
- Don't touch the rest.

This is faster than a full rebuild but more complex. v1 doesn't implement partial rebuild; full rebuild is the only mechanism. Partial rebuild is an open question.

## 9. The maintenance worker schedule

The worker runs:
- On a regular interval (default 5 minutes) to check stats.
- After bulk operations (consolidation, migration, large FORGETs).
- On-demand via `ADMIN_REBUILD_ANN`.

The interval is conservative; most shards won't need maintenance most of the time. The worker's check itself is cheap (just reading stats); only the rebuild action is expensive.

## 10. Maintenance and snapshots

When a snapshot is taken (`ADMIN_SNAPSHOT_CREATE`), the snapshot includes the HNSW snapshot file (if persistence is enabled) reflecting the current state.

Maintenance shouldn't run concurrently with snapshot creation. The substrate serializes them: if a snapshot is being taken, maintenance defers; if maintenance is running, snapshot waits briefly.

## 11. Manual rebuild

`ADMIN_REBUILD_ANN` triggers an immediate full rebuild. Use cases:

- After a known event that degraded the index (mass deletion, model migration).
- Before a benchmark, to ensure fresh graph quality.
- For debugging.

The operation:
1. Rejects if a rebuild is already in progress.
2. Triggers the rebuild.
3. Returns immediately (rebuild is async).
4. Status is queryable via `ADMIN_STATS`.

## 12. Failure handling

A rebuild can fail due to:

- **OOM** during build. The build is aborted; the old HNSW remains active.
- **Crash during build.** Same as OOM — the old HNSW is what we have at startup.
- **Inconsistency detected** (e.g., a vector fails norm validation during insertion). The substrate logs the corrupted memory; rebuild continues with valid ones; the corrupted memories are flagged for repair.

A failed rebuild doesn't degrade the running state; the old index continues to serve.

## 13. Monitoring

Metrics exposed by the maintenance worker:

- `last_check_at`, `last_check_decision` — last decision and timestamp.
- `last_rebuild_at`, `last_rebuild_duration_ms` — last successful rebuild.
- `current_recall_estimate` — most recent recall estimate.
- `pending_rebuild_eta` — if a rebuild is scheduled, when it will start.

Operators monitor these to ensure the worker is functioning.

## 14. Maintenance and write throughput

During a rebuild, write throughput may dip slightly because:

- The build phase consumes CPU.
- The catch-up phase blocks the writer briefly (to apply pending inserts).

The dip is typically 10-20% during the build phase, recovering after. For most workloads, this is acceptable. For latency-critical workloads, schedule rebuilds during low-traffic windows.

## 15. Future: continuous incremental cleanup

A more sophisticated maintenance approach: as inserts happen, periodically clean up nearby tombstoned nodes. This avoids the "stop the world" feel of full rebuilds.

The technique is well-documented but implementation-heavy. v1 sticks with the simpler full-rebuild approach. Continuous cleanup is tracked in [`11_open_questions.md`](11_open_questions.md).

---

*Continue to [`08_concurrency.md`](08_concurrency.md) for concurrency.*
