# Phase 4 — Task 4.3: Tombstone bitmap

**Classification:** simple. Bit-packed `Vec<u64>` plus a thin `HnswIndex` accessor that consults the id_map. Does **not** wire into search — that's 4.4. Spec anchor: `spec/06_ann_index/05_deletion.md`.

## 1. Scope

In:

- `crates/brain-index/src/tombstones.rs` (new) — `TombstoneBitmap` struct + tests.
- `crates/brain-index/src/hnsw.rs` — add `HnswIndex::mark_tombstoned(MemoryId)`, `is_tombstoned(MemoryId)`, `tombstone_count() -> usize`. Extend `HnswError` with `MemoryIdNotFound`.
- `crates/brain-index/src/lib.rs` — `pub mod tombstones;`.

Out (deferred):

- **Search filtering** — sub-task 4.4 wraps `search` with the bitmap consult.
- **`mark_removed` on hnsw_rs** (spec §06/05 §6) — full ghost-node removal for slot reclamation. Phase 8 maintenance worker.
- **`tombstone_ratio` metric exposure** — sub-task 4.7 or Phase 11 (observability). 4.3 exposes the count; the ratio is `count / len`.

## 2. Spec quotes that bind the design

> **§06/05 §2 (tombstone discipline):**
> ```rust
> fn search_results_filter(candidates: Vec<...>, k: usize) -> Vec<...> {
>     candidates.into_iter()
>         .filter(|c| !memory_is_tombstoned(c.memory_id))
>         .take(k)
>         .collect()
> }
> ```
> "The filter is implicit in every search." → 4.3 builds the bitmap; 4.4 wires it in.
>
> **§06/05 §3:** "The substrate tracks `tombstone_ratio` per shard." → 4.3 exposes a count accessor.
>
> **§06/05 §10:** "The id_map_forward / id_map_reverse retain entries for tombstoned memories." → tombstoning is **not** id_map removal. The id_map entry stays; only the bitmap bit flips.

## 3. Design decisions

### 3.1 Bit-packed `Vec<u64>` per the orientation plan

Confirmed in the Phase 4 orientation. 10M bits ≈ 1.25 MB. One shift+mask per check. No new dep.

```rust
pub struct TombstoneBitmap {
    bits: Vec<u64>,   // 64 bits per element
    count: usize,     // running count of set bits, O(1) accessor
}
```

`count` is tracked incrementally (incremented on first set of a bit, decremented on `clear_one`) to avoid an O(N/64) summing scan on every `tombstone_count()` call. Spec §06/05 §13 mentions per-shard metrics — this needs to be cheap.

### 3.2 Bitmap operates on internal `u32` ids, not `MemoryId`

The bitmap is dense in internal-id space (0..=`id_map.next_id-1`), which matches hnsw_rs's return type. `MemoryId` lookups go through `HnswIndex` (which holds both the bitmap and the id_map). Two-layer separation:

- `TombstoneBitmap` — pure bit ops on `u32`.
- `HnswIndex::mark_tombstoned(MemoryId)` — id_map lookup → forward to bitmap.

### 3.3 Lazy growth

Initial bitmap is empty. `set(id)` extends `bits` to fit `id / 64 + 1` words on demand. No upfront allocation — fresh `HnswIndex` doesn't reserve 1.25 MB it might never use.

### 3.4 Idempotent set / clear

`set(id)` on an already-set bit doesn't double-count. `clear_one(id)` on an unset bit is a no-op. Both update `count` only on actual transitions.

### 3.5 `clear()` (full reset) for rebuild

Sub-task 4.6's `rebuild` skips tombstoned memories, so the new index starts with an empty bitmap. `TombstoneBitmap::clear()` zeros all bits and resets count to 0 — used by `HnswIndex::rebuild` (4.6). v1 4.3 still exposes it; tested.

### 3.6 `HnswError::MemoryIdNotFound` for unknown MemoryId

Spec §06/05 says the writer marks known tombstones; defensive: return an error if the MemoryId isn't in the id_map rather than silently no-op. Same fail-stop discipline as `DuplicateMemoryId`.

```rust
#[error("memory_id not found in id_map: {memory_id_bytes:?}")]
MemoryIdNotFound { memory_id_bytes: [u8; 16] },
```

### 3.7 Public surface

`tombstones.rs`:

```rust
pub struct TombstoneBitmap { /* ... */ }
impl TombstoneBitmap {
    pub fn new() -> Self;
    pub fn set(&mut self, id: u32);
    pub fn clear_one(&mut self, id: u32);
    pub fn clear(&mut self);
    pub fn is_set(&self, id: u32) -> bool;
    pub fn count(&self) -> usize;
}
```

Method names match `Vec<bool>`'s convention (`set`/`clear` for individual bits), not "tombstone"/"untombstone" which feels redundant inside a `TombstoneBitmap`.

`HnswIndex`:

```rust
pub fn mark_tombstoned(&mut self, memory_id: MemoryId) -> Result<(), HnswError>;
pub fn is_tombstoned(&self, memory_id: MemoryId) -> bool;  // false if unknown
pub fn tombstone_count(&self) -> usize;
```

`is_tombstoned` returns `false` for unknown MemoryIds rather than erroring — query path is hot; we don't want to plumb errors for "you asked about a memory that doesn't exist." `mark_tombstoned` (state-changing) does error.

## 4. Files touched

- `crates/brain-index/src/tombstones.rs` (new) — ~130 LOC including tests.
- `crates/brain-index/src/hnsw.rs` — add the three accessors + new error variant; ~20 LOC.
- `crates/brain-index/src/lib.rs` — `pub mod tombstones;` + re-export `TombstoneBitmap`.

No new deps. No changes to brain-core / brain-storage / brain-metadata.

## 5. Tests (gated `#[cfg(test)]`)

### tombstones.rs (8 tests)

1. **`new_is_empty`** — `count() == 0`, `is_set(0) == false`.
2. **`set_and_query`** — `set(5)`; `is_set(5) == true`; `count() == 1`.
3. **`unmarked_returns_false`** — `set(5)`; `is_set(100) == false`.
4. **`set_grows_lazily`** — `set(1000)` doesn't panic; bitmap allocated 16 `u64` words (`1000/64 + 1 = 16`).
5. **`set_is_idempotent`** — `set(5)` twice; `count() == 1`.
6. **`clear_one_resets_bit`** — `set(5); clear_one(5)`; `is_set(5) == false`; `count() == 0`.
7. **`clear_one_idempotent_on_unset`** — `clear_one(5)` on a fresh bitmap; `count() == 0` (no underflow).
8. **`clear_resets_all`** — set 5 distinct bits, `clear()`, `count() == 0`, all bits `is_set == false`.

### hnsw.rs (4 new tests)

9. **`mark_tombstoned_consults_idmap`** — insert M; `mark_tombstoned(M)` returns `Ok(())`; `is_tombstoned(M) == true`.
10. **`mark_tombstoned_unknown_returns_error`** — fresh index, `mark_tombstoned(M)` returns `HnswError::MemoryIdNotFound`.
11. **`is_tombstoned_unknown_returns_false`** — fresh index, `is_tombstoned(M)` returns `false` (no error — query path).
12. **`tombstone_count_pin`** — insert 3, mark 2 of them, `tombstone_count() == 2`.

Total: 12 tests added (8 bitmap + 4 hnsw). brain-index 22 → 34.

## 6. Verification

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-index"
```

Expected: brain-index 22 → 34 tests. Workspace clippy clean.

## 7. Commit

Branch: `feature/brain-index` (continuing). AUTONOMY §5 format:

```
feat(brain-index): tombstone bitmap (sub-task 4.3)
```

Body summarises: bit-packed `Vec<u64>`, lazy growth, O(1) count via incremental tracking, `HnswIndex` accessors, `MemoryIdNotFound` error for the state-changing path (query path is fail-soft), 12 new tests. 4.4 next wires the bitmap into search.

## 8. Done when

- [ ] `TombstoneBitmap` exposes `new` / `set` / `clear_one` / `clear` / `is_set` / `count`.
- [ ] `HnswIndex::{mark_tombstoned, is_tombstoned, tombstone_count}` work via id_map lookup.
- [ ] `mark_tombstoned` on unknown MemoryId returns `HnswError::MemoryIdNotFound`.
- [ ] Bitmap grows lazily; 1000-bit set doesn't pre-allocate 10M bits.
- [ ] `count()` is O(1), incremental.
- [ ] 12 new tests green; clippy clean.

PLAN READY.
