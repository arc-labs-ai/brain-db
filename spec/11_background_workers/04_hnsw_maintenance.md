# 11.04 HNSW Maintenance Worker

The HNSW maintenance worker monitors index quality and rebuilds when needed. The mechanism is described in [06.07 Maintenance](../06_ann_index/07_maintenance.md); this file describes the worker side.

## 1. The cycle

Every 5 minutes, the worker:

1. Reads per-shard index statistics.
2. Estimates current recall via sampled queries.
3. Decides on action:
   - None: no work needed.
   - Schedule rebuild: if degradation is mild.
   - Immediate rebuild: if severe.
4. If rebuilding, runs the rebuild process.

## 2. Decision criteria (recap)

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

The thresholds are configurable. Defaults are conservative — most shards never trigger a rebuild.

## 3. The recall estimation

For sampled recent queries:

```rust
async fn estimate_recall(state: &ShardState) -> f32 {
    let samples = state.recent_query_samples.read(50);
    let mut overlap_sum = 0.0;
    
    for sample in samples {
        let baseline_results = sample.original_results;        // Top K with normal ef
        let truth_results = state.hnsw.search(
            &sample.query,
            sample.k,
            ef = 500,                                          // Much larger ef
        ).await?;
        
        let overlap = compute_overlap(&baseline_results, &truth_results);
        overlap_sum += overlap;
    }
    
    overlap_sum / samples.len() as f32
}
```

The estimate is based on a small sample (50 queries). It's noisy but gives a useful signal.

## 4. The rebuild process

When a rebuild is decided:

```rust
async fn rebuild(state: &ShardState) -> Result<()> {
    let snapshot_lsn = state.current_lsn();
    let new_index = HnswIndex::new(M=16, ef_construction=200);
    
    // Iterate active memories from metadata
    let rtxn = state.metadata.begin_read()?;
    let memories = rtxn.open_table(MEMORIES)?;
    let mut count = 0;
    
    for entry in memories.iter()? {
        let (id, m) = entry?;
        if !m.is_active() { continue; }
        
        let vector = state.arena.read_vector(m.slot_id);
        new_index.insert(id, vector);
        count += 1;
        
        if count % 500 == 0 {
            glommio::yield_now().await;
        }
    }
    
    // Catch up to current LSN
    let pending_inserts = state.wal.records_since(snapshot_lsn).await?;
    for record in pending_inserts {
        if let WalRecord::Encode(e) = record {
            new_index.insert(e.memory_id, e.vector);
        }
    }
    
    // Atomic swap
    state.hnsw.swap(Arc::new(new_index));
    
    Ok(())
}
```

The rebuild reads from the metadata, builds a fresh HNSW, then atomically swaps it in.

## 5. The atomic swap

The swap is a single ArcSwap operation. After the swap:
- New queries see the rebuilt index.
- In-flight queries (using the old index) continue and complete.
- The old index is freed when no readers reference it.

## 6. Memory during rebuild

During rebuild, two HNSW indexes are in memory:
- The active one (still serving queries).
- The new one being built.

For a 1M-memory index: ~300 MB peak usage during rebuild. For 10M: ~3 GB.

The substrate has a configuration `ann.rebuild_max_memory_gb` to bound this. If a rebuild would exceed the limit, it's aborted (with a warning).

## 7. The catch-up phase

Between starting the rebuild and finishing, new encodes happen. These are missed by the rebuild's read-from-metadata snapshot.

The catch-up phase replays WAL records from `snapshot_lsn` to current. It applies any encodes that happened during the build.

The catch-up is fast (typically <1 second) because the rebuild itself is the long part (10s of seconds at scale).

## 8. The "swap" timing

The swap is a single atomic operation:

```rust
state.hnsw.store(Arc::new(new_index));
```

After this, all new queries use the new index. The pre-swap state is captured by readers' Arc references; their queries complete on the old index, then drop it.

The swap moment is microseconds. Tail latency around the swap may have a brief spike (Arc deallocation), but typically negligible.

## 9. Rebuild duration

For typical workloads:

| Memory count | Rebuild duration |
|---|---|
| 100K | ~1 sec |
| 1M | ~10 sec |
| 10M | ~2 min |
| 100M | ~30 min |

The rebuild is parallel within the build phase (using multiple Glommio tasks for inserts).

## 10. The "rebuild while running" guarantee

During rebuild:
- Reads continue at full performance against the old index.
- Writes continue against the old index (they're applied to it, then queued for the catch-up of the new one).
- The new index is built in the background.

The substrate's request latencies aren't affected (other than the brief spike at swap time and the memory pressure during rebuild).

## 11. The cost

A rebuild costs:
- CPU: ~10-30 sec at full speed for 1M memories.
- Memory: ~150 MB additional (the new index).
- Disk I/O: ~2 GB read (vectors from arena).

This is significant but bounded. A shard rebuilds rarely (typically every few weeks under normal workloads).

## 12. Monitoring rebuild progress

Per-rebuild metrics:

- `hnsw_rebuild_in_progress`: 0 or 1.
- `hnsw_rebuild_progress_pct`: 0–100.
- `hnsw_rebuild_estimated_remaining_sec`: estimate.
- `hnsw_rebuild_total_count`: counter, increments on each rebuild.
- `hnsw_rebuild_last_duration_sec`: how long the last one took.

Operators monitor these to track maintenance health.

## 13. The "manual rebuild" override

`ADMIN_REBUILD_ANN <shard_id>` triggers an immediate rebuild, bypassing the threshold-based scheduling. Use cases:

- After a known degradation event (mass deletion).
- Before a benchmark.
- For debugging.

The operation is async; returns immediately, rebuild runs in the background.

## 14. The "no rebuild" option

If rebuild is disabled (workers.hnsw_maintenance.enabled = false):
- The HNSW degrades over time (more tombstones, drift).
- Recall slowly drops.
- Eventually, manual intervention is needed.

For deployments that operate in narrow windows where rebuild can't run, this option exists. Most deployments leave it enabled.

## 15. The "rebuild backlog"

If the workload generates tombstones faster than rebuilds can remove them, the substrate logs warnings. The cycle of "rebuild → fill with tombstones → trigger another rebuild" can dominate background work.

Operators address this by:
- Reducing tombstone generation rate (less FORGET).
- Increasing parallelism on rebuilds.
- Splitting the shard.

## 16. The interaction with snapshots

When a snapshot is taken (`ADMIN_SNAPSHOT_CREATE`), the snapshot includes the current HNSW state (if persistence is enabled).

The maintenance worker shouldn't run concurrent with snapshot creation. The substrate serializes them: if a snapshot is in progress, the worker waits; if the worker is running, the snapshot waits briefly.

## 17. The post-rebuild verification

After a rebuild, the worker verifies:
- The new index has the same node count as the metadata's active memory count.
- Sampled queries against the new index return reasonable results.

If verification fails, the substrate logs an alert and (in some configurations) reverts to the old index. Such failures are bugs; reverts are a safety net.

---

*Continue to [`05_idempotency_cleanup.md`](05_idempotency_cleanup.md) for idempotency cleanup.*
