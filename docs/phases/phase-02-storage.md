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

### Task 2.1 — `Lsn` newtype and `WalRecord` framing

**Reads:** `spec/05_storage_arena_wal/05_wal_records.md`

**Writes:** `crates/brain-storage/src/wal/record.rs`

**What to build:**
- `pub struct Lsn(u64)` with `next()`, ordering, `Display`.
- `pub struct WalRecord { lsn: Lsn, kind: WalRecordKind, payload: Vec<u8>, crc: u32 }`
- Encode/decode for the on-disk layout (length prefix, kind byte, payload, CRC) per spec.

**Done when:**
- [ ] Round-trip tests for every `WalRecordKind`.
- [ ] Truncated-record detection: a partial record at end-of-stream returns `Truncated`, not a parse error.

**Pitfalls:** torn writes — the decoder must distinguish "ran out of bytes mid-record" (truncated, normal at tail) from "bytes looked complete but CRC failed" (corruption).

---

### Task 2.2 — `WalRecordKind` enum

**Reads:** `spec/05_storage_arena_wal/05_wal_records.md`

**Writes:** `crates/brain-storage/src/wal/kinds.rs`

**What to build:**
- One variant per record type per spec: `EncodeMemory`, `Tombstone`, `LinkEdge`, `UnlinkEdge`, `Checkpoint`, `SlotReclaim`, etc.
- Each carries the spec's payload schema (rkyv-serialized).

**Done when:**
- [ ] Every spec'd kind has a variant.
- [ ] Per-variant payload round-trip tested.

---

### Task 2.3 — Arena slot byte layout

**Reads:** `spec/05_storage_arena_wal/02_arena_layout.md`

**Writes:** `crates/brain-storage/src/arena/slot.rs`

**What to build:**
- `#[repr(C)] struct Slot { vector: [f32; 384], metadata: SlotMeta, padding: [u8; N], crc: u32 }` — exact layout per spec, total 1600 bytes, 64-byte aligned.
- (Or 1536 if dimensions differ — confirm with spec; current default is 384 for BGE-small.)
- `SlotMeta` carries: agent_id (16B), context_id (16B), kind (1B), salience (4B), flags (1B), reserved bytes.
- `Slot::compute_crc(&self) -> u32` — CRC over all bytes except the CRC field.
- `Slot::is_valid(&self) -> bool`.

**Done when:**
- [ ] `assert_eq!(size_of::<Slot>(), SLOT_SIZE_BYTES)`.
- [ ] `align_of::<Slot>() == 64`.
- [ ] CRC verifies for a roundtripped slot.

**Pitfalls:**
- Padding must be explicit (named field) to satisfy `bytemuck::Pod`. No implicit holes.
- Check the vector dimension against the spec: BGE-small is 384, but the slot is sized for forward-compatibility with larger models. If dim < 384·4 = 1536, the rest is reserved.

---

### Task 2.4 — Arena file: open, mmap, grow

**Reads:** `spec/05_storage_arena_wal/03_arena_growth.md`

**Writes:** `crates/brain-storage/src/arena/file.rs`

**What to build:**
- `pub struct ArenaFile { mmap: MmapMut, file: File, capacity_slots: usize }`
- `fn open(path, initial_capacity_slots) -> Result<Self>` — creates if missing, mmaps.
- `fn slot(&self, idx: SlotIndex) -> &Slot` and `slot_mut`.
- `fn grow(&mut self, new_capacity_slots) -> Result<()>` — resizes file, remaps. Spec defines the growth policy (e.g. doubling).

**Done when:**
- [ ] Open + read + write + grow + reopen → all data preserved.
- [ ] Concurrent reads of disjoint slots compile (by passing through a shared handle).

**Pitfalls:**
- `unsafe` is required here. Each block needs a `// SAFETY:` comment.
- mmap remap on Linux: prefer `mremap(2)` if the platform allows; otherwise unmap + remap. Spec may pin one.
- Don't store `&mut Slot` borrows across calls that might grow the arena — they'd dangle.

---

### Task 2.5 — Slot allocator with free list and version bumping

**Reads:** `spec/05_storage_arena_wal/02_arena_layout.md` + `spec/02_data_model/03_identifiers.md`

**Writes:** `crates/brain-storage/src/arena/allocator.rs`

**What to build:**
- `pub struct SlotAllocator { ... }` — owns the free list and the next-fresh-slot pointer.
- `fn alloc(&mut self) -> SlotIndex` — pops free list, else returns next-fresh.
- `fn free(&mut self, idx: SlotIndex)` — pushes onto free list and bumps the slot's version field.
- `fn version_of(&self, idx: SlotIndex) -> SlotVersion`.

**Done when:**
- [ ] alloc/free/alloc returns the same slot but with version+1.
- [ ] Property test: a sequence of alloc/free operations leaves `total_slots == fresh_alloc_count` invariant.

---

### Task 2.6 — WAL segment writer (no fsync yet)

**Reads:** `spec/05_storage_arena_wal/04_wal_overview.md`, `spec/05_storage_arena_wal/05_wal_records.md`

**Writes:** `crates/brain-storage/src/wal/segment.rs`

**What to build:**
- `pub struct WalSegment { file: File, offset: u64, segment_id: u64 }`
- `fn append(&mut self, record: &WalRecord) -> Result<()>` — writes bytes (no sync).
- `fn flush(&mut self)` — calls `write_all`, no sync.
- Segment rollover at `WAL_SEGMENT_SIZE_BYTES`.

**Done when:**
- [ ] Records can be written and read back via `WalReader` (next task).

---

### Task 2.7 — `WalReader` over a directory of segments

**Reads:** `spec/05_storage_arena_wal/05_wal_records.md`, `spec/05_storage_arena_wal/08_recovery.md`

**Writes:** `crates/brain-storage/src/wal/reader.rs`

**What to build:**
- `pub struct WalReader { ... }` — opens all segments in a directory, sorts by ID.
- `impl Iterator for WalReader { type Item = Result<WalRecord> }` — streams records in LSN order.
- Handles truncated tail: if the last record is partial, the iterator ends cleanly (not as an error).

**Done when:**
- [ ] Round-trip: write 1000 records, read them back, all match.
- [ ] Truncate file mid-record; reader stops at the last good record.

---

### Task 2.8 — Group commit with `pwritev2(RWF_DSYNC)`

**Reads:** `spec/05_storage_arena_wal/06_wal_durability.md`

**Writes:** `crates/brain-storage/src/wal/group_commit.rs`

**What to build:**
- A queue of pending records, each tied to an oneshot channel for "your fsync is done."
- Single committer task: drains the queue periodically (or when full), calls `pwritev2` with `RWF_DSYNC`, signals all waiters.
- Use `nix` or raw libc for the syscall (and confirm with spec which is preferred).

**Done when:**
- [ ] Sequential ops: append → wait → file is durable.
- [ ] Concurrent ops: 100 appends batched into ≤ 5 fsyncs; all complete with success.
- [ ] Crash test: kill mid-batch, reopen, only records that signaled completion are visible.

**Pitfalls:**
- `RWF_DSYNC` requires kernel ≥ 4.7 (which is fine for any supported target).
- Group commit window: spec may pin a max latency (e.g. 5ms). Implement a configurable window.

---

### Task 2.9 — `Wal` public type

**Reads:** `spec/05_storage_arena_wal/07_write_path.md`

**Writes:** `crates/brain-storage/src/wal/mod.rs`

**What to build:**
- `pub struct Wal { ... }` — single public handle composing segment writer + group commit + reader.
- `pub async fn append(&self, record: WalRecord) -> Result<Lsn>` — returns the LSN once durable.
- `pub fn reader(&self) -> WalReader`.

**Done when:**
- [ ] End-to-end: write through `Wal::append` → read via `wal.reader()` returns the record.

---

### Task 2.10 — Recovery driver

**Reads:** `spec/05_storage_arena_wal/08_recovery.md`, `spec/15_failure_recovery/02_crash_recovery.md`

**Writes:** `crates/brain-storage/src/recovery.rs`

**What to build:**
- `pub fn recover(arena_path, wal_dir, metadata_sink: &mut impl MetadataSink) -> Result<RecoveryReport>`
- Algorithm per spec:
  1. Open arena and metadata-sink.
  2. Read last checkpoint marker → `durable_lsn`.
  3. Replay WAL records with `lsn > durable_lsn`; for each, apply to metadata-sink (idempotent) and update arena slots.
  4. Stop on torn-tail (acceptable) or CRC failure (halt with `Corruption`).
- `MetadataSink` is a trait; real impl in Phase 3, fake impl here for testing.

**Done when:**
- [ ] Recovery on a clean shutdown is a no-op (no records past durable_lsn).
- [ ] Recovery after a kill replays the WAL and matches the pre-kill state.
- [ ] Recovery is idempotent (running twice produces the same state).

---

### Task 2.11 — Random-kill recovery test

**Reads:** `spec/16_benchmarks_acceptance/06_durability_criteria.md`

**Writes:** `crates/brain-storage/tests/random_kill.rs`

**What to build:**
- Test that:
  1. Spins up a `Wal` and an arena.
  2. Issues N concurrent operations.
  3. At a random byte offset within the run, simulates kill (drops handles abruptly).
  4. Reopens via `recover(...)`.
  5. Verifies every operation that returned `Ok(lsn)` before the kill is durable.
  6. Verifies no other operations are visible.
- Run with N=100 ops, 1000 iterations.
- Use proptest or hand-rolled randomization with a seed printed on failure.

**Done when:**
- [ ] 1000 iterations, 0 failures.
- [ ] Failure mode (if any) prints a reproducible seed.

**Pitfalls:**
- "Drops handles abruptly" can't actually `kill -9` from inside a test. Approximate by `mem::forget` on the handles + manually replaying via `recover`. Document the limitation.
- For real `kill -9` testing, write a separate harness (Phase 11 chaos suite).

---

### Task 2.12 — Checkpoint writer

**Reads:** `spec/05_storage_arena_wal/09_checkpointing.md`

**Writes:** `crates/brain-storage/src/wal/checkpoint.rs`

**What to build:**
- `pub fn write_checkpoint(wal: &Wal, durable_lsn: Lsn, arena_size: u64)` — writes a `WalRecordKind::Checkpoint` record.
- Recovery reads the latest checkpoint to fix the start LSN.

**Done when:**
- [ ] Checkpoint written → recovery starts from `durable_lsn + 1`.
- [ ] Multiple checkpoints: recovery uses the latest.

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
