# Sub-task 8.2 — Decay worker

**Spec:** `spec/11_background_workers/02_decay.md`
**Phase doc:** `docs/phases/phase-08-workers.md` §8.2
**Done when:** Salience decays per the half-life rules per memory kind. Test with mocked time.

---

## 1. Scope

8.2 is **pure decay**. Access-boost is a separate worker (spec §7) and lives in 8.3. Auto-FORGET below a salience threshold (spec §9) is opt-in agent config that doesn't exist in v1 — out of scope.

In:
- `DecayWorker` implementing `Worker`.
- Per-kind half-life table (Episodic 30d / Semantic 365d / Consolidated 90d) — spec §1, §10.
- Closed-form decay: `salience = salience_initial × 2^(-age_days / half_life_days)` — spec §2.
- Cursor-based incremental scan: each cycle covers up to `batch_size` memories, advances cursor, wraps when end-of-table reached — spec §3, §5.
- "Minor change" skip: write only if `|new - old| >= 0.001` — spec §6.
- 10+ integration tests via direct table seeding + age-backdating.

Out (deferred):
- Access-boost worker → 8.3.
- Auto-FORGET sub-threshold → no agent config to honour in v1.
- Per-kind half-life overrides via TOML → server config plumbing (Phase 9).

---

## 2. Salience semantics — picking the formula

Spec §2 gives the closed-form: `s(t) = s_0 × 2^(-t/h)`. Spec §8 says decay may overwrite boosts: "if a boost happened in between, the decay worker may overwrite it ... a subsequent boost cycle re-applies it." Net effect of the two reads together: **decay always recomputes from `salience_initial` (frozen at ENCODE) plus age — it does NOT compound on the current salience field.**

That's the interpretation we ship. Consequences:

- Boost (8.3) bumps `salience`; decay (8.2) re-asserts the closed-form value next hour; boost re-applies on the next 10s cycle. Boosts are visible ~99% of the time given the 1h vs 10s cadence ratio.
- Decay is therefore **idempotent**: running it twice produces the same result. Restart-safe (spec §11/00 §13).
- The formula only reads `salience_initial`, `created_at_unix_nanos`, and `kind` — all immutable post-ENCODE.

---

## 3. The cycle

```rust
async fn run_cycle(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
    let cfg = self.config();
    let now_nanos = now_unix_nanos();
    let start_cursor = self.cursor.lock();  // Option<MemoryId>
    let mut updates: Vec<(MemoryId, f32)> = Vec::with_capacity(cfg.batch_size);

    // ── Read phase: snapshot the batch in one read txn. ──────────
    let metadata = ctx.ops.executor.metadata.clone();
    {
        let db = metadata.lock();
        let rtxn = db.read_txn().map_err(|e| WorkerError::Ops(e.to_string()))?;
        let table = rtxn.open_table(MEMORIES_TABLE).map_err(|e| WorkerError::Ops(...))?;
        let range = match *start_cursor {
            Some(last) => /* range strictly above `last` */,
            None => /* full range */,
        };
        for entry in table.range(range)? {
            if updates.len() >= cfg.batch_size { break; }
            let (key, value) = entry?;
            let meta = value.value();
            let new_salience = compute_decayed_salience(&meta, now_nanos);
            if (new_salience - meta.salience).abs() >= 0.001 {
                updates.push((MemoryId::from_be_bytes(key.value()), new_salience));
            }
        }
    }

    let scanned_to_end = updates.len() < cfg.batch_size;  // imperfect; see §3.3

    // ── Write phase: one wtxn for the whole batch. ───────────────
    if !updates.is_empty() {
        let mut db = metadata.lock();
        let wtxn = db.write_txn().map_err(...)?;
        {
            let mut table = wtxn.open_table(MEMORIES_TABLE)?;
            for (id, new_sal) in &updates {
                if let Some(access) = table.get(id.to_be_bytes())? {
                    let mut meta = access.value();
                    drop(access);
                    meta.salience = *new_sal;
                    table.insert(id.to_be_bytes(), meta)?;
                }
            }
        }
        wtxn.commit()?;
    }

    // ── Cursor advance. ──────────────────────────────────────────
    *start_cursor = match updates.last() {
        Some((last_id, _)) => Some(*last_id),
        None if scanned_to_end => None,  // wrap to start
        None => *start_cursor,           // no changes, no skip; advance separately
    };
    Ok(updates.len())
}
```

### 3.1 Why not `drive_batch`

`drive_batch` is per-unit-of-work. The decay cycle is "scan in one read txn → write in one write txn." That pattern doesn't decompose cleanly into independent units. We implement `run_cycle` directly, honouring the same batch_size + max_runtime semantics by checking inside the read loop.

### 3.2 Cursor

In-memory, lost on restart. Spec §11/00 §10 explicitly allows this: "workers re-discover what to do." After restart, decay starts from the lowest MemoryId again and converges within `(n_memories / batch_size)` cycles. Persistence is future work.

### 3.3 "End of table" detection

If `updates.len() < batch_size`, **it doesn't necessarily mean we reached the end** — many memories may have been skipped by the "minor change" filter. We track the **last scanned MemoryId** (separately from the last *written* one) and base the wrap decision on whether the scan exhausted the range. If it did, reset cursor; otherwise advance to the last scanned id even if no updates happened.

---

## 4. Helpers

```rust
// crates/brain-workers/src/decay.rs

pub const EPISODIC_HALF_LIFE_DAYS:     f64 = 30.0;
pub const SEMANTIC_HALF_LIFE_DAYS:     f64 = 365.0;
pub const CONSOLIDATED_HALF_LIFE_DAYS: f64 = 90.0;

const NANOS_PER_DAY: f64 = 86_400.0 * 1_000_000_000.0;
const MIN_DELTA_FOR_WRITE: f32 = 0.001;

pub fn half_life_days(kind: MemoryKind) -> f64 {
    match kind {
        MemoryKind::Episodic     => EPISODIC_HALF_LIFE_DAYS,
        MemoryKind::Semantic     => SEMANTIC_HALF_LIFE_DAYS,
        MemoryKind::Consolidated => CONSOLIDATED_HALF_LIFE_DAYS,
    }
}

pub fn decayed_salience(
    salience_initial: f32,
    age_unix_nanos: u64,         // now_nanos - created_at_unix_nanos
    kind: MemoryKind,
) -> f32 {
    let age_days = age_unix_nanos as f64 / NANOS_PER_DAY;
    let h = half_life_days(kind);
    let factor = (-age_days / h).exp2();   // 2^(-age/h)
    (salience_initial as f64 * factor).max(0.0) as f32
}
```

Pure function → unit-testable without a runtime.

---

## 5. `DecayWorker`

```rust
pub struct DecayWorker {
    config: WorkerConfig,
    /// Cursor: the last MemoryId scanned (not necessarily updated).
    /// `None` = start from the beginning. Wrapped after a full pass.
    cursor: parking_lot::Mutex<Option<MemoryId>>,
}

impl DecayWorker {
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::Decay),
            cursor: parking_lot::Mutex::new(None),
        }
    }
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }
}

impl Worker for DecayWorker {
    fn name(&self) -> &'static str { WorkerKind::Decay.name() }
    fn kind(&self) -> WorkerKind   { WorkerKind::Decay }
    fn config(&self) -> WorkerConfig { self.config.clone() }
    fn run_cycle<'a>(&'a self, ctx: &'a WorkerContext) -> Pin<Box<dyn Future<...>>> {
        Box::pin(do_cycle(self, ctx))
    }
}
```

---

## 6. Tests (`crates/brain-workers/tests/decay.rs`)

### Pure-function correctness (5)
1. `episodic_30_days_old_halves` — s_0=1.0, age=30d → ~0.5 (±1e-6).
2. `semantic_365_days_old_halves` — s_0=0.8, age=365d → ~0.4.
3. `consolidated_90_days_old_halves` — s_0=1.0, age=90d → ~0.5.
4. `age_zero_is_identity` — s_0=0.5, age=0 → 0.5.
5. `extreme_age_clamps_above_zero` — s_0=1.0, age=10_000d Episodic → ≥ 0.0, no NaN/Inf.

### Cycle behaviour (6)
6. `cycle_decays_one_memory_when_past_threshold` — seed memory backdated 30d, run cycle, salience halved.
7. `cycle_skips_recent_memories_under_minor_change_threshold` — seed memory backdated 1 minute, run cycle, salience unchanged.
8. `cycle_respects_batch_size` — seed 50 memories all backdated; batch_size=10 → exactly 10 updates per cycle.
9. `cycle_advances_cursor_across_invocations` — seed 30 memories; batch_size=10; three cycles cover all 30 (no duplicates, no gaps).
10. `cycle_wraps_cursor_after_full_pass` — after cursor reaches end, fourth cycle starts over and re-scans the same memories (idempotent).
11. `cycle_processed_count_feeds_metrics` — run worker via scheduler; processed_total equals number of memories updated.

### Worker integration (2)
12. `worker_registers_with_correct_kind_and_name` — register, check `metrics("decay")` exists, `config("decay").interval == 1h`.
13. `disabled_decay_worker_does_not_modify_salience` — register with `enabled=false`, sleep, verify salience untouched.

Total: 13 tests.

---

## 7. File-by-file plan

| File | Action | Notes |
| ---- | ------ | ----- |
| `crates/brain-workers/src/decay.rs` | NEW | Pure functions + `DecayWorker` |
| `crates/brain-workers/src/lib.rs`   | Edit | `pub mod decay; pub use decay::{DecayWorker, decayed_salience, half_life_days, ...}` |
| `crates/brain-workers/tests/decay.rs` | NEW | 13 tests |
| `crates/brain-workers/Cargo.toml`   | Edit | Add `redb.workspace = true`, `brain-metadata = { path = "../brain-metadata" }` |

No spec, wire, or other crate touched.

---

## 8. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Holding the metadata mutex too long blocks the writer | Read/write are split into separate `lock()` calls; cycle is bounded by `max_runtime`; spec §11/01 §6 yield-every-50 enforced in the read loop |
| Cursor lost on restart → re-decay everything | Spec §11/00 §10 allows this; decay is idempotent so re-running on the same memory is a no-op (delta < threshold) |
| Floating-point precision for very-old memories | `decayed_salience` clamps at 0.0; `f64` math then casts to `f32` — sufficient |
| Test flake from real wall-clock time | Tests seed `created_at_unix_nanos` explicitly to control age; `now` is computed once per cycle from `SystemTime::now()` and tests run instantly |

---

## 9. Done criteria

- [ ] `crates/brain-workers/src/decay.rs` exists with pure functions + `DecayWorker`.
- [ ] `Worker` impl wired; `WorkerKind::Decay` path through scheduler verified.
- [ ] 13 tests pass first run.
- [ ] `cargo test --workspace` green.
- [ ] clippy + fmt clean.
- [ ] Commit subject: `feat(brain-workers): decay worker (sub-task 8.2)`.

~400 LOC impl + ~500 LOC tests, single commit, no spec/wire changes.
