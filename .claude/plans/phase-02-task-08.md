# Phase 2 — Task 2.8: Group commit with `pwritev2(RWF_DSYNC)`

**Classification:** heavy. First sub-task that:

- Introduces a background thread.
- Adds a syscall path that has to be durable across crashes.
- Has spec internal tensions (see §3) that need explicit deviation decisions.

**Spec:** `spec/05_storage_arena_wal/06_wal_durability.md` (full). Cross-checked: `04_wal_overview.md` §§7–8 (synchronization, group commit), `08_recovery.md` §10 (failure modes), `12_open_questions.md` (no relevant entries).

## 1. Scope

Deliver the durability mechanism: callers append records, get a handle, and block on that handle until the record is on stable storage. Concurrent appenders' records get batched into a single `pwritev2(RWF_DSYNC)` call ("group commit"). A crash mid-batch is observable on reopen — only records whose handles signaled success appear in the recovered WAL.

In:

- `WalSegment::flush_durable()` — drains the in-memory buffer to the file via `pwritev2(fd, iovecs, offset, RWF_DSYNC)`. Synchronous in 2.8; will be swapped for an io_uring-backed version when Glommio is wired up.
- `GroupCommitter` — owns a `WalSegment`, runs a dedicated committer thread, accepts records on a channel, returns `AppendHandle`s.
- `AppendHandle::wait()` — blocks until the record's batch has been fsync'd; returns `Ok(lsn)` or `Err`.
- `GroupCommitter::shutdown()` — drains the queue, flushes the last batch durably, returns the wrapped `WalSegment` so the caller can sequence segment rollover.
- Triggers: time-based (default 100 µs commit window) and size-based (default 60 KB buffer threshold) per spec §06 §4.
- Tests for sequential durability, observable batching, and torn-write recovery via `WalReader`.

Out:

- **`O_DIRECT`** — deferred. See §3.1. Plain buffered I/O + `RWF_DSYNC` for now.
- **`io_uring`** — deferred. See §3.2. Synchronous `pwritev2` from a dedicated thread.
- **Segment rollover** — owned by the `Wal` public type in sub-task 2.9.
- **Multi-segment durability** — `GroupCommitter` works on a single segment; rolling between segments at the durability boundary is 2.9.
- **LSN allocation** — caller supplies records with `lsn` already set (2.9's job).
- **fsync of the parent directory** on segment creation — spec §06 §7, but only relevant during rollover. Deferred to 2.9.
- **Recovery driver** (apply records to arena/metadata) — 2.10.
- **Real-process crash test** — 2.11 (separate subprocess kill). 2.8 covers torn-write simulation via a test-only hook.

## 2. Spec quotes that bind the design

> **§06 §2.2 (`RWF_DSYNC`):** `pwritev2` with `RWF_DSYNC` "is performed. The kernel ensures the data is on stable storage before returning. Equivalent to `pwritev` followed by `fdatasync`, but in a single syscall."
>
> **§06 §3 (buffer + flush):** "The WAL writer maintains an aligned buffer for accumulating records … When a flush happens: 1. Round the buffer's used size up to the next 4 KB boundary (padding with zero bytes that recovery will interpret as a CRC-failed record and stop at)."
>
> **§06 §4 (group commit timing):** "Two triggers fire a group commit: time-based (100 µs window) and size-based (buffer reaches 60 KB out of 64 KB capacity)."
>
> **§06 §9 (failure handling):** "If `pwritev2` returns an error … the substrate marks the WAL as 'broken'; no further records can be written. Existing in-flight records receive errors."
>
> **§04 §8 (group commit purpose):** "Instead of fsync-per-record (slow), the WAL uses group commit: many records share a single fsync."

## 3. Spec tensions and explicit deviations

Two spec requirements interact in a way that needs spelling out. Both deviations are flagged in code (module docs + comments) and listed here for your sign-off before I implement.

### 3.1 `O_DIRECT` + 4 KB padding vs mid-segment recoverability

**Spec wants:** open WAL with `O_DIRECT` (§ 2.1); on every flush, "round the buffer's used size up to the next 4 KB boundary (padding with zero bytes that recovery will interpret as a CRC-failed record and stop at)" (§ 3).

**The conflict:** if each batch is padded to a 4 KB multiple and `file_offset` advances by the padded amount, then mid-segment there are zero-padding gaps between batches. `WalReader` (2.7) encounters those gaps and gets `Err(UnknownRecordType(0))` from `WalRecord::decode_one` (since the zero `record_type` byte is reserved). The reader's "tail vs mid-segment" rule (`UnknownRecordType` is *always* an error, regardless of position) means a gap between batches becomes `MidSegmentCorruption` — the WAL becomes unreadable after its first flush.

The spec's "padding that recovery stops at" wording only works if the padding is at the *very end* of the WAL, not between batches. Reconciling this in a way that supports both `O_DIRECT` alignment and multi-batch segments would require WAL pages (each 4 KB chunk gets its own header so the reader can skip page-aligned padding) — a significant format change beyond 2.8 scope.

**Plan for 2.8:** open the segment file *without* `O_DIRECT`. Use plain `pwritev2(RWF_DSYNC)`, no padding, records pack tightly across batches. `WalReader` works unchanged. Page cache holds the WAL pages briefly until kernel writeback; on modern Linux that's negligible memory overhead and acceptable per spec OQ-ST-5's "page cache works well for our access patterns" stance (applied here to the WAL's write path).

The full O_DIRECT-correct design (WAL pages with continuation markers) is a follow-up — likely Phase 9 server wire-up when Glommio + io_uring land and we can do this once with the right substrate.

**Deviation surfaced in:**
- Module-level doc comment in `wal/group_commit.rs`.
- Phase 2 plan + commit message.
- One-line note in the spec deviation log (`docs/spec-deviations.md` — if it doesn't exist yet, I'll create it; see §4.5).

### 3.2 `io_uring` vs synchronous `pwritev2`

**Spec wants:** "Rather than calling `pwritev2` synchronously, the substrate submits writes via io_uring" (§ 2.3). "The Glommio runtime wraps it."

**The conflict:** Glommio hasn't been wired into `brain-storage`. Bringing in Glommio for 2.8 alone would mean adding the runtime, picking an executor model, and coupling the committer to it — before the rest of the system (the request handler, the server, …) is ready to live on Glommio.

**Plan for 2.8:** synchronous `pwritev2` from a dedicated OS thread. Each `GroupCommitter` spawns one committer thread that owns the WAL segment file descriptor. The thread blocks on a `crossbeam_channel` for incoming records + a `recv_timeout` for the 100 µs commit window.

When Phase 9 lands the server + Glommio, the committer thread gets replaced with a Glommio coroutine using io_uring. The `GroupCommitter` public API (`append` → `AppendHandle::wait`) doesn't change.

**Deviation surfaced in:** same locations as §3.1.

## 4. Architecture

### 4.1 `WalSegment::flush_durable`

Add one method to `WalSegment` (sub-task 2.6 left a clean seam by separating `flush` from durability):

```rust
impl WalSegment {
    /// Drain the buffer to the file via pwritev2(RWF_DSYNC).
    /// Caller-supplied iovec layout; we pass one iovec covering the buffer.
    pub fn flush_durable(&mut self) -> Result<(), WalSegmentError> {
        if self.write_buf.is_empty() { return Ok(()); }
        let offset = WAL_SEGMENT_HEADER_LEN as i64 + self.bytes_on_disk as i64;
        let iov = libc::iovec {
            iov_base: self.write_buf.as_ptr() as *mut c_void,
            iov_len: self.write_buf.len(),
        };
        // SAFETY: fd is valid (we own File); &iov is valid; iovcnt=1.
        let n = unsafe { libc::pwritev2(fd, &iov, 1, offset, RWF_DSYNC as i32) };
        if n < 0 { return Err(io::Error::last_os_error().into()); }
        if (n as usize) != self.write_buf.len() {
            return Err(WalSegmentError::ShortWrite { wanted: self.write_buf.len(), got: n as usize });
        }
        self.bytes_on_disk += self.write_buf.len();
        self.write_buf.clear();
        Ok(())
    }
}
```

`RWF_DSYNC` numeric value `0x2` per spec §06 §2.2 + Linux UAPI. Define as a const in our crate; `libc` exposes `libc::RWF_DSYNC` on glibc but not always on musl — using the literal `0x2` matches spec and avoids the portability question.

A new error variant: `WalSegmentError::ShortWrite { wanted, got }`. (`pwritev2` is technically allowed to short-write per POSIX, but in practice on Linux it returns the full count on success.)

### 4.2 `GroupCommitter` shape

```rust
pub struct GroupCommitter {
    submission_tx: crossbeam_channel::Sender<Submission>,
    shutdown_tx: crossbeam_channel::Sender<ShutdownSignal>,
    committer_thread: Option<std::thread::JoinHandle<Result<WalSegment, CommitError>>>,
}

enum Submission {
    Append { record: WalRecord, ack_tx: crossbeam_channel::Sender<Result<u64, CommitError>> },
    /// Test-only: drop a flush mid-write to simulate a torn write.
    #[cfg(test)]
    SimulateTornWriteAfter { bytes: usize },
}

enum ShutdownSignal {
    Graceful,                     // drain queue, flush, return WAL segment
}

pub struct AppendHandle {
    ack_rx: crossbeam_channel::Receiver<Result<u64, CommitError>>,
}

pub struct GroupCommitConfig {
    pub commit_window: std::time::Duration,    // default 100 µs
    pub max_batch_bytes: usize,                // default 60 KiB
}

#[derive(thiserror::Error, Debug)]
pub enum CommitError {
    #[error("WAL is broken: {0}")]
    WalBroken(String),                         // sticky after first I/O failure
    #[error("WAL segment error: {0}")]
    Segment(#[from] WalSegmentError),
    #[error("committer thread shut down before the record could be flushed")]
    ShutDown,
    #[error("ack channel was dropped before the flush completed")]
    AckChannelClosed,
}
```

### 4.3 Public API

```rust
impl GroupCommitter {
    pub fn start(segment: WalSegment, config: GroupCommitConfig) -> Self;

    /// Enqueue a record. Returns immediately; the handle blocks until the
    /// record's batch is fsync'd.
    pub fn append(&self, record: WalRecord) -> Result<AppendHandle, CommitError>;

    /// Drain the queue, flush the final batch durably, and return the
    /// inner WalSegment. Consumes self.
    pub fn shutdown(self) -> Result<WalSegment, CommitError>;
}

impl AppendHandle {
    /// Block until the record is durable. Returns the record's LSN on success.
    pub fn wait(self) -> Result<u64, CommitError>;
    pub fn wait_timeout(self, dur: Duration) -> Result<Result<u64, CommitError>, RecvTimeoutError>;
}
```

`append` returning a `Result` covers the case where the committer thread has already shut down (channel closed).

### 4.4 Committer thread loop

```rust
fn committer_loop(
    mut segment: WalSegment,
    submission_rx: Receiver<Submission>,
    shutdown_rx: Receiver<ShutdownSignal>,
    config: GroupCommitConfig,
) -> Result<WalSegment, CommitError> {
    let mut pending_acks: Vec<Sender<Result<u64, CommitError>>> = Vec::new();
    let mut broken: Option<String> = None;

    loop {
        // Drain currently-queued submissions (non-blocking) into the segment buffer.
        let drained_any = drain_into_segment(&submission_rx, &mut segment, &mut pending_acks)?;

        // If we have pending bytes AND have reached the batch threshold OR shutdown is requested,
        // flush now. Otherwise wait up to commit_window for more to arrive.
        let should_flush_now =
            segment.write_buf_len() >= config.max_batch_bytes
            || shutdown_rx.try_recv().is_ok();

        if !should_flush_now && pending_acks.is_empty() {
            // Idle: wait for the first submission. (No active batch to commit.)
            match select(&[&submission_rx, &shutdown_rx], None) { ... }
            continue;
        }

        if !should_flush_now {
            // Have pending records; wait up to commit_window for more.
            let timeout = config.commit_window;
            match select(&[&submission_rx, &shutdown_rx], Some(timeout)) { ... }
            // Drain anything that arrived, then fall through to flush.
        }

        // Flush.
        if let Some(reason) = &broken {
            for ack in pending_acks.drain(..) { let _ = ack.send(Err(CommitError::WalBroken(reason.clone()))); }
            continue;
        }
        match segment.flush_durable() {
            Ok(()) => {
                for ack in pending_acks.drain(..) { let _ = ack.send(Ok(/* the record's LSN */)); }
            }
            Err(e) => {
                let reason = e.to_string();
                broken = Some(reason.clone());
                for ack in pending_acks.drain(..) { let _ = ack.send(Err(CommitError::Segment(e.clone()))); }
            }
        }

        // If shutdown was requested AND queue is empty, exit.
        if shutdown_pending && submission_rx.is_empty() && pending_acks.is_empty() {
            return Ok(segment);
        }
    }
}
```

The exact `crossbeam_channel::select!` machinery is fiddly; I'll iterate on it in code. The shape above captures the algorithm:

1. Drain incoming submissions into the segment buffer.
2. If batch threshold hit or shutdown requested → flush.
3. Else if there are pending records, wait up to `commit_window` for more, then flush.
4. Else (no pending) → block on the receivers.
5. On flush error: mark WAL broken; all in-flight handles get `Err`; subsequent submissions also get `Err`.

### 4.5 Spec deviation log

Adding `docs/spec-deviations.md` (new file) with one entry per known deviation:

```markdown
# Spec Deviations

Track every place implementation has consciously diverged from the spec, with the rationale and the plan to reconcile.

## SD-2.4-1: Header CRC range `[0..80]` instead of spec literal `[0..76]`
- Spec: §05/02 §2 says CRC covers bytes [0..76].
- Implementation: covers [0..80].
- Reason: spec literal cuts `last_grow_at` u64 in half — almost certainly a typo.
- See: `.claude/plans/phase-02-task-04.md` §3.1.

## SD-2.3-1: Slot meta CRC range `[0..40]` instead of spec literal `[0..36]`
- Spec: §05/02 §3.2 says CRC covers bytes [0..36].
- Implementation: covers [0..40].
- Reason: spec literal cuts `last_modified_at` u64 in half.
- See: `.claude/plans/phase-02-task-03.md` §3.1.

## SD-2.8-1: No `O_DIRECT` on WAL segments
- Spec: §05/06 §2.1 mandates `O_DIRECT`.
- Implementation: plain buffered I/O + `RWF_DSYNC`.
- Reason: spec §06 §3's "pad to 4 KB on every flush" combined with §06 §2.1's `O_DIRECT` would produce zero-padded gaps mid-segment that `WalReader` (2.7) treats as `MidSegmentCorruption`. Supporting both correctly requires WAL pages (each 4 KB chunk with its own header), beyond 2.8 scope.
- See: `.claude/plans/phase-02-task-08.md` §3.1.
- Reconcile in: TBD — likely Phase 9 server wire-up when io_uring lands.

## SD-2.8-2: Synchronous `pwritev2` from a dedicated thread, not `io_uring`
- Spec: §05/06 §2.3 prescribes `io_uring` via Glommio.
- Implementation: synchronous `pwritev2` in a `std::thread`.
- Reason: Glommio hasn't been wired into the crate; pulling it in for 2.8 alone is premature.
- See: `.claude/plans/phase-02-task-08.md` §3.2.
- Reconcile in: Phase 9 (server wire-up).
```

This is the first deviation log; future tasks (2.3, 2.4) get backfilled.

### 4.6 LSN handling for the ack signal

The `AppendHandle::wait` returns the record's LSN. Where does it come from?

The submission carries the record (with its `lsn` field already set by the caller — 2.9's job). The committer extracts the LSN before adding to the batch; on successful flush, each pending ack gets its own LSN value (zipped from the order of submissions in the batch).

Implementation: `pending_acks: Vec<(Sender<...>, u64 /* lsn */)>` — store the LSN alongside the ack channel.

## 5. Files touched

- `crates/brain-storage/src/wal/segment.rs` — add `flush_durable()` + `ShortWrite` error variant + `write_buf_len()` accessor for the committer.
- `crates/brain-storage/src/wal/group_commit.rs` — new, ~450 lines including tests.
- `crates/brain-storage/src/wal/mod.rs` — re-exports.
- `crates/brain-storage/Cargo.toml` — add `crossbeam-channel.workspace = true` (already in workspace deps from Cargo.toml).
- `docs/spec-deviations.md` — new, ~50 lines.
- `docs/phases/phase-02-storage.md` — mark 2.8 done.

`crossbeam-channel` is already pinned in the workspace's `[workspace.dependencies]` (line 61 of `Cargo.toml`); just add the per-crate `crossbeam-channel.workspace = true`.

## 6. Trade-offs

| Option | Verdict | Why |
|---|---|---|
| **A. Dedicated OS thread (chosen)** | ✓ | Cleaner separation; testable via observable thread state; trivial to upgrade to a Glommio coroutine later. |
| B. Leader-follower pattern (no dedicated thread; one appender becomes the committer per batch) | ✗ | More synchronization nuance for marginal benefit. Hard to handle the 100 µs timer cleanly. |
| C. Synchronous `pwritev2` per record, no batching | ✗ | Fails the phase doc's "100 appends → ≤ 5 fsyncs" criterion; defeats the purpose. |
| **D. Drop `O_DIRECT` (chosen)** | ✓ | See §3.1. Required to keep `WalReader` happy without introducing WAL pages. |
| E. Implement WAL pages now | ✗ | Major format change; would invalidate 2.6 / 2.7. Beyond 2.8 scope. |
| **F. Synchronous `pwritev2` from the committer thread (chosen)** | ✓ | See §3.2. |
| G. Pull in Glommio now | ✗ | Premature; the crate doesn't host a runtime yet. |
| **H. `crossbeam_channel::Sender` for ack (chosen)** | ✓ | Std-compatible; appender can `.recv()` (blocking) or `.recv_timeout()`. No tokio dep. |
| I. `Arc<(Mutex, Condvar)>` hand-rolled oneshot | ✗ | More code for the same behavior. |

## 7. Risks

- **Spec deviations** (§3) — both documented in `docs/spec-deviations.md` and the module doc-comment.
- **Threading bugs.** Mutexes/channels are easy to mis-order. The plan keeps the committer thread *single*; appenders only push to channels. Each `AppendHandle` owns its `Receiver` exclusively; no shared mutable state outside the channel buffers.
- **`pwritev2` short-write.** Theoretically possible; in practice Linux returns the full count on success. We return `ShortWrite` to fail loudly rather than silently retry — a short write probably means EINTR or an interrupted device, both of which should be inspected.
- **Test for "100 appends → ≤ 5 fsyncs".** We need to count fsyncs. Approach: instrument `flush_durable` with a `#[cfg(test)]` atomic counter exposed via a test-only `flush_count()` accessor on `WalSegment`. Or count via a wrapper trait. Simpler: bump a static counter in tests.
- **Torn-write test.** A `#[cfg(test)] enum Submission::SimulateTornWriteAfter { bytes }` lets a test inject "write half the buffer, then return error" into the committer. WalReader on the resulting file should treat the partial bytes as tail truncation.
- **Shutdown race.** If a record is enqueued *just* as shutdown begins, we drain the queue before exiting. The committer's loop pattern (drain → flush → check shutdown) handles this — the drain catches anything that arrived before shutdown.
- **Drop of `GroupCommitter` without `shutdown`.** Means the committer thread keeps running, holding the segment file. Implement `Drop`: send shutdown, join the thread (best-effort; ignore errors in drop).

## 8. Test plan

All tests use `tempfile::TempDir` + 2.6's `WalSegment::create_new` + a fresh `GroupCommitter`. Read the WAL back with 2.7's `WalReader` to verify durability semantics.

### `WalSegment::flush_durable` (3)

1. Round-trip: create segment, append 1 record, `flush_durable`, reopen with `WalReader`, the record decodes.
2. `flush_durable` on an empty buffer is a no-op.
3. `bytes_on_disk` advances by exactly the buffer's pre-flush length.

### `GroupCommitter` sequential durability (2 — done-when #1)

4. Append one record, `handle.wait()` returns `Ok(lsn)`; `WalReader` decodes the record after `shutdown`.
5. Append 10 records sequentially (each `append` followed by `handle.wait()`); all 10 are durable on reopen.

### `GroupCommitter` batching (2 — done-when #2)

6. Spawn N threads, each appending a record; all `wait()` calls return `Ok`. `WalReader` sees all N records.
7. **100 concurrent appends are batched into ≤ 5 fsyncs.** Instrument `flush_durable` with a `#[cfg(test)] AtomicUsize` counter (`FLUSH_DURABLE_CALLS`); the test asserts `<= 5` after the 100 records all wait. (Realistically should be 1–3 batches given the 100 µs window and 60 KB threshold.)

### Torn-write recovery (2 — done-when #3)

8. Append 50 records, then send `SimulateTornWriteAfter { bytes: 1024 }`. The torn batch's appenders get `Err`; previously-acked records remain durable on reopen.
9. WalReader on the torn file decodes all pre-tear records, hits a Truncated/CrcMismatch at the tail of (the only) segment, terminates cleanly.

### Failure modes (2)

10. After a flush error, further `append` returns `Err(WalBroken)` and existing in-flight handles receive `Err`.
11. Drop `GroupCommitter` without `shutdown`: the thread terminates cleanly and the segment file is in a valid state (last-batch may or may not be durable; surviving records are decodable).

**Total: 11 tests.**

## 9. Estimated commit shape

One commit on `feature/brain-storage`:

> `feat(brain-storage): group commit with pwritev2(RWF_DSYNC) (sub-task 2.8)`

Body covers:
- The two spec deviations (O_DIRECT + io_uring) and their reconciliation paths.
- `WalSegment::flush_durable` + `ShortWrite` error.
- `GroupCommitter` shape, committer thread loop, batching triggers.
- New `docs/spec-deviations.md` log.
- Test count.

Files touched: as in §5.

Verify gate: `cargo fmt --all -- --check && ./scripts/check-skills.sh && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-storage --all-targets` inside the dev container.

---

PLAN READY: see `.claude/plans/phase-02-task-08.md` — confirm to proceed.
