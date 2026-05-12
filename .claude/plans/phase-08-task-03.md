# Sub-task 8.3 — Access-boost worker

**Spec:** `spec/11_background_workers/02_decay.md` §7, §8, §16
**Phase doc:** `docs/phases/phase-08-workers.md` §8.3
**Done when:** Recently-accessed memories get a transient salience bump.

---

## 1. Scope

8.3 ships:

- A per-shard **access buffer** that RECALL fills and the boost worker drains.
- The **`AccessBoostWorker`** running at 10 s cadence (spec §11/01 §11 default).
- Boost formula `new = min(1.0, salience × 1.10)` (spec §2, §10).
- Wire-up in `brain-ops::recall::handle_recall` to push every returned memory's id into the buffer.

Out of scope:
- Custom boost factor via TOML — Phase 9 server config.
- Boost on PLAN/REASON-traversed memories (spec ties boost to RECALL only; the "rich-get-richer" effect §17 already calls out the RECALL-only feedback loop).
- Tracking access counts separately from salience — `MemoryMetadata.access_count` exists but its read-path update is out-of-scope for 8.3 (not a salience concern).

---

## 2. The access buffer

```rust
// crates/brain-ops/src/access_buffer.rs  (NEW)

/// Bounded, dedup-on-record buffer of MemoryIds that RECALL returned
/// in the recent past. The access-boost worker drains it on its
/// 10 s cycle.
///
/// Dedup-on-record means a memory accessed N times within one drain
/// window receives one boost — chosen because:
/// 1. Spec §7 phrases the unit as "MemoryIds in the buffer", not
///    "accesses".
/// 2. Bounded write amplification (one update per memory per cycle).
/// 3. Salience caps at 1.0 anyway; the difference between 1 and N
///    boosts on a high-salience memory is small.
pub struct AccessBuffer {
    inner: parking_lot::Mutex<AccessBufferInner>,
    capacity: usize,
}

struct AccessBufferInner {
    ids: HashSet<MemoryId>,
    overflowed: u64,
}

impl AccessBuffer {
    pub fn new(capacity: usize) -> Self;
    /// Record an access. No-op if the buffer is at capacity (the
    /// boost will be picked up on a future access).
    pub fn record(&self, id: MemoryId);
    /// Swap out the current set. Returns the deduped ids.
    pub fn drain(&self) -> Vec<MemoryId>;
    /// Test/metric helper.
    pub fn len(&self) -> usize;
    pub fn overflowed_count(&self) -> u64;
}
```

Default capacity: 10 000. Spec §11/01 §11 defaults the boost worker batch_size to 1 000; 10 K headroom covers a recall storm.

### Wire-up in OpsContext

```rust
pub struct OpsContext {
    pub executor: ExecutorContext,
    pub planner_ctx: PlannerContext,
    pub txn_store: Arc<TxnStore>,
    pub events: Arc<EventBus>,
    pub subscriptions: Arc<SubscriptionRegistry>,
    pub subscribe_poll_window: Duration,
    pub access_buffer: Arc<AccessBuffer>,   // NEW
}
```

`OpsContext::new` defaults to a 10 K buffer. `with_access_buffer(buf)` builder for tests.

### Recording in `recall.rs`

After `hits.truncate(req.top_k as usize);` (line 61 of recall.rs):

```rust
for h in &hits {
    ctx.access_buffer.record(h.memory_id);
}
```

That's the entire integration. Inside-txn RECALL also records — boosts are applied at COMMIT-visible state, not txn-pending, but the buffer is per-shard and txn-agnostic so it doesn't matter.

---

## 3. AccessBoostWorker

```rust
// crates/brain-workers/src/access_boost.rs

pub const DEFAULT_BOOST_FACTOR: f32 = 0.10;
pub const MAX_SALIENCE: f32 = 1.0;

pub fn boosted_salience(current: f32, boost_factor: f32) -> f32 {
    ((current * (1.0 + boost_factor)).min(MAX_SALIENCE)).max(0.0)
}

pub struct AccessBoostWorker {
    config: WorkerConfig,
    boost_factor: f32,
}

impl AccessBoostWorker {
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::AccessBoost),
            boost_factor: DEFAULT_BOOST_FACTOR,
        }
    }
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self { ... }
    pub fn with_boost_factor(mut self, f: f32) -> Self { ... }
}

impl Worker for AccessBoostWorker { ... }
```

### Cycle

```rust
async fn do_boost_cycle(worker: &AccessBoostWorker, ctx: &WorkerContext) -> Result<usize, WorkerError> {
    // 1. Drain the buffer. (Cheap; one mutex pop.)
    let ids = ctx.ops.access_buffer.drain();
    if ids.is_empty() { return Ok(0); }

    // 2. Apply boost in one write txn. Bound by batch_size.
    let cfg = worker.config();
    let take_n = ids.len().min(cfg.batch_size);
    let metadata = ctx.ops.executor.metadata.clone();
    let started = Instant::now();
    let mut applied = 0usize;
    {
        let mut db = metadata.lock();
        let wtxn = db.write_txn().map_err(|e| WorkerError::Ops(...))?;
        {
            let mut table = wtxn.open_table(MEMORIES_TABLE)?;
            for id in &ids[..take_n] {
                if started.elapsed() >= cfg.max_runtime { break; }
                if ctx.is_shutdown() { break; }
                let key = id.to_be_bytes();
                let Some(access) = table.get(key)? else { continue; };  // tombstoned/deleted
                let mut meta = access.value();
                drop(access);
                let new = boosted_salience(meta.salience, worker.boost_factor);
                if (new - meta.salience).abs() < f32::EPSILON { continue; }  // already at cap
                meta.salience = new;
                meta.access_count = meta.access_count.saturating_add(1);
                table.insert(key, meta)?;
                applied += 1;
            }
        }
        wtxn.commit()?;
    }
    // 3. Re-queue overflow ids (if any) so future cycles pick them up.
    for id in ids.into_iter().skip(take_n) {
        ctx.ops.access_buffer.record(id);
    }
    Ok(applied)
}
```

The "skip dead rows" path means we don't error on FORGET-then-RECALL races; we just skip.

`access_count` bump on every applied boost — matches `MemoryMetadata.access_count` semantic and gives operators a real counter to look at.

---

## 4. File-by-file plan

| File | Action | Notes |
| ---- | ------ | ----- |
| `crates/brain-ops/src/access_buffer.rs` | NEW | `AccessBuffer` type |
| `crates/brain-ops/src/context.rs` | Edit | Add `access_buffer` field + builder |
| `crates/brain-ops/src/lib.rs` | Edit | `pub mod access_buffer; pub use access_buffer::AccessBuffer;` |
| `crates/brain-ops/src/recall.rs` | Edit | Record post-truncate hits |
| `crates/brain-workers/src/access_boost.rs` | NEW | `AccessBoostWorker` |
| `crates/brain-workers/src/lib.rs` | Edit | Re-export |
| `crates/brain-workers/tests/access_boost.rs` | NEW | 12 tests |
| `crates/brain-ops/tests/recall.rs` | Edit | Tiny addition: confirm RECALL fills the buffer |

No spec, no wire, no Cargo.toml additions (`brain-ops` already has parking_lot; `brain-workers` already has everything from 8.2).

---

## 5. Tests (`crates/brain-workers/tests/access_boost.rs`)

### Pure function (3)
1. `boost_50_percent_to_55_percent` — `boosted_salience(0.5, 0.10) == 0.55`.
2. `boost_caps_at_one` — `boosted_salience(0.95, 0.10) == 1.0`.
3. `boost_of_zero_stays_zero` — `boosted_salience(0.0, 0.10) == 0.0`.

### Buffer (3)
4. `buffer_dedups_records` — record id X three times → `drain()` returns one entry.
5. `buffer_overflow_drops_and_increments_counter` — capacity=4, record 10 distinct ids → `len() ≤ 4`, `overflowed_count() > 0`.
6. `drain_empties_buffer` — record, drain, len=0, second drain returns empty.

### Cycle (5)
7. `cycle_boosts_one_recorded_memory` — seed memory salience=0.5, record id, run cycle → salience=0.55.
8. `cycle_caps_at_one` — seed salience=0.95, record id, run cycle → salience=1.0.
9. `cycle_skips_missing_memory` — record an id with no row → no error, processed=0.
10. `cycle_increments_access_count` — seed access_count=0, boost twice → access_count=2.
11. `cycle_requeues_overflow_when_batch_too_small` — capacity=20, record 15 ids, batch_size=10 → first cycle boosts 10, buffer has 5 left.

### Integration (1)
12. `recall_fills_buffer_then_boost_worker_applies` — encode 2 memories, RECALL, then run boost cycle, salience bumped on both.

Plus a small touch in `brain-ops/tests/recall.rs`:
13. (existing-file edit) `recall_records_hits_in_access_buffer` — RECALL once, assert `ctx.access_buffer.len() == result_count`.

---

## 6. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Concurrent decay + boost write to MEMORIES → one overwrites the other | Spec §8 explicitly allows; both workers hit the parking_lot mutex; mutex serialises |
| Buffer fills under recall storm, boosts dropped | Documented; `overflowed_count()` exposed for operators; capacity tunable |
| Recording inside-txn RECALLs leaks pending ids to the buffer | Pending ids are real `MemoryId`s (reserved at txn time); if committed, boost applies; if aborted, table.get returns None and we skip |
| boost_factor of 0 means no-op | Allowed; `with_boost_factor(0.0)` effectively disables the worker without `enabled=false` |
| f32 precision near cap | `boosted_salience` returns `f32`; tests use generous tolerances near 1.0 |

---

## 7. Done criteria

- [ ] `AccessBuffer` lives in brain-ops; `OpsContext.access_buffer` wired through `new()`.
- [ ] `RECALL` records every returned hit's id.
- [ ] `AccessBoostWorker` implements `Worker`; default cadence 10 s; default factor 10%.
- [ ] 12 new tests in `access_boost.rs` + 1 added to `recall.rs` integration tests.
- [ ] `cargo test --workspace` green; clippy + fmt clean.
- [ ] Commit subject: `feat(brain-workers,brain-ops): access-boost worker (sub-task 8.3)`.

~350 LOC impl + ~450 LOC tests, single commit, no spec/wire changes.
