# Sub-task 9.7b — Per-shard OpsContext + workers wired into brain-server

**Reads:** plan `phase-09-task-07.md`, audit §5.3 + §6 + §8.2, the new shapes from 9.7a (`brain-planner::WriterHandle: !Send`, `brain-workers::WorkerScheduler` Glommio-driven, `WorkerContext.shutdown: Arc<AtomicBool>`).
**Phase doc:** `docs/phases/phase-09-server.md` §9.7 (continuation of 9.7a).
**Done when:** `brain-server::shard::spawn_shard` constructs the full per-shard stack inside its Glommio closure — real `MetadataDb`, `HnswIndex`, `Dispatcher`, `RealWriterHandle`, `ExecutorContext`, `OpsContext`, `WorkerScheduler` with all Phase-8 workers registered. Recovery applies to the redb sink (not `InMemoryMetadataSink`). The shard is end-to-end testable from `AppendWalRecord` through the writer to durable storage.

---

## 1. Where 9.7a left off

9.7a finished the brain-planner / brain-ops / brain-workers cascade:
- `WriterHandle: !Send + !Sync`.
- `WorkerScheduler` runs on Glommio via `glommio::spawn_local`.
- `WorkerContext.shutdown` is `Arc<AtomicBool>` (runtime-agnostic).
- 992 tests across 63 suites green in container.

brain-server's `shard::spawn_shard` (sub-task 9.6) still:
- Opens arena + reads/generates shard.uuid (sync, on caller thread).
- Runs recovery with a **stand-in** `InMemoryMetadataSink` — the durable sink is the missing piece.
- Spawns a Glommio executor whose main loop handles `Ping`, `AllocSlot`, `AppendWalRecord` — no real ops dispatched yet.

9.7b plugs the rest of the stack in.

---

## 2. Scope

### 2.1 Cargo deps (brain-server)

Add to `[target.'cfg(target_os = "linux")'.dependencies]`:

```toml
brain-metadata = { path = "../brain-metadata" }
brain-index = { path = "../brain-index" }
brain-embed = { path = "../brain-embed" }
brain-planner = { path = "../brain-planner" }
brain-ops = { path = "../brain-ops" }
brain-workers = { path = "../brain-workers" }
parking_lot.workspace = true
```

The macOS host build stays gated out via the existing `#[cfg(target_os = "linux")] mod shard;` in main.rs.

### 2.2 Shard struct (new fields)

```rust
struct Shard {
    shard_id: ShardId,
    arena: ArenaFile,
    allocator: SlotAllocator,
    wal: Option<Wal>,
    // NEW in 9.7b:
    metadata: Arc<Mutex<MetadataDb>>,                  // SharedMetadataDb alias
    hnsw: SharedHnsw<384>,
    writer: Arc<dyn WriterHandle>,                     // RealWriterHandle inside
    ops: Arc<OpsContext>,
    scheduler: Option<WorkerScheduler>,                // Option so shutdown can `.take()`
}
```

### 2.3 Spawn flow

```
caller thread (sync):
  1. dir layout + shard.uuid + arena open  (9.5, unchanged)
  2. Open MetadataDb (sync via redb)
  3. Run recover() with &mut MetadataDb (real sink — replaces InMemoryMetadataSink)
  4. Bytes-on-disk + next_lsn from recover()

Glommio executor (inside spawn closure):
  5. SharedHnsw::new(IndexParams::default_v1())
     – tombstones loaded later (9.7b uses the Phase-8 worker for that)
  6. NopDispatcher (config-driven choice deferred to 9.15's adapter)
  7. RealWriterHandle::new(metadata, hnsw_writer)
  8. ExecutorContext::new(dispatcher, hnsw_shared, metadata, writer)
  9. OpsContext::new(executor)
  10. Wal::open_existing or Wal::create_with_config (9.6 path)
  11. WorkerScheduler::new
  12. Register every Phase-8 worker (see §2.4)
  13. shard_main_loop(...)
```

### 2.4 Worker registration

All 12 Phase-8 workers, each with a sensible default constructor. Phase-8 seams (Summarizer, RebuildSource, CacheEvictionSource, WalRetentionSource, SnapshotSource) use the `Disabled*` defaults — real adapters land in 9.8 / 9.15.

| Worker | Constructor | Notes |
| ------ | ----------- | ----- |
| AccessBoostWorker | `::new()` | Drains `OpsContext.access_buffer` |
| DecayWorker | `::new()` | |
| ConsolidationWorker | `::new(Arc::new(DisabledSummarizer))` | 9.15 swaps |
| HnswMaintenanceWorker | `::new(Arc::new(DisabledRebuildSource))` | 9.8 may swap |
| IdempotencyCleanupWorker | `::new()` | |
| EdgeScrubWorker | `::new()` | |
| SlotReclaimWorker | `::new()` | |
| StatisticsUpdateWorker | `::new()` | |
| CounterReconcileWorker | `::new()` | |
| CacheEvictionWorker | `::new(Arc::new(DisabledCacheEvictionSource))` | 9.8 |
| WalRetentionWorker | `::new(Arc::new(DisabledWalRetentionSource))` | 9.8 |
| SnapshotWorker | `::new(Arc::new(DisabledSnapshotSource))` | 9.8 |

The constructors and Disabled defaults already exist from Phase 8.

### 2.5 ShardSpawnConfig delta

Add the configs the workers and dispatcher need:

```rust
pub struct ShardSpawnConfig {
    // existing 9.4 / 9.5 / 9.6:
    pub channel_capacity: usize,
    pub pin_cpu: Option<usize>,
    pub data_dir: PathBuf,
    pub arena_initial_capacity_slots: u64,
    pub wal_config: WalConfig,
    // NEW in 9.7b:
    pub hnsw_params: brain_index::IndexParams,
    pub worker_intervals: WorkerIntervals,            // per-worker tick cadences
}
```

`WorkerIntervals` is a thin record matching `config/dev.toml`'s `[workers]` block (already typed in brain-server::config from 9.1).

### 2.6 Shutdown path

`shard_main_loop`'s cleanup currently does WAL shutdown + arena msync. Add:

1. `scheduler.take().shutdown().await` — drains the 12 worker tasks within 5 s.
2. Then `wal.shutdown().await` (existing).
3. Then `arena.msync_all()` (existing).

### 2.7 Tests

Two new integration tests in `crates/brain-server/tests/shard.rs`:

1. `shard_constructs_full_ops_stack` — spawn, ping, alloc_slot, drop, joiner.join. Asserts no panic; smoke-checks the wire-up.
2. `workers_run_at_least_once` — spawn with `WorkerIntervals` set to 10 ms; ping (keeps the shard alive); wait ~50 ms; assert `scheduler.metrics("decay").cycles_total >= 1`. (Need a way to peek at metrics — either expose via `ShardHandle::worker_metrics(name)` or skip this test and rely on 9.10's frame dispatcher tests later.)

For 9.7b: skip the metrics-peek test if it requires invasive plumbing; add a simple `shard_with_workers_does_not_crash` instead.

---

## 3. Cargo workspace edits

`Cargo.lock` regenerates. No other workspace `Cargo.toml` changes.

---

## 4. File-by-file

| File | Action | LOC |
| ---- | ------ | --- |
| `crates/brain-server/Cargo.toml` | Edit | +7 deps |
| `crates/brain-server/src/shard.rs` | Edit | +~400 (Shard struct + spawn flow + worker registration + shutdown wiring) |
| `crates/brain-server/tests/shard.rs` | Edit | +~100 (2 new tests) |

Total: ~500 LOC. Single commit. Subject: `feat(brain-server): per-shard OpsContext + workers (sub-task 9.7b)`.

---

## 5. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `SharedHnsw::new` returns `(Self, Writer)`; the Writer is owned by `RealWriterHandle` and we also need to give the worker a `SharedHnsw` for reads | Pass the `SharedHnsw` half through `ExecutorContext`; the Writer half stays inside `RealWriterHandle`. Both are constructed from the same call. |
| HnswIndex tombstones aren't populated on startup — recovery applies FORGETs to metadata but the in-memory bitmap is empty | The `HnswMaintenanceWorker` is supposed to scan + rebuild this; for 9.7b we accept a brief recall-correctness window after restart until that worker runs once. Spec-acceptable for v1; 9.12 / 9.13 polish. |
| `ConsolidationWorker::new` takes a Summarizer; the `DisabledSummarizer` from Phase 8 may not exist by that exact name — confirm at impl time | If the naming differs, use whatever Disabled-variant Phase 8 actually shipped; reference `crates/brain-workers/src/summarizer.rs`. |
| 12-worker registration + the executor's startup cost may make `spawn_shard` slow | Acceptable for v1 — startup is one-time. Phase 11 (observability) measures. |
| `Arc<MetadataDb>` is wrapped in `Mutex` (existing `SharedMetadataDb` convention); recovery takes `&mut MetadataDb` — need a temporary unwrap before the executor sees it | Recovery runs on the caller thread BEFORE we put the MetadataDb in an Arc. After recovery completes, wrap in `Arc<Mutex<...>>` for the executor. |
| `NopDispatcher` returns zero vectors — encode/recall correctness tests would fail, but 9.7b doesn't test them | Document. 9.10+ wire real ops via the frame dispatcher; that's when real dispatcher matters. |
| brain-server's cargo dep tree grows substantially (candle, hnsw_rs, redb) — first container build will be slow | Already paid in 9.6a's WAL port. Incremental build state in the named volume reuses. |

---

## 6. Done criteria

- [ ] brain-server's Linux cfg-gated deps include all six brain-* crates.
- [ ] `Shard` struct holds the full stack: arena + wal + metadata + hnsw + writer + ops + scheduler.
- [ ] Spawn flow constructs everything in the documented order.
- [ ] All 12 Phase-8 workers register against the per-shard scheduler.
- [ ] `recover()` runs against `MetadataDb` (not `InMemoryMetadataSink`).
- [ ] Shutdown drains workers → WAL → arena msync.
- [ ] 2 new integration tests pass in container.
- [ ] `just docker-verify` green workspace-wide.
- [ ] Audit doc §12 status rows for §5.3 + §6.x flipped to **done**.
- [ ] Phase doc 9.7 marked `[x]`.

---

## 7. What 9.7b explicitly defers

- **Real Summarizer adapter (OpenAI / Ollama).** Stays `DisabledSummarizer`. 9.15.
- **RebuildSource / WalRetentionSource / CacheEvictionSource / SnapshotSource real impls.** Stay `Disabled*`. 9.8.
- **Cognitive ops dispatched against OpsContext from the wire frame.** 9.10's frame dispatcher.
- **Cross-shard SUBSCRIBE fan-out.** 9.11.
- **HNSW rebuild from arena on startup.** Empty index until the maintenance worker fires; brief recall-miss window. Accepted v1 trade.
- **Real `CpuDispatcher` with model load.** `NopDispatcher` (zero vectors). 9.10+.

These will not unblock by working harder on 9.7b. Don't scope-creep.

---

*Implement on approval.*
