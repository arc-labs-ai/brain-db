# Phase 4 — Task 4.2: `MemoryId ↔ usize` adapter

**Classification:** simple-moderate. Adds a bidirectional ID map between the public `MemoryId` and hnsw_rs's internal `usize` discriminator. Replaces 4.1's raw-`usize` public API with a `MemoryId`-aware surface. Defensive: rejects duplicate-MemoryId inserts.

**Spec:** `spec/06_ann_index/03_insertion.md` §1–2 (id_map pattern), §10 (duplicate-MemoryId is a bug). Cross-checked `05_deletion.md` §10 (id_maps retain entries for tombstoned memories; cleanup happens at maintenance/rebuild — out of scope here).

## 1. Scope

In:

- `crates/brain-index/src/idmap.rs` (new) — `IdMap` struct + `MemoryIdAlreadyInserted` error. Owns the two HashMaps + sequential allocator.
- `crates/brain-index/src/hnsw.rs` — `HnswIndex<D>` composes an `IdMap`; replace `insert(usize, ...)` / `search(...) -> Vec<(usize, f32)>` with `insert(MemoryId, ...)` / `search(...) -> Vec<(MemoryId, f32)>`. Extend `HnswError` with the duplicate-id variant.
- `crates/brain-index/src/lib.rs` — `pub mod idmap;` + re-exports.

Out (deferred):

- **Tombstone-aware filtering** — sub-task 4.3 + 4.4 layer on top.
- **id_map persistence** — sub-task 4.5's snapshot writes the maps' contents.
- **id_map rebuild from metadata scan** — sub-task 4.6.
- **Atomic next_id with concurrent inserts** — spec §06/03 §1 example shows `AtomicU32::fetch_add`, but Brain's single-writer-per-shard discipline (enforced via `HnswIndex::insert(&mut self, ...)`) makes the atomic unnecessary. Plain `u32`.
- **Stale-ID cleanup on slot reclamation** — spec §06/05 §11; Phase 8 maintenance worker.

## 2. Spec quotes that bind the design

> **§06/03 §1 (insert procedure):**
> ```rust
> let internal_id = index.next_internal_id.fetch_add(1, Ordering::Relaxed);
> index.id_map_forward.insert(memory_id, internal_id);
> index.id_map_reverse.insert(internal_id, memory_id);
> index.hnsw.insert((vector, internal_id));
> ```
>
> **§06/03 §2:**
> ```rust
> struct HnswIndex {
>     hnsw: hnsw_rs::Hnsw<f32, DistCosine>,
>     id_map_forward: HashMap<MemoryId, u32>,
>     id_map_reverse: HashMap<u32, MemoryId>,
>     next_internal_id: AtomicU32,
> }
> ```
> Internal IDs are `u32`; mapped to `usize` only at the hnsw_rs API boundary.
>
> **§06/03 §10 (duplicate):** "If the same MemoryId is inserted twice, the second insert overwrites the first internally. Brain treats this as a bug — the writer should never re-insert an existing memory."

## 3. Design decisions

### 3.1 Internal IDs are `u32`, cast to `usize` at the hnsw_rs boundary

Spec §06/03 §2's struct shows `u32`. At 10M memories the savings are real: forward map (`MemoryId → u32`) is 20 B/entry vs 24 B for `usize`; reverse (`u32 → MemoryId`) is 20 B vs 24 B. ~80 MB saved at full capacity per shard.

`u32::MAX ≈ 4.3 billion` is well above the spec's per-shard ceiling of ~10M. Overflow protection: `insert` returns `IdMapExhausted` if `next_id == u32::MAX`. Tested.

### 3.2 `next_id: u32` (no atomic)

Spec §06/03 §1's `AtomicU32::fetch_add` example assumes concurrent insert paths. Brain's discipline (CLAUDE.md §5 invariant 2) is single-writer-per-shard, encoded via `HnswIndex::insert(&mut self, ...)`. The `&mut` borrow rules out concurrent calls at compile time → plain `u32` is sufficient.

Same call we made on `Wal::append(&mut self)`. Consistent with the discipline.

### 3.3 Duplicate-MemoryId detection: defensive error

Spec §06/03 §10 explicitly says re-inserting an existing MemoryId is a bug; hnsw_rs silently overwrites. We detect and return `HnswError::DuplicateMemoryId { memory_id }` *before* calling hnsw_rs. The internal_id counter is **not advanced** on duplicate (caller's mistake shouldn't burn IDs).

Alternative: panic. Rejected — recoverable error is a better contract for code that may genuinely hit this in chaos/recovery edge cases.

### 3.4 Public surface swap: remove the raw-`usize` API entirely

4.1 shipped `HnswIndex::insert(&mut self, usize, &[f32; D])` and `search(...) -> Vec<(usize, f32)>` as scaffolding. 4.2 replaces both:

- `pub fn insert(&mut self, memory_id: MemoryId, vector: &[f32; D]) -> Result<(), HnswError>`
- `pub fn search(&self, query: &[f32; D], k: usize, ef: Option<usize>) -> Vec<(MemoryId, f32)>`

No `insert_raw` / `search_raw` survives — the MemoryId-aware surface is the only public path, and 4.2's tests exercise it directly. Cleaner; matches the spec's intent that hnsw_rs's `usize` is an internal detail.

### 3.5 Search post-translation: tolerate but log missing reverse map entries

If hnsw_rs's `search` returns a `usize` we don't have in `id_map_reverse` (which shouldn't happen — every inserted memory was added to both maps), the code logs a `tracing::warn!` and skips that result. This is a defense-in-depth measure for partial-state scenarios that may arise during rebuild/maintenance in later phases. v1 result: missing-mapping is treated as "skip" rather than "error" so a single bug doesn't crash a whole search.

### 3.6 No `remove` / `mark_tombstone` yet

Sub-task 4.3 owns the tombstone bitmap; sub-task 4.4 wires it into search. 4.2 keeps `IdMap` insert-only (consistent with hnsw_rs's `insert`-only-during-build pattern). Cleanup of id_map entries on slot reclamation is spec §06/05 §11 — Phase 8 territory.

### 3.7 `IdMap` struct shape

```rust
pub struct IdMap {
    forward: HashMap<[u8; 16], u32>,   // MemoryId.to_be_bytes() → internal id
    reverse: HashMap<u32, [u8; 16]>,
    next_id: u32,
}

impl IdMap {
    pub fn new() -> Self { ... }
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn contains(&self, memory_id: MemoryId) -> bool;
    pub fn insert(&mut self, memory_id: MemoryId) -> Result<u32, MemoryIdAlreadyInserted>;
    pub fn lookup_forward(&self, memory_id: MemoryId) -> Option<u32>;
    pub fn lookup_reverse(&self, internal_id: u32) -> Option<MemoryId>;
}
```

Forward map keyed by `[u8; 16]` (not `MemoryId`) to avoid hashing through `MemoryId`'s `u128` representation. MemoryId implements `Hash` already (derived) so either works; `[u8; 16]` is the on-disk wire form and matches brain-metadata's table key pattern.

Internally, `MemoryId` → `[u8; 16]` is `to_be_bytes()`; reverse is `MemoryId::from_be_bytes()`.

### 3.8 Error variant additions

Add to `HnswError`:

```rust
#[derive(Debug, Error)]
pub enum HnswError {
    #[error("invalid params: {0}")]
    InvalidParams(#[from] IndexParamsError),

    #[error("memory_id already inserted")]
    DuplicateMemoryId { memory_id_bytes: [u8; 16] },

    #[error("id_map exhausted: u32::MAX internal ids allocated")]
    IdMapExhausted,
}
```

Using `memory_id_bytes` rather than `MemoryId` in the variant so `HnswError: Debug` doesn't require `MemoryId: Debug` (it does — but keeping the variant byte-typed avoids future coupling).

## 4. Files touched

- `crates/brain-index/src/idmap.rs` (new) — ~120 LOC including tests.
- `crates/brain-index/src/hnsw.rs` — swap public API; ~30-line diff; existing tests migrate to MemoryId.
- `crates/brain-index/src/lib.rs` — `pub mod idmap;` + re-export `IdMap`.

No changes to brain-core / brain-storage / brain-metadata.

## 5. Tests (gated `#[cfg(test)]`)

### idmap.rs (5 tests)

1. **`new_is_empty`** — fresh `IdMap` has `len() == 0`, `is_empty() == true`.
2. **`insert_allocates_sequential_ids`** — three inserts return `Ok(0)`, `Ok(1)`, `Ok(2)`.
3. **`insert_populates_both_directions`** — after `insert(m)`, `lookup_forward(m) == Some(id)` and `lookup_reverse(id) == Some(m)`.
4. **`duplicate_insert_rejects_and_does_not_advance_id`** — insert m → Ok(0); insert m again → `Err(MemoryIdAlreadyInserted)`; `len() == 1`; next `insert(m2)` returns `Ok(1)` (counter not burned).
5. **`contains_pin`** — `contains(m)` is `false` before insert, `true` after.

### hnsw.rs (existing tests migrate + 3 new)

Migrated tests (replacing 4.1's `usize` calls with `MemoryId`):

6. `insert_with_memory_id_increments_len` (was: `insert_increments_len`).
7. `identical_vector_self_match_returns_memory_id` (was: `identical_vector_self_match_returns_distance_near_zero`).
8. `search_returns_at_most_k` (unchanged behaviour; signature update).
9. `search_results_are_sorted_ascending` (signature update).
10. `ef_search_max_caps_per_query_override` (signature update).
11. `empty_index_search_returns_empty` (unchanged).
12. `new_rejects_invalid_params` (unchanged).
13. `new_with_defaults` (unchanged).
14. `resolve_ef_clamps_to_k_and_ef_search_max` (unchanged).

New tests for 4.2-specific surface:

15. **`duplicate_memory_id_returns_error`** — insert M; second insert returns `HnswError::DuplicateMemoryId`; index `len()` stays at 1.
16. **`search_results_carry_memory_ids`** — insert two distinct MemoryIds; search returns `(MemoryId, f32)` tuples matching the inserted IDs.
17. **`contains_after_insert`** — `idx.contains(m)` is false; after `insert(m, ...)`, true.

Plus an `IdMapExhausted` test would require setting `next_id` close to `u32::MAX` — feasible by exposing an internal-only constructor for tests, OR skip the test (overflow path is documented but expensive to exercise). **Decision:** add a `#[cfg(test)] pub(crate) fn with_next_id(seed: u32) -> Self` constructor on `IdMap` and one test:

18. **`u32_overflow_returns_exhausted`** — seed an `IdMap` at `next_id = u32::MAX`, insert returns `IdMapExhausted`. Confirms the overflow guard works.

Total: 18 tests (5 idmap + 13 hnsw including migrations).

## 6. Verification

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-index"
```

Expected: brain-index goes 13 → 18 tests. Workspace clippy clean.

## 7. Commit

Branch: `feature/brain-index` (continuing). AUTONOMY §5 format:

```
feat(brain-index): MemoryId ↔ usize adapter layer (sub-task 4.2)
```

Body summarises: `IdMap` with `u32` internal IDs (per spec §06/03 §2; saves ~80 MB vs `usize` at 10M-scale), non-atomic counter (single-writer via `&mut self`), defensive duplicate-MemoryId detection, public API now takes `MemoryId` and returns `Vec<(MemoryId, f32)>`, 5 new idmap tests + migrated hnsw tests + 3 new MemoryId-specific tests + u32-overflow guard test.

## 8. Done when

- [ ] `IdMap` struct + 5 tests in `idmap.rs`.
- [ ] `HnswIndex::insert(MemoryId, vector)` and `search(...) -> Vec<(MemoryId, f32)>` are the only public APIs.
- [ ] Duplicate-MemoryId insert returns `HnswError::DuplicateMemoryId` and doesn't burn an internal id.
- [ ] u32 overflow returns `IdMapExhausted`.
- [ ] 18 tests green; clippy clean; workspace tests pass.

PLAN READY.
