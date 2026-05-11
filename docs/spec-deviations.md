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

---

## SD-3.5-1: `IdempotencyEntry` adds a `request_hash` field beyond spec §2's struct listing

- **Spec:** `spec/07_metadata_graph/06_idempotency.md` §2 lists four fields on `IdempotencyEntry`: `response_kind`, `memory_id`, `response_payload`, `created_at`.
- **Implementation:** stores those four plus a fifth field `request_hash: [u8; 32]` (BLAKE3 over the canonical request form).
- **Reason:** spec §5 mandates a conflict-detection check that compares "a hash of the canonical form of the request" against the stored entry on retry. The response payload alone isn't reversible into the canonical request (responses include server-generated `MemoryId`s, encoded responses, etc.), so the hash must be stored alongside. 32 bytes per row is negligible against the ~50 B/row figure spec §7 uses for capacity planning (dominated by `response_payload`). Storing the hash also keeps the storage layer decision-free: the Phase 9 handler computes it from the canonical request bytes; storage just keeps the bytes.
- **Plan reference:** `.claude/plans/phase-03-task-05.md` §3.1.
- **Reconcile by:** raising a spec PR to add `request_hash: [u8; 32]` to the `IdempotencyEntry` struct listing in §07/06 §2. No code change pending — the implementation is the correct shape; the spec text under-specifies it.

---

## SD-3.11-1: `MetadataSink::apply` signature extended with `timestamp_ns: u64`

- **Spec:** `spec/05_storage_arena_wal/08_recovery.md` describes the sink-callback contract conceptually but doesn't pin a specific Rust signature.
- **Implementation:** `apply(&mut self, lsn: u64, payload: &WalPayload)` → `apply(&mut self, lsn: u64, timestamp_ns: u64, payload: &WalPayload)`. brain-storage's `MetadataSink` trait, `InMemoryMetadataSink::apply`, and the recovery dispatch all updated. brain-metadata's real sink uses the timestamp to populate `CheckpointMeta.completed_at_unix_nanos` on `CheckpointEnd` (and is forward-compatible with future variants that need it — UpdateKind / UpdateContext / others audit-trail timestamps).
- **Reason:** the WAL record carries `timestamp_ns` already; threading it through `apply` means sinks don't have to buffer a parallel record-header stream just to populate audit/observability timestamps. The alternative was extending each payload that needs a timestamp (CheckpointEndPayload, then any future variant), which would duplicate the record-level timestamp inside every variant.
- **Plan reference:** `.claude/plans/phase-03-task-11.md` §1.1.
- **Reconcile by:** none needed — internal API. Recorded so a future spec/§05/08 amendment can pin the signature.

---

## SD-3.11-2: Reclaim's memory-row cleanup is O(N) scan during recovery

- **Spec:** `spec/07_metadata_graph/02_table_layout.md` §13 describes the `slot_versions` table for "lazy reclaim" but doesn't specify how the corresponding `memories` row is located when only a `(slot_id, old_version)` pair is on hand.
- **Implementation:** `ReclaimPayload` carries `slot_id` + `old_version` + `new_version` but **not** the original `MemoryId`. To delete the memory row + its text, the sink scans `memories` looking for a row whose `slot_id` and `slot_version` match. O(N) per reclaim where N is the number of memory rows in the shard.
- **Reason:** the wire/worker layer that emits Reclaim has the `MemoryId` in scope; carrying it forward in the payload would make the storage-layer reclaim path O(1). v1 accepts the cost because (a) reclaims are rare during recovery (only after grace expiry), (b) live ops shouldn't go through this apply path (the writer task composes the same operations with the MemoryId already known), (c) extending `ReclaimPayload` requires a brain-storage WAL-payload change which we've already done once this phase (SD-3.11-1) and prefer to batch.
- **Plan reference:** `.claude/plans/phase-03-task-11.md` §3.6.
- **Reconcile by:** extend `ReclaimPayload` with `memory_id: MemoryId` in a future Phase 2 amendment; the sink then deletes by key in O(1) instead of scanning. Tracked as a follow-up.
- **Status:** **Reconciled** by SD-3.11-3 (audit-followups-1 batch). `ReclaimPayload` now carries `memory_id`; the sink uses an O(1) primary-key delete.

---

## SD-3.11-3: `ReclaimPayload` carries `memory_id` beyond spec §05/05 §10's three-field listing

- **Spec:** `spec/05_storage_arena_wal/05_wal_records.md` §10 declares `struct ReclaimRecord { slot_id, old_version, new_version }` — three fields.
- **Implementation:** adds a fourth field `memory_id: MemoryId`, encoded after `new_version`. On-disk layout is `slot_id (u64) | old_version (u32) | new_version (u32) | memory_id (16 B)`.
- **Reason:** closes SD-3.11-2. The metadata sink needs the row's primary key (`MemoryId.to_be_bytes()`) to delete the `memories` and `texts` rows during recovery. Without `memory_id` in the payload, the sink scans the entire `memories` table looking for a row matching `(slot_id, slot_version)` — O(N) per reclaim. Adding the field is 16 bytes per Reclaim record (a rare record type during recovery; routine but bounded during live ops).
- **Plan reference:** post-Phase-3 audit-followups batch.
- **Reconcile by:** raise a spec PR to update §05/05 §10 to declare the four-field layout. No code change pending — the implementation is the correct shape.

---

## SD-4.5-1: HNSW snapshot is a directory of three files, not the single file spec §06/06 §5.1 describes

- **Spec:** `spec/06_ann_index/06_persistence.md` §5.1 describes the snapshot as a single file with embedded sections: 64-byte BHN0 header, then "graph data: serialized via hnsw_rs's built-in serialization", then "id_map data: serialized HashMaps", then an 8-byte BLAKE3 footer.
- **Implementation:** the snapshot is a **directory** containing three files at the same `basename`:
  - `<basename>.hnsw.graph` — written by `hnsw_rs::Hnsw::file_dump`.
  - `<basename>.hnsw.data` — written by `hnsw_rs::Hnsw::file_dump`.
  - `<basename>.brain` — our wrapper file with the 64-byte BHN0 header, id_map entries, `next_internal_id`, tombstone bitmap, and 8-byte BLAKE3 footer covering the `.brain` file only. Written **last** so its presence is the "snapshot complete" marker.
- **Reason:** `hnsw_rs::Hnsw::file_dump(path, basename)` writes two separate files and exposes no `Write` / `Cursor` interface for in-memory serialization. To honour the spec's single-file format we'd dump to a temp directory, read both files into memory, and concatenate into our wrapper — extra I/O, extra disk, complicated atomic-write story. The directory-of-three layout matches hnsw_rs's idiom natively and gives us the same integrity properties (header CRC32C on the `.brain` file; BLAKE3 footer over `.brain`; the `.hnsw.*` files validated transitively by hnsw_rs's own format on load).
- **Special case:** for empty indexes (`graph_node_count == 0`) we skip the `.hnsw.*` files entirely — hnsw_rs's `file_dump` errors on zero-node graphs. The loader notices the header's `graph_node_count == 0` and constructs a fresh empty inner instead of calling `HnswIo::load_hnsw_with_dist`.
- **Plan reference:** `.claude/plans/phase-04-task-05.md` §3.1.
- **Reconcile by:** raise a spec PR amending §06/06 §5.1 to describe the directory-of-three layout. The integrity properties are equivalent; the change is documentation-only.

---

## SD-4.5-2: `HnswIo` is `Box::leak`'d on snapshot load

- **Spec:** silent on implementation detail.
- **Implementation:** `HnswIndex::load_snapshot` calls `Box::leak(Box::new(HnswIo::new(dir, basename)))` to get a `&'static HnswIo`, then calls `load_hnsw_with_dist` on it. The returned `Hnsw<'b, T, D>` has `'b ≤ 'a` (where `'a` is the `HnswIo`'s borrow lifetime); we hold `Hnsw<'static, ...>` inside `HnswIndex` to keep the wrapper lifetime-free.
- **Reason:** `hnsw_rs`'s `Hnsw<'b, ...>` lifetime parameter is for mmap-backed data borrowed from the `HnswIo`. In non-mmap mode (which we use), the returned graph owns all its data and the `'b` is artificial — but the public API doesn't expose that. Without the leak, `HnswIndex` would need to be lifetime-generic (`HnswIndex<'a, const D: usize>`), forcing the lifetime to thread through every caller. The leak is `~few hundred bytes per snapshot load`; loads are startup-time (one per shard per restart), so the leaked memory is bounded by shard count and reclaimed at process exit.
- **Plan reference:** `.claude/plans/phase-04-task-05.md` §3.6 (and 4.5 mid-flight discovery).
- **Reconcile by:** Phase 11+ alternatives — patch hnsw_rs to expose a `'static`-returning loader for non-mmap mode, or migrate `HnswIndex` to be lifetime-generic. Neither is urgent at v1 scale.

---

## SD-4.8-1: `Arc<RwLock<HnswIndex>>` instead of `ArcSwap<HnswState>` for shared access

- **Spec:** `spec/06_ann_index/08_concurrency.md` §3 mandates lock-free reads via `ArcSwap<HnswState>`, with a pending-insert buffer (§10) that periodically rebuilds and publishes a new state.
- **Implementation:** `Arc<parking_lot::RwLock<HnswIndex<D>>>`. Concurrent reads (multiple readers proceed in parallel through `RwLock::read()`), exclusive writes (writers acquire `RwLock::write()`, briefly blocking readers).
- **Reason:** the spec's ArcSwap pattern requires the writer to periodically clone or rebuild the published HNSW state. `hnsw_rs::Hnsw<f32, DistCosine>` doesn't implement `Clone`, and at the spec's 1M-node target a deep clone would cost ~150 MB and seconds — far past the spec's 100 ms flush cadence (§10). The pattern as written presumes a custom HNSW where clone-and-swap is cheap; with hnsw_rs (mandated by CLAUDE.md §6), it isn't.
- **Trade-off:** writes briefly block readers (~1–3 ms per insert at 1M nodes per spec §06/03 §4). At typical write-to-read ratios this is acceptable; the spec's lock-free reader was specifically for high write-throughput scenarios.
- **Other parts preserved:** single-writer-per-shard (spec §06/08 §1) is enforced at the type level via the `(SharedHnsw, Writer)` pair: `SharedHnsw` is `Clone`, `Writer` is not. Only one `Writer` can exist per `SharedHnsw`. Inserts take `&mut self` on the `Writer`.
- **What's not implemented:** the pending-insert buffer (§10), the epoch protocol (§5), the read-after-write hint (§11). Under RwLock these become no-ops — writes are immediately visible to subsequent readers because they commit before the write lock is released.
- **Plan reference:** `.claude/plans/phase-04-task-08.md` §3.8.
- **Reconcile by:** future Phase 11+ work — either (a) patch hnsw_rs upstream to expose a clone-aware mutation model that supports atomic publication, or (b) replace `hnsw_rs` with a custom HNSW that does. Both are significant efforts that conflict with Phase 4's ship-quickly goal.
