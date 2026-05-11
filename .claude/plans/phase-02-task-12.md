# Phase 2 — Task 2.12: Checkpoint writer

**Classification:** light. Pure composition over 2.4 (msync arena), 2.9 (`Wal::append`), and 2.10 (sink behavior on `CheckpointEnd`). One small `ArenaFile` extension (`msync_all`) and one `InMemoryMetadataSink` extension. Phase 2's final sub-task.

**Spec:** `spec/05_storage_arena_wal/09_checkpointing.md` (full — particularly §2 marker shape, §3 procedure, §11 fast-restart, §12 failure modes). Cross-checked against `08_recovery.md` §3 (start point identification).

## 1. Scope

In:

- `pub fn write_checkpoint(wal: &mut Wal, arena: &ArenaFile, plan: CheckpointPlan) -> Result<CheckpointReport, CheckpointError>` — implements the spec §09 §3 procedure (BEGIN → msync arena → END). Lives in `crates/brain-storage/src/wal/checkpoint.rs`.
- `CheckpointPlan { checkpoint_id: u64, target_lsn: Option<u64> }` — caller-supplied. If `target_lsn` is `None`, use the WAL's `next_lsn() - 1` at call time (the LSN of the most recent durably-written record).
- `CheckpointReport { checkpoint_id, durable_lsn, lsn_begin, lsn_end, arena_capacity_at_checkpoint, started_at_unix_nanos, completed_at_unix_nanos }`.
- `ArenaFile::msync_all(&self)` — new pub method that `msync(MS_SYNC)`s the entire mmap region. The existing 2.4 code only msyncs the header page during `grow_to`.
- Extend `InMemoryMetadataSink::apply` to update `durable_lsn` when it sees a `CheckpointEnd` payload: `self.durable_lsn = self.durable_lsn.max(p.durable_lsn)`. Single 2-line change.

Out:

- **Drain step (spec §3 step 2).** Our sync API serializes appends via `&mut self`; there's no in-flight write to drain when `write_checkpoint` is called.
- **Metadata commit (spec §3 step 4).** That's the redb sink's job in Phase 3. We just write the WAL records; the sink picks up the new `durable_lsn` from the WAL on its next `apply(CheckpointEnd)` call.
- **HNSW dump (spec §3 step 5).** Phase 4.
- **Background scheduling (spec §4 "every 10 min or 1 GiB").** The worker is a separate task (Phase 8). 2.12 is the *primitive*; the worker calls `write_checkpoint` on a schedule.
- **WAL retention sweep (spec §6).** Phase 8 worker.
- **Non-blocking checkpoint mode (spec §5).** Spec lists it as an open question.

## 2. Spec quotes that bind the design

> **§09 §2 (marker shape):** `Checkpoint { checkpoint_id, durable_lsn, arena_capacity_at_checkpoint, metadata_version_at_checkpoint, started_at, completed_at }`. We omit `metadata_version_at_checkpoint` from `CheckpointReport` — it's the redb sink's concern (Phase 3).
>
> **§09 §3 (procedure):**
> > 1. Begin. Write a `CHECKPOINT_BEGIN` WAL record. Note the current LSN as `target_lsn`.
> > 2. Drain. Wait for all in-flight writes to complete. ⟵ trivially satisfied by `&mut self`.
> > 3. Sync arena. Issue `msync(MS_SYNC)` on the arena.
> > 4. Sync metadata. ⟵ deferred to redb sink.
> > 5. Sync HNSW state. ⟵ Phase 4.
> > 6. End. Write a `CHECKPOINT_END` WAL record with `durable_lsn = target_lsn`. Update the checkpoint table in the metadata store.
> > 7. Resume.
>
> **§09 §12.1 (disk full during checkpoint):** "The CHECKPOINT_BEGIN record is in the WAL. No CHECKPOINT_END record is written. The previous checkpoint remains the active recovery target." → `recover()` already does the right thing because the sink only updates `durable_lsn` on `CheckpointEnd`. A BEGIN without END is recorded but doesn't change the recovery start point.
>
> **§09 §11 (fast restart):** "For a graceful shutdown, the substrate runs a checkpoint just before exit." → 2.12 is the primitive that supports this; the caller (server in Phase 9) decides when.

## 3. Design decisions

### 3.1 Free function, not a method on `Wal`

Phase doc 2.12 prescribes `pub fn write_checkpoint(wal: &Wal, ...)`. Free function. Putting it on `Wal` would bloat `Wal`'s API surface; the checkpoint procedure spans two crates' worth of concerns (WAL + arena + sink). A free function in `wal/checkpoint.rs` that takes `&mut Wal` + `&ArenaFile` is the cleanest seam.

Caller passes `&mut wal` (mutable for the two `append` calls) and `&arena` (immutable — `msync_all` only reads from the mapped region; the syscall itself doesn't need `&mut`).

### 3.2 Sink picks up `durable_lsn` via `apply(CheckpointEnd)`

We don't take a `MetadataSink` parameter in `write_checkpoint`. The sink learns about checkpoints the next time it reads the WAL (i.e., during `recover()`). Two reasons:

1. **Simpler API.** No extra parameter.
2. **Crash-safety.** If we crash between writing the WAL and notifying the sink, the WAL is the source of truth — recovery picks it up. Pushing sink notifications into `write_checkpoint` would create a window where the WAL and sink disagree.

This means: between calling `write_checkpoint` at runtime and the next `recover()`, the sink's `durable_lsn` is stale. For Phase 2, that's fine (recovery is the only consumer). Phase 8's WAL retention worker will need a different signal (it should derive `durable_lsn` from its own checkpoint observations, not from the sink).

### 3.3 `target_lsn` defaults to `next_lsn() - 1`

If the caller passes `target_lsn: None`, we compute `target_lsn = wal.next_lsn().saturating_sub(1)` at the moment `write_checkpoint` is called. Trade-off: this is racing against any prior `wal.append` that hasn't returned yet — but `&mut wal` serializes, so when `write_checkpoint` holds `&mut wal`, no append is in flight.

`target_lsn == 0` is allowed: it means "no records before this checkpoint." After such a checkpoint, recovery would skip nothing (durable_lsn = 0). Useful for an empty-WAL graceful shutdown.

### 3.4 New method: `ArenaFile::msync_all`

```rust
impl ArenaFile {
    /// `msync(MS_SYNC)` the entire mmap region. Blocks until all dirty
    /// pages reach stable storage. Called by the checkpoint writer
    /// per spec §05/09 §3 step 3.
    pub fn msync_all(&self) -> std::io::Result<()> {
        // SAFETY: base/file_size are the live mmap region; MS_SYNC is
        // a valid flag; the kernel handles synchronization with any
        // concurrent reads (none in our single-writer-per-shard model).
        let rc = unsafe {
            libc::msync(
                self.base.as_ptr() as *mut c_void,
                self.file_size,
                libc::MS_SYNC,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}
```

`&self` — no mutation. The Sync impl already covers this.

### 3.5 `InMemoryMetadataSink::apply` extension

```rust
fn apply(&mut self, lsn: u64, payload: &WalPayload) -> Result<(), MetadataSinkError> {
    self.by_lsn.insert(lsn, payload.clone());
    if let WalPayload::CheckpointEnd(p) = payload {
        self.durable_lsn = self.durable_lsn.max(p.durable_lsn);
    }
    Ok(())
}
```

Three lines added to 2.10's impl. Defensive `max` (rather than direct assignment) handles out-of-order recovery — though our `recover` iterates in LSN order, so monotonicity is already enforced.

### 3.6 Error type

```rust
#[derive(thiserror::Error, Debug)]
pub enum CheckpointError {
    #[error("WAL error during checkpoint: {0}")]
    Wal(#[from] WalError),
    #[error("arena msync failed: {0}")]
    Msync(#[source] std::io::Error),
}
```

Two failure modes. WalError covers `append` failures (broken WAL, rollover error). Msync wraps the io::Error from `ArenaFile::msync_all`.

If BEGIN succeeds and msync fails, we still try to record what we attempted: return `CheckpointError::Msync` without writing END. The next `recover()` sees BEGIN without END → checkpoint didn't complete (spec §12.1).

### 3.7 Timestamps

Spec §2 lists `started_at` and `completed_at` in the marker. We capture both around the procedure:

```rust
let started = unix_nanos_now();
let begin_record = ... payload = CheckpointBeginPayload { checkpoint_id: plan.checkpoint_id, started_at_unix_nanos: started };
let lsn_begin = wal.append(begin_record)?;

arena.msync_all().map_err(CheckpointError::Msync)?;

let end_record = ... payload = CheckpointEndPayload { checkpoint_id, durable_lsn, arena_capacity };
let lsn_end = wal.append(end_record)?;

let completed = unix_nanos_now();
```

The report carries both timestamps. The on-disk CHECKPOINT_BEGIN payload carries `started_at`; CHECKPOINT_END carries `durable_lsn + arena_capacity` (CheckpointEndPayload in our 2.2 doesn't have `completed_at` — the field doesn't exist in spec §05/05 §15's record layout; only the in-metadata `Checkpoint` struct has it. So `completed_at` only lives in the runtime `CheckpointReport`).

## 4. Architecture

### 4.1 Files

- `crates/brain-storage/src/wal/checkpoint.rs` (new, ~280 lines incl. tests).
- `crates/brain-storage/src/wal/mod.rs` — `pub mod checkpoint;` + re-export.
- `crates/brain-storage/src/arena/file.rs` — add `msync_all`.
- `crates/brain-storage/src/recovery.rs` — extend `InMemoryMetadataSink::apply` (3 lines).
- `docs/phases/phase-02-storage.md` — mark 2.12 done.

### 4.2 Types

```rust
#[derive(Debug, Clone, Copy)]
pub struct CheckpointPlan {
    pub checkpoint_id: u64,
    /// `target_lsn` for the checkpoint. `None` → `wal.next_lsn() - 1`.
    pub target_lsn: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckpointReport {
    pub checkpoint_id: u64,
    pub durable_lsn: u64,
    pub lsn_begin: u64,
    pub lsn_end: u64,
    pub arena_capacity_at_checkpoint: u64,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
}

pub fn write_checkpoint(
    wal: &mut Wal,
    arena: &ArenaFile,
    plan: CheckpointPlan,
) -> Result<CheckpointReport, CheckpointError>;
```

### 4.3 Implementation sketch

```rust
pub fn write_checkpoint(wal, arena, plan) -> Result<CheckpointReport, CheckpointError> {
    let started_at_unix_nanos = unix_nanos_now();
    let target_lsn = plan.target_lsn.unwrap_or_else(|| wal.next_lsn().saturating_sub(1));
    let arena_capacity = arena.capacity_slots();

    // Step 1: CHECKPOINT_BEGIN.
    let begin_payload = WalPayload::CheckpointBegin(CheckpointBeginPayload {
        checkpoint_id: plan.checkpoint_id,
        started_at_unix_nanos,
    });
    let begin_record = WalRecord::from_typed(Lsn(0), 0, started_at_unix_nanos, 0, &begin_payload);
    let lsn_begin = wal.append(begin_record)?.raw();

    // Step 3: msync arena.
    arena.msync_all().map_err(CheckpointError::Msync)?;

    // Step 6: CHECKPOINT_END.
    let end_payload = WalPayload::CheckpointEnd(CheckpointEndPayload {
        checkpoint_id: plan.checkpoint_id,
        durable_lsn: target_lsn,
        arena_capacity,
    });
    let end_record = WalRecord::from_typed(Lsn(0), 0, unix_nanos_now(), 0, &end_payload);
    let lsn_end = wal.append(end_record)?.raw();

    let completed_at_unix_nanos = unix_nanos_now();
    Ok(CheckpointReport {
        checkpoint_id: plan.checkpoint_id,
        durable_lsn: target_lsn,
        lsn_begin, lsn_end,
        arena_capacity_at_checkpoint: arena_capacity,
        started_at_unix_nanos,
        completed_at_unix_nanos,
    })
}
```

`unix_nanos_now` is a private helper duplicated from `wal/segment.rs` and `arena/file.rs`. (Three call sites → moves the bar for extracting to a shared `time.rs`. Acceptable for now; one-line follow-up if a fourth caller appears.)

## 5. Trade-offs

| Question | Choice | Why |
|---|---|---|
| Method on `Wal` vs free function | Free function | Phase doc shape; cleaner cross-module composition. |
| Take `MetadataSink` parameter | No | Sink learns via `apply(CheckpointEnd)` during next `recover`; avoids dual-write inconsistency. |
| `target_lsn`: caller-supplied vs computed | Both (Option) | Default to `next_lsn() - 1`; let the worker override if it has stricter semantics (e.g., "checkpoint exactly LSN N"). |
| `msync_all` placement | Method on `ArenaFile` | Belongs with the mmap owner; reuses existing `&self` + `unsafe`. |
| Defer sink-side checkpoint tracking | Yes for 2.12; redb sink owns it in Phase 3 | Matches the layered design. |
| Handle BEGIN-without-END crash | Implicitly correct | Sink only updates `durable_lsn` on `CheckpointEnd`; a stray BEGIN has no recovery effect. |

## 6. Risks

- **`msync_all` on a large arena could be slow.** Spec §13 says "msync proportional to the number of dirty pages" — for typical workloads, fine. We don't add timing or progress reporting; later sub-tasks can.
- **`completed_at` is not in the on-disk `CheckpointEnd` payload.** Spec §2's marker shape includes `completed_at`, but the WAL record format in §05/05 §15 doesn't. The runtime `CheckpointReport` carries it; the redb sink in Phase 3 will need to compute it from observed `apply` timing or accept that it's approximate.
- **Two timestamp captures + arbitrary thread scheduling.** `started_at` is captured before BEGIN; `completed_at` after END. They could be wildly apart on a slow machine. Acceptable — they're for diagnostics, not correctness.
- **`unix_nanos_now` triplicated.** Already noted; tolerable.

## 7. Test plan

All tests in `wal/checkpoint.rs`'s `#[cfg(test)] mod tests`. Reuse the `Wal` + `ArenaFile` + `InMemoryMetadataSink` + `recover` primitives from prior sub-tasks.

### Basic mechanics (3)

1. **`write_checkpoint` on a fresh WAL succeeds.** No prior records; `target_lsn = 0`. Report has `lsn_begin = 1`, `lsn_end = 2`, `durable_lsn = 0`. The WAL now has the two checkpoint records.
2. **After 10 appends, `target_lsn = None` resolves to 10.** Default behavior.
3. **Explicit `target_lsn` is honored.** Caller passes `target_lsn = Some(5)` after writing 10 records; report shows `durable_lsn = 5`.

### Recovery integration (3 — phase doc done-when)

4. **Checkpoint → recovery starts from `durable_lsn + 1`** (done-when #1). Write 10 records, `write_checkpoint(target=10)`, run `recover` twice. After the first recover, sink.durable_lsn = 10. After the second (on the same WAL+sink), records 1..=10 are skipped; only BEGIN and END are applied.
5. **Multiple checkpoints → recovery uses the latest** (done-when #2). Write 10 records, checkpoint(target=10). Write 10 more (LSNs 13..=22 — accounting for BEGIN at 11, END at 12). Checkpoint(target=22). Run `recover` twice. After the second run, the sink's `durable_lsn` = 22 (the later checkpoint's value).
6. **Idempotent across re-runs.** Run `recover` three times on the same WAL+sink; the final sink state is identical to running it twice.

### Failure handling (2)

7. **BEGIN-without-END (simulated msync failure) doesn't change `durable_lsn`.** Write 10 records, manually append a CHECKPOINT_BEGIN to the WAL (no matching END), run `recover`. Sink's `durable_lsn` stays 0 (only END moves it).
8. **`msync_all` is called between BEGIN and END.** Smoke test that the writer actually issues the syscall (count via a `#[cfg(test)]` static, parallel to 2.8's `FLUSH_DURABLE_CALLS`). Plan §3.4 — add `MSYNC_ALL_CALLS` to `ArenaFile`.

### Smoke (1)

9. **`ArenaFile::msync_all` on a brand-new arena succeeds** (no dirty pages, returns Ok).

**Total: 9 tests.**

## 8. Estimated commit shape

One commit on `feature/brain-storage`:

> `feat(brain-storage): checkpoint writer (sub-task 2.12)`

Body:
- Free function `write_checkpoint(wal, arena, plan)` implementing spec §09 §3.
- New `ArenaFile::msync_all`.
- `InMemoryMetadataSink::apply` extension for `CheckpointEnd` → `durable_lsn`.
- BEGIN-without-END crash semantics.
- Test count.

Phase 2 exit: this is the last sub-task. After commit, run the full verify in the dev container (including `--ignored` for the 1000-iter random-kill test), then tag `phase-2-complete`.

Files touched: as in §4.1. No new deps. Verify gate: `cargo fmt --all -- --check && ./scripts/check-skills.sh && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-storage --all-targets` inside the dev container, plus `cargo test -p brain-storage --test random_kill -- --ignored` for the full sweep.

---

PLAN READY: see `.claude/plans/phase-02-task-12.md` — confirm to proceed.
