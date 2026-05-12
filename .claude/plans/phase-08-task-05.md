# Sub-task 8.5 — HNSW maintenance worker

**Spec:** `spec/11_background_workers/04_hnsw_maintenance.md`, `spec/06_ann_index/07_maintenance.md`
**Phase doc:** `docs/phases/phase-08-workers.md` §8.5
**Done when:** Triggers rebuild when tombstone ratio > 30% or recall estimate < 0.85; rebuild produces a fresh ArcSwap-published index.

---

## 1. Honest scope

Same architectural gap as 8.4: **no vector lookup by memory_id** in v1. The HNSW backend doesn't expose `vector_for(id)`; the arena lands in Phase 9. So a real rebuild can't run until then.

What we can build:
1. **Stats + decision logic.** `IndexStats { total_active, tombstone_count, tombstone_ratio, recall_estimate }` + a pure `decide_action(stats, thresholds) -> Action` matching spec §3.
2. **Pluggable `RebuildSource` trait** as the seam where Phase 9 plugs in arena-backed vector iteration. Default = `DisabledRebuildSource` returning `Disabled`.
3. **`SharedHnsw::swap()`** method (small brain-index addition) — replaces the inner index under the write lock. Spec §5 says ArcSwap; v1's `Arc<RwLock>` swap is equivalent in semantics (microsecond-scale writer-wait at swap time).
4. **`HnswMaintenanceWorker`** that collects stats every 5 min, evaluates the decision, calls the rebuild source if `FullRebuild`, swaps the new index in.
5. **Recall estimate**: v1 has no query-sample log, so the worker sets `recall_estimate = 1.0` (never triggers). The `decide_action` function still tests recall thresholds via the pure-function tests.

Out of scope:
- Query-sample logging + recall estimation → Phase 9.
- Catch-up phase (WAL replay) → Phase 9 (no WAL yet).
- `ADMIN_REBUILD_ANN` manual trigger → Phase 9 admin handler.
- Partial rebuild (spec §06/07 §8) → "open question" per spec; v1 never does it.
- `ann.rebuild_max_memory_gb` config → Phase 9 server config.
- Pre/post-swap verification (spec §17) → Phase 9.

---

## 2. brain-index change: `SharedHnsw::swap`

```rust
// crates/brain-index/src/shared.rs

impl<const D: usize> Writer<D> {
    /// Atomically replace the inner HNSW index. Existing readers
    /// complete their queries against whichever index their `read()`
    /// guard captured; new reads see the replacement.
    ///
    /// Spec §11/04 §5: "swap is a single ArcSwap operation." Our
    /// `Arc<RwLock<HnswIndex>>` realises the same semantics — the
    /// write lock briefly blocks readers, but only for the pointer
    /// swap itself.
    pub fn swap(&mut self, new_index: HnswIndex<D>) {
        let mut guard = self.inner.write();
        *guard = new_index;
    }
}
```

That's the entire brain-index touchpoint. One method on `Writer<D>`.

Wait — the worker holds a `SharedHnsw` (reader) via `OpsContext.executor.index`, not a `Writer`. Need to add the swap method to `SharedHnsw` instead. Looking at the type: both `SharedHnsw` and `Writer` hold `Arc<RwLock<HnswIndex<D>>>`. We can put `swap` on `SharedHnsw` directly since the underlying lock allows write access through any Arc clone.

Actually, the spec §06/08 §1 wants "single-writer-per-shard." For v1 the maintenance worker is logically a writer too — it does swap the index. The `Writer<D>` type currently enforces single-writer via not-Clone. Putting `swap` on `SharedHnsw` would let any reader become a writer.

**Resolution:** put `swap` on `Writer<D>`. The brain-ops `RealWriterHandle` already holds the `Writer<384>`. We expose a method on `RealWriterHandle::swap_index(new)` so the maintenance worker calls into the writer, preserving the single-writer discipline (the mutex inside `RealWriterHandle::hnsw_writer` serialises).

```rust
// crates/brain-ops/src/writer.rs

impl RealWriterHandle {
    pub fn swap_hnsw(&self, new_index: HnswIndex<384>) {
        self.hnsw_writer.lock().swap(new_index);
    }
}
```

Hmm, but `RealWriterHandle` is `Arc<dyn WriterHandle>` in `OpsContext`. The trait surface doesn't include `swap_hnsw`. Options:

(a) Add `submit_hnsw_swap` to the `WriterHandle` trait. Pure plumbing.
(b) Expose `SharedHnsw::swap` directly (give up the strict single-writer discipline for v1, with a doc comment that swap is "rare and serialised by the maintenance worker's single-instance-per-shard contract").

For minimal changes and v1's reality (one shard, one worker, one writer), I'll go with **(b)** but route through a small SharedHnsw method that takes `&mut HnswIndex` via the write lock. The maintenance worker is a single async task per shard so concurrent swaps don't happen. Documented.

Final shape:
```rust
// crates/brain-index/src/shared.rs

impl<const D: usize> SharedHnsw<D> {
    /// Atomically replace the inner index with `new`. Used by the
    /// HNSW maintenance worker (sub-task 8.5).
    ///
    /// **Discipline**: only one task should call `swap` at a time;
    /// the scheduler's single-worker-per-name guarantee enforces this
    /// at the runtime level.
    pub fn swap(&self, new: HnswIndex<D>) {
        let mut guard = self.inner.write();
        *guard = new;
    }
}
```

This works through the existing `inner: Arc<RwLock<HnswIndex<D>>>`.

---

## 3. Decision logic

```rust
// crates/brain-workers/src/hnsw_maint.rs

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IndexStats {
    pub total_entries: usize,    // SharedHnsw::len()
    pub tombstone_count: usize,
    pub tombstone_ratio: f32,
    pub recall_estimate: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RebuildThresholds {
    pub tombstone_full_rebuild: f32,    // default 0.30
    pub recall_full_rebuild: f32,       // default 0.90
    pub tombstone_schedule: f32,        // default 0.15
    pub recall_schedule: f32,           // default 0.93
}

impl Default for RebuildThresholds {
    fn default() -> Self {
        Self {
            tombstone_full_rebuild: 0.30,
            recall_full_rebuild: 0.90,
            tombstone_schedule: 0.15,
            recall_schedule: 0.93,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    None,
    ScheduleRebuildSoon,
    FullRebuild,
}

#[must_use]
pub fn decide_action(stats: IndexStats, t: RebuildThresholds) -> Action {
    if stats.tombstone_ratio > t.tombstone_full_rebuild {
        return Action::FullRebuild;
    }
    if stats.recall_estimate < t.recall_full_rebuild {
        return Action::FullRebuild;
    }
    if stats.tombstone_ratio > t.tombstone_schedule
        || stats.recall_estimate < t.recall_schedule
    {
        return Action::ScheduleRebuildSoon;
    }
    Action::None
}
```

Pure, unit-testable, matches spec §3 exactly.

---

## 4. RebuildSource trait

```rust
#[derive(Debug, thiserror::Error)]
pub enum RebuildSourceError {
    #[error("rebuild source disabled (no vector lookup in v1)")]
    Disabled,
    #[error("rebuild source failed: {0}")]
    Failed(String),
}

pub trait RebuildSource<const D: usize>: Send + Sync + 'static {
    fn snapshot_vectors<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(MemoryId, [f32; D])>, RebuildSourceError>>
        + Send + 'a>>;
}

pub struct DisabledRebuildSource;
impl<const D: usize> RebuildSource<D> for DisabledRebuildSource {
    fn snapshot_vectors<'a>(&'a self) -> Pin<Box<...>> {
        Box::pin(async { Err(RebuildSourceError::Disabled) })
    }
}
```

Same `Pin<Box<Future>>` pattern as `Summarizer`. Production injects an arena-backed impl in Phase 9.

---

## 5. `HnswMaintenanceWorker`

```rust
pub struct HnswMaintenanceWorker {
    config: WorkerConfig,
    thresholds: RebuildThresholds,
    rebuild_source: Arc<dyn RebuildSource<{ brain_embed::VECTOR_DIM }>>,
}

impl HnswMaintenanceWorker {
    pub fn new(rebuild_source: Arc<dyn RebuildSource<384>>) -> Self;
    pub fn with_config(self, cfg: WorkerConfig) -> Self;
    pub fn with_thresholds(self, t: RebuildThresholds) -> Self;
}
```

### Cycle

```rust
async fn do_cycle(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
    let index = ctx.ops.executor.index.clone();

    // 1. Collect stats.
    let total = index.len();
    let tombstoned = index.tombstone_count();
    let active = total.saturating_sub(tombstoned);
    let ratio = if total == 0 { 0.0 } else { tombstoned as f32 / total as f32 };
    let stats = IndexStats {
        total_entries: total,
        tombstone_count: tombstoned,
        tombstone_ratio: ratio,
        recall_estimate: 1.0,    // v1: no query-sample logging
    };
    let action = decide_action(stats, self.thresholds);

    match action {
        Action::None => Ok(0),
        Action::ScheduleRebuildSoon => {
            // v1: just record it. Phase 9 will defer a rebuild to a
            // less-busy window.
            trace!(?stats, "hnsw maintenance: schedule rebuild soon");
            Ok(0)
        }
        Action::FullRebuild => {
            match self.rebuild_source.snapshot_vectors().await {
                Ok(vectors) => {
                    let params = index.params();
                    let (new_idx, _report) = HnswIndex::<384>::rebuild(params, vectors)
                        .map_err(|e| WorkerError::Ops(format!("rebuild: {e:?}")))?;
                    index.swap(new_idx);
                    trace!(?stats, "hnsw maintenance: full rebuild complete");
                    Ok(1)
                }
                Err(RebuildSourceError::Disabled) => {
                    trace!(?stats, "hnsw maintenance: rebuild needed but source disabled");
                    Ok(0)
                }
                Err(RebuildSourceError::Failed(e)) => {
                    Err(WorkerError::Ops(format!("rebuild source: {e}")))
                }
            }
        }
    }
}
```

Returns `processed = 1` on successful rebuild, `0` otherwise — matches the standard "units of work" semantics.

---

## 6. File-by-file plan

| File | Action | Notes |
| ---- | ------ | ----- |
| `crates/brain-index/src/shared.rs` | Edit | Add `SharedHnsw::swap()` method |
| `crates/brain-workers/src/hnsw_maint.rs` | NEW | Stats, decision, RebuildSource, HnswMaintenanceWorker |
| `crates/brain-workers/src/lib.rs` | Edit | Re-export |
| `crates/brain-workers/tests/hnsw_maint.rs` | NEW | ~14 tests |

No spec, wire, or Cargo.toml changes.

---

## 7. Tests (`crates/brain-workers/tests/hnsw_maint.rs`)

### decide_action (6)
1. `none_below_all_thresholds` — ratio=0.1, recall=1.0 → None.
2. `full_rebuild_when_tombstone_above_30` — ratio=0.35 → FullRebuild.
3. `full_rebuild_when_recall_below_90` — recall=0.85 → FullRebuild.
4. `schedule_when_tombstone_between_15_and_30` — ratio=0.20 → ScheduleRebuildSoon.
5. `schedule_when_recall_between_90_and_93` — recall=0.91 → ScheduleRebuildSoon.
6. `custom_thresholds_honoured` — pass `RebuildThresholds { tombstone_full_rebuild: 0.5, ... }`; verify lower defaults don't fire.

### Stats collection (2)
7. `cycle_observes_zero_tombstones_initially` — fresh fixture; `tombstone_ratio = 0`, action = None.
8. `cycle_reports_tombstone_after_forget` — encode then forget; ratio > 0; depending on threshold, action could be FullRebuild for 1/1 → 100%.

### Rebuild source (3)
9. `disabled_source_returns_disabled_error`.
10. `stub_source_returns_provided_vectors`.
11. `failed_source_propagates_error_as_worker_error`.

### Cycle (3)
12. `cycle_with_no_action_returns_zero` — empty index → action=None → processed=0.
13. `full_rebuild_via_stub_source_swaps_index_and_returns_one` — fixture w/ encoded memories + many tombstones (force FullRebuild) + stub source returning their vectors; after cycle, post-rebuild search still finds them, `tombstone_count() == 0`.
14. `disabled_source_with_rebuild_needed_returns_zero_no_swap` — same fixture; DisabledRebuildSource; cycle returns 0; index unchanged.

### Worker integration (2)
15. `worker_registers_with_correct_kind_and_default_cadence` — 5min interval.
16. `disabled_worker_via_config_does_not_run`.

Total: 16 tests.

---

## 8. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Swap blocks readers briefly via the RwLock write | Spec §5 calls out the microsecond-scale wait; documented |
| Concurrent swap calls from multiple sources | Scheduler guarantees one task per worker name; documented discipline |
| Rebuild from stub returns vectors that don't match the live index | Tests use a stub that mirrors the fixture's actual encoded vectors; production deployments inject an arena-backed source in Phase 9 |
| Recall threshold never fires in v1 | `recall_estimate = 1.0` constant; `decide_action` still tested for recall paths via pure-fn tests; phase 9 wires query sampling |
| `tombstone_ratio` jumps from 0 to 1.0 on FORGET in a 1-row index → constant FullRebuild thrashing | v1 batch_size=1 + tests use Disabled source most of the time; once real workloads exist, thresholds should be paired with a min-total-entries guard (Phase 9 tuning) |

---

## 9. Done criteria

- [ ] `SharedHnsw::swap()` in brain-index.
- [ ] `hnsw_maint.rs` with all the surface above.
- [ ] 16 tests in `tests/hnsw_maint.rs` pass first run.
- [ ] `cargo test --workspace` green; clippy + fmt clean.
- [ ] Commit subject: `feat(brain-workers,brain-index): HNSW maintenance worker (sub-task 8.5)`.

~450 LOC impl + ~550 LOC tests, single commit.
