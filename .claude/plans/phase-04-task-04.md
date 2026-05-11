# Phase 4 — Task 4.4: Search with post-filtering

**Classification:** moderate. Changes `HnswIndex::search` from a thin pass-through to the spec's full post-search contract: similarity instead of distance, implicit tombstone filter, user filter callback, over-fetch + bailout retry. Spec anchors: `spec/06_ann_index/04_search.md` + `spec/06_ann_index/09_filtering.md`.

## 1. Scope

In:

- `crates/brain-index/src/hnsw.rs` — rewrite `search` signature and body. Add `over_factor` constant; add `search_with_default_filter` as a no-filter convenience. Update existing tests; add ~6 new.

Out (deferred):

- **Per-filter selectivity estimates** (spec §09 §6) — needs metadata-store statistics. Phase 8 worker territory.
- **`AnnFilter` struct** (spec §09 §3) — high-level filter helper with kind/context/salience fields. Phase 7 (operations) composition; 4.4 ships only the `impl Fn(MemoryId) -> bool` callback shape.
- **Context inverted index** (spec §09 §8) — large optimization for very-selective context filters. Out of v1 4.4.
- **Brute-force fallback for N < 1000** (spec §04 §10) — optional; not v1.
- **Cross-query metadata caching** (spec §09 §12) — Phase 11 observability adjacent.
- **Search result caching** (spec §04 §11) — explicitly not v1 per spec.

## 2. Spec quotes that bind the design

> **§04 §1 (signature shape):**
> ```rust
> Some((*memory_id, 1.0 - r.distance))  // distance → similarity
> ```
> Results expose **similarity** (1.0 = identical, 0 = orthogonal, -1 = opposite), not raw distance.
>
> **§04 §7:** "Tombstoned memories (marked deleted but not yet removed from HNSW) may appear in search results. The substrate filters them out post-search via the filter mechanism. … If too many results are tombstoned, search may return fewer than K results. The substrate detects this and re-queries with a higher ef."
>
> **§09 §2 (post-filter pattern):**
> ```rust
> let candidates = hnsw.search(query, k * over_factor, ef);
> candidates.into_iter()
>     .filter(|c| filters_match(c.memory_id, filters))
>     .take(k)
>     .collect()
> ```
>
> **§09 §7 (bailout retry):**
> ```rust
> while results.len() < k && ef < ef_max {
>     // ... search + filter
>     if results.len() < k { ef *= 2; }
> }
> ```
>
> **§09 §10:** filters AND together; OR is the caller's job via a custom closure.

## 3. Design decisions

### 3.1 Return similarity, not distance — breaking change to `search`

Spec §04 §1 is explicit: external results carry similarity in `[-1, 1]`. 4.1's `search` returned hnsw_rs's raw distance because the post-search conversion was deferred. 4.4 is the right sub-task to fix it.

```rust
// Before 4.4: Vec<(MemoryId, f32)>   where f32 was distance (lower = better)
// After 4.4:  Vec<(MemoryId, f32)>   where f32 is similarity (higher = better)
```

`similarity = 1.0 - distance`. For L2-normalised vectors and `DistCosine`, this equals the dot product directly — clean.

Existing test assertions migrate:
- `identical_vector_self_match`: previously `distance.abs() < 1e-5`; becomes `similarity > 1.0 - 1e-5`.
- `search_results_are_sorted_ascending` (by distance): becomes `search_results_are_sorted_descending` (by similarity).

### 3.2 Filter callback shape: `impl Fn(MemoryId) -> bool`

Caller-supplied predicate. Returns `true` to keep, `false` to drop. The tombstone filter is always applied **in addition** — even with `|_| true` (no extra filter), tombstoned memories are excluded.

```rust
pub fn search<F: Fn(MemoryId) -> bool>(
    &self,
    query: &[f32; D],
    k: usize,
    ef: Option<usize>,
    filter: F,
) -> Vec<(MemoryId, f32)>
```

Plus a convenience for the common no-filter case (just exclude tombstones):

```rust
pub fn search_active(&self, query, k, ef) -> Vec<(MemoryId, f32)> {
    self.search(query, k, ef, |_| true)
}
```

Spec §09 §14's "minimal filter" fast path is implicit — if the caller passes `|_| true`, the closure is inlined and only the tombstone check survives. No struct-level "is_minimal" flag needed.

### 3.3 Over-fetch: 2× by default, capped at 4×

Spec §09 §2's `over_factor` is per-filter-selectivity. v1 4.4 uses a fixed 2× with a 4× cap inside the retry loop. Phase 8 can introduce selectivity-aware estimation.

```rust
const OVER_FACTOR: usize = 2;
const OVER_FACTOR_CAP: usize = 4;
```

If the filter rejects everything, retry escalates the fetch size to `k * 4`. Beyond that, we accept fewer-than-K results.

### 3.4 Bailout retry: double `ef` up to `ef_search_max`

Per spec §09 §7. The loop:

```rust
let mut ef = self.resolve_ef(k, ef_override);
let mut fetch_multiplier = OVER_FACTOR;
let mut results = Vec::new();
loop {
    let candidates = self.inner.search(query, k * fetch_multiplier, ef);
    results = candidates.into_iter()
        .filter_map(|n| /* tombstone + user filter + id_map */)
        .take(k)
        .collect();
    if results.len() >= k { break; }
    if ef >= self.params.ef_search_max && fetch_multiplier >= OVER_FACTOR_CAP {
        break; // give up; return what we have
    }
    if fetch_multiplier < OVER_FACTOR_CAP {
        fetch_multiplier = OVER_FACTOR_CAP;
    } else {
        ef = (ef * 2).min(self.params.ef_search_max);
    }
}
```

Bounded: at most ~log2(ef_search_max / initial_ef) iterations. For default ef_search=64 → max iterations ≈ 4 (64 → 128 → 256 → 500).

### 3.5 Implicit tombstone filter applies always

No opt-out. Spec §06/05 §2: "The filter is implicit in every search." Tests confirm: even with `|_| true`, tombstoned memories are dropped.

### 3.6 `search` keeps its name; `search_active` is the convenience

The phase doc's table called this "search(query, k, filter)". Two options:
- Single `search(query, k, ef, filter)` — caller always passes a filter (closure).
- `search` + `search_active` — closure-free common case.

I'll go with both for ergonomics. `search_active` delegates to `search(..., |_| true)`.

### 3.7 No new error variants

Filter callback can't fail (it returns `bool`). The bailout-exhausted case returns fewer-than-K results, not an error — spec §09 §7 ("the substrate gives up and returns whatever it found"). Logging via `tracing::debug!` for observability.

### 3.8 No special-case for empty index

`search` on an empty index returns `[]` immediately. The bailout loop's first iteration handles this — hnsw_rs returns `[]`, filter chain returns `[]`, `results.len() == 0 < k` but `len(hnsw) == 0` so further retries are pointless. We add an early-return guard: `if self.is_empty() { return Vec::new(); }`.

## 4. Files touched

- `crates/brain-index/src/hnsw.rs` — rewrite `search` body + signature; add `search_active`. ~80 LOC diff including tests.
- `crates/brain-index/src/lib.rs` — no change (no new public types).

No new deps. No changes to brain-core / brain-storage / brain-metadata.

## 5. Tests (gated `#[cfg(test)]`)

### Migrated from 4.1/4.2 (signature/return-value updates)

1. `identical_vector_self_match_returns_memory_id` — now checks `similarity > 1.0 - 1e-5`.
2. `search_results_are_sorted_descending` (renamed from `_ascending`) — distances inverted.
3. `search_returns_at_most_k` (unchanged behaviour).
4. `ef_search_max_caps_per_query_override` (unchanged).
5. `empty_index_search_returns_empty` (unchanged).
6. `search_results_carry_memory_ids` (unchanged).

### New 4.4 tests

7. **`tombstoned_memories_excluded`** — insert 3, mark 1 tombstoned, `search_active(q, 5, None)` returns 2 results. The tombstoned MemoryId is absent.
8. **`custom_filter_excludes`** — insert 5 memories with mid(1)..=mid(5); search with filter `|m| m.slot() % 2 == 0` returns only even-slot results.
9. **`filter_composition_with_tombstones`** — insert 4, mark 1 tombstoned, search with filter excluding mid(2). Expect 2 results (the one tombstoned + the one filtered = 2 dropped).
10. **`search_active_excludes_tombstones`** — `search_active` is the no-extra-filter helper; tombstone filter still applies.
11. **`bailout_retries_when_filter_drops_most`** — insert 100 vectors, mark 95 tombstoned, request k=3. Even with default ef=64 and 5% pass rate, the retry loop scales up and returns 3. Pins the retry behaviour.
12. **`over_fetch_caps_with_always_false_filter`** — insert 10, search with `|_| false` filter. Returns empty (not infinite loop). Pins the bounded retry.
13. **`similarity_score_in_range`** — insert orthogonal vectors; similarity ~0. Pin `[-1, 1]` range.
14. **`similarity_score_descending_order`** — same as #2 but assert similarity strictly non-increasing.

Total: 6 migrated + 8 new = 14 search-area tests in hnsw.rs. brain-index 34 → 38 (4 net new; 4 migrated + 8 new, 2 of which replace existing names).

Actually let me recount: 4.1+4.2+4.3 left hnsw.rs with 16 tests, plus 4 in params.rs, 6 in idmap.rs, 8 in tombstones.rs = 34. 4.4 modifies 5 existing hnsw tests (signature shifts but counted as same) and adds 6 truly new. So brain-index 34 → 40.

## 6. Verification

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-index"
```

Expected: brain-index 34 → 40 tests. Workspace clippy clean.

## 7. Commit

Branch: `feature/brain-index` (continuing). AUTONOMY §5 format:

```
feat(brain-index): search with post-filtering + similarity scores (sub-task 4.4)
```

Body summarises: search returns similarity (spec §04 §1, breaking change vs 4.1/4.2's distance), `impl Fn(MemoryId) -> bool` filter callback, implicit tombstone filter, 2×/4× over-fetch with bailout that doubles `ef` up to `ef_search_max`, `search_active` convenience for the no-extra-filter case. 6 truly new tests + 5 migrations.

## 8. Done when

- [ ] `search` returns similarity in `[-1, 1]` (not distance).
- [ ] `search` takes a filter callback `impl Fn(MemoryId) -> bool`.
- [ ] Tombstoned memories are excluded regardless of filter.
- [ ] Bailout retry kicks in when results < k; bounded by `ef_search_max` and 4× over-fetch cap.
- [ ] `search_active` no-filter convenience exists.
- [ ] 40 tests green in brain-index; clippy clean.

PLAN READY.
