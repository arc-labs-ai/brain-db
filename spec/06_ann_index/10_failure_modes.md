# 06.10 ANN Index Failure Modes

What can go wrong with the HNSW index and how the substrate responds.

## 1. Insert OOM

**Failure mode.** The HNSW insert tries to allocate (for new edges, ID maps, etc.) and fails.

**Detection.** Allocation returns null / error.

**Response.** The encode operation enters a degraded state:
- The WAL record is durable.
- The metadata is updated.
- The HNSW insert failed.

The maintenance worker periodically retries failed HNSW inserts.

**Operator action.** Address memory pressure: add RAM, reduce shard count per node, restart.

## 2. Search returns empty when results expected

**Failure mode.** A search returns 0 results when the agent expects more.

**Possible causes:**
- The shard is empty (no memories yet).
- All matching memories are tombstoned.
- Filters are too restrictive.
- The HNSW is corrupted.
- The current model fingerprint has no matching memories (cross-model exclusion).

**Detection.** No automatic detection. Application observes "empty results" and investigates.

**Response.** None automatic. The substrate reports per-search statistics that help diagnose:
- `candidates_returned_by_hnsw` — how many before filtering.
- `candidates_filtered_out` — broken down by filter.
- `tombstones_filtered` — how many were excluded as tombstones.

## 3. HNSW corruption

**Failure mode.** The in-memory HNSW data structure is corrupted (a bug, hardware fault).

**Detection.** Searches return clearly-wrong results, panics during traversal, etc.

**Response.** The substrate triggers a full rebuild from arena + metadata. After rebuild, the HNSW is consistent.

**Operator action.** Investigate the cause. Check error logs.

## 4. Recall regression

**Failure mode.** Recall has dropped over time (degraded graph quality).

**Detection.** The maintenance worker's recall estimate falls below threshold.

**Response.** The maintenance worker triggers a rebuild.

**Operator action.** None unless rebuilds aren't catching up (very high churn rates). Adjust maintenance thresholds if needed.

## 5. Excessive tombstones

**Failure mode.** Tombstone ratio > 30%, search performance suffers.

**Detection.** Stats expose tombstone ratio.

**Response.** Maintenance worker rebuilds; rebuild removes tombstones.

**Operator action.** Generally none. For workloads with very high deletion rates, consider increasing the rebuild frequency or threshold.

## 6. Insert during entry-point change

**Failure mode.** A read sees a stale entry point because an insert was updating it.

**Detection.** Implementation handles this via atomic publication; readers see either the old or new entry consistently.

**Response.** None needed; this is correctness-preserving.

**Operator action.** None.

## 7. ID map inconsistency

**Failure mode.** id_map_forward and id_map_reverse disagree about a node — e.g., forward says ID X maps to internal 5, reverse says 5 maps to ID Y ≠ X.

**Detection.** Search might return a wrong MemoryId; or an insert collision.

**Response.** This is a bug. The substrate detects via consistency assertions during maintenance and triggers a rebuild.

**Operator action.** Report the bug.

## 8. Vector slot doesn't match expected memory

**Failure mode.** HNSW says memory M is at slot S; metadata says memory M is at slot S, but the slot's stored MemoryId doesn't match M (suggesting reuse without proper cleanup).

**Detection.** Maintenance worker validates this consistency periodically.

**Response.** Remove the inconsistent HNSW node; rebuild affected region.

**Operator action.** None unless this happens frequently (would suggest a bug).

## 9. Search latency spike

**Failure mode.** Search latency p99 jumps from typical ~5 ms to >50 ms.

**Possible causes:**
- HNSW degradation (high tombstones).
- Page cache misses (cold memory).
- Concurrent heavy operations on the same executor.

**Detection.** Monitor exposes per-search timing.

**Response.** None automatic. The substrate logs the spike for analysis.

**Operator action.** Investigate. Possibly trigger manual rebuild, allocate more RAM, or split the shard.

## 10. The "first search after restart" cold path

**Failure mode.** First few searches after restart are very slow (cold caches).

**Detection.** Implicit; first searches see cold-cache misses on arena pages.

**Response.** None directly. The substrate "warms" the cache by:
- Doing a few queries on common patterns at startup.
- Pre-touching the HNSW's most-traveled paths.

**Operator action.** For latency-sensitive deployments, schedule a "warmup" period after restart before redirecting traffic.

## 11. Rebuild can't keep up

**Failure mode.** The rebuild trigger fires repeatedly because the workload generates tombstones faster than rebuilds can remove them.

**Detection.** `last_rebuild_at` is recent and yet tombstone ratio is rising.

**Response.** None automatic. The substrate logs the situation.

**Operator action.** Reduce the workload's deletion rate, increase shard count (so each shard's churn is lower), or invest in faster hardware.

## 12. Misalignment between vectors and HNSW IDs

**Failure mode.** A vector is in the arena at slot S, but HNSW thinks the vector at slot S is a different memory (due to a missed reclaim event, e.g.).

**Detection.** Periodic scrubbing compares each HNSW node's memory_id against the slot's stored memory_id (via slot_version).

**Response.** Mark the HNSW node as inconsistent; remove during maintenance.

**Operator action.** None unless frequent.

## 13. Concurrent query during rebuild

**Failure mode.** A search runs during a rebuild. Which HNSW does it use?

**Detection.** Implementation handles this via arc-swap: the search uses whichever HNSW was published when it loaded its reference. Rebuilds publish atomically; searches don't see partial state.

**Response.** None needed.

**Operator action.** None.

## 14. The hnsw_rs internal bug

**Failure mode.** A bug in the underlying hnsw_rs crate causes wrong results, panics, or memory issues.

**Detection.** Hard to detect generally; specific bugs may show up as crashes or poor recall.

**Response.** Upgrade hnsw_rs; report upstream.

**Operator action.** Stay on stable hnsw_rs versions.

---

*Continue to [`11_open_questions.md`](11_open_questions.md) for unresolved questions.*
