# Phase 3 — Task 3.7: `slot_versions` table

> **Realignment note.** The phase doc originally titled this sub-task "Tombstone table" with value shape `(memory_id, tombstoned_at, grace_until)`. That table does not appear in the spec's 13-table catalog (`spec/07_metadata_graph/02_table_layout.md` §1). Tombstone *state* lives as `flags & HARD_FORGOTTEN` + `forgot_at_unix_nanos` on the existing `memories` row (already in 3.2's `MemoryMetadata`); the reclaim worker scans for `forgot_at + grace < now` per spec §09/06 §16. The actual reclaim-related table in the spec catalog (§07/02 §13) is `slot_versions: u64 → u32`. This sub-task realigns to that.
>
> The phase-doc update (flipping 3.7 to ✅ post-implementation) records the realignment in-line.

**Classification:** trivial-to-simple. One table with built-in scalar types (no rkyv wrapper); one helper that does atomic read-modify-write inside an existing redb transaction; one error variant for the u32-overflow corner case.

**Spec:** `spec/07_metadata_graph/02_table_layout.md` §13 (table shape + purpose). Cross-checked `spec/02_data_model/03_identifiers.md` §2.1 (MemoryId structure: 32-bit version field), `spec/05_storage_arena_wal/07_write_path.md` §2.3 ("`slot_version_new` is `current_version + 1` for reclaimed slots, or 1 for never-used slots"), `spec/05_storage_arena_wal/08_recovery.md` §6 (recovery verifies arena's slot_version matches stored).

## 1. Scope

In:

- `crates/brain-metadata/src/tables/slot_version.rs` (new):
  - `SLOT_VERSIONS_TABLE: TableDefinition<'static, u64, u32>` — keyed by `slot_id` (48 bits in the MemoryId, padded to a `u64` here per spec §07/02 §13), valued by the 32-bit version. redb's built-in scalar `Value` impls — no rkyv wrapper.
  - `pub fn increment(table: &mut Table<'_, u64, u32>, slot_id: u64) -> Result<u32, SlotVersionError>` — read-modify-write inside the caller's transaction. Missing row → start at 1 (spec §05/07 §2.3). Existing row → returns `current + 1`. Returns the new version on success.
  - `pub enum SlotVersionError { Storage(redb::StorageError), Exhausted { slot_id: u64 } }` — `Exhausted` triggers at u32::MAX. Fail-stop is the right behaviour: a slot that overflowed its version is no longer safe to re-issue (spec §02/03 §2.3's invariant: "A `MemoryId` that previously identified memory M never identifies a different memory"). The reclaim worker decides how to surface; storage just refuses to write.

Out (deferred):

- **Composition with FORGET / reclaim** (read current version + increment + remove memory row + zero arena slot) — `MetadataDb` (3.10) and Phase 8 worker.
- **MemoryId minting** — the `u64 slot_id + u32 new_version → MemoryId` packing lives in `brain-core` already (or will when we hit that piece in Phase 4 / ENCODE wiring). This sub-task only stores the version; it doesn't mint IDs.
- **Recovery cross-check** ("arena slot's metadata `slot_version` matches the metadata store's record", spec §05/08 §6) — that's a recovery step composing this table with brain-storage's arena. Phase 3.11 `MetadataSink` work.
- **Retirement strategy for u32::MAX-overflowed slots** — spec doesn't address; v1 returns `Exhausted` and lets the caller decide. Likely "mark slot permanently retired in a future table"; not in scope here.

## 2. Spec quotes that bind the design

> **§07/02 §1, row 13 (catalog):**
> ```
> slot_versions | u64 (slot_id) | u32 (version) | Per-slot versions, for lazy reclaim
> ```
>
> **§07/02 §13 (purpose):**
> > "When a slot is reclaimed (after FORGET + grace period), its version is incremented. The new version is recorded so that future MemoryIds with the new version know which slot they refer to. The table maps `slot_id → current_version`. Looked up: During ENCODE to allocate a fresh MemoryId for a reclaimed slot. During recovery to verify HNSW node IDs match the slot's current state."
>
> **§05/07 §2.3 (initial value):** "`slot_version_new` is `current_version + 1` for reclaimed slots, or 1 for never-used slots." → missing row means never-used; start at 1.
>
> **§02/03 §2.1 (field width):** the version sub-field of the MemoryId is 32 bits. → u32 is the right type; overflow is a real (if astronomically rare) concern.

## 3. Design decisions

### 3.1 Use redb's built-in `u64` and `u32` scalar `Value`s

Same reasoning as 3.6's `&[u8]` choice: no struct to evolve, no need for rkyv's encode/decode pass, no `AlignedVec` alignment-copy workaround. The table is `TableDefinition<u64, u32>` directly. redb provides `Value` for every primitive integer.

### 3.2 `increment` is a free function over `&mut Table`, not a method on a wrapper

Same pattern as 3.5's `prune_expired`. The caller already has a write transaction and an open table handle (because reclaim / ENCODE wraps multiple table operations atomically); the helper takes `&mut Table` so it composes naturally inside whatever transaction the caller is running. No `&self` MetadataDb yet (3.10 owns that surface).

### 3.3 Missing row → start at 1, not 0

Spec §05/07 §2.3 fixes this: a never-used slot's first MemoryId has version 1. Returning 1 on missing means the helper is the canonical "give me the next version for this slot" call, no matter whether the slot is fresh or reclaimed. The caller doesn't need to distinguish.

### 3.4 u32 overflow → `Exhausted` error, no write

`current.checked_add(1)` returns `None` at `u32::MAX`. Returning `SlotVersionError::Exhausted { slot_id }` is fail-stop: storage refuses the write rather than wrap to 0 (which would silently violate spec §02/03 §2.3's MemoryId-stability invariant). The reclaim worker can log + surface; v1 doesn't auto-retire.

Practical note: at one reclamation per slot per second, u32 takes ~136 years to exhaust. Test it anyway — the cost is one cheap test, and silent wrap would be catastrophic.

### 3.5 `SlotVersionError` derives only what `redb::StorageError` allows

Same constraint hit in 3.4: `redb::StorageError` doesn't impl `Clone/Copy/PartialEq`. So the error derives `Debug` + `thiserror::Error` only.

### 3.6 No `type_name` versioning concern

Scalar value type. Nothing to evolve. redb's built-in u32 `type_name` is fine.

### 3.7 Helper file naming: `slot_version.rs`, not `slot_versions.rs`

Matches the existing convention: `memory.rs` (not `memories.rs`), `agent.rs` (not `agents.rs`), `context.rs`, `edge.rs`, `text.rs`. Module is singular; the table inside it is plural (`SLOT_VERSIONS_TABLE`).

## 4. Files touched

- `crates/brain-metadata/src/tables/slot_version.rs` (new) — ~150 LOC including tests.
- `crates/brain-metadata/src/tables/mod.rs` — one new `pub mod slot_version;`.
- `docs/phases/phase-03-metadata.md` — flip 3.7 to ✅ with the realignment recorded in `What was built`, including a callout that the original "Tombstone table" framing was a phase-doc artifact (tombstone state lives on `MemoryMetadata` from 3.2; reclaim worker scans memories).

No new SD entry — this is not a deviation from spec; it's the phase doc being realigned *to* the spec.

## 5. Tests (gated `#[cfg(all(test, not(miri)))]`)

1. **`increment_missing_starts_at_one`** — slot_id with no row → `increment` returns `Ok(1)`, stored value is 1.
2. **`increment_existing_returns_next`** — slot_id at version 5 → `increment` returns `Ok(6)`, stored value is 6.
3. **`increment_is_monotonic_across_calls`** — repeat 10×, watch versions go 1..=10.
4. **`independent_slots_dont_interfere`** — two different slot_ids each incremented N times; each ends at N.
5. **`overflow_returns_exhausted_and_does_not_write`** — pre-seed slot_id with `u32::MAX`; call `increment`; assert `Err(Exhausted { slot_id })`; assert the row is still `u32::MAX` (no wrap-to-zero, no partial write).
6. **`direct_get_after_insert`** — manual `t.insert(&slot, &42u32)` round-trips through `t.get(&slot)`.
7. **`range_scan_returns_in_order`** — insert slot_ids 100, 50, 200; iterate `range(..)`; assert keys come back sorted (50, 100, 200) — pins redb's u64 lexicographic-key behaviour for `slot_id`.
8. **`missing_key_get_returns_none`** — vanilla redb behaviour pin.

## 6. Verification

Same Linux dev-container harness as 3.5 / 3.6:

```
docker run --rm \
  -v "$(pwd)":/workspaces/brain ... \
  brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-metadata"
```

Expected: 63 brain-metadata tests pass (55 prior + 8 new).

## 7. Commit

Branch: `feature/brain-metadata` (continuing). AUTONOMY §5 format:

```
feat(brain-metadata): slot_versions table for lazy reclaim (sub-task 3.7)
```

Body summarises: realignment (phase doc said "tombstone table"; spec catalog has `slot_versions`; tombstone state already on `MemoryMetadata`), table shape, `increment` helper, `SlotVersionError::Exhausted` fail-stop at u32::MAX, 8 new tests, **10 of 13 spec'd tables done.**

## 8. Done when

- [ ] `SLOT_VERSIONS_TABLE: TableDefinition<u64, u32>` defined; opens cleanly.
- [ ] `increment` returns 1 for missing rows, N+1 for existing, `Exhausted` at u32::MAX.
- [ ] 8 tests green; full brain-metadata suite green.
- [ ] `docs/phases/phase-03-metadata.md` Task 3.7 retitled to "`slot_versions` table" (✅), with a "Realignment" note that the original "tombstone table" framing wasn't backed by the spec.

PLAN READY.
