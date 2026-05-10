# Phase 8 — Background Workers

## Goal

Implement the 12 background workers: decay, access boost, consolidation, HNSW maintenance, idempotency cleanup, slot reclamation, WAL retention, edge scrub, counter reconciliation, statistics, embedder cache eviction, and snapshots. Each runs cooperatively, yields generously, and is observable.

## Prerequisites

- [x] Phase 7 complete.

## Reading list

1. [`spec/11_background_workers/00_purpose.md`](../../spec/11_background_workers/00_purpose.md)
2. [`spec/11_background_workers/01_worker_architecture.md`](../../spec/11_background_workers/01_worker_architecture.md)
3. [`spec/11_background_workers/02_decay.md`](../../spec/11_background_workers/02_decay.md)
4. [`spec/11_background_workers/02_decay.md`](../../spec/11_background_workers/02_decay.md)
5. [`spec/11_background_workers/03_consolidation.md`](../../spec/11_background_workers/03_consolidation.md)
6. [`spec/11_background_workers/04_hnsw_maintenance.md`](../../spec/11_background_workers/04_hnsw_maintenance.md)
7. [`spec/11_background_workers/05_idempotency_cleanup.md`](../../spec/11_background_workers/05_idempotency_cleanup.md)
8. [`spec/11_background_workers/06_slot_reclamation.md`](../../spec/11_background_workers/06_slot_reclamation.md)
9. [`spec/11_background_workers/07_wal_retention.md`](../../spec/11_background_workers/07_wal_retention.md)
10. [`spec/11_background_workers/08_misc_workers.md`](../../spec/11_background_workers/08_misc_workers.md)
11. [`spec/11_background_workers/08_misc_workers.md`](../../spec/11_background_workers/08_misc_workers.md)
12. [`spec/11_background_workers/08_misc_workers.md`](../../spec/11_background_workers/08_misc_workers.md)
13. [`spec/11_background_workers/08_misc_workers.md`](../../spec/11_background_workers/08_misc_workers.md)

## Outputs

- `crates/brain-workers` exports a `Worker` trait and 12 implementations.
- A `WorkerScheduler` that runs them at configured intervals.
- Workers don't degrade foreground latency by more than X% (X spec'd).
- Tag: `phase-8-complete`.

## Sub-tasks

### Task 8.1 — `Worker` trait & scheduler
**Reads:** `spec/11_background_workers/01_worker_architecture.md`
**Writes:** `crates/brain-workers/src/worker.rs`, `scheduler.rs`
**What to build:**
- `trait Worker { fn name() -> &'static str; async fn run_cycle(&mut self, ctx: &Ctx) -> Result<()>; fn interval() -> Duration; }`
- Scheduler runs each worker on its interval; each cycle yields cooperatively.

### Task 8.2 — Decay worker
**Reads:** `spec/11_background_workers/02_decay.md`
**Writes:** `crates/brain-workers/src/decay.rs`
**Done when:** Salience decays per the half-life rules per memory kind. Test with mocked time.

### Task 8.3 — Access boost worker
**Reads:** `spec/11_background_workers/02_decay.md`
**Writes:** `crates/brain-workers/src/access_boost.rs`
**Done when:** Recently-accessed memories get a transient salience bump.

### Task 8.4 — Consolidation worker
**Reads:** `spec/11_background_workers/03_consolidation.md`
**Writes:** `crates/brain-workers/src/consolidation.rs`
**Done when:** Episodic memories meeting consolidation criteria become Consolidated; original episodics retained per spec.

### Task 8.5 — HNSW maintenance worker
**Reads:** `spec/11_background_workers/04_hnsw_maintenance.md`, `spec/06_ann_index/07_maintenance.md`
**Writes:** `crates/brain-workers/src/hnsw_maint.rs`
**Done when:** Triggers rebuild when tombstone ratio > 30% or recall estimate < 0.85; rebuild produces a fresh ArcSwap-published index.

### Task 8.6 — Idempotency cleanup worker
**Reads:** `spec/11_background_workers/05_idempotency_cleanup.md`
**Writes:** `crates/brain-workers/src/idempotency_cleanup.rs`
**Done when:** Sweeps idempotency table; entries older than 24h are removed.

### Task 8.7 — Slot reclamation worker
**Reads:** `spec/11_background_workers/06_slot_reclamation.md`
**Writes:** `crates/brain-workers/src/slot_reclaim.rs`
**Done when:** Tombstones past their grace period have their slots reclaimed (free list updated, version bumped).

### Task 8.8 — WAL retention worker
**Reads:** `spec/11_background_workers/07_wal_retention.md`
**Writes:** `crates/brain-workers/src/wal_retention.rs`
**Done when:** Old segments are deleted only after their records are checkpointed. Invariant: no gap in retained LSN ranges.

### Task 8.9 — Edge scrub worker
**Reads:** `spec/11_background_workers/08_misc_workers.md`
**Writes:** `crates/brain-workers/src/edge_scrub.rs`
**Done when:** Edges referencing reclaimed slots (whose version no longer matches) are removed.

### Task 8.10 — Counter reconciliation worker
**Reads:** `spec/11_background_workers/08_misc_workers.md`
**Writes:** `crates/brain-workers/src/counter_reconcile.rs`
**Done when:** Per-shard counters are recomputed from full scans periodically; drift between counter and ground truth detected and fixed.

### Task 8.11 — Statistics update worker
**Reads:** `spec/11_background_workers/08_misc_workers.md`
**Writes:** `crates/brain-workers/src/stats.rs`
**Done when:** Histograms of salience, edge degree, age etc. are updated; planner can query them.

### Task 8.12 — Embedder cache eviction worker
**Reads:** `spec/11_background_workers/08_misc_workers.md`
**Writes:** `crates/brain-workers/src/cache_evict.rs`
**Done when:** Stale entries evicted; cache size bound respected.

### Task 8.13 — Snapshot worker
**Reads:** `spec/05_storage_arena_wal/10_snapshots.md` (if present, else `spec/15_failure_recovery/06_disaster_recovery.md`)
**Writes:** `crates/brain-workers/src/snapshot.rs`
**Done when:** Periodic snapshot trigger writes a checkpoint and copies storage files to a snapshot directory.

### Task 8.14 — Performance regression test
**Reads:** `spec/16_benchmarks_acceptance/02_latency_targets.md`
**Writes:** `crates/brain-workers/tests/no_regression.rs`
**What to build:**
- Drive foreground load while workers run; measure latency.
- Compare to baseline-without-workers; assert overhead < threshold (spec'd).

## Phase exit checklist

- [ ] All 13 sub-tasks complete (12 workers + scheduler).
- [ ] `just verify` green.
- [ ] Each worker has a unit test.
- [ ] Performance regression test green.
- [ ] Tag `phase-8-complete`.
