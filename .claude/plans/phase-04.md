# Phase 4 — ANN Index (HNSW)

Orientation plan. Surfaces the spec-grounded decisions before sub-task 4.1's plan goes in. Implementation lives in `crates/brain-index/` (currently a 25-line stub).

## 0. Goal

`brain-index` exposes an `HnswIndex` per shard. Given a 384-dim query vector and a tombstone filter, it returns the approximate top-K nearest active memories with recall@10 ≥ 0.95 at default parameters. Supports insert (single-writer per shard), tombstone-marking, snapshot/load, and rebuild from an external source iterator. Tag: `phase-4-complete`.

## 1. Spec grounding

| Spec § | Topic | Anchors |
|---|---|---|
| 00 Purpose | why HNSW (vs flat-scan / IVF / FAISS) | Read first; no code anchor. |
| 01 Primer | layered graph structure | Read for vocab (entry point, layers, M, ef). |
| **02 Parameters** | **M=16, ef_construction=200, ef_search=64** (overridable up to 500). Per-query `ef = max(K, default)` | Sub-task 4.1 — encode as `IndexParams { m, ef_construction, ef_search }`. |
| 03 Insertion | layer assignment, neighbour selection (heuristic) | hnsw_rs handles this; we configure params and call insert. |
| 04 Search | greedy descent + beam search at bottom layer | hnsw_rs handles this; we call search. |
| **05 Deletion** | **tombstone-bitmap; periodic rebuild compacts** | Sub-task 4.3 + 4.6. hnsw_rs doesn't support true delete; tombstone post-filter is the only option. |
| **06 Persistence** | **rebuild from arena+metadata is the source of truth; snapshot is optional fast-restart only** | Sub-tasks 4.5, 4.6. Snapshot format: BHN0 magic + 64 B header + hnsw_rs serialization + BLAKE3 footer. |
| 07 Maintenance | periodic rebuild when tombstone ratio crosses threshold | Phase 8 worker; sub-task 4.6 provides the rebuild primitive. |
| **08 Concurrency** | **`ArcSwap<HnswState>` for lock-free reads; writer task batches inserts in a pending buffer flushed every 1000 inserts / 100 ms / on read-after-write hint** | Sub-task 4.8. v1 might ship without ArcSwap and add it in Phase 7+. |
| 09 Filtering | pre/post; we use post (cheap and supported) | Sub-task 4.4. Over-fetch 2× then filter, capped to prevent pathological scans. |
| 10 Failure modes | corrupted snapshot → fall back to rebuild; stale snapshot detected by LSN | Inside sub-task 4.5. |
| 11 Open questions | warm rebuild (partial readiness), multi-writer experiments | Out of v1 scope. |

## 2. Crate-level structure

```
crates/brain-index/
├── Cargo.toml          (add: hnsw_rs, blake3, crc32c, tracing; dev: criterion, brain-storage path-dep for rebuild test, brain-metadata path-dep)
└── src/
    ├── lib.rs          (re-exports + module declarations)
    ├── params.rs       (IndexParams, defaults, validation)
    ├── hnsw.rs         (HnswIndex — single-threaded write, &mut self)
    ├── idmap.rs        (MemoryId ↔ usize bi-directional map)
    ├── tombstones.rs   (Vec<bool> bitmap, mark/clear, query)
    ├── persistence.rs  (snapshot/load with BHN0 file format)
    ├── rebuild.rs      (consume Iterator<Item=(MemoryId, Vec<f32>)>)
    └── shared.rs       (Phase 4.8 — ArcSwap<HnswState> wrapper; may slip)
```

Plus eventually `benches/recall.rs` for sub-task 4.7.

## 3. Cross-crate boundaries

`brain-index` is the **closed leaf** for HNSW. It does NOT depend on `brain-storage` or `brain-metadata` (would couple HNSW to row layout). The integration story:

- **Phase 4 sub-task 4.6 (rebuild)** takes an external `Iterator<Item=(MemoryId, Vec<f32>)>`. The caller (Phase 7's writer task) composes the iterator from `brain_metadata::tables::memory` scan + `brain_storage::ArenaFile::slot(i).vector` reads. The rebuild test in `brain-index/tests/` uses a synthetic iterator to keep the crate dependency-free.
- **Phase 7+** wires the cross-crate composition (likely via a `brain-ops` or `brain-server` glue crate).

This keeps `brain-index` pure: vectors in, candidates out.

## 4. Design decisions to surface before 4.1

### 4.1 Vector dim: const generic or runtime?

Spec mandates 384-dim BGE-small in v1 (§04/03 §1). Two options:

| Option | Pros | Cons |
|---|---|---|
| **const generic `HnswIndex<const D: usize>`** | compile-time dim check; zero-cost; matches `Slot { vector: [f32; 384] }` in brain-storage | trickier to change at runtime; every caller has to thread the const |
| **runtime dim** | flexibility for future multi-dim shards | runtime length checks on every insert/search |

**Proposal (recommended):** const generic with workspace-level `pub const VECTOR_DIM: usize = 384;`. brain-storage's slot is already const-sized; aligning brain-index keeps the type system honest. Future multi-dim work is a v2 concern.

### 4.2 ArcSwap from 4.1 or only at 4.8?

Spec §06/08 mandates ArcSwap publication for lock-free reads. Options:

- **A. Build single-threaded `HnswIndex` in 4.1–4.7, wrap in 4.8.** Cleanest progression; the single-threaded core is testable on its own. The wrapping adds a thin `SharedHnsw` type.
- **B. ArcSwap from 4.1.** Forces every sub-task to think about publication boundaries; complicates testing.

**Proposal:** Option A. 4.8 introduces `SharedHnsw` wrapping `HnswIndex` + pending buffer + ArcSwap publication.

### 4.3 hnsw_rs persistence: built-in or custom format?

Spec §06/06 §5.1 prescribes a BHN0-magic wrapper with header CRC + BLAKE3 footer. Inside, the graph data uses "hnsw_rs's built-in serialization."

`hnsw_rs 0.3` ships `file_dump`/`file_load` (need to verify API surface in 4.5 plan). The plan: our `persistence.rs` wraps hnsw_rs's serialization with the BHN0 header + footer.

### 4.4 Tombstone bitmap representation

Spec §06/05 §3 says "bitmap of tombstoned IDs." Number of vectors per shard: up to ~10M (spec §01). Choices:

- `Vec<bool>` — 10 MB at 10M nodes; simple.
- Bit-packed `Vec<u64>` — 1.25 MB at 10M; one byte op per check.
- Roaring bitmap — compresses sparse tombstones; pulls in `roaring` dep.

**Proposal:** Bit-packed `Vec<u64>` (1.25 MB worst case). No new dep. Simple `fn is_tombstoned(id: u32) -> bool { self.bits[id as usize / 64] >> (id % 64) & 1 != 0 }`.

### 4.5 Sub-task ordering tweak

Phase doc has 4.1–4.8 in roughly the right order. One adjustment: **4.2 (id_map) should land before 4.1 fully completes** because `HnswIndex::insert(MemoryId, …)` needs the id_map to convert to hnsw_rs's internal `usize`. Either:
- Bundle them (4.1 owns id_map).
- Split: 4.1 = "raw hnsw_rs wrapper accepting `usize` IDs", 4.2 = "MemoryId ↔ usize adapter on top."

**Proposal:** Split per the phase doc. Sub-task 4.1's first commit will accept `usize` (matching hnsw_rs); 4.2 adds the MemoryId adapter layer.

## 5. The 8 sub-tasks (re-confirmed against spec)

| # | Title | Spec anchor | New scope notes |
|---|---|---|---|
| 4.1 | Wrap hnsw_rs with our params | §02 | Const-generic `HnswIndex<const D: usize>`; `IndexParams` struct; raw `insert(usize, &[f32; D])` and `search(&[f32; D], k, ef) -> Vec<(usize, f32)>` |
| 4.2 | id_map: `MemoryId ↔ usize` | §03 | Bi-directional `HashMap`; sequential `usize` allocator; doesn't reuse on tombstone (slot version covers staleness) |
| 4.3 | Tombstone bitmap | §05 | Bit-packed `Vec<u64>`; grows lazily |
| 4.4 | Search with post-filter | §04, §09 | Over-fetch 2× → filter → truncate to K; cap at 4× to avoid pathological loops |
| 4.5 | Persistence (snapshot/load) | §06 §5 | BHN0 header + hnsw_rs serialization + BLAKE3 footer; LSN-stamped; CRC-validated |
| 4.6 | Rebuild from iterator | §06 §2, §07 | `fn rebuild(params, iter) -> Self`; clears tombstones; deterministic given same iteration order |
| 4.7 | Recall benchmark | §16/05 | 100K random unit vectors; seeded RNG; recall@10 vs exhaustive ground truth ≥ 0.95 |
| 4.8 | Concurrency wrapper | §06 §8 | `SharedHnsw` with `ArcSwap<HnswState>` + pending buffer (1000 inserts / 100 ms flush); concurrent reads test |

## 6. New dependencies (none net-new at workspace level; just enables in brain-index)

- `hnsw_rs.workspace = true` — already in workspace
- `blake3.workspace = true` — already in workspace (used by 4.5 footer)
- `crc32c.workspace = true` — already in workspace (used by 4.5 header)
- `tracing.workspace = true` — already in workspace
- `criterion.workspace = true` (dev) — already in workspace (used by 4.7)

Plus `[[bench]] name = "recall" harness = false` in Cargo.toml at 4.7.

## 7. Phase exit criteria (unchanged from existing phase-04 doc)

- [ ] Sub-tasks 4.1–4.8 ✅.
- [ ] `just verify` green (with brain-index now active in the workspace).
- [ ] Recall@10 ≥ 0.95 at default params, 100K vectors (4.7 bench).
- [ ] Persistence round-trip: snapshot → load → search returns identical results to pre-snapshot search.
- [ ] Rebuild correctness: rebuild from synthetic iter produces a search-equivalent index (recall identical within ε).
- [ ] 8 concurrent searches + 1 background insert: no panic, no data race (loom or run-and-watch).
- [ ] Tag `phase-4-complete`.

## 8. Open items for the user before 4.1

Three calls worth confirming up front:

1. **Vector dim representation:** const generic `HnswIndex<384>` (proposal) vs runtime?
2. **ArcSwap timing:** introduce in 4.8 (proposal) vs from 4.1?
3. **Tombstone bitmap:** bit-packed `Vec<u64>` (proposal) vs `Vec<bool>` vs roaring?

After confirmation, sub-task 4.1's plan goes in next.

PLAN READY.
