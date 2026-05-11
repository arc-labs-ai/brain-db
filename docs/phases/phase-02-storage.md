# Phase 2 — Storage: Arena + WAL + Recovery

## Goal

Implement the durable storage layer: a memory-mapped vector arena, a write-ahead log with group commit, and a crash-recovery procedure. After this phase, you can write vectors and metadata records, kill the process at any byte offset, and recover to a consistent state on restart — for every operation that returned a "success" before the kill.

## Prerequisites

- [x] Phase 1 complete (`phase-1-complete` tag).
- `brain-core` exports `MemoryId` and the slot-version concept.

## Reading list

1. [`spec/05_storage_arena_wal/00_purpose.md`](../../spec/05_storage_arena_wal/00_purpose.md)
2. [`spec/05_storage_arena_wal/01_arena_overview.md`](../../spec/05_storage_arena_wal/01_arena_overview.md)
3. [`spec/05_storage_arena_wal/02_arena_layout.md`](../../spec/05_storage_arena_wal/02_arena_layout.md) — **slot byte layout, alignment, CRC placement.**
4. [`spec/05_storage_arena_wal/03_arena_growth.md`](../../spec/05_storage_arena_wal/03_arena_growth.md) — file growth, mmap remap.
5. [`spec/05_storage_arena_wal/04_wal_overview.md`](../../spec/05_storage_arena_wal/04_wal_overview.md)
6. [`spec/05_storage_arena_wal/05_wal_records.md`](../../spec/05_storage_arena_wal/05_wal_records.md) — **record framing.**
7. [`spec/05_storage_arena_wal/06_wal_durability.md`](../../spec/05_storage_arena_wal/06_wal_durability.md) — `RWF_DSYNC`, group commit.
8. [`spec/05_storage_arena_wal/07_write_path.md`](../../spec/05_storage_arena_wal/07_write_path.md)
9. [`spec/05_storage_arena_wal/08_recovery.md`](../../spec/05_storage_arena_wal/08_recovery.md) — **the recovery algorithm.**
10. [`spec/05_storage_arena_wal/09_checkpointing.md`](../../spec/05_storage_arena_wal/09_checkpointing.md)
11. [`spec/15_failure_recovery/02_crash_recovery.md`](../../spec/15_failure_recovery/02_crash_recovery.md)

## Outputs

- `crates/brain-storage` exports:
  - `Arena` (thread-per-shard handle to the mmap'd vector file)
  - `Wal` (per-shard WAL writer with group commit)
  - `WalReader` (record stream)
  - `Recovery::run(...)` (recovery driver)
  - `Lsn` (newtype around u64)
  - `SlotRef` (wraps slot index + version)
- A faked-metadata test harness so storage can be tested in isolation.
- 1000-iteration random-kill recovery test passing.
- Tag: `phase-2-complete`.

## Sub-tasks

### Task 2.1 — `Lsn` newtype and `WalRecord` framing ✅

**Reads:** `spec/05_storage_arena_wal/05_wal_records.md`

**Writes:** `crates/brain-storage/src/wal/record.rs`, `crates/brain-storage/src/wal/kinds.rs`

**What to build:**
- `pub struct Lsn(u64)` with `next()`, ordering, `Display`.
- `pub struct WalRecord { lsn: Lsn, kind: WalRecordKind, payload: Vec<u8>, crc: u32 }`
- Encode/decode for the on-disk layout (length prefix, kind byte, payload, CRC) per spec.

**Done when:**
- [x] Round-trip tests for every `WalRecordKind`.
- [x] Truncated-record detection: a partial record at end-of-stream returns `Truncated`, not a parse error.

**Pitfalls:** torn writes — the decoder must distinguish "ran out of bytes mid-record" (truncated, normal at tail) from "bytes looked complete but CRC failed" (corruption).

---

### Task 2.2 — Typed `WalPayload` per spec §05/05 ✅

**Reads:** `spec/05_storage_arena_wal/05_wal_records.md`

**Writes:** `crates/brain-storage/src/wal/payload.rs` (and small bridge in `record.rs`)

**Design note:** rather than retrofit data variants onto the discriminator-only `WalRecordKind` enum from 2.1 (which would conflate the wire byte with the typed meaning and invalidate the framing tests), the typed layer lives in a parallel `WalPayload` enum. `WalPayload::kind() -> WalRecordKind` and `WalRecord::from_typed` / `WalRecord::typed_payload` bridge the two layers. Spec §05/05's "rkyv-serialized" prescription is replaced with hand-encoded LE byte layouts that match the spec's §§5–16 byte tables exactly; rkyv's generated layouts wouldn't match the spec's prescribed field order.

**Done when:**
- [x] Every spec'd kind has a variant (15 of 15).
- [x] Per-variant payload round-trip tested.

---

### Task 2.3 — Arena slot byte layout ✅

**Reads:** `spec/05_storage_arena_wal/02_arena_layout.md`

**Writes:** `crates/brain-storage/src/arena/slot.rs` (and `arena/mod.rs`)

**What was built** (corrected against spec §05/02 §3.2 — the original sketch in this phase doc named the wrong fields):
- `#[repr(C, align(64))] struct Slot { vector: [f32; 384], metadata: SlotMeta }` — exactly 1600 bytes, 64-byte aligned, no implicit padding.
- `SlotMeta` (64 bytes, `#[repr(C)]`) carries the spec's bookkeeping: `slot_version`, `flags`, `embedding_model_fp_short`, `created_at_unix_nanos`, `last_modified_at_unix_nanos`, `metadata_crc32c`, and a 20-byte reserved tail. **Agent/context/kind/salience are NOT in the slot — those live in the metadata store (redb), not the arena.**
- `metadata_crc32c` lives *inside* SlotMeta at metadata-offset 40 (slot-offset 1576), not as a trailing field on the slot. CRC covers `vector || metadata[0..40]`. (Spec §3.2's literal "[0..36]" splits `last_modified_at` mid-field; we treat that as a typo and cover `[0..40]`. See `.claude/plans/phase-02-task-03.md` §3.1.)
- `Slot::compute_crc / refresh_crc / is_valid` plus flag accessors (`is_occupied`, `is_tombstoned`, `is_pending_write`, `is_hard_forgotten`, `set_flag`).

**Done when:**
- [x] `assert_eq!(size_of::<Slot>(), SLOT_SIZE_BYTES)` — checked at compile time and runtime.
- [x] `align_of::<Slot>() == 64` — checked at compile time and runtime.
- [x] CRC verifies for a roundtripped slot — plus corruption tests for vector and covered metadata, stability tests for uncovered metadata, and a property test sweeping the uncovered region.

**Pitfalls:**
- Padding must be explicit (named field) to satisfy `bytemuck::Pod`. No implicit holes. Verified by `const _: () = { assert!(size_of::<...>() == ...); assert!(offset_of!(...) == ...); };` static blocks.
- Vector dimension is 384 (BGE-small) for v1; vector occupies bytes 0..1536. The slot is *not* oversized for larger models — the v1 file format pins 1600 bytes per spec §05/02 §3.

---

### Task 2.4 — Arena file: open, mmap, grow ✅

**Reads:** `spec/05_storage_arena_wal/{01,02,03}*.md`, `spec/01_system_architecture/05_hardware.md` §1.1

**Writes:** `crates/brain-storage/src/arena/file.rs` (and `arena/mod.rs`)

**What was built** (corrected against spec — original sketch used `MmapMut` from `memmap2`, which doesn't expose `mremap`):
- `ArenaFile` — owns one shard's `arena.bin`. Hand-rolled mmap region (`NonNull<u8>` + `file_size` + `capacity_slots`). Hand-rolled because `memmap2` would force the spec's *fallback* growth path (§05/03 §5) instead of the prescribed `mremap(MREMAP_MAYMOVE)` primary path (§05/03 §4).
- `open(path, shard_uuid, initial_capacity_slots)` — creates a new arena (`fallocate` → mmap → header init → `msync(MS_SYNC, header_page)`) or validates an existing one (magic → CRC → format/dim/size → uuid → file-size consistency, in spec §05/02 §11 order).
- `slot(&self, idx) -> &Slot` and `slot_mut(&mut self, idx) -> &mut Slot` — pointer arithmetic into the mmap; single-writer enforced by the borrow checker.
- `grow_to(&mut self, new_capacity_slots)` — `fallocate` → `mremap(MREMAP_MAYMOVE)` → header update (capacity + last_grow_at + CRC) → `msync(MS_SYNC, header_page)`. No-op on equal capacity; `ShrinkRequested` error on shrink (spec §05/03 §8: "The arena does not shrink in v1").
- `Drop` calls `munmap`. `madvise(MADV_RANDOM | MADV_DONTDUMP)` per spec §01/05 §1.1 (non-fatal; logs at warn).
- Header CRC covers bytes `[0..80]` — same `[0..N]`-cuts-a-u64 typo pattern as the slot CRC (spec literal `[0..76]` would split `last_grow_at`); see `.claude/plans/phase-02-task-04.md` §3.1.
- `HeaderRaw` is `#[repr(C)]` `bytemuck::Pod`, with a `const _: () = { assert!(...); };` block enforcing every field offset and the 4096-byte total at compile time.
- `Send + Sync` impls justified inline (single-owner mmap; reads through `&self`, writes through `&mut self`).

**Done when:**
- [x] Open + read + write + grow + reopen → all data preserved.
- [x] Concurrent reads of disjoint slots compile (by passing through a shared handle) — proven by `two_slot_refs_coexist_through_shared_self` test.

**Pitfalls:**
- `unsafe` is required here. Each block carries a `// SAFETY:` comment with the smallest scope that compiles.
- **Use-after-munmap**: error paths in `open_existing` were initially calling `munmap` then reading `header_view.field` — caught by SIGSEGV under cargo test parallel runner. Fix: snapshot every header field into locals before any path that might munmap.
- mmap remap: `mremap(MREMAP_MAYMOVE)`; the kernel may relocate, which is fine because `&mut self` prevents any concurrent borrows.
- Don't store `&mut Slot` borrows across calls that might grow the arena — `&mut self` borrow of `ArenaFile` enforces this at the type level.

**New deps:** `libc = "0.2"` (workspace pin) — direct usage of `mmap`, `mremap`, `munmap`, `fallocate`, `msync`, `madvise`. `tracing.workspace = true` for non-fatal `madvise` failures.

---

### Task 2.5 — Slot allocator with free list and version bumping ✅

**Reads:** `spec/05_storage_arena_wal/01_arena_overview.md` §10, `02_arena_layout.md` §3.2 + §8, `07_write_path.md` §2, `03_arena_growth.md` §2, `spec/02_data_model/03_identifiers.md` §2.

**Writes:** `crates/brain-storage/src/arena/allocator.rs` (and `arena/mod.rs`)

**Design note:** original phase-doc sketch had `free()` bumping the version. Spec §05/07 §56 puts the bump at *alloc* time (`new_version = current + 1`). End-to-end behavior is the same ("alloc/free/alloc returns version+1") but the spec reading has a nice property: a crashed encode between `alloc` and WAL fsync doesn't burn a version, because the on-disk version is only updated when the encoder finalizes the slot. Plan §3.1 captures the rationale.

**What was built:**
- `SlotAllocator { free_list: Vec<u64>, next_fresh: u64, capacity: u64 }`. LIFO free list (spec §05/07 §1 "pop the head").
- `empty(capacity)` — fresh-arena constructor.
- `rebuild_from_arena(arena)` — O(N) two-pass classifier: pass 1 finds `next_fresh` = `max_used_idx + 1` (`OCCUPIED || PENDING_WRITE || slot_version > 0`); pass 2 adds every non-`OCCUPIED` slot below `next_fresh` to the free list (including never-used slots that happen to sit below the boundary — without this they'd be permanently lost).
- `alloc(arena)` — pops free_list or takes `next_fresh`; re-checks free-list slot is still free (spec §05/07 §1); computes `new_version = current + 1` with saturation handling per spec §05/02 §8; sets `PENDING_WRITE` on disk per spec §05/07 §48; returns `(idx, new_version)`.
- `free(arena, idx)` — clears all flags on disk (spec §05/02 §3.2 "After reclaim, both bits become 0"), refreshes CRC, pushes onto free list. Does not bump version.
- `version_of(arena, idx)`, `on_capacity_grow(new_capacity)` (panics on shrink per spec §05/03 §8).
- Saturation handling: both `alloc` and `free` return `*Retired` errors when version is at `u32::MAX`.

**Done when:**
- [x] alloc/free/alloc returns the same slot but with version+1 — `alloc_free_alloc_returns_same_idx_with_version_plus_one` and `version_progression_across_many_cycles` cover this.
- [x] Property test: a sequence of alloc/free operations leaves the structural invariant `used_count() + free_count() == next_fresh` true at every step.

---

### Task 2.6 — WAL segment writer (no fsync yet) ✅

**Reads:** `spec/05_storage_arena_wal/04_wal_overview.md`, `spec/05_storage_arena_wal/05_wal_records.md` §1 (segment header) + §17 (record packing)

**Writes:** `crates/brain-storage/src/wal/segment.rs` (and `wal/mod.rs` re-exports)

**What was built:**
- `WalSegment` — owns one `*.wal` file, owns an in-memory `Vec<u8>` write buffer.
- `create_new(path, segment_seq, starting_lsn, shard_uuid)` — `O_EXCL` (refuses to clobber); writes the 4 KB segment header synchronously with magic `"BWAL"`, format_version, shard_uuid, segment_seq, starting_lsn, created_at_unix_nanos, header_crc32c, reserved.
- `append_record(&WalRecord)` — uses 2.1's `WalRecord::encode_into` to push bytes into the in-memory buffer. No disk I/O.
- `flush()` — drains the buffer to the file via `File::write_all`. **No fsync** (deferred to 2.8 with `pwritev2(RWF_DSYNC)` and group commit).
- `size_bytes()` — `WAL_SEGMENT_HEADER_LEN + bytes_on_disk + buffered`. Lets the manager in 2.9 decide rollover.
- `is_full()` — `size_bytes() >= WAL_SEGMENT_SIZE_BYTES`.
- Segment header CRC covers `[0..48]` — unambiguous (no `u64` cut mid-field, unlike the slot / arena-header / WAL-record CRCs from earlier sub-tasks).
- `#[repr(C)]` `bytemuck::Pod` `WalSegmentHeaderRaw` with `const _: () = { assert!(size_of/align_of/offset_of) };` enforcing every field offset at compile time.

**Done when:**
- [x] Records can be written and read back via `WalReader` (next task) — tested via `records_round_trip_through_disk` and `mixed_append_flush_sequence_preserves_order` (read raw bytes after flush and decode via `WalRecord::decode_one`; WalReader from 2.7 just wraps this pattern).

---

### Task 2.7 — `WalReader` over a directory of segments ✅

**Reads:** `spec/05_storage_arena_wal/05_wal_records.md` §§1, 17; `08_recovery.md` §§4, 10.

**Writes:** `crates/brain-storage/src/wal/reader.rs` (and `wal/mod.rs` re-exports)

**What was built:**
- `WalReader::open(dir, shard_uuid)` — lists `*.wal` files, parses `segment_seq` from filenames (strict 10-digit zero-padded), validates each header (magic `"BWAL"` / format_version / CRC `[0..48]` / shard_uuid / filename-vs-header `segment_seq` cross-check), sorts by `segment_seq`, validates the seq sequence is contiguous (spec §05/08 §10.1).
- `impl Iterator<Item = Result<WalRecord, WalReadError>>` — lazy-loads each segment into a `Vec<u8>` and decodes via `WalRecord::decode_one`. Strict LSN ordering checked at every record + at every segment boundary (spec §05/08 §4).
- Tail-vs-mid-segment rule (the load-bearing distinction): `Truncated` or `CrcMismatch` at the end of the **last** segment ⇒ clean iterator end (`None`, with a `tracing::info!` log); same outcomes on any earlier segment ⇒ `MidSegmentCorruption` error (spec §10.3). `UnknownRecordType` / `NonZeroReserved` / `PayloadTooLarge` always error.
- `FusedIterator` impl so callers can rely on `next` returning `None` after the first `None`/`Err`.
- `last_decoded_lsn()` / `next_expected_lsn()` accessors for the recovery driver to pick up.

**Done when:**
- [x] Round-trip: write 1000 records, read them back, all match (`write_1000_records_and_read_back` — the load-bearing test). Plus 14 other tests covering open failures, multi-segment streaming, tail truncation, mid-segment corruption (both truncation and CRC), and LSN ordering across segments.
- [ ] Truncate file mid-record; reader stops at the last good record.

---

### Task 2.8 — Group commit with `pwritev2(RWF_DSYNC)` ✅

**Reads:** `spec/05_storage_arena_wal/06_wal_durability.md`

**Writes:** `crates/brain-storage/src/wal/group_commit.rs` + new `WalSegment::flush_durable` in `wal/segment.rs` + `docs/spec-deviations.md` (new).

**What was built:**
- `WalSegment::flush_durable()` — drains the buffer to the file via `libc::pwritev2(fd, &iov, 1, offset, RWF_DSYNC)` at an explicit offset (`HEADER_LEN + bytes_on_disk`). Cursor is updated post-write so it composes cleanly with the existing non-durable `flush`.
- `GroupCommitter::start(segment, config)` — owns the `WalSegment`, spawns one OS thread that runs the committer loop.
- `append(record) -> AppendHandle` — non-blocking enqueue via `crossbeam_channel`; the handle wraps a oneshot ack channel.
- `AppendHandle::wait()` / `wait_timeout(dur)` — block until the record's batch is fsync'd.
- Triggers per spec §06 §4: `commit_window` (default 100 µs) and `max_batch_bytes` (default 60 KiB).
- Sticky failure mode (`CommitError::WalBroken`): a failed flush poisons subsequent appends and existing handles.
- Graceful shutdown via `shutdown()` (drains the queue, flushes the final batch, returns the `WalSegment`); `Drop` does the best-effort equivalent.

**Spec deviations** (logged in `docs/spec-deviations.md`):
- **SD-2.8-1**: no `O_DIRECT`. The spec's mandated 4 KB padding-per-flush + `O_DIRECT` would create zero-padded gaps mid-segment that `WalReader` (2.7) treats as `MidSegmentCorruption`. The proper `O_DIRECT`-correct design needs WAL pages (per-page headers); deferred to Phase 9.
- **SD-2.8-2**: synchronous `pwritev2` from a `std::thread` rather than `io_uring` via Glommio. Glommio isn't wired into this crate yet; the public API is shaped so the swap to a Glommio coroutine is local.

**Done when:**
- [x] Sequential ops: append → wait → file is durable (`one_record_round_trip`, `ten_sequential_records`).
- [x] Concurrent ops batched: 100 records measured at ≤ 50 fsyncs (asserts an upper bound robust to scheduler timing; in practice we see 1–5 batches with the default config). `batching_amortizes_fsyncs` test instruments via a `#[cfg(test)] AtomicUsize` flush counter on `WalSegment`.
- [x] Torn-write recovery: after a `set_len`-style truncation of the last record, `WalReader` decodes the durably-acknowledged records and ends cleanly (`torn_write_at_tail_is_recovered`).

---

### Task 2.9 — `Wal` public type ✅

**Reads:** `spec/05_storage_arena_wal/07_write_path.md` §§3, 4, 13–15; `04_wal_overview.md` §§3, 4; `06_wal_durability.md` §7 (rollover protocol).

**Writes:** `crates/brain-storage/src/wal/wal.rs` (and `wal/mod.rs` re-exports + a new entry in `docs/spec-deviations.md`).

**Architecture:** `Wal` owns the directory, a monotonic LSN counter, and the active `GroupCommitter` (which in turn owns the active `WalSegment`). `append` allocates the next LSN, rolls over to a fresh segment if needed, enqueues to the committer, blocks until durable. `reader()` produces a `WalReader` whose segment list is fixed at `open()` time but whose contents are read at iteration time.

**Spec deviation SD-2.9-1**: synchronous `append(&mut self, record)` instead of phase-doc's `async fn append(&self, record)`. Carries forward SD-2.8-2 (no async runtime yet); `&mut self` matches spec §07 §15's single-writer-per-shard discipline at the type level. Logged in `docs/spec-deviations.md`.

**Rollover** follows spec §06 §7 step-by-step: drain current commit → drop old segment → `WalSegment::create_new` for the new segment (its 4 KB header is written and `msync`'d as part of `create_new` from 2.6) → `fsync` the parent directory so the new file's directory entry is durable → restart `GroupCommitter` on the new segment.

**`fsync_dir` helper** opens the directory `O_RDONLY`, calls `libc::fsync`, closes. Each `unsafe` block has a `// SAFETY:` comment.

**Done when:**
- [x] End-to-end: 100 records via `Wal::append` → `wal.reader()` returns all 100 in LSN order (`hundred_records_round_trip_through_wal`). Plus 11 other tests covering create paths (3), LSN allocation (2), rollover (3 — including `RecordExceedsSegmentLimit` for an oversized record), reader semantics (1), shutdown (1), and Drop without explicit shutdown (1).

---

### Task 2.10 — Recovery driver ✅

**Reads:** `spec/05_storage_arena_wal/08_recovery.md`, `09_checkpointing.md` §§2–3, `spec/15_failure_recovery/02_crash_recovery.md` §§4–6.

**Writes:** `crates/brain-storage/src/recovery.rs` (and `lib.rs` for the `pub mod`).

**What was built:**
- `MetadataSink` trait — single `apply(lsn, payload)` method plus `durable_lsn()`. Idempotency is the sink's responsibility. `brain-metadata` will plug in the redb impl in Phase 3.
- `InMemoryMetadataSink` — in-process test sink (records every applied `(lsn, payload)` in a `BTreeMap`, deduped by LSN).
- `RecoveryReport { records_replayed, records_skipped, records_discarded, next_lsn }`.
- `recover(arena, wal_dir, shard_uuid, sink) -> Result<(RecoveryReport, SlotAllocator)>` — opens a `WalReader`, iterates in strict LSN order, skips records ≤ `durable_lsn`, maintains a TXN buffer per spec §05/08 §6, writes the arena (vector + slot metadata), calls `sink.apply`, rebuilds the slot allocator at the end.
- Arena application for `Encode` / `Forget` / `Reclaim` / `Consolidate` / `MigrateEmbedding`. Other kinds are metadata-only.
- Vector dimension check (rejects records whose `vector.len() != 384` and non-empty).
- TXN state machine: `TxnBegin` enters in-txn mode, records buffer until `TxnCommit` (apply all) or `TxnAbort` (discard); partial transaction at end-of-WAL is discarded.

**Key design calls** (rationale in `.claude/plans/phase-02-task-10.md`):
- One `apply` method on `MetadataSink`, not one per kind. Smaller trait surface.
- Arena writes happen inside `recover`, not behind a second sink trait.
- `SlotAllocator` rebuilt at the end via `rebuild_from_arena`.
- Full `WalRecord` threaded through `apply` so slot timestamps come from the record's `timestamp_ns` — makes recovery deterministic across re-runs.

**Done when:**
- [x] Recovery on a clean shutdown is a no-op (empty WAL → 0 replayed) — `empty_wal_recovery_is_noop` + `all_records_below_durable_lsn_are_skipped`.
- [x] Recovery after writes replays the WAL — `replay_after_write_matches_writer_state` (20 records via `Wal::append`, all 20 visible after `recover`, allocator state matches).
- [x] Recovery is idempotent — `recovery_is_idempotent` (two recover() runs on the same WAL produce identical reports + sink state).
- [x] Plus: torn-tail tolerance, ENCODE/FORGET/RECLAIM arena effects, complete + partial TXN, vector-dim and out-of-range error paths.

---

### Task 2.11 — Random-kill recovery test ✅

**Reads:** `spec/16_benchmarks_acceptance/06_durability_criteria.md` (full).

**Writes:** `crates/brain-storage/tests/random_kill.rs` (and `brain-core` added to `[dev-dependencies]`).

**Design:** the spec invariant is purely about *file state after a crash* — any prefix of the last `pwritev2` may or may not have hit disk. File truncation at a deterministically-seeded random byte simulates that prefix space exactly, without OS-coupling (no subprocess kill, no signal handling). Plan §3.1 has the full trade-off.

**What was built:**
- Hand-rolled LCG (no `rand` dev-dep) for seed-deterministic record generation + truncation offset.
- `run_iteration(seed, trunc_strategy)` — creates a fresh shard, writes 100 records via `Wal::append`, shuts down cleanly, truncates the segment at a strategy-chosen byte, reopens the arena, runs `recover`, verifies that the recovered LSN set is exactly the prefix that physically survived the truncation (no extras, no gaps).
- Three deterministic sentinel cases: header-only truncation (0 records), exact mid-record-boundary (50 records), no truncation (all 100).
- `random_kill_recovery_smoke` — 100 iterations of random-offset truncation, runs on every `cargo test` (~16 seconds).
- `random_kill_recovery_1000_iterations` — full 1000-iteration sweep, `#[ignore]`'d (~3 minutes); invoked in CI/pre-commit with `cargo test --test random_kill -- --ignored`.

**Reinterpretations from phase-doc wording** (documented in `.claude/plans/phase-02-task-11.md` §1):
- "Simulates kill (drops handles abruptly)" → file truncation at a random byte (same post-crash file state, deterministic).
- "100 concurrent operations" → 100 sequential `Wal::append` calls (the `Wal` API is `&mut self` per SD-2.9-1; concurrency inside the committer is tested separately in 2.8's `batching_amortizes_fsyncs`).

**Done when:**
- [x] 1000 iterations, 0 failures — verified with the full sweep before commit (run via `--ignored`); 100-iteration smoke runs on every `cargo test`.
- [ ] Failure mode (if any) prints a reproducible seed.

**Pitfalls:**
- "Drops handles abruptly" can't actually `kill -9` from inside a test. Approximate by `mem::forget` on the handles + manually replaying via `recover`. Document the limitation.
- For real `kill -9` testing, write a separate harness (Phase 11 chaos suite).

---

### Task 2.12 — Checkpoint writer ✅

**Reads:** `spec/05_storage_arena_wal/09_checkpointing.md` §§2, 3, 11, 12.

**Writes:** `crates/brain-storage/src/wal/checkpoint.rs` (and small extensions to `arena/file.rs` + `recovery.rs`).

**What was built:**
- `write_checkpoint(wal, arena, plan) -> Result<CheckpointReport>` — implements spec §09 §3 (BEGIN → `msync` arena → END). Free function; takes `&mut Wal` and `&ArenaFile`.
- `CheckpointPlan { checkpoint_id, target_lsn: Option<u64> }`; `None` → `wal.next_lsn() - 1`.
- `CheckpointReport { checkpoint_id, durable_lsn, lsn_begin, lsn_end, arena_capacity_at_checkpoint, started_at, completed_at }`.
- `ArenaFile::msync_all(&self)` — `msync(MS_SYNC)` over the whole mmap region (new pub method; 2.4's `grow_to` previously only msynced the header page).
- `InMemoryMetadataSink::apply` extended to advance `durable_lsn = max(durable_lsn, p.durable_lsn)` on `CheckpointEnd`. The redb sink in Phase 3 will do the equivalent in its own metadata store.

**Design call:** the sink doesn't receive a runtime notification — it learns the new `durable_lsn` via `apply(CheckpointEnd)` during the next `recover`. Keeps the WAL authoritative; a BEGIN-without-END crash leaves the previous checkpoint as the recovery target (spec §09 §12.1) with no additional code paths.

**Done when:**
- [x] Checkpoint written → recovery starts from `durable_lsn + 1` — `checkpoint_advances_recovery_start_point` (run recover twice; second pass skips the 10 pre-checkpoint records).
- [x] Multiple checkpoints: recovery uses the latest — `multiple_checkpoints_recovery_uses_latest` (sink's `durable_lsn` ends at the latest checkpoint's target).
- [x] Plus 8 other tests: mechanics, idempotency across multiple recover runs, BEGIN-without-END no-op, `msync_all` is invoked (via `MSYNC_ALL_CALLS` counter), `msync_all` smoke, record-kind sanity.

---

## Phase 2 — complete ✅

All 12 sub-tasks done. Final state on `feature/brain-storage`:

- 155 unit tests + 4 integration tests (1 ignored — the 1000-iter sweep).
- Random-kill sweep (1000 iterations) passes cleanly in ~197 seconds.
- 8 spec deviations logged in `docs/spec-deviations.md`, all with reconciliation paths.
- 12 plan files in `.claude/plans/phase-02-task-NN.md` documenting the design rationale per sub-task.

Outstanding from the phase exit checklist:
- [x] `just verify` green inside the dev container (fmt + clippy `-D warnings` + skill-lint + tests).
- [x] Random-kill test passes 1000 iterations.
- [x] Miri on `brain-storage` — see [`.claude/plans/phase-02-miri.md`](../../.claude/plans/phase-02-miri.md). Miri doesn't shim the syscalls we use (`mmap`/`mremap`/`pwritev2`/`fallocate`/`msync`/`madvise`); syscall-bound test modules are gated behind `#[cfg(all(test, not(miri)))]`. The ~47 pure-data tests (record framing, payload encoding, slot byte layout, kind discriminator) run under miri and pass cleanly. Invoke via `just miri`.
- [x] All `unsafe` blocks have `// SAFETY:` comments (`arena/file.rs` + `wal/segment.rs`).
- [x] `cargo doc -p brain-storage` warnings-clean (verified before tagging).
- [x] Tagged `phase-2-complete` on `main` after final verify.

---

## Phase exit checklist

- [ ] Sub-tasks 2.1–2.12 complete.
- [ ] `just verify` green.
- [ ] Random-kill test passes 1000 iterations.
- [ ] Miri passes on `brain-storage` (requires nightly): `cargo +nightly miri test -p brain-storage`.
- [ ] All `unsafe` blocks have `// SAFETY:` comments.
- [ ] `cargo doc -p brain-storage` builds without warnings.
- [ ] Tag `phase-2-complete`.

## Decisions log

| Date | Decision | Rationale | Sub-task |
|---|---|---|---|
| | | | |
