# Sub-task 8.6 — Idempotency cleanup worker

**Spec:** `spec/11_background_workers/05_idempotency_cleanup.md`
**Phase doc:** `docs/phases/phase-08-workers.md` §8.6
**Done when:** Sweeps idempotency table; entries older than 24h are removed.

---

## 1. What v1 already has

Most of the substrate is in place:

- `brain_metadata::tables::idempotency::IdempotencyEntry { request_hash, response_payload, created_at_unix_nanos, ... }`.
- `brain_metadata::tables::idempotency::prune_expired(table, now, ttl) -> u64` — full-sweep helper that already exists.
- Writers (encode/forget/link/unlink/batch) insert entries with `created_at_unix_nanos` per spec §07/06.

The only missing piece is the worker loop that calls the prune helper on a schedule.

---

## 2. Why a bounded variant

Spec §3 calls for **incremental cleanup** (max 1000 deletes per txn, loop until done). The existing `prune_expired` is a single-pass full sweep — fine for small tables, bad for the 4 GB / 86M-row steady state spec §4 anticipates. Add a tiny brain-metadata addition:

```rust
// crates/brain-metadata/src/tables/idempotency.rs (extend)

/// Like [`prune_expired`] but stops after `max` deletions. Returns
/// `(deleted_count, scanned_to_end)` so callers know whether another
/// cycle is needed.
pub fn prune_expired_bounded(
    table: &mut Table<'_, [u8; 16], IdempotencyEntry>,
    now_unix_nanos: u64,
    ttl_nanos: u64,
    max: usize,
) -> Result<(u64, bool), redb::StorageError>;
```

Implementation mirrors `prune_expired` but caps `victims.len()` at `max` and returns whether the scan ran to completion (signal for the worker to schedule another cycle immediately vs sleep).

---

## 3. The worker

```rust
// crates/brain-workers/src/idempotency_cleanup.rs

pub const DEFAULT_IDEMPOTENCY_TTL: Duration = Duration::from_secs(24 * 3600);

pub struct IdempotencyCleanupWorker {
    config: WorkerConfig,            // default: 1h interval, batch_size 1000, max_runtime 5s
    ttl: Duration,                   // default: 24h
}

impl IdempotencyCleanupWorker {
    pub fn new() -> Self;
    pub fn with_config(self, cfg: WorkerConfig) -> Self;
    pub fn with_ttl(self, ttl: Duration) -> Self;
}
```

### Cycle

```rust
async fn do_cleanup_cycle(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
    let cfg = self.config();
    if cfg.batch_size == 0 { return Ok(0); }
    let now_nanos = now_unix_nanos();
    let ttl_nanos = self.ttl.as_nanos() as u64;
    let metadata = ctx.ops.executor.metadata.clone();
    let started = Instant::now();
    let mut total_deleted = 0usize;

    // Loop until: max_runtime expires, no more to delete (scanned_to_end),
    // or shutdown requested.
    loop {
        if started.elapsed() >= cfg.max_runtime { break; }
        if ctx.is_shutdown() { break; }

        let (deleted, scanned_to_end) = {
            let mut db = metadata.lock();
            let wtxn = db.write_txn()?;
            let n;
            let end;
            {
                let mut table = wtxn.open_table(IDEMPOTENCY_TABLE)?;
                let (d, e) = prune_expired_bounded(&mut table, now_nanos, ttl_nanos, cfg.batch_size)?;
                n = d;
                end = e;
            }
            wtxn.commit()?;
            (n, end)
        };
        total_deleted += deleted as usize;

        if scanned_to_end { break; }
        // Yield between batches so we don't monopolise the mutex.
        tokio::task::yield_now().await;
    }
    Ok(total_deleted)
}
```

Returns the count for `processed_total` metrics.

---

## 4. File-by-file plan

| File | Action | Notes |
| ---- | ------ | ----- |
| `crates/brain-metadata/src/tables/idempotency.rs` | Edit | Add `prune_expired_bounded` + a couple of unit tests in the existing `#[cfg(test)] mod tests` |
| `crates/brain-workers/src/idempotency_cleanup.rs` | NEW | `IdempotencyCleanupWorker`, `DEFAULT_IDEMPOTENCY_TTL` |
| `crates/brain-workers/src/lib.rs` | Edit | Re-export |
| `crates/brain-workers/tests/idempotency_cleanup.rs` | NEW | ~10 tests |

No spec, no wire, no other crate changes.

---

## 5. Tests

### `brain-metadata/src/tables/idempotency.rs` (extend the existing mod) — 3 unit tests
1. `prune_expired_bounded_empty_table_returns_zero_scanned_to_end`.
2. `prune_expired_bounded_caps_at_max_and_reports_not_scanned_to_end` — seed 50 expired, max=10 → returns `(10, false)`.
3. `prune_expired_bounded_returns_scanned_to_end_when_all_consumed` — seed 50 expired, max=100 → returns `(50, true)`.

### `crates/brain-workers/tests/idempotency_cleanup.rs` — 10 tests

#### Cycle (5)
4. `expired_entries_are_removed` — seed 5 entries with `created_at = now - 25h`; cycle returns 5; table empty.
5. `young_entries_are_kept` — seed 5 entries with `created_at = now`; cycle returns 0; table still has 5.
6. `mixed_entries_only_expired_removed` — 3 expired + 3 young; cycle returns 3; table has 3 young.
7. `multi_cycle_convergence` — seed 25 expired, batch_size=10. First cycle removes ≤ batch_size×K (the loop continues until scanned_to_end or budget). Should remove all 25 in one or two cycles; table empty afterwards.
8. `custom_ttl_honoured` — seed entries created 2h ago; worker with `ttl = 1h` removes them; worker with `ttl = 24h` doesn't.

#### Worker integration (3)
9. `worker_registers_with_correct_kind_and_default_cadence` — default 1h interval, kind=IdempotencyCleanup.
10. `disabled_worker_via_config_does_not_run`.
11. `cycle_processed_count_feeds_metrics` — via scheduler; processed_total reflects deletes.

#### Edge cases (2)
12. `empty_table_cycle_is_noop`.
13. `shutdown_short_circuits_mid_cycle` — seed > batch_size expired, batch_size small, fire shutdown after first batch → processed > 0 but < total.

Total: 13 tests (3 unit + 10 integration).

---

## 6. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Single wtxn for large delete batch blocks writers | `prune_expired_bounded` caps each wtxn at `batch_size`; tokio::yield between batches |
| Concurrent writer commits new entries during scan | redb's MVCC handles it — bounded scan reads at the wtxn's snapshot point; new entries unaffected |
| TTL changed at runtime | Worker picks up the new TTL on next cycle (read from `self.ttl`); spec §15 expects "large batch on first cycle after a shrink" — our bounded loop handles it |
| Mutex held across `.await` | Cycle releases the mutex between batches; only `yield_now()` happens outside the lock |

---

## 7. Done criteria

- [ ] `prune_expired_bounded` in brain-metadata + 3 unit tests.
- [ ] `IdempotencyCleanupWorker` in brain-workers.
- [ ] 10 integration tests pass first run.
- [ ] `cargo test --workspace` green; clippy + fmt clean.
- [ ] Commit subject: `feat(brain-workers,brain-metadata): idempotency cleanup worker (sub-task 8.6)`.

Out of scope (Phase 9): `ADMIN_IDEMPOTENCY_PRUNE` manual trigger, `idem_cleanup_oldest_age` metric (we don't compute the oldest age — operators get `processed_total` and `cycles_total` for now).

~200 LOC impl + ~400 LOC tests. Smaller commit than 8.4/8.5.
