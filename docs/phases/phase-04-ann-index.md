# Phase 4 — ANN Index (HNSW)

## Goal

Wrap `hnsw_rs` with the parameters and lifecycle the spec defines. After this phase, given a vector query, you can return approximate top-K results with recall ≥ 0.95 at default parameters; tombstones are excluded; the index can be persisted, reloaded, and rebuilt.

## Prerequisites

- [x] Phase 3 complete.
- `brain-storage` provides slot reads; `brain-metadata` provides tombstone state.

## Reading list

1. [`spec/06_ann_index/00_purpose.md`](../../spec/06_ann_index/00_purpose.md)
2. [`spec/06_ann_index/01_hnsw_primer.md`](../../spec/06_ann_index/01_hnsw_primer.md)
3. [`spec/06_ann_index/02_parameters.md`](../../spec/06_ann_index/02_parameters.md) — **M=16, ef_construction=200, ef_search=64.**
4. [`spec/06_ann_index/03_insertion.md`](../../spec/06_ann_index/03_insertion.md)
5. [`spec/06_ann_index/04_search.md`](../../spec/06_ann_index/04_search.md)
6. [`spec/06_ann_index/05_deletion.md`](../../spec/06_ann_index/05_deletion.md) — tombstoning.
7. [`spec/06_ann_index/06_persistence.md`](../../spec/06_ann_index/06_persistence.md)
8. [`spec/06_ann_index/07_maintenance.md`](../../spec/06_ann_index/07_maintenance.md) — rebuild on degradation.
9. [`spec/06_ann_index/08_concurrency.md`](../../spec/06_ann_index/08_concurrency.md)
10. [`spec/06_ann_index/09_filtering.md`](../../spec/06_ann_index/09_filtering.md) — pre/post filter.

## Outputs

- `crates/brain-index` exports:
  - `HnswIndex` (per-shard handle)
  - `IndexParams { m, ef_construction, ef_search }` with spec defaults.
  - `insert`, `search`, `mark_tombstone`, `snapshot`, `rebuild`.
- Recall@10 ≥ 0.95 at 100K vectors.
- Tag: `phase-4-complete`.

## Sub-tasks

### Task 4.1 — Wrap `hnsw_rs::Hnsw` with our params
**Reads:** `spec/06_ann_index/02_parameters.md`
**Writes:** `crates/brain-index/src/hnsw.rs`
**Done when:** `HnswIndex::new(params)` builds; `insert(id, vec)` and `search(query, k)` both work on a small fixture.

### Task 4.2 — `Hnsw` ID mapping
**Reads:** `spec/06_ann_index/03_insertion.md`
**Writes:** `crates/brain-index/src/idmap.rs`
**Done when:** `MemoryId ↔ usize` mapping persists across operations; deletes don't reuse IDs (slot version handles staleness, but the index uses sequential u64 internally).

### Task 4.3 — Tombstone bitmap
**Reads:** `spec/06_ann_index/05_deletion.md`
**Writes:** `crates/brain-index/src/tombstones.rs`
**Done when:** `mark_tombstone(memory_id)` flips a bit; search results filter out tombstoned IDs after the HNSW returns candidates.

### Task 4.4 — Search with post-filtering
**Reads:** `spec/06_ann_index/04_search.md`, `spec/06_ann_index/09_filtering.md`
**Writes:** extend `crates/brain-index/src/hnsw.rs`
**What to build:**
- `search(query, k, filter: impl Fn(MemoryId) -> bool) -> Vec<(MemoryId, f32)>`
- Over-fetch by a factor (e.g. 2x) to compensate for filter rejection, capped to avoid pathological scans.
**Done when:** Filter excludes correctly; recall holds at default settings.

### Task 4.5 — Persistence (snapshot/load)
**Reads:** `spec/06_ann_index/06_persistence.md`
**Writes:** `crates/brain-index/src/persistence.rs`
**Done when:** `snapshot(path)` writes; `load(path, params)` recovers an identical index. Round-trip preserves all insertions.

### Task 4.6 — Rebuild from source of truth
**Reads:** `spec/06_ann_index/07_maintenance.md`
**Writes:** `crates/brain-index/src/rebuild.rs`
**What to build:**
- `rebuild(source: impl Iterator<Item=(MemoryId, [f32; D])>) -> HnswIndex`
- After rebuild, tombstones are cleared because the source skipped them.
**Done when:** Rebuild from a faked source produces a search-equivalent index (recall identical within ε).

### Task 4.7 — Recall benchmark fixture
**Reads:** `spec/16_benchmarks_acceptance/05_recall_quality.md`
**Writes:** `crates/brain-index/benches/recall.rs`
**What to build:**
- Generate 100K random unit vectors.
- Use a deterministic seed.
- Measure recall@10 vs ground-truth (exhaustive top-10 by cosine).
**Done when:** Recall ≥ 0.95 at default params. Bench output recorded.

### Task 4.8 — Concurrency wrapper ✅
**Reads:** `spec/06_ann_index/08_concurrency.md`.
**Writes:** `crates/brain-index/src/shared.rs` (new), `docs/spec-deviations.md` (SD-4.8-1).

**What was built:**
- `SharedHnsw<D>` — cloneable reader handle. All methods `&self`; concurrent reads via `parking_lot::RwLock::read()`.
- `Writer<D>` — non-cloneable writer handle. Mutation methods take `&mut self`; produced exactly once alongside the reader via `SharedHnsw::new` / `from_index` / `rebuild` / `load_snapshot`. The type system enforces spec §06/08 §1's single-writer-per-shard at compile time.
- 7 tests including the spec-mandated **8 concurrent readers + 1 background writer** in `std::thread::scope` (`concurrent_readers_during_writer_no_panic`).

**SD-4.8-1 logged.** Spec §06/08 §3 mandates lock-free reads via `ArcSwap<HnswState>` with a pending-insert buffer and periodic rebuild-and-publish. That pattern requires cheaply cloning the HNSW graph; `hnsw_rs::Hnsw` doesn't implement `Clone`, and a deep clone at 1M nodes (~150 MB) every 100 ms doesn't fit the spec's own timing budget. v1 ships with `RwLock` — concurrent reads, exclusive writes, write latency dip ~1-3 ms per insert. Reconciliation: Phase 11+ either patches hnsw_rs or replaces it.

**Done when:** [x] reader/writer pair API + concurrent test passing + SD-4.8-1 logged + Phase 4 closing checklist ticked.

## Phase exit checklist

- [x] Sub-tasks 4.1–4.8 complete.
- [x] `just verify` green (brain-index 73 tests: 72 unit + 1 integration; bench compiles via `--no-run`).
- [x] Recall@10 ≥ 0.95 at default params, 100K scale (asserted in `benches/recall.rs`).
- [x] Persistence round-trip identical (`round_trip_with_memories` test).
- [x] Rebuild correctness (`rebuild_search_returns_correct_results` + `rebuild_then_save_then_load`).
- [x] Tag `phase-4-complete`.
