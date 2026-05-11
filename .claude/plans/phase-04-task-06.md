# Phase 4 — Task 4.6: Rebuild from external iterator

**Classification:** simple-to-moderate. Constructor function that consumes an iterator and produces a fresh `HnswIndex`. No I/O, no format. Spec anchors: `spec/06_ann_index/06_persistence.md` §2 (the rebuild procedure) + `spec/06_ann_index/07_maintenance.md` §5 (full-rebuild flow).

## 1. Scope

In:

- `crates/brain-index/src/rebuild.rs` (new) — `RebuildReport` struct (small observability surface) + the `rebuild_impl` body. ~100 LOC + tests.
- `crates/brain-index/src/hnsw.rs` — `HnswIndex::rebuild` static constructor. ~30 LOC.
- `crates/brain-index/src/lib.rs` — `pub mod rebuild;` + re-export `RebuildReport`.

Out (deferred):

- **Parallel insertion** (spec §07 §6 mentions hnsw_rs's `parallel_insert`) — v1 sequential. Profile-driven optimisation; spec §06 §2 says 1M memories rebuild in ~30 sec single-threaded which is acceptable for the maintenance-worker cadence.
- **Atomic swap with old index** (spec §07 §5 steps 3–4) — that's the caller's job (Phase 8 maintenance worker); 4.6 returns a fresh `HnswIndex`, the worker swaps via `ArcSwap` (sub-task 4.8).
- **Catch-up phase** (spec §07 §5 step 2 — apply WAL records that arrived during build) — also Phase 8; 4.6 is a pure builder.
- **Partial rebuild** (spec §07 §8) — open question, not v1.
- **Corrupted-vector skip** (spec §07 §12) — caller iterates *valid* memories; corruption filtering is upstream.

## 2. Spec quotes that bind the design

> **§06/06 §2 (rebuild procedure):**
> ```
> 1. Initialize an empty HNSW with configured parameters.
> 2. Iterate over all active memories in the metadata store.
> 3. For each memory:
>    a. Read the vector from the arena.
>    b. Insert into HNSW.
>    c. Update id maps.
> 4. The HNSW is now consistent with the arena and metadata.
> ```
>
> **§06/06 §3:** "Only active (non-tombstoned) memories are inserted into the rebuilt HNSW. Tombstoned memories are skipped — the rebuild is also a 'compaction' in the sense that it strips out tombstones."
>
> **§06/06 §4:** "The order of insertion during rebuild affects HNSW quality slightly. The substrate uses metadata-store order (B-tree by MemoryId, roughly time-ordered via UUIDv7)."
>
> **§06/07 §5 (the full-rebuild flow):** four phases — Build, Catch-up, Atomic-swap, Cleanup. **4.6 implements only the Build phase**; the other three are caller's responsibility (Phase 8 maintenance worker).

## 3. Design decisions

### 3.1 Iterator-based API; caller owns the filter

```rust
impl<const D: usize> HnswIndex<D> {
    pub fn rebuild<I>(params: IndexParams, source: I) -> Result<(Self, RebuildReport), HnswError>
    where
        I: IntoIterator<Item = (MemoryId, [f32; D])>;
}
```

The iterator yields `(MemoryId, [f32; D])` pairs. Spec §06/06 §3's tombstone filter is the **caller's job** — they pass an iterator that's already filtered. Same for spec §07 §12's corrupted-vector skip. brain-index stays closed-leaf: vectors in, fresh index out.

`IntoIterator` is more ergonomic than `Iterator` directly (callers can pass slices, vecs, or any iterator-yielding type).

### 3.2 Return tuple includes a `RebuildReport`

```rust
pub struct RebuildReport {
    pub memories_inserted: u64,
    pub duration: Duration,
}
```

Phase 8 maintenance worker uses these for the `last_rebuild_duration_ms` metric (spec §07 §13). No need for richer observability in v1.

### 3.3 Sequential insertion in v1

Spec §07 §6 mentions parallel insertion as a performance target (10M → 5 sec with 16 threads). hnsw_rs's `parallel_insert_slice` is available. **v1 is sequential** because:

- Phase 7 worker (caller) controls the threading model — if it wants parallelism, it pre-batches and calls a future `rebuild_parallel`.
- Sequential insertion is deterministic and easier to test.
- For typical Brain shards (10K–1M), sequential rebuild is well under a minute; not on the critical path.

Adding `rebuild_parallel` is a small additive change later (spec §07 §6).

### 3.4 Fail-fast on errors

If the iterator produces a duplicate `MemoryId`, `rebuild` returns `Err(HnswError::DuplicateMemoryId)` and the partial index is dropped. Spec §06/03 §10 treats duplicate-MemoryId as a caller bug; same treatment during rebuild.

If `IdMapExhausted` (`u32::MAX` inserts), same — fail fast.

If the iterator returns < 10M items but stalls or panics in the middle, our function propagates via Rust's standard panic mechanism. Spec §07 §12: "A failed rebuild doesn't degrade the running state" — the caller catches via `Result::Err` and falls back to using the old index.

### 3.5 Empty iterator → empty index

`rebuild(params, [])` returns `Ok((HnswIndex::new(params)?, RebuildReport { 0, 0 }))`. The hnsw_rs internals tolerate empty inserts (no actual `inner.insert_slice` calls in this path).

### 3.6 Tombstones start empty

Fresh `HnswIndex` from `rebuild` has `tombstone_count() == 0` and an empty `TombstoneBitmap` — spec §06/06 §3 calls this "compaction."

### 3.7 No new module boundaries

`rebuild.rs` exists for organisation (the function is small but documents heavily) and to keep `hnsw.rs` from sprawling. The public API stays at `HnswIndex::rebuild`; the module is implementation detail re-exported through `lib.rs` only for `RebuildReport`.

## 4. Files touched

- `crates/brain-index/src/rebuild.rs` (new) — `RebuildReport` + `rebuild_impl<D, I>` free function. ~80 LOC including tests.
- `crates/brain-index/src/hnsw.rs` — `HnswIndex::rebuild` thin wrapper around `rebuild_impl`. ~30 LOC.
- `crates/brain-index/src/lib.rs` — `pub mod rebuild;` + `pub use rebuild::RebuildReport`.

No new deps. No changes to brain-core / brain-storage / brain-metadata.

## 5. Tests (gated `#[cfg(test)]`)

### rebuild.rs (5 tests)

1. **`rebuild_empty_iterator`** — `rebuild(params, std::iter::empty())` produces `Ok((idx, report))` with `idx.len() == 0` and `report.memories_inserted == 0`.
2. **`rebuild_from_iterator_yields_correct_len`** — 3 pairs → `idx.len() == 3`, `report.memories_inserted == 3`.
3. **`rebuild_uses_provided_params`** — rebuild with `m: 32` (non-default); `idx.params().m == 32`.
4. **`rebuild_starts_with_empty_tombstones`** — fresh rebuilt index has `tombstone_count() == 0` regardless of source.
5. **`rebuild_rejects_duplicate_memory_id`** — iterator with two same MemoryIds → `Err(HnswError::DuplicateMemoryId { .. })`.

### hnsw.rs (3 tests on the integrated path)

6. **`rebuild_search_returns_correct_results`** — rebuild from 3 known vectors, search for one of them, top hit is itself.
7. **`rebuild_report_records_duration`** — rebuild completes; `report.duration > Duration::ZERO`. Doesn't assert a numeric bound (CI variance); just that the field is populated.
8. **`rebuild_then_save_then_load`** — end-to-end: rebuild → save_snapshot → load_snapshot → search returns same MemoryIds. Pins 4.5 + 4.6 compose correctly.

Total: 8 tests. brain-index 57 → 65.

## 6. Verification

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-index"
```

Expected: brain-index 57 → 65 tests. Workspace clippy clean.

## 7. Commit

Branch: `feature/brain-index` (continuing). AUTONOMY §5:

```
feat(brain-index): rebuild from external iterator (sub-task 4.6)
```

Body summarises: iterator-based constructor, caller-owned filter (tombstones / corruption upstream), sequential insertion (parallel deferred), `RebuildReport` with insert count + duration, fail-fast on duplicate MemoryId, 8 new tests including an end-to-end rebuild → save → load → search round trip.

## 8. Done when

- [ ] `HnswIndex::rebuild(params, iter) -> Result<(Self, RebuildReport), HnswError>` works.
- [ ] Rebuilt index has no tombstones, regardless of source.
- [ ] Duplicate MemoryId in iterator returns `HnswError::DuplicateMemoryId`.
- [ ] `RebuildReport` carries `memories_inserted` + `duration`.
- [ ] 8 tests green; clippy clean; workspace tests pass.

PLAN READY.
