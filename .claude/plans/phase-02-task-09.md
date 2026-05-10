# Phase 2 — Task 2.9: `Wal` public type

**Classification:** moderate. Pure composition over 2.6–2.8: the `Wal` type owns LSN allocation, segment rollover, and the public `append` / `reader` surface. No new syscalls; existing primitives do the heavy lifting.

**Spec:** `spec/05_storage_arena_wal/07_write_path.md` §§3, 4, 13, 14, plus `04_wal_overview.md` §§3, 4 (LSN model + segment lifecycle), `06_wal_durability.md` §7 (rollover protocol).

## 1. Scope

In:

- `Wal` — single public handle that owns the active `GroupCommitter` (and its `WalSegment`), the monotonic LSN counter, and the directory.
- `Wal::create(dir, shard_uuid)` — fresh-create path: creates `wal/0000000000.wal`, starts the committer.
- `append(&mut self, record)` — overwrites `record.lsn` with the next LSN, triggers a segment rollover if needed, enqueues to the committer, blocks until durable, returns the assigned `Lsn`.
- `reader(&self)` — snapshot reader over all segments in the directory.
- Segment rollover when the active segment plus the next record would exceed `max_segment_bytes` (configurable; default = `WAL_SEGMENT_SIZE_BYTES` from `lib.rs`). Per spec §06 §7: shut down current committer → reclaim segment → close → create next segment → fsync directory → restart committer.
- `shutdown(self)` — drains the queue, flushes the final batch, closes the active segment cleanly.

Out:

- **Reopen / recovery.** Opening an existing WAL with previously-written segments requires scanning for the tail and resuming there. That's part of the recovery driver in sub-task 2.10. `Wal::create` requires an empty directory.
- **Async API.** Phase doc 2.9 prescribes `pub async fn append(&self, ...) -> Result<Lsn>`. We're synchronous (SD-2.8-2 carries forward — no Glommio runtime yet). The `&mut self` signature is honest about the current single-writer-per-shard model. Tracked as **SD-2.9-1**.
- **TXN_BEGIN / TXN_COMMIT / TXN_ABORT batching.** Spec §07 §13 describes transactional grouping. Beyond 2.9; per-record durability is enough to satisfy 2.9's done-when. The transactional path will compose `append` calls with explicit TXN markers when the request handler needs it.
- **Higher-level helpers** that build a `WalRecord` from a typed `WalPayload`. Caller (sub-task 2.10's recovery, future writer task in Phase 9) constructs the `WalRecord`; `Wal` doesn't translate from `WalPayload`. Keeps the boundary minimal.

## 2. Spec quotes that bind the design

> **§04 §3 (LSN):** "LSN 0 is reserved (never used). The first WAL record after a fresh shard creation is LSN 1."  
> **§04 §4 (segments):** "Segment names are 10-digit zero-padded sequence numbers." "When the active segment fills (reaches ~256 MiB), a new segment is started."  
> **§06 §7 (rollover):**
>
> > 1. The current group commit completes (flushing the last records into the old segment).
> > 2. A new segment file is created.
> > 3. The new segment's header is written and fsync'd.
> > 4. The directory containing segments is fsync'd.
> > 5. Subsequent records go to the new segment.
>
> **§07 §3 (write path step 1):** "next_lsn" — the LSN counter is the WAL's responsibility.  
> **§07 §15 (concurrent ENCODEs):** "Within a shard, the writer task serializes them — single-writer-per-shard."

## 3. Spec deviations surfaced

### 3.1 Synchronous `append`, not async

**Spec / phase doc** prescribe `pub async fn append(&self, record) -> Result<Lsn>`. Async-ness presupposes a runtime (Glommio per spec §06 §2.3, OR tokio).

**Implementation:** synchronous `pub fn append(&mut self, record) -> Result<Lsn, WalError>`. Carries forward the SD-2.8-2 stance: synchronous primitives now, swap to Glommio coroutines in Phase 9.

**The `&mut self` change** (rather than `&self` + interior mutability) reflects the single-writer-per-shard discipline (spec §07 §15) at the type level — the borrow checker enforces that there's only one active writer. Multiple `wal.reader()` calls are allowed (each returns an owned snapshot); concurrent appenders aren't.

Tracked as **SD-2.9-1** in `docs/spec-deviations.md`.

## 4. Architecture

### 4.1 Type shape

```rust
pub struct Wal {
    dir: PathBuf,
    shard_uuid: [u8; 16],
    next_lsn: u64,                      // monotonic
    active_segment_seq: u64,
    bytes_in_active_segment: usize,     // tracks size locally to decide rollover
    committer: Option<GroupCommitter>,  // Option to allow take-and-replace on rollover
    config: WalConfig,
}

#[derive(Debug, Clone, Copy)]
pub struct WalConfig {
    pub group_commit: GroupCommitConfig,
    /// Hard cap on segment file size. Default = `WAL_SEGMENT_SIZE_BYTES`
    /// (256 MiB from `lib.rs`).
    pub max_segment_bytes: usize,
}

impl Default for WalConfig { /* ... */ }
```

`bytes_in_active_segment` is updated locally on each successful `append` (incremented by `record.encoded_len()`). Avoids reaching into the committer thread for size info; the approximation is exact because we own the LSN order and every append is observed.

### 4.2 Public API

```rust
impl Wal {
    /// Create a fresh WAL in `dir`. Writes the first segment file
    /// (`0000000000.wal`) with `starting_lsn = 1`.
    ///
    /// Errors if `dir` already contains any `*.wal` files. Reopening an
    /// existing WAL is sub-task 2.10's job.
    pub fn create(dir: impl AsRef<Path>, shard_uuid: [u8; 16]) -> Result<Self, WalError>;

    pub fn create_with_config(
        dir: impl AsRef<Path>,
        shard_uuid: [u8; 16],
        config: WalConfig,
    ) -> Result<Self, WalError>;

    pub fn shard_uuid(&self) -> [u8; 16];
    pub fn next_lsn(&self) -> u64;
    pub fn active_segment_seq(&self) -> u64;
    pub fn dir(&self) -> &Path;

    /// Append a record. Sets `record.lsn` to the next monotonic LSN,
    /// triggers segment rollover if needed, enqueues to the committer,
    /// blocks until durable. Returns the assigned LSN.
    pub fn append(&mut self, record: WalRecord) -> Result<Lsn, WalError>;

    /// Take a point-in-time snapshot reader. Records currently buffered
    /// (not yet durable) won't be visible. For most callers, you'll only
    /// call this after every concurrent append has returned.
    pub fn reader(&self) -> Result<WalReader, WalError>;

    /// Drain + flush + close. Consumes self.
    pub fn shutdown(self) -> Result<(), WalError>;
}
```

### 4.3 Append flow

```rust
pub fn append(&mut self, mut record: WalRecord) -> Result<Lsn, WalError> {
    let lsn = self.next_lsn;
    record.lsn = Lsn(lsn);

    // Project the post-append size.
    let projected = WAL_SEGMENT_HEADER_LEN
        + self.bytes_in_active_segment
        + record.encoded_len();

    if projected > self.config.max_segment_bytes {
        self.rollover()?;   // also resets bytes_in_active_segment
    }

    let committer = self.committer.as_ref().expect("committer is always Some between rollovers");
    let handle = committer.append(record.clone())?;   // clone so we can also size-track
    let durable_lsn = handle.wait()?;
    debug_assert_eq!(durable_lsn, lsn);

    self.bytes_in_active_segment += record.encoded_len();
    self.next_lsn = lsn + 1;
    Ok(Lsn(lsn))
}
```

**`record.clone()`** is the simplest way to feed the committer while keeping the size info. `WalRecord::clone` copies the payload `Vec<u8>` — typically 16–2000 bytes. For a hot path we'd want zero-copy, but for 2.9 we prioritize clarity. Optimization is a follow-up if the writer task profiles hot.

**Rollover edge case**: if `record.encoded_len()` alone exceeds `max_segment_bytes - WAL_SEGMENT_HEADER_LEN`, the projected check fires twice (once for the existing segment + the new record, then again for the brand-new segment + the same record) and we'd loop. The record itself is too big. Return `WalError::RecordExceedsSegmentLimit`. Realistically `MAX_PAYLOAD = 16 MiB` (spec §05/05 §19) and `max_segment_bytes = 256 MiB`, so this is impossible in production — but worth surfacing as an explicit error.

### 4.4 Rollover flow

```rust
fn rollover(&mut self) -> Result<(), WalError> {
    // Spec §06 §7:
    //   1. Current group commit completes (drain via shutdown).
    //   2. New segment file created.
    //   3. Header written + fsync'd.  (WalSegment::create_new does this.)
    //   4. Directory fsync'd.
    //   5. Subsequent records go to the new segment.
    let old = self.committer.take().expect("committer present").shutdown()?;
    drop(old);   // close the old segment file

    let new_seq = self.active_segment_seq + 1;
    let new_path = segment_path(&self.dir, new_seq);
    let new_segment = WalSegment::create_new(&new_path, new_seq, self.next_lsn, self.shard_uuid)?;

    fsync_dir(&self.dir)?;   // step 4: durable directory entry

    self.committer = Some(GroupCommitter::start(new_segment, self.config.group_commit));
    self.active_segment_seq = new_seq;
    self.bytes_in_active_segment = 0;
    Ok(())
}
```

`fsync_dir` opens the directory with `O_RDONLY`, calls `libc::fsync(fd)`, closes. `nix` would have a wrapper but we only need `libc`. Helper lives in `wal/wal.rs` since it's a one-liner.

### 4.5 `Wal::create` flow

```rust
pub fn create_with_config(dir, shard_uuid, config) -> Result<Self, WalError> {
    let dir_path = dir.as_ref();
    fs::create_dir_all(dir_path)?;
    // Ensure dir is empty of any *.wal files.
    let any_wal = fs::read_dir(dir_path)?.any(|e| {
        e.ok().and_then(|e| e.path().extension().map(|s| s == "wal")).unwrap_or(false)
    });
    if any_wal {
        return Err(WalError::DirectoryNotEmpty { dir: dir_path.to_path_buf() });
    }

    let seg_path = segment_path(dir_path, 0);
    let segment = WalSegment::create_new(&seg_path, 0, 1, shard_uuid)?;
    fsync_dir(dir_path)?;   // first segment's dir entry must be durable

    let committer = GroupCommitter::start(segment, config.group_commit);

    Ok(Self {
        dir: dir_path.to_path_buf(),
        shard_uuid,
        next_lsn: 1,
        active_segment_seq: 0,
        bytes_in_active_segment: 0,
        committer: Some(committer),
        config,
    })
}
```

### 4.6 Errors

```rust
#[derive(thiserror::Error, Debug)]
pub enum WalError {
    #[error("directory {dir:?} already contains *.wal files; use the recovery driver to reopen")]
    DirectoryNotEmpty { dir: PathBuf },

    #[error("record encoded size ({record_bytes}) exceeds max_segment_bytes ({segment_max})")]
    RecordExceedsSegmentLimit { record_bytes: usize, segment_max: usize },

    #[error("WAL segment error: {0}")]
    Segment(#[from] WalSegmentError),

    #[error("commit error: {0}")]
    Commit(#[from] CommitError),

    #[error("WAL read error: {0}")]
    Read(#[from] WalReadError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
```

### 4.7 Module placement

Currently `crates/brain-storage/src/wal/mod.rs` is the directory module declaring submodules. The `Wal` type goes in a new `wal/wal.rs` (yes the doubled name; matches `arena/file.rs` precedent). Re-exported from `wal/mod.rs`.

Phase doc 2.9 says **Writes:** `crates/brain-storage/src/wal/mod.rs`. I'll deviate slightly — putting the type in `wal/wal.rs` keeps `mod.rs` as a clean re-export hub. Not a spec deviation; just a phase-doc quibble.

### 4.8 `Drop`

```rust
impl Drop for Wal {
    fn drop(&mut self) {
        if let Some(committer) = self.committer.take() {
            // Best-effort: ignore errors (we're already cleaning up).
            let _ = committer.shutdown();
        }
    }
}
```

## 5. Files touched

- `crates/brain-storage/src/wal/wal.rs` (new, ~280 lines including tests).
- `crates/brain-storage/src/wal/mod.rs` (re-export `Wal`, `WalConfig`, `WalError`).
- `docs/spec-deviations.md` (new entry SD-2.9-1).
- `docs/phases/phase-02-storage.md` (mark 2.9 done).

No new dependencies.

## 6. Trade-offs

| Question | Choice | Why |
|---|---|---|
| Sync vs async `append` | Sync, `&mut self` (deviation SD-2.9-1) | Carry forward 2.8 stance; honest about single-writer; the borrow checker enforces it. |
| Track segment size locally vs query the committer | Local | Avoid plumbing `current_size_bytes()` through the committer thread; size advances are deterministic. |
| `Wal::create` vs `Wal::open_or_create` | `create` only (errors on non-empty dir) | Reopen-after-recovery is 2.10's responsibility; keeping 2.9 narrow. |
| `record.clone()` on hot path | Yes for 2.9 | Profile + optimize later if measurable. Records are typically 100–2000 bytes; `Vec<u8>::clone` is one alloc. |
| Module placement (`wal.rs` vs in `mod.rs`) | New `wal/wal.rs` | `mod.rs` stays a re-export hub (consistent with `arena/file.rs` and `arena/slot.rs`). |
| Rollover triggers on size, time, or both | Size only | Spec §04 §4 mentions size only. Time-based rollover is operational sugar (e.g., for backup window alignment); not v1 scope. |

## 7. Risks

- **fsync_dir on the parent directory.** Linux supports `fsync` on a directory fd; opening a directory `O_RDONLY` is the standard pattern. We use `libc::open(C_str, O_RDONLY)` + `libc::fsync(fd)` + `libc::close(fd)`. Each call carries a `// SAFETY:` comment.
- **Rollover atomicity.** If we crash *during* rollover (after fallocate'ing the new segment but before fsync'ing the directory), recovery sees: (a) old segment is sealed and durable, (b) new empty segment exists on disk but the directory entry may not be durable. Spec §06 §7 step 4 (the directory fsync) is the durability barrier for the new file's existence. If we crash before it, the new file may "vanish" — recovery starts from the old segment. Records are durable up to the rollover point either way. This is the spec's atomicity model; 2.9 implements it; 2.10's recovery test exercises it.
- **`bytes_in_active_segment` drift after a failed `committer.append`.** If the committer returns `Err` mid-batch, our local counter is out of sync. We don't increment in that branch (the `Ok(_)` path is the only place the counter advances), so we're safe.
- **Drop without explicit shutdown.** The Drop impl shuts down the committer best-effort; the segment's tail is durably-flushed for everything that was acked, undefined for the trailing batch. Recovery handles it.
- **WalRecord::clone perf.** Acceptable for 2.9; flagged as a follow-up if profiling shows it.

## 8. Test plan

All tests use `tempfile::TempDir`. Read records back via `Wal::reader()`.

### Create path (3)

1. `Wal::create` on a fresh empty dir creates `0000000000.wal`, `next_lsn() == 1`, `active_segment_seq() == 0`.
2. `Wal::create` on a dir containing a `*.wal` file errors with `DirectoryNotEmpty`.
3. `Wal::create` creates the directory if it doesn't exist (uses `create_dir_all`).

### LSN allocation (2)

4. After 5 appends, `next_lsn()` is 6 and the records have LSNs 1..=5.
5. `record.lsn` is overwritten — supplying `Lsn(99)` doesn't put 99 on the disk; the actual stored LSN is the WAL-assigned one.

### End-to-end durability (1 — phase doc done-when)

6. **Append 100 records via `Wal::append`; read back via `Wal::reader()` returns the same 100 records in LSN order.** This is the load-bearing test.

### Rollover (3)

7. Configure `max_segment_bytes` small (~8 KiB) and write enough records to trigger ≥ 2 rollovers. After the test: the directory contains `0000000000.wal`, `0000000001.wal`, `0000000002.wal` (or however many); `wal.reader()` streams all records in order; LSNs are contiguous across segments.
8. Each rollover emits a fresh segment with header.starting_lsn = the LSN of the first record that lands in it.
9. A record larger than `max_segment_bytes - WAL_SEGMENT_HEADER_LEN` returns `RecordExceedsSegmentLimit` and doesn't mutate state (`next_lsn` unchanged).

### Reader (1)

10. `Wal::reader()` snapshot doesn't see records buffered after the snapshot was taken (since `append` blocks until durable, this only matters within a single test that calls `reader()` between appends).

### Shutdown (1)

11. `wal.shutdown()` flushes and closes; the resulting directory is in a consistent state (subsequent `WalReader::open` succeeds and yields all records).

### Drop (1)

12. Dropping `Wal` without `shutdown` doesn't panic; previously durable records are still readable.

**Total: 12 tests.**

## 9. Estimated commit shape

One commit on `feature/brain-storage`:

> `feat(brain-storage): Wal public type composing segment + committer + reader (sub-task 2.9)`

Body covers:
- `Wal` shape, LSN allocation, append flow.
- Segment rollover protocol per spec §06 §7 (including the directory `fsync`).
- SD-2.9-1: synchronous `&mut self` API instead of `async &self`.
- Test count.

Files touched: as in §5. No new deps. Verify gate: `cargo fmt --all -- --check && ./scripts/check-skills.sh && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-storage --all-targets` inside the dev container.

---

PLAN READY: see `.claude/plans/phase-02-task-09.md` — confirm to proceed.
