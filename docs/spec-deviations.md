# Spec Deviations

Every place implementation has consciously diverged from the spec, with the rationale and a reconciliation plan. New entries get appended; reconciled entries get marked **Reconciled** with the commit that closed them.

The spec wins in general (per `CLAUDE.md` §2 and `AUTONOMY.md` §2); the entries below are the explicit exceptions, surfaced for review.

---

## SD-2.3-1: Slot metadata CRC range `[0..40]` instead of spec literal `[0..36]`

- **Spec:** `spec/05_storage_arena_wal/02_arena_layout.md` §3.2 says the slot metadata CRC covers bytes `[0..36]`.
- **Implementation:** covers bytes `[0..40]`.
- **Reason:** byte 36 splits the `last_modified_at` u64 (which spans bytes 32..40) — almost certainly a spec typo. The `[0..40]` reading covers every metadata field before the CRC itself and matches the pattern used elsewhere in the spec (CRC excludes only itself + reserved).
- **Plan reference:** `.claude/plans/phase-02-task-03.md` §3.1.
- **Reconcile by:** raising a spec PR to fix `[0..36]` → `[0..40]`. No code change pending.

---

## SD-2.4-1: Arena header CRC range `[0..80]` instead of spec literal `[0..76]`

- **Spec:** `spec/05_storage_arena_wal/02_arena_layout.md` §2 says the header CRC covers bytes `[0..76]`.
- **Implementation:** covers bytes `[0..80]`.
- **Reason:** byte 76 splits the `last_grow_at` u64 (which spans bytes 72..80) — same typo pattern as SD-2.3-1. `[0..80]` is the natural reading (every field before the CRC).
- **Plan reference:** `.claude/plans/phase-02-task-04.md` §3.1.
- **Reconcile by:** raising a spec PR alongside SD-2.3-1. No code change pending.

---

## SD-2.4-2: Phase doc's `padding + crc` slot layout vs spec's in-meta CRC

- **Spec:** `spec/05_storage_arena_wal/02_arena_layout.md` §3.2 places `metadata_crc32c` *inside* `SlotMeta` at metadata-offset 40 (slot-offset 1576).
- **Phase doc 2.3 sketch:** showed `Slot { vector, metadata, padding, crc }` with the CRC as a trailing slot field.
- **Implementation:** follows the spec (CRC inside `SlotMeta`); phase doc was corrected in the same commit.
- **Plan reference:** `.claude/plans/phase-02-task-03.md` §3.3.
- **Status:** **Reconciled** — phase doc updated; no ongoing deviation.

---

## SD-2.4-3: Hand-rolled `libc` mmap instead of `memmap2::MmapMut`

- **Spec:** `spec/05_storage_arena_wal/03_arena_growth.md` §4 prescribes `mremap(2)` with `MREMAP_MAYMOVE`.
- **Phase doc 2.4 sketch:** showed `ArenaFile { mmap: MmapMut, ... }` (the `memmap2` crate).
- **Implementation:** hand-rolled `libc::mmap` / `mremap` / `munmap`.
- **Reason:** `memmap2` does not expose `mremap`. Going through it would force the spec's *fallback* growth path (§05/03 §5: unmap-then-mmap) instead of the spec's primary `mremap` path.
- **Plan reference:** `.claude/plans/phase-02-task-04.md` §3.2.
- **Status:** **Reconciled** — phase doc updated to match the implemented spec-faithful design.

---

## SD-2.5-1: Version-bump-on-alloc instead of version-bump-on-free

- **Spec:** `spec/05_storage_arena_wal/07_write_path.md` §56 says `slot_version_new = current_version + 1` — the new version is computed at alloc time.
- **Phase doc 2.5 sketch:** said `free()` bumps the slot's version.
- **Implementation:** follows the spec; `alloc` computes `current + 1`, `free` does not touch the version.
- **Reason:** spec wording is unambiguous; phase doc was a sketch. The spec reading has the additional property that a crashed encode between `alloc` and WAL fsync doesn't burn a version.
- **Plan reference:** `.claude/plans/phase-02-task-05.md` §3.1.
- **Status:** **Reconciled** — phase doc updated.

---

## SD-2.8-1: No `O_DIRECT` on WAL segment files

- **Spec:** `spec/05_storage_arena_wal/06_wal_durability.md` §2.1 mandates `O_DIRECT` for WAL segment files.
- **Implementation:** plain buffered I/O. `RWF_DSYNC` (via `pwritev2`) still provides the durability guarantee.
- **Reason:** spec §06 §3 mandates "round the buffer's used size up to the next 4 KB boundary (padding with zero bytes that recovery will interpret as a CRC-failed record and stop at)" on every flush. With `O_DIRECT` + this padding, every batch boundary becomes a zero-padded gap mid-segment. `WalReader` (sub-task 2.7) treats the all-zero region as `Err(UnknownRecordType(0))` (since the zero `record_type` byte is reserved) and reports `MidSegmentCorruption` — the WAL becomes unreadable after its first flush. The spec's "padding that recovery stops at" wording only works when the padding is at the *very end* of the WAL; supporting it mid-segment requires WAL pages (per-page headers so the reader can skip page-aligned padding), which is a major format change beyond 2.8's scope.
- **Plan reference:** `.claude/plans/phase-02-task-08.md` §3.1.
- **Reconcile by:** Phase 9 (server wire-up) — once Glommio + `io_uring` are in, do a single coordinated change that introduces WAL pages, opens segments with `O_DIRECT`, and updates `WalReader` to skip page-aligned padding. Track as a follow-up.

---

## SD-2.8-2: Synchronous `pwritev2` from a `std::thread`, not `io_uring`

- **Spec:** `spec/05_storage_arena_wal/06_wal_durability.md` §2.3 prescribes `io_uring` (Glommio).
- **Implementation:** synchronous `pwritev2(RWF_DSYNC)` from a dedicated OS thread per `GroupCommitter`.
- **Reason:** Glommio hasn't been wired into `brain-storage`. Pulling it in for 2.8 alone would mean adding the runtime, picking an executor model, and coupling the committer to it — all before the rest of the system (request handler, server) is ready to live on Glommio.
- **Plan reference:** `.claude/plans/phase-02-task-08.md` §3.2.
- **Reconcile by:** Phase 9 — replace the committer thread with a Glommio coroutine using `io_uring`. The `GroupCommitter` public API (`append → AppendHandle::wait`) is shaped so the swap is local.

---

## SD-2.9-1: Synchronous `Wal::append(&mut self, ...)` instead of `async fn append(&self, ...)`

- **Spec / phase doc:** phase-02 sub-task 2.9 prescribes `pub async fn append(&self, record: WalRecord) -> Result<Lsn>`. Spec §07 §3 implies an async writer task.
- **Implementation:** synchronous `pub fn append(&mut self, record: WalRecord) -> Result<Lsn, WalError>`.
- **Reason:** carries forward SD-2.8-2 — there's no async runtime in `brain-storage` yet. The `&mut self` change (rather than `&self` + interior mutability) reflects spec §07 §15's single-writer-per-shard discipline at the type level: the borrow checker enforces that there's only one active writer.
- **Plan reference:** `.claude/plans/phase-02-task-09.md` §3.1.
- **Reconcile by:** Phase 9, alongside SD-2.8-2. Becomes `pub async fn append(&self, record) -> Result<Lsn>` once the writer runs as a Glommio coroutine and the committer is `&self`-safe via the runtime's task-local guarantees.
