# Sub-task 9.6a — WAL io_uring port

**Reads:** `spec/05_storage_arena_wal/06_wal_durability.md` (§2.3 io_uring prescription, §4 group commit), `docs/spec-deviations.md` SD-2.8-2 + SD-2.9-1, audit (`docs/phases/phase-09-glommio-port.md`) §8.3.
**Phase doc:** new sub-task added by the audit; lands before 9.6.
**Done when:** `brain-storage::wal` writes go through Glommio's io_uring path; the dedicated OS-thread committer is replaced with a `Task::local()` coroutine on the shard's executor; `Wal::append` becomes `async fn`. Spec deviations SD-2.8-2 and SD-2.9-1 are reconciled. SD-2.8-1 (O_DIRECT) **stays open** — explicit non-goal.

---

## 1. Why this exists

Audit §8.3 flagged it: a sync `libc::pwritev2(RWF_DSYNC)` inside a Glommio future blocks the entire shard's executor for the fsync duration. On NVMe that's 50–500 µs of stall per commit. With group commit at ~10K commits/sec, the executor is stalled ~5% of the time — every shard's tail latency takes a real hit, and the spec §16/02 targets become unreachable in synthetic benchmarks.

The audit picked option (b): port to io_uring via Glommio. This sub-task is the implementation.

**What we're *not* doing:**
- O_DIRECT (SD-2.8-1). The mid-segment 4 KB padding problem requires a WAL-page format change that's out of scope for the runtime port. Stays as a known deviation.
- Changing the WAL on-disk format. Records, segments, headers, CRC — unchanged.
- Changing the group-commit windowing (100 µs / 60 KiB triggers). Unchanged.
- Touching recovery (`WalReader`). Reads stay sync — they only run at startup, off the hot path, and `mmap`-based scans don't benefit from io_uring.

---

## 2. The runtime shift

| Layer | Before (sync) | After (Glommio) |
| ----- | ------------- | --------------- |
| `WalSegment` file handle | `std::fs::File` | `glommio::io::BufferedFile` |
| Open / create | `OpenOptions::open` + `libc::fallocate` | `BufferedFile::create` + `pre_allocate` (both async) |
| Append flush | `libc::pwritev2(RWF_DSYNC)` (one syscall) | `BufferedFile::write_at(Vec<u8>, pos).await` + `BufferedFile::fdatasync().await` (two syscalls, both io_uring) |
| Group committer | dedicated `std::thread`, `crossbeam_channel` | `glommio::Task::local`, `flume::async` |
| `Wal::append` | `fn(&mut self, …) -> Result<Lsn>` | `async fn(&self, …) -> Result<Lsn>` (interior mutability) |
| `Wal::create / open` | sync | `async` |

`BufferedFile::write_at` takes the buffer by value (`Vec<u8>`). We `std::mem::take` the segment's write buffer into the call so the kernel can keep it for the duration of the operation; on completion we get a fresh empty `Vec` back from the segment (or rehydrate from the heap — both fine, the GC happens at the executor level).

### Two-syscall fsync semantics

Spec §05/06 §2.2 prescribes `pwritev2(…, RWF_DSYNC)` as a single syscall combining write + fdatasync. Glommio's typed API doesn't expose `RWF_DSYNC` on `BufferedFile::write_at`; the equivalent is `write_at(buf).await` followed by `fdatasync().await`. **One extra syscall per batch.** At 10K commits/sec with group-commit windowing this is negligible (~100 µs/sec aggregate), and it preserves the durability guarantee: the kernel returns from `fdatasync` only when the prior writes are on stable storage. Acceptable trade; document as **SD-2.8-2-b** (refinement of the existing SD-2.8-2 — same intent, now io_uring-shaped).

Alternative (rejected): drop into raw uring submission via Glommio's underlying ring. Not exposed by a stable public API. Defer to v2 if benchmarks demand it.

---

## 3. File-by-file plan

| File | Change | Notes |
| ---- | ------ | ----- |
| `crates/brain-storage/Cargo.toml` | Add `glommio.workspace = true`, `flume.workspace = true` | Both target-gated to Linux (already implied by crate-level compile_error) |
| `crates/brain-storage/src/wal/segment.rs` | Convert `WalSegment` from `std::fs::File` to `glommio::io::BufferedFile`. All open/create/append/flush methods become `async`. | The 538-LOC source; ~50% delta. |
| `crates/brain-storage/src/wal/group_commit.rs` | Replace `std::thread` committer with `glommio::Task::local` coroutine. Replace `crossbeam_channel` with `flume`. `GroupCommitter::start` becomes async (spawns the task on the current executor). | 651 LOC source; ~60% delta. |
| `crates/brain-storage/src/wal/wal.rs` | `Wal::create / open / append / shutdown` become `async`. Interior mutability for active-segment + committer via `RefCell` (single-thread). | 598 LOC source; ~30% delta. |
| `crates/brain-storage/src/wal/reader.rs` | **Unchanged.** Reads remain sync (mmap-based, startup-only). |
| `crates/brain-storage/src/wal/checkpoint.rs` | Inspect; likely unchanged. The checkpoint metadata write may become async if it currently uses `File::write_all`. |
| `crates/brain-storage/tests/*.rs` | All WAL tests migrate to a Glommio-driven test harness (helper fn that spins a `LocalExecutor`). | ~50 tests across multiple files. Mechanical. |
| `docs/spec-deviations.md` | Update SD-2.8-2 → **Reconciled**. SD-2.9-1 → **Reconciled**. SD-2.8-1 stays open. Note the new SD-2.8-2-b (two-syscall fsync). | |

---

## 4. The `async`-on-`&self` requirement

Spec §07 §15: single-writer-per-shard. With the writer running as a Glommio task, the `Wal` is owned by the shard (single-thread), so `&self + RefCell` over the inner state is correct: the borrow checker still enforces the single-writer rule at runtime (any concurrent borrow attempt panics), and the executor's single-threaded discipline means concurrent `.await`s on `Wal::append` from the same shard never produce a Rust-level race.

The alternative — `async fn append(&mut self, …)` — would force every caller to thread `&mut Wal` through their futures, which clashes with `Arc<Wal>` sharing later in 9.7's `OpsContext`. `&self` is the right choice.

```rust
// crates/brain-storage/src/wal/wal.rs (sketch)

pub struct Wal {
    inner: RefCell<WalInner>,   // !Send; lives on one Glommio executor
}

struct WalInner {
    config: WalConfig,
    dir: PathBuf,
    shard_uuid: [u8; 16],
    next_lsn: u64,
    active_segment_seq: u64,
    committer: GroupCommitter,
}

impl Wal {
    pub async fn append(&self, record: WalRecord) -> Result<Lsn, WalError> {
        let handle = {
            let mut inner = self.inner.borrow_mut();
            let lsn = inner.next_lsn();
            // ... record framing, segment rollover check ...
            inner.committer.append(record)?
        };  // drop borrow before await
        handle.wait().await
    }
}
```

The `drop borrow before await` discipline is critical: holding a `RefCell::borrow_mut()` across `.await` would deadlock on the next concurrent append. The pattern is "borrow → enqueue → drop → await ack". Documented in the source.

---

## 5. Tests strategy

### 5.1 Test harness

Add `crates/brain-storage/src/test_util.rs` (cfg(test) only):
```rust
pub fn glommio_run<F, Fut, T>(test: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("wal-test")
        .spawn(test)
        .expect("spawn test executor")
        .join()
        .expect("test executor returned")
}
```

Each test:
```rust
#[test]
fn wal_appends_one_record() {
    glommio_run(|| async {
        // ... existing assertions, now async-aware
    });
}
```

That's the *only* mechanical churn per test. ~50 tests × ~3 lines of churn each = ~150 LOC.

### 5.2 New tests for Glommio paths

- `wal_append_does_not_block_executor` — spawn a sibling task that increments a counter every 100 µs; submit 1000 appends; assert the counter advanced ≥ 800 times during the burst (proves the executor stayed responsive during fsync). Spec §16/02 acceptance fixture in miniature.
- `wal_concurrent_appends_serialise_correctly` — three sibling tasks each appending 100 records concurrently; assert the resulting LSNs are dense and monotone (no holes, no duplicates).
- `wal_committer_task_exits_on_drop` — drop the `GroupCommitter`; assert the spawned task observes channel close and exits cleanly.

### 5.3 Container-only verification

The entire WAL test suite is already gated to Linux via `compile_error!` in `brain-storage/lib.rs`. After this sub-task they also need io_uring runtime perms — `--ulimit memlock=-1 --security-opt seccomp=unconfined` in the container. macOS host build of brain-storage will continue to fail at compile time (intentional — same as today).

---

## 6. Migration order

To keep `cargo check` green at every commit-internal step (we still ship as one commit, but the impl path should not flounder):

1. Add `glommio` + `flume` to brain-storage Cargo.toml.
2. Port `WalSegment` to BufferedFile + async ops. Leave `GroupCommitter` and `Wal` calling the new async surface from inside their existing sync code via `block_on` (panics — temporary scaffold).
3. Port `GroupCommitter::start` to a Glommio task. Inside-the-thread crossbeam usage → flume async.
4. Port `Wal::append` / `Wal::create` / `Wal::open` / `Wal::shutdown` to async. Drop the temporary `block_on` scaffolds.
5. Migrate tests. New tests for executor-responsiveness.
6. Update `docs/spec-deviations.md`.

Single commit on `feature/brain-server`.

---

## 7. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Glommio's BufferedFile mutates the buffer ownership model — `write_at(Vec<u8>, pos)` consumes the Vec | Already factored into the plan: `std::mem::take` the segment's `write_buf`; receive a fresh empty `Vec` at the call site. Re-allocate if needed; the buffer pool optimization (spec §06 §3 mentions 4 KB-aligned reuse) can come later. |
| `RefCell::borrow_mut` across `.await` panics at runtime | Documented invariant in source: "drop the borrow before awaiting the committer ack." Add an `#[cfg(debug_assertions)]` runtime check if needed. |
| Test churn explodes — many tests use `File` or `BufReader` directly to inspect WAL bytes | Read paths (`WalReader`) stay sync. Tests can read bytes via plain `std::fs::read` after the writer flushes. Only the *write* side of every test needs `glommio_run`. |
| `Wal::append` becoming async breaks downstream callers we haven't ported yet (`brain-ops`, `brain-workers`) | None of those crates *currently* call `Wal::append` (audit §6: no tokio in brain-ops src/, and brain-storage isn't imported from brain-ops). The first caller is 9.6's per-shard WAL hookup, which is *next* — designed against this async API. No regressions. |
| Bench gate from sub-task 8.14 (`no_regression`) runs without WAL — still valid | Confirmed: 8.14 uses in-memory metadata fixtures; no WAL involvement. Won't regress. |
| Hidden tokio in test helpers in brain-storage | Audit §3: brain-storage has zero tokio in src/ AND tests/. Verified during the 9.2 audit. No conflict. |

---

## 8. Sizing

- `wal/segment.rs`: ~250 LOC delta (open/create/append/flush become async; ~50% of file).
- `wal/group_commit.rs`: ~400 LOC delta (committer thread → Glommio task; channel swap).
- `wal/wal.rs`: ~200 LOC delta (async wrapping + RefCell discipline).
- Tests + harness: ~250 LOC delta (50 tests × ~3 LOC + harness + 3 new tests × ~50 LOC).
- `Cargo.toml` + `docs/spec-deviations.md`: ~15 lines.

Total: **~1100 LOC net**. Single commit, but large. Subject: `refactor(brain-storage): WAL io_uring port (sub-task 9.6a)`.

---

## 9. Verification plan

1. macOS host: still rejected at brain-storage compile time (`compile_error!` unchanged). Confirm no accidental cfg leak.
2. Linux container (io_uring perms): full `cargo test -p brain-storage` green.
3. Linux container: `cargo clippy -p brain-storage --all-targets -- -D warnings` clean.
4. Linux container: `cargo test -p brain-server` (validate brain-server still builds against the new async WAL API even though it doesn't call it yet — adding `brain-storage` to brain-server's Linux deps means we depend on the WAL surface even before 9.6 wires it).

If 9.6a takes longer than projected or hits a Glommio API surprise (e.g. `write_at` ownership semantic mismatches), STOP and surface. Don't paper over with `block_on` — that would defeat the entire sub-task.

---

## 10. Done criteria

- [ ] `WalSegment`, `GroupCommitter`, `Wal::append` all on the async io_uring path.
- [ ] No `std::thread::spawn` in brain-storage WAL code.
- [ ] No `crossbeam_channel` in brain-storage WAL code (flume only).
- [ ] No `libc::pwritev2` in brain-storage WAL code (replaced by `write_at + fdatasync`).
- [ ] All brain-storage tests pass in container with io_uring perms.
- [ ] At least one new test asserts the executor stays responsive during commit bursts.
- [ ] `docs/spec-deviations.md` SD-2.8-2 + SD-2.9-1 marked **Reconciled** with this commit's hash; new SD-2.8-2-b added (two-syscall fsync vs RWF_DSYNC).
- [ ] Phase doc 9.6a added & marked `[x]`.
- [ ] Audit doc (`phase-09-glommio-port.md`) §11 status table row "WAL group commit (io_uring port)" flipped to `done`.

---

## 11. What 9.6a explicitly doesn't do

- **O_DIRECT (SD-2.8-1)** — stays a known deviation. The mid-segment padding format change is a separate sub-task, possibly post-Phase-9.
- **Wire WAL into the shard** — that's 9.6 (next). After 9.6a, `Wal` is async-ready; 9.6 plumbs it into the shard's `Shard` struct alongside the arena.
- **Recovery** — `WalReader` stays sync. Startup recovery loop in 9.6 calls it before the executor starts taking requests.

---

*Implement on approval.*
