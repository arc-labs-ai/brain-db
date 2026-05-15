# Sub-task 16.3 — Entity HNSW per shard

> Per-sub-task plan. Plan-first convention.

## Goal

A per-shard HNSW index over entity embeddings, distinct from the
substrate's memory HNSW. After this sub-task:

- An `EntityHnswIndex` can `insert(entity_id, vector)`, `search(query,
  k)`, `mark_tombstoned(entity_id)`, and `rebuild(...)`.
- Parameters match spec §18/02: M=16, ef_construction=100,
  ef_search=64 (smaller than memory HNSW's ef_construction=200,
  ef_search=64 — entity counts are 10-100× smaller per shard).
- Tombstoned entities are filtered from search results; a full
  `rebuild` purges them and resets the underlying `hnsw_rs::Hnsw`.

Out of scope for 16.3:

- **Persistence to `entity.hnsw` on disk.** In-memory only. The path
  is reserved by 15.3's `ShardPaths::entity_hnsw()` but no I/O
  happens here. Persistence lands as a follow-up after 16.5 (when
  the resolver actually drives entity creation and we have a clear
  shape for what to persist).
- **Concurrency wrapper** (`ArcSwap`-published reads). Phase 16.3
  ships a single-owner index; 16.5+ adds the `SharedEntityHnsw`
  variant when the resolver needs concurrent reads. (Mirrors how the
  substrate's `SharedHnsw` was layered on top of `HnswIndex` in
  sub-task 4.8.)
- **Async embedding worker** — synchronous embed-on-write per phase
  plan F-3.

## Reading list

1. `spec/18_entities/02_storage.md` § "Entity embedding HNSW" — the
   parameter set + 384-dim envelope.
2. `spec/06_ann_index/02_parameters.md` — substrate HNSW parameter
   precedent.
3. `crates/brain-index/src/hnsw.rs` — memory HNSW we mirror.
4. `crates/brain-index/src/idmap.rs` — `IdMap<MemoryId>` pattern.
5. `crates/brain-index/src/tombstones.rs` — `TombstoneBitmap` is
   id-type-agnostic; we reuse it directly.
6. `crates/brain-index/src/params.rs` — `IndexParams` validation
   precedent.

## Pre-flight findings

### F-1 — `IdMap` is `MemoryId`-typed; we don't refactor

The existing `IdMap` is hardcoded to `MemoryId`. Generalizing it
over a type parameter is a bigger change that ripples through the
substrate HNSW. For 16.3 the cleanest answer is to inline a small
parallel mapping inside `EntityHnswIndex`:

```rust
// internal -> EntityId
forward: Vec<Option<EntityId>>,
// EntityId -> internal u32
reverse: HashMap<EntityId, u32>,
```

`forward` is sparse (`Option<>`) so tombstoned-then-removed slots
stay addressable until rebuild. ~30 lines of code; mirrors `IdMap`
without entangling it.

Possible future refactor: extract a `ByteKeyedIdMap` shared by both
indexes. Tracked as a phase-16 follow-up; not blocking.

### F-2 — `TombstoneBitmap` is byte-id agnostic

`TombstoneBitmap` operates on the internal `u32` ID. Reusable as-is
for entity HNSW. We just instantiate a separate `TombstoneBitmap`
per `EntityHnswIndex`.

### F-3 — `IndexParams` defaults are memory-tuned

The existing `IndexParams::default_v1()` is `ef_construction=200`.
Entity HNSW wants `ef_construction=100`. Two paths:

- **New `EntityHnswParams` type** — separate, no field-set drift
  risk with memory HNSW.
- **Reuse `IndexParams`, add `default_entity_v1()` constructor** —
  shared type; one validation path.

**Recommended: new `EntityHnswParams`.** They're conceptually
different — entity HNSW may diverge further (e.g., `ef_search`
becoming caller-supplied per spec §18/01's resolver config). A
distinct type avoids tomorrow's "is this the entity or memory
default?" confusion.

The new type carries the same `validate()` ranges as `IndexParams`
(spec §06/02 ranges apply to both).

### F-4 — Vector dimension

Both substrate and entity HNSW use 384-dim (BGE-small-en-v1.5).
Phase 16.3 hardcodes `D = 384` in `EntityHnswIndex<384>`. If a
future phase changes embedders, both indexes follow at the same
time.

### F-5 — `hnsw_rs::Hnsw` API surface

`hnsw_rs::Hnsw<f32, DistL2>` (or `DistCosine` per substrate
precedent) is the backing type. Methods:

- `Hnsw::new(M, max_elements_hint, max_layer, ef_construction,
  distance) -> Hnsw<f32, _>`
- `insert((vector: &[f32], id: usize))`
- `search(query: &[f32], k, ef_search) -> Vec<Neighbour>` where
  `Neighbour { d_id: usize, distance: f32 }`

L2-normalized BGE vectors mean cosine similarity ≈ negative L2
distance. Memory HNSW uses `DistCosine` per `hnsw.rs`; entity HNSW
matches.

### F-6 — Rebuild semantics

`rebuild(input: I)` where `I: IntoIterator<Item = (EntityId, Vec<f32>)>`:

1. Reset internal state: drop the existing `hnsw_rs::Hnsw`, clear
   `forward` / `reverse`, clear `TombstoneBitmap`, reset `next_id`.
2. Construct a fresh `hnsw_rs::Hnsw` with the same params.
3. Insert every `(EntityId, vector)` from the iterator (skips ones
   whose embeddings the caller already filtered as tombstoned).
4. Return a `RebuildReport`-like struct (count + tombstoned-purged
   count).

Spec §06/04 ("tombstone+rebuild cycle") applies to entity HNSW the
same way as memory HNSW. Memory HNSW's `HnswIndex::rebuild` is the
template; entity HNSW takes `EntityId` instead of `MemoryId`.

### F-7 — Vector type

Spec §18/02 doesn't pin the wire form. We accept `&[f32; 384]` at
the API boundary; the caller is responsible for embedding text
through `brain-embed` (out of scope for 16.3) and passing the
normalized vector. Test fixtures construct fixed-pattern vectors
without invoking the embedder.

## Design decisions

### D1 — Module layout in `brain-index`

```
crates/brain-index/src/
├── entity_hnsw.rs    NEW — EntityHnswIndex + EntityHnswParams
└── ...
```

`entity_hnsw.rs` is self-contained; uses `TombstoneBitmap` and the
`hnsw_rs` crate. Does NOT use the existing `IdMap` (memory-typed)
or `HnswIndex` (memory-typed).

### D2 — `EntityHnswParams` (new type)

```rust
pub struct EntityHnswParams {
    pub m: usize,                 // default 16
    pub ef_construction: usize,   // default 100 (vs memory 200)
    pub ef_search: usize,         // default 64
    pub max_layer: usize,         // 16, same as memory
    pub capacity_hint: usize,     // default 256 (vs memory 1024)
}

impl EntityHnswParams {
    pub const fn default_v1() -> Self { /* spec §18/02 */ }
    pub fn validate(&self) -> Result<(), EntityHnswParamsError> { /* ... */ }
}
```

`capacity_hint=256` matches the spec's "entity count ~1000-10000
typical" — fewer entities than memories, smaller initial allocation.

### D3 — `EntityHnswIndex` struct

```rust
pub struct EntityHnswIndex {
    inner: hnsw_rs::Hnsw<'static, f32, DistCosine>,
    params: EntityHnswParams,
    /// internal u32 -> EntityId. `None` only if rebuild dropped a slot.
    forward: Vec<Option<EntityId>>,
    /// EntityId -> internal u32 (next free is `forward.len()`).
    reverse: HashMap<EntityId, u32>,
    tombstones: TombstoneBitmap,
}

impl EntityHnswIndex {
    pub fn new(params: EntityHnswParams) -> Result<Self, EntityHnswError>;
    pub fn insert(&mut self, id: EntityId, vector: &[f32; VECTOR_DIM])
        -> Result<(), EntityHnswError>;
    pub fn search(&self, query: &[f32; VECTOR_DIM], k: usize)
        -> Result<Vec<(EntityId, f32)>, EntityHnswError>;
    pub fn search_with_ef(&self, query, k, ef_search) -> Result<...>;
    pub fn mark_tombstoned(&mut self, id: EntityId) -> Result<(), EntityHnswError>;
    pub fn is_tombstoned(&self, id: EntityId) -> bool;
    pub fn contains(&self, id: EntityId) -> bool;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn tombstone_count(&self) -> usize;
    pub fn params(&self) -> EntityHnswParams;
    pub fn rebuild<I>(&mut self, entities: I) -> Result<RebuildReport, EntityHnswError>
        where I: IntoIterator<Item = (EntityId, [f32; VECTOR_DIM])>;
}
```

### D4 — Single `EntityHnswError` enum

```rust
#[derive(thiserror::Error, Debug)]
pub enum EntityHnswError {
    #[error("invalid parameters: {0}")]
    Params(#[from] EntityHnswParamsError),
    #[error("entity {0:?} already present")]
    DuplicateEntity(EntityId),
    #[error("entity {0:?} not present")]
    UnknownEntity(EntityId),
    #[error("k must be > 0")]
    InvalidK,
    #[error("ef_search {ef} is above ef_search_max {max}")]
    EfSearchTooLarge { ef: usize, max: usize },
}
```

Smaller surface than the memory HnswError because entity HNSW has
no I/O in 16.3 (no save/load failures).

### D5 — Search filters tombstoned post-hoc

`hnsw_rs::Hnsw::search` returns top-k by approximate distance with
no filter callback in the underlying crate. We over-fetch by a
factor of 2× (or to `min(k * 2, ef_search)`), drop tombstoned IDs,
and truncate to `k`. Same approach as substrate `HnswIndex::search`.

If the over-fetch isn't enough (e.g., 50% of the index is
tombstoned), the post-filter result may have <k entries. Acceptable
for 16.3 — rebuild is the recovery path. The resolver caller in
16.5 doesn't need exact top-k semantics; it asks for top-K and
takes what's available.

### D6 — `RebuildReport`

```rust
pub struct RebuildReport {
    pub inserted: usize,
    /// Tombstoned-but-also-supplied: usually zero (callers pre-filter).
    pub tombstoned_skipped: usize,
}
```

Matches the memory HNSW's `crate::rebuild::RebuildReport` shape
where possible (cross-crate consistency).

### D7 — Tests

In `entity_hnsw.rs`:

- `params_default_matches_spec` — M=16, ef_c=100, ef_s=64,
  max_layer=16, capacity_hint=256.
- `params_validate_rejects_out_of_range` — same edge cases as
  memory.
- `insert_then_contains` — inserted ID is contained; new ID isn't.
- `insert_rejects_duplicate` — second insert with same EntityId
  errors.
- `search_returns_inserted_self_first` — insert one vector,
  search for it, top-1 is itself with distance ~0.
- `search_topk_size_bounded` — insert 10 distinct vectors, search
  k=5, len(result) ≤ 5.
- `mark_tombstoned_excludes_from_search` — insert 3, tombstone 1,
  search k=3 returns 2 of them; tombstoned not present.
- `is_tombstoned_round_trip` — tombstone marks; query reads.
- `tombstone_count_tracks_marks` — bumps on each tombstone.
- `rebuild_drops_tombstones_and_resets_state` — insert 5,
  tombstone 2, rebuild with 3 fresh entities, search returns
  only the 3 new IDs.
- `rebuild_report_counts_inserted` — count matches input.
- `unknown_entity_tombstone_errors` — `mark_tombstoned` on missing
  EntityId returns `UnknownEntity`.
- `search_with_ef_below_default_clamped` — passing ef_search < k
  clamps to k (matches memory HNSW behavior).

## File plan

- `crates/brain-index/src/entity_hnsw.rs` — **new**, ~350 lines +
  tests.
- `crates/brain-index/src/lib.rs` — `pub mod entity_hnsw;` plus
  re-exports of `EntityHnswIndex`, `EntityHnswParams`,
  `EntityHnswError`.

No new external dependencies. `hnsw_rs` is already used by the
memory HNSW.

## Done-when

- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace
  --tests` clean.
- All ~13 tests in `entity_hnsw.rs` type-check.
- 15.5's `knowledge_compat` substrate-only regression still
  passes — `brain-index` adds a new module; no substrate hot path
  touches it.
- One commit: `feat(index): 16.3 — entity HNSW per shard`.

## Risk register

| Risk | Mitigation |
|---|---|
| `hnsw_rs::Hnsw::search` returns approximate results — top-1 self-match may not be the first | Use a moderate `ef_search` (≥64) in tests; assert "result contains the inserted id" rather than "result[0] == inserted_id" where the test would be sensitive to ordering. |
| Vec<Option<EntityId>> grows unboundedly even after rebuild if we don't reset | `rebuild` constructs a fresh `Vec`; old slots dropped. Asserted in `rebuild_drops_tombstones_and_resets_state`. |
| `EntityHnswParams` validation drifts from `IndexParams` (silently allowing wider ranges) | New error type but identical ranges; one extra unit test asserts the M / ef_construction / ef_search ranges match spec §06/02 §1 literally. |
| Post-hoc tombstone filter over-fetches the entire index when most entries are tombstoned | Cap the over-fetch at `min(k * 2, max(ef_search, 32))`. Document the failure mode (rare); rebuild is the operator-side fix. |
| `EntityHnswIndex` is not `Send` because `hnsw_rs::Hnsw` isn't | Mirror the substrate HNSW's `!Send` constraint. The shard executor owns the index inside its Glommio task — `Send` is not required there. SharedEntityHnsw (later) adds an ArcSwap envelope if needed. |
| Capacity hint too small triggers reallocation on entity bulk-insert | 256 is a hint, not a cap. hnsw_rs grows on demand. Phase 14 perf work can tune the default later. |

## Open questions for your approval

1. **`EntityHnswParams` new type (D2/F-3)** — not a constructor on
   the existing `IndexParams`? **Recommended: new type.** Avoids
   memory-vs-entity default confusion; matches the conceptual
   distinction in spec §18/02.
2. **No persistence in 16.3** — `entity.hnsw` file path reserved
   but unused? **Recommended: yes.** Persistence wires up after
   16.5 when the resolver drives entity creation and we know the
   shape; 16.3 ships in-memory only. The phase doc's "done when"
   doesn't list persistence.
3. **No concurrency wrapper in 16.3** — `EntityHnswIndex` is
   single-owner; `SharedEntityHnsw` lands when the resolver in
   16.5 needs concurrent reads? **Recommended: yes.** Mirrors how
   the substrate's `SharedHnsw` was a separate sub-task.
4. **Inline mapping (D1/F-1)** — `Vec<Option<EntityId>>` +
   `HashMap<EntityId, u32>` inside `EntityHnswIndex`, not a
   generalized `IdMap<Id>` refactor? **Recommended: inline.** Keeps
   16.3 to one file; the generalization is a follow-up.

## Workflow

On your nod: implement, run `cargo zigbuild --target
x86_64-unknown-linux-gnu --workspace --tests`, commit as
`feat(index): 16.3 — entity HNSW per shard`, then stop and draft
16.4's plan (trigram index + Jaccard scoring).
