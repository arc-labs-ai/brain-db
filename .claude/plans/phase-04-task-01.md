# Phase 4 — Task 4.1: Wrap `hnsw_rs::Hnsw` with our params

**Classification:** moderate. Sets up `brain-index` for real (it's currently a 25-line stub). Establishes the const-generic `HnswIndex<D>` shape every later sub-task will extend; settles params, distance metric, and the raw-`usize` insert/search surface that 4.2's id_map sits on top of.

**Spec:** `spec/06_ann_index/02_parameters.md` (defaults: M=16, ef_construction=200, ef_search=64). Cross-checked `01_hnsw_primer.md` §7 (distance metric: cosine = dot product on L2-normalised vectors), `03_insertion.md` §29 (type alias confirmed: `hnsw_rs::Hnsw<f32, DistCosine>`), `04_search.md` (search visits ~1000 nodes for 1M-vector index, ~50 µs distance compute).

## 1. Scope

In:

- `crates/brain-index/Cargo.toml` — enable `hnsw_rs` + `tracing` deps; add `[dev-dependencies]` for criterion and `tempfile` (used by later sub-tasks; harmless to declare now).
- `crates/brain-index/src/lib.rs` — replace stub with real module declarations + re-exports.
- `crates/brain-index/src/params.rs` (new) — `IndexParams` struct + spec defaults + validation.
- `crates/brain-index/src/hnsw.rs` (new) — `HnswIndex<const D: usize>` + `insert(usize, &[f32; D])` + `search(&[f32; D], k, ef) -> Vec<(usize, f32)>` + a `len()` accessor.
- 8–10 tests across both modules.

Out (deferred):

- **MemoryId mapping** — sub-task 4.2. 4.1's `insert(id: usize, ...)` accepts raw `usize` matching hnsw_rs's internal type.
- **Tombstone bitmap** — sub-task 4.3.
- **Post-filtering search** — sub-task 4.4. 4.1's `search` returns whatever hnsw_rs returns, no filter callback.
- **Persistence (snapshot/load)** — sub-task 4.5.
- **Rebuild** — sub-task 4.6.
- **Recall benchmark** — sub-task 4.7.
- **Concurrency wrapper** (`SharedHnsw` with `ArcSwap`) — sub-task 4.8.

## 2. Spec quotes that bind the design

> **§02 §1 (defaults):**
> | Parameter | Default | Range |
> |---|---|---|
> | M | 16 | 4–64 |
> | ef_construction | 200 | 50–500 |
> | ef_search | 64 | 10–500 |
>
> **§02 §5 (ef vs K):** "ef_search must be >= K… The convention is `ef_search = max(K, default_ef_search)`."
>
> **§02 §8 (config keys):**
> ```
> [ann]
> m = 16
> ef_construction = 200
> ef_search = 64
> ef_search_max = 500
> ```
>
> **§01 §7:** "Brain's vectors are L2-normalized; cosine similarity equals the dot product. The hnsw_rs crate supports cosine distance directly."
>
> **§03 §29:** `hnsw: hnsw_rs::Hnsw<f32, DistCosine>` — type alias confirmed.

## 3. Design decisions

### 3.1 `HnswIndex<const D: usize>` const-generic over vector dim

Confirmed by the user. `pub const VECTOR_DIM: usize = 384;` lives at workspace level (or alongside `IndexParams`); all type signatures take `[f32; D]`. The compile-time dim check matches brain-storage's `Slot { vector: [f32; 384] }`. Future multi-dim work bumps a major version.

For ergonomic test code we add a type alias `pub type Hnsw384 = HnswIndex<384>;` (not exported as a top-level convention; callers should use the full `HnswIndex<384>` form).

### 3.2 Distance metric: `DistCosine`

Per spec §01 §7 + §03 §29. BGE-small outputs L2-normalised vectors; cosine similarity reduces to dot product. hnsw_rs's `DistCosine` returns `1 - dot_product` (a *distance*, lower is better). We don't pre-normalise — the embedding layer (Phase 5) is responsible for that, and 4.1 takes whatever vectors the caller hands in.

A test pins the metric semantics: identical vectors return distance 0; orthogonal unit vectors return ~1.

### 3.3 `IndexParams` with builder-free struct + validate()

```rust
pub struct IndexParams {
    pub m: usize,                 // default 16
    pub ef_construction: usize,   // default 200
    pub ef_search: usize,         // default 64
    pub ef_search_max: usize,     // default 500; caps per-query overrides
}

impl IndexParams {
    pub const fn default_v1() -> Self { ... }
    pub fn validate(&self) -> Result<(), IndexParamsError> { ... }
}
```

No builder pattern (overkill for 4 fields). `Default` impl returns `default_v1()`. `validate()` enforces spec ranges (M ∈ 4..=64, ef_construction ∈ 50..=500, ef_search ∈ 10..=500, ef_search_max ≥ ef_search).

### 3.4 `HnswIndex::insert` and `search` signatures

```rust
impl<const D: usize> HnswIndex<D> {
    pub fn new(params: IndexParams) -> Result<Self, HnswError> { ... }
    pub fn insert(&mut self, id: usize, vector: &[f32; D]);
    pub fn search(&self, query: &[f32; D], k: usize, ef: Option<usize>) -> Vec<(usize, f32)>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

- `insert` takes `&mut self` (encodes single-writer-per-shard at the type level; matches `Wal::append`).
- `search` takes `&self` (concurrent reads OK).
- `ef: Option<usize>` — `None` uses `params.ef_search`; `Some(v)` clamps to `[k, params.ef_search_max]`. Implements spec §02 §5's `ef = max(K, default)` rule.
- Return: `Vec<(id, distance)>` sorted ascending by distance. hnsw_rs already returns sorted; we pass through.

### 3.5 No tombstone awareness yet

Sub-task 4.1 doesn't know about tombstones. `search` returns whatever hnsw_rs returns. 4.4 wraps `search` with the filter callback that consults the tombstone bitmap (4.3).

### 3.6 `HnswError` taxonomy

```rust
#[derive(thiserror::Error, Debug)]
pub enum HnswError {
    #[error("invalid params: {0}")]
    InvalidParams(#[from] IndexParamsError),
    #[error("insert into capacity-bounded index: id {id} exceeds nb_layer capacity")]
    CapacityExceeded { id: usize },
}
```

`hnsw_rs` doesn't surface most insert failures (it's an in-memory structure with no I/O). The error space stays minimal in 4.1; 4.5 (persistence) will add I/O variants.

### 3.7 hnsw_rs internal locks: accepted

Per spec §08 §8 — hnsw_rs has internal locks for its (unused) multi-writer mode. ~5% overhead in single-writer. We don't patch it.

### 3.8 No SIMD intrinsics in 4.1

Spec §04 §3 mentions AVX2/NEON SIMD for dot products. hnsw_rs handles its own SIMD internally; 4.1 doesn't write custom kernels. If recall@10 misses ≥0.95 in 4.7, we revisit.

## 4. Files touched

- `crates/brain-index/Cargo.toml` — add `hnsw_rs.workspace = true`, `tracing.workspace = true`. Optional dev-deps come with later sub-tasks.
- `crates/brain-index/src/lib.rs` — replace placeholder with `pub mod hnsw; pub mod params; pub use ...`.
- `crates/brain-index/src/params.rs` (new) — `IndexParams`, `IndexParamsError`, defaults, validation.
- `crates/brain-index/src/hnsw.rs` (new) — `HnswIndex<D>`, `HnswError`, the two operations.

No edits to brain-core, brain-storage, brain-metadata. No new workspace deps (`hnsw_rs` is already declared at the workspace level).

## 5. Tests (gated `#[cfg(test)]`; no `not(miri)` needed unless hnsw_rs uses syscalls)

### params.rs (4 tests)

1. **`default_v1_matches_spec`** — `IndexParams::default_v1()` returns `{ m: 16, ef_construction: 200, ef_search: 64, ef_search_max: 500 }`.
2. **`validate_accepts_spec_range`** — defaults pass `validate()`.
3. **`validate_rejects_out_of_range`** — M=0 fails; M=128 fails; ef_construction=0 fails; ef_search > ef_search_max fails.
4. **`validate_rejects_ef_search_max_below_ef_search`** — pins the constraint.

### hnsw.rs (6 tests)

5. **`new_with_defaults`** — `HnswIndex::<4>::new(IndexParams::default_v1())` constructs without panic; `len() == 0`.
6. **`insert_increments_len`** — insert 3 vectors → `len() == 3`.
7. **`identical_vector_self_match`** — insert one vector at id=42; search for that same vector with k=1 returns `[(42, ~0.0)]`. Distance ≤ 1e-6.
8. **`search_returns_at_most_k`** — insert 5 vectors; search with k=3 returns ≤ 3 results.
9. **`search_results_are_sorted_ascending`** — insert 5 distinct vectors; search returns distances in ascending order.
10. **`ef_search_max_caps_per_query_override`** — `search(q, k=10, Some(9999))` doesn't panic; uses the cap (`ef_search_max = 500`).
11. **`empty_index_returns_empty`** — fresh index, search returns `vec![]`.

(11 tests across 2 files; can prune if hnsw_rs's API forces structural changes.)

## 6. Verification

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-index"
```

Expected: brain-index goes from 1 test (the placeholder marker) to ~12 tests (1 prior + 11 new). Workspace clippy clean.

## 7. Commit

Branch: new `feature/brain-index` from `main`. AUTONOMY §5 format:

```
feat(brain-index): HnswIndex wrapper around hnsw_rs with spec defaults (sub-task 4.1)
```

Body summarises: const-generic `HnswIndex<const D: usize>`, `IndexParams` with M=16/ef_construction=200/ef_search=64/ef_search_max=500, `DistCosine` metric, raw-`usize`-id insert + search surface, 11 new tests. 4.2 next layers the MemoryId adapter.

## 8. Done when

- [ ] `crates/brain-index/src/{lib,params,hnsw}.rs` exist with the surface above.
- [ ] `HnswIndex::<384>::new(params)` constructs cleanly.
- [ ] `insert` + `search` round-trip a small fixture (identical-vector match returns distance ≤ 1e-6).
- [ ] 11 tests green; `just verify` workspace-wide green.
- [ ] No new workspace-level deps (only enables existing).

PLAN READY.
