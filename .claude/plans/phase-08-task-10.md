# Sub-task 8.10 — Counter reconciliation worker

**Spec:** `spec/11_background_workers/08_misc_workers.md` §2
**Phase doc:** `docs/phases/phase-08-workers.md` §8.10
**Done when:** Per-shard counters are recomputed from full scans periodically; drift between counter and ground truth detected and fixed.

---

## 1. Scope

v1 has exactly one set of denormalized counters worth reconciling: **`MemoryMetadata.edges_out_count` and `edges_in_count`**. Both are bumped/decremented by the writer (encode-inline edges, LINK, UNLINK, slot reclamation) but a bug in any of those paths can drift the count from the actual edge-table truth.

Other counters spec §2.1 mentions:
- `ContextMetadata.memory_count` — no `CONTEXTS_TABLE` exists in v1; deferred.
- `AgentMetadata.context_count`/`memory_count` — fields exist on `AgentMetadata`, but agent writes aren't wired through any v1 handler (no agent admin ops yet). Deferred.
- Per-shard cluster totals — no cluster layer. Deferred.

So this worker reconciles `edges_*_count` only. Spec §2.2 says sample (not full-scan) to keep cost down.

---

## 2. The cycle

```rust
async fn do_reconcile_cycle(&self, ctx) -> Result<usize, WorkerError> {
    let cfg = self.config();
    let metadata = ctx.ops.executor.metadata.clone();
    let started = Instant::now();

    // 1. Sample up to batch_size memory ids (deterministic walk from
    //    cursor). Spec §2.2 says sample; for simplicity v1 uses a
    //    cursor like decay rather than random sampling.
    let cursor = *self.cursor.lock();
    let candidates = collect_candidates(metadata, cursor, cfg.batch_size, started, cfg.max_runtime)?;

    // 2. For each, count real edges and compare to stored counters.
    //    Collect mismatches.
    let mismatches = collect_mismatches(metadata, &candidates, started, cfg.max_runtime)?;

    // 3. One wtxn fixes them all.
    let mut fixed = 0;
    if !mismatches.is_empty() {
        let mut db = metadata.lock();
        let wtxn = db.write_txn()?;
        let mut memories = wtxn.open_table(MEMORIES_TABLE)?;
        for (id, true_out, true_in) in &mismatches {
            if let Some(access) = memories.get(id.to_be_bytes())? {
                let mut row = access.value();
                drop(access);
                if row.edges_out_count != *true_out || row.edges_in_count != *true_in {
                    row.edges_out_count = *true_out;
                    row.edges_in_count = *true_in;
                    memories.insert(id.to_be_bytes(), row)?;
                    fixed += 1;
                }
            }
        }
        wtxn.commit()?;
    }
    // 4. Advance cursor or wrap.
    Ok(fixed)
}
```

`collect_candidates` walks `MEMORIES_TABLE` above the cursor. `collect_mismatches` for each candidate ranges `EDGES_OUT[id, *, *]` and `EDGES_IN[id, *, *]` to recompute the true counts.

Spec §2.3 — drift detection: log a warning if `mismatches.len() / candidates.len() > 0.001` (>0.1%). v1 just emits at trace level; alerting plumbing is Phase 9.

---

## 3. `CounterReconcileWorker`

```rust
pub struct CounterReconcileWorker {
    config: WorkerConfig,                                // 1h default, batch_size 1, max_runtime 30s
    cursor: parking_lot::Mutex<Option<MemoryId>>,
}

impl CounterReconcileWorker {
    pub fn new() -> Self;
    pub fn with_config(self, cfg: WorkerConfig) -> Self;
}
```

`batch_size: 1` from `WorkerKind::CounterReconcile` defaults — spec §2.2 says "samples 1000 memories per day", so the default cadence×batch_size combo matches that. Per-cycle work is small.

For testability we'll let `with_config` bump batch_size up so a single cycle can cover all seeded memories.

---

## 4. File-by-file plan

| File | Action | Notes |
| ---- | ------ | ----- |
| `crates/brain-workers/src/counter_reconcile.rs` | NEW | Worker + helpers |
| `crates/brain-workers/src/lib.rs` | Edit | Re-export |
| `crates/brain-workers/tests/counter_reconcile.rs` | NEW | ~10 tests |

No spec, wire, or other-crate changes.

---

## 5. Tests

### Cycle (7)
1. `correctly_stamped_memory_needs_no_fix` — counts match ground truth → returns 0.
2. `under_counted_out_is_fixed` — seed memory with edges_out_count=0 but 2 outgoing edges → returns 1, count updated to 2.
3. `over_counted_in_is_fixed` — seed memory with edges_in_count=5 but 1 incoming edge → returns 1, count updated to 1.
4. `mixed_drift_both_directions_fixed_in_one_cycle`.
5. `multiple_memories_reconciled_in_one_cycle`.
6. `batch_size_caps_per_cycle`.
7. `cursor_advances_across_cycles`.

### Worker integration (2)
8. `worker_registers_with_correct_kind_and_default_cadence` — 1h interval.
9. `disabled_worker_via_config_does_not_fix`.

### Edge cases (1)
10. `empty_memories_table_cycle_is_noop`.

Total: 10 tests.

---

## 6. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Recounting edges per candidate is O(out_degree + in_degree) — slow for dense memories | Bounded by batch_size + max_runtime; spec §2.4 sizes the worker for ~100µs/memory |
| Mutex held across `.await` | Two-phase pattern (read → drop mutex → write); `yield_now()` between phases |
| Reconciliation races a live LINK / UNLINK | Spec §2 expects drift; reconciliation snapshot may be ~1 cycle behind. Acceptable per §2.3 — drift is rare and harmless |

---

## 7. Done criteria

- [ ] `CounterReconcileWorker` + helpers in `counter_reconcile.rs`.
- [ ] 10 tests pass first run.
- [ ] `cargo test --workspace` green; clippy + fmt clean.
- [ ] Commit subject: `feat(brain-workers): counter reconciliation worker (sub-task 8.10)`.

~300 LOC impl + ~400 LOC tests. Small commit.

Out of scope (Phase 9): `ContextMetadata` / `AgentMetadata` counts (no admin ops), per-shard cluster totals, drift-rate alerting plumbing.
