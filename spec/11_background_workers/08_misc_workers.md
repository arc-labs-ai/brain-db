# 11.08 Miscellaneous Workers

The remaining workers, smaller in scope but still important for substrate health.

---

## 1. Edge Scrub Worker

Removes orphaned edges — edges whose source or target memory no longer exists.

### 1.1 The problem

When a memory is reclaimed (after FORGET + grace), its edges in `edges_out` and `edges_in` are deleted. But edges from other memories that pointed to it (in `edges_in`) and edges from it to others (in `edges_out`) — wait, let me re-read.

Actually, edges are stored bidirectionally. When memory M is reclaimed:
- Entries in `edges_out[M, *, *]` are deleted (M's outgoing edges).
- Entries in `edges_in[M, *, *]` are deleted (M's incoming edges).

But the *paired* entries (where M is mentioned but isn't the indexing key) need separate cleanup:
- `edges_in[X, kind, M]` — X had an incoming edge from M; not deleted by M's reclamation.
- `edges_out[X, kind, M]` — X had an outgoing edge to M; not deleted by M's reclamation.

These dangling references are what the edge scrub worker handles.

### 1.2 The cycle

Every 30 minutes:

1. Iterate `edges_out` (and `edges_in`) in batches.
2. For each edge, check both endpoints exist (in the `memories` table).
3. If an endpoint doesn't exist, the edge is orphaned — delete it.

### 1.3 Cost

For 8M edges total: each cycle iterates a batch (~10K edges), checks 20K memory IDs (one per endpoint). ~100 ms of work per cycle.

A full pass through all edges takes ~3 hours (if running every 30 min with 10K-batch).

### 1.4 Optimization

The reclamation worker could pre-compute which edges to scrub when it reclaims a memory. This would make scrub more efficient. Not implemented in v1; the periodic full-scan is simpler.

---

## 2. Counter Reconciliation Worker

Verifies and corrects denormalized counters.

### 2.1 The problem

The substrate has denormalized counts in some places:

- `MemoryMetadata.edges_out_count`, `edges_in_count` — per-memory edge counts.
- `ContextMetadata.memory_count` — per-context memory count.
- Per-shard total counts (in cluster metadata).

These are updated by mutations. Bugs or partial recoveries might cause drift.

### 2.2 The cycle

Daily (configurable):

1. For a sample of memories, recompute edges_out_count and edges_in_count from the edge tables.
2. Compare to stored count; correct if different.
3. Same for context counts.
4. Same for shard-wide totals.

The worker doesn't iterate all memories — that would be expensive. Instead it samples (e.g., 1000 memories per day) and corrects discrepancies.

### 2.3 The drift detection

If discrepancies are common (more than ~0.1% of samples), the worker logs an alert. This indicates a likely bug worth investigating.

In normal operation, drifts should be very rare (zero or near-zero).

### 2.4 The cost

Per sampled memory: ~100 µs for the recount. 1000 memories per day: 100 ms total. Negligible.

---

## 3. Statistics Update Worker

Refreshes the per-shard statistics that operators monitor.

### 3.1 The stats

Per shard:

- `memory_count` — total active memories.
- `tombstone_count` — tombstoned memories.
- `tombstone_ratio` — tombstones / (active + tombstoned).
- `arena_used_bytes` — arena slots in use.
- `arena_capacity_bytes` — arena slots total.
- `wal_size_bytes` — current WAL on-disk size.
- `metadata_size_bytes` — metadata redb file size.
- `oldest_memory_age` — age of the oldest memory.
- `newest_memory_age` — age of the newest.

These are exposed via metrics and `ADMIN_STATS`.

### 3.2 The cycle

Every 5 minutes:

1. Query each table for counts.
2. Read filesystem for file sizes.
3. Update the in-memory stats cache.

The cache is what `ADMIN_STATS` reads. Without the cache, every stats request would do these queries — expensive.

### 3.3 The cost

Per cycle: ~50 ms of metadata queries plus ~5 ms of filesystem stat calls.

For latency-critical stats, the cache may be stale by up to 5 minutes. For monitoring purposes, that's fine.

---

## 4. Embedder Cache Eviction

Manages the embedder's cue cache (recall queries with cached embeddings).

### 4.1 The cache

The cue cache (described in [04.05 Caching](../04_embedding_layer/05_caching.md)) maps `(text, model_fp) → vector`. It's an LRU cache, default size 10K entries.

### 4.2 The eviction worker

LRU is automatic on each access. But:

- Stale entries (very old) might consume memory without being useful.
- The worker periodically prunes entries older than 7 days.

### 4.3 The cycle

Hourly:

1. Iterate the cache.
2. Remove entries with `last_used > 7 days ago`.

Cost: trivial.

---

## 5. Cluster Health Worker (Future)

For distributed deployments, a worker that monitors cluster health.

Not implemented in v1.

### 5.1 What it would do

- Check shard liveness (heartbeat).
- Detect failed nodes.
- Trigger rebalancing if needed.
- Report cluster-wide stats.

In v1, deployments are single-node; cluster health is N/A. Tracked for v2.

---

## 6. Snapshot Worker (Optional)

If automated periodic snapshots are configured, a worker that triggers them.

### 6.1 The cycle

Every N hours (configurable; e.g., every 6 hours):

1. Trigger a checkpoint.
2. Trigger a snapshot creation (writes the current state to disk).
3. Apply retention policy (delete old snapshots).

### 6.2 Configuration

```toml
[workers.snapshot]
enabled = false                # Off by default
interval = "6h"
retention_count = 7            # Keep last 7 snapshots
retention_max_age = "30d"
```

Snapshots are heavyweight; many deployments prefer external backup tooling. The substrate's built-in snapshot worker is a convenience.

---

## 7. The "all workers" list

A complete list of workers in v1:

1. Decay (`workers.decay`).
2. Access boost (`workers.access_boost`).
3. Consolidation (`workers.consolidation`).
4. HNSW maintenance (`workers.hnsw_maintenance`).
5. Idempotency cleanup (`workers.idempotency_cleanup`).
6. Slot reclamation (`workers.slot_reclamation`).
7. WAL retention (`workers.wal_retention`).
8. Edge scrub (`workers.edge_scrub`).
9. Counter reconciliation (`workers.counter_reconciliation`).
10. Statistics update (`workers.statistics_update`).
11. Embedder cache eviction (`workers.embedder_cache_eviction`).
12. (Optional) Snapshot (`workers.snapshot`).

All run per-shard except for global-cluster-related ones (none in v1).

---

## 8. The "should you add a worker?" question

When considering a new worker, ask:

- Is the work needed for substrate health (correctness, performance)?
- Can it be done in the request path? (Probably not — that's why it's a worker.)
- Can it be done by an operator action instead? (If yes, prefer that.)
- What's the maintenance cost (testing, debugging)?

Workers add complexity. Each one is a long-running task that can fail. Add them sparingly.

---

*Continue to [`09_failure_modes.md`](09_failure_modes.md) for failure modes.*
