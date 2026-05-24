# Phase 25 — HNSW + Product Quantization

## Goal

Add a per-corpus opt-in PQ compression layer to the existing HNSW indexes. After this phase, a shard can be reconfigured to store its memory / entity / statement vectors as 8-byte PQ codes inside the HNSW graph nodes (down from ~1.5 KB of full-precision `f32`), with the full vectors retained in the arena for a re-rank pass. Recall@10 ≥ 95% after re-rank at default `K′ = 4·K`. Default behaviour is unchanged — every existing deployment stays on pure HNSW unless an admin opts in.

This resolves the "IVF + Product Quantization on top of HNSW" wedge in [`spec/01_architecture/07_wedges_and_roadmap.md`](../../spec/01_architecture/07_wedges_and_roadmap.md) in HNSW-extension form. Pure IVF (no graph) remains deferred.

## Prerequisites

- [x] Phase 4 (ANN index) — `crates/brain-index` already wraps `hnsw_rs`
- [x] Recent: two-tier lock-free `SharedHnsw<D>` with `MainEpoch<D>` + pending buffer (`crates/brain-index/src/shared.rs`)
- [x] Recent: §18 deterministic snapshot+restore chaos tests
- All v1.0 release-blockers landed on main (ENCODE_VECTOR_DIRECT, TXN 1000-op cap, TXN session-drop abort)

## Reading list

1. [`spec/09_indexing/07_hnsw_pq.md`](../../spec/09_indexing/07_hnsw_pq.md) — **the design.** Read end-to-end before sub-task 25.1.
2. [`spec/09_indexing/00_purpose.md`](../../spec/09_indexing/00_purpose.md) §§1-8 — context for what's being extended.
3. [`spec/09_indexing/01_hnsw_basics.md`](../../spec/09_indexing/01_hnsw_basics.md) — algorithm refresher; the PQ distance kernels plug into the same insertion/search loops.
4. [`spec/09_indexing/03_hnsw_lifecycle.md`](../../spec/09_indexing/03_hnsw_lifecycle.md) §§4-5 — rebuild path and persistence; PQ activation is a one-shot Rebuild event.
5. [`spec/09_indexing/04_concurrency.md`](../../spec/09_indexing/04_concurrency.md) — confirms the `MainEpoch` + pending invariants the PQ work must preserve.
6. [`spec/19_benchmarks/06_complete_acceptance.md`](../../spec/19_benchmarks/06_complete_acceptance.md) — the gate; recall measurements must pass with PQ enabled.
7. `crates/brain-index/src/shared.rs` — current two-tier model; extending `MainEpoch<D>` to carry `pq: Option<PqArtifacts>`.
8. `crates/brain-index/src/hnsw.rs` — current `Hnsw<f32, DistCosine>` wrapper; the new `PqHnswIndex` mirrors its surface.
9. `crates/brain-index/src/rebuild.rs` — rebuild orchestration; PQ activation runs through it.

## Outputs

- `crates/brain-index/src/pq/` module with:
  - `Codebook<const D: usize, const M: usize>` — trained quantiser
  - `PqEncoder<const D: usize, const M: usize>` — `f32` → `[u8; M]`
  - `PqDist` — `hnsw_rs::Distance<u8>` impl using ADC (search) and SDC (construction)
  - `Lut` + `SdcTable` — precomputed distance tables
- `crates/brain-index/src/pq_hnsw.rs` — `PqHnswIndex<const D: usize, const M: usize>` wrapping `Hnsw<u8, PqDist>` with the same insert/search/snapshot surface as `HnswIndex<D>`
- `MainEpoch<D>` extended with `pq: Option<PqArtifacts>`; readers see codebook + graph atomically
- `IndexParams.pq: Option<PqParams>` end-to-end through corpus configuration
- Admin op `ADMIN_INDEX_CONFIGURE` for enabling/disabling PQ per corpus per shard
- Recall benchmark `benches/pq_recall.rs` proving the ≥ 95% target on a 100K-vector corpus
- Acceptance suite re-run with PQ enabled on memory + statement corpora (entity left on HNSW — lower-cardinality)
- Tag: `phase-25-complete`

## Sub-tasks

### Task 25.1 — Codebook + k-means trainer

**Reads:** §09.07 §4
**Writes:** `crates/brain-index/src/pq/codebook.rs`, `crates/brain-index/src/pq/kmeans.rs`
**What to build:**
- `Codebook<const D: usize, const M: usize>` holding `m × 2^bits × D/m` centroids as a flat `Vec<f32>`.
- `train(sample: &[[f32; D]], iters: u8, seed: u64) -> Codebook<D, M>`:
  - k-means++ seeding per subspace
  - `iters` Lloyd iterations
  - Collapsed-cell re-seed (furthest-point) when assignment count hits zero
- Deterministic given `(sample, seed)`. Seed derived from `blake3(sorted_memory_ids)` truncated to 64 bits (use `blake3` from the approved crate list).
**Done when:**
- Unit test: train on the synthetic `make_blobs`-style fixture and assert centroids land near ground truth.
- Determinism test: same `(sample, seed)` produces byte-identical codebooks across runs.
- Property test (proptest): k-means assignment cost monotonically decreases across iterations.

### Task 25.2 — PQ encoder

**Reads:** §09.07 §5
**Writes:** `crates/brain-index/src/pq/encode.rs`
**What to build:**
- `encode(vector: &[f32; D], codebook: &Codebook<D, M>) -> [u8; M]`
- Uses `wide::f32x8` for the per-subspace squared-distance loop (matches the existing SIMD pattern in `brain-storage`).
**Done when:**
- Round-trip test: encode known centroids → codes correspond to the right indices.
- Benchmark: encode 1M 384-d vectors under 250 ms total (target ~250 ns per vector on a modern x86_64).

### Task 25.3 — Distance kernels (ADC + SDC)

**Reads:** §09.07 §6
**Writes:** `crates/brain-index/src/pq/distance.rs`
**What to build:**
- `Lut<const M: usize, const K: usize>` — query-conditioned table built once per search.
- `SdcTable<const M: usize, const K: usize>` — symmetric distance table built once per codebook.
- `build_lut(query: &[f32; D], codebook: &Codebook<D, M>) -> Lut<M, K>`
- `build_sdc_table(codebook: &Codebook<D, M>) -> SdcTable<M, K>`
- `adc_distance(lut: &Lut, code: &[u8; M]) -> f32`
- `sdc_distance(table: &SdcTable, a: &[u8; M], b: &[u8; M]) -> f32`
- `PqDist` struct implementing `hnsw_rs::dist::Distance<u8>`:
  - Holds `Arc<SdcTable>` for construction-time eval
  - Reads `Lut` from a thread-local for search-time eval (installed by the search shim)
**Done when:**
- Unit test: ADC distance between query and code matches the cosine distance against the dequantised vector within 5% on a 95th percentile of random pairs.
- Unit test: SDC distance is symmetric and zero on identical codes.
- Bench: per-candidate `adc_distance` ≤ 12 ns at `M=8` on aarch64; ≤ 8 ns on x86_64 (table lookups + adds).

### Task 25.4 — `PqHnswIndex` wrapper

**Reads:** §09.07 §§6.3, 7; `crates/brain-index/src/hnsw.rs` for the surface to mirror
**Writes:** `crates/brain-index/src/pq_hnsw.rs`
**What to build:**
- `PqHnswIndex<const D: usize, const M: usize>` wrapping `Hnsw<'static, u8, PqDist>`.
- Mirrors `HnswIndex<D>`'s public surface: `new`, `insert`, `search`, `search_with_filter`, `mark_tombstone`, `snapshot`, `load`, `len`.
- `insert(memory_id, vector: &[f32; D])` — encodes via the trained codebook before pushing to `hnsw_rs`.
- `search(query, k, ef, filter)` — installs LUT, calls inner search with `k_inflated`, **does NOT re-rank** (that's task 25.5).
- `snapshot` writes the codebook + the inner HNSW snapshot; `load` reads both.
**Done when:**
- Round-trip test: build a small `PqHnswIndex`, search, assert candidates are non-empty and ordered.
- Snapshot round-trip: build → snapshot → load → search returns equivalent candidates.

### Task 25.5 — Re-rank pass

**Reads:** §09.07 §7
**Writes:** `crates/brain-index/src/pq_hnsw.rs` (extend), or a new `pq_hnsw_search.rs` if 25.4 grew too large
**What to build:**
- `rerank(candidates: &[(MemoryId, f32)], query: &[f32; D], arena: &impl ArenaReader) -> Vec<(MemoryId, f32)>`
- Reads each candidate's full-precision vector from the arena; tombstoned-during-search candidates are skipped.
- Sorts by exact cosine and truncates to `k`.
**Done when:**
- Recall@10 ≥ 95% on a 100K-vector fixture (gate per §19.06 §3 recall harness).
- Test: candidate tombstoned between HNSW search and re-rank — result count drops, no panic, `partial_results` flag set.

### Task 25.6 — `SharedHnsw` extension for codebook epoch

**Reads:** §09.07 §§8, 9; `crates/brain-index/src/shared.rs`
**Writes:** `crates/brain-index/src/shared.rs` (extend, do not break existing surface)
**What to build:**
- Extend `MainEpoch<D>` to:
  ```rust
  pub struct MainEpoch<D> {
      pub index: HnswFlavour<D>,
      pub epoch_id: u64,
  }
  pub enum HnswFlavour<D> {
      Pure(HnswIndex<D>),
      Pq(PqHnswIndex<D, 8>),
  }
  ```
- `SharedHnsw::flush_with_rebuild` accepts a closure returning either flavour; the atomic swap is unchanged.
- Reader path (`SharedHnsw::search`) dispatches on the flavour internally; callers don't branch.
**Done when:**
- All existing `SharedHnsw` tests pass (no regressions on the pure-HNSW path).
- New test: `flush_with_rebuild` can flip Pure → Pq → Pure across three epochs; reader running on the old epoch finishes correctly.
- Clippy clean.

### Task 25.7 — `PqParams` plumbing

**Reads:** §09.07 §3
**Writes:** `crates/brain-index/src/params.rs`, `crates/brain-server/src/shard/mod.rs`, callers
**What to build:**
- `PqParams` struct with the five fields from §3.
- `IndexParams.pq: Option<PqParams>` field; default `None`.
- Per-corpus override at shard construction (memory / entity / statement read independent values).
- Configuration source: a new section in `config/brain.toml`:
  ```toml
  [index.memory.pq]
  enabled = false
  m = 8
  bits = 8
  ```
**Done when:**
- Config round-trips through the TOML loader.
- Shard built with `pq.enabled = true` activates the PqHnswIndex flavour during initialisation.

### Task 25.8 — Admin op: `ADMIN_INDEX_CONFIGURE`

**Reads:** §09.07 §§2, 8; `spec/04_wire_protocol/03_opcodes.md` for opcode allocation
**Writes:** `crates/brain-protocol/src/codec/opcode.rs`, `crates/brain-protocol/src/ops/admin.rs`, `crates/brain-ops/src/handlers/admin/index_configure.rs`, `crates/brain-sdk-rust/src/ops/admin.rs`
**What to build:**
- New wire opcodes `AdminIndexConfigureReq = 0x0070`, `AdminIndexConfigureResp = 0x00F0`.
- Request body: `{ corpus: Memory | Entity | Statement, mode: Hnsw | HnswPq(PqParams) }`.
- Handler:
  - Validates the request (params in range, `m` divides `D`).
  - Triggers a `Rebuild` on the corpus's `SharedHnsw` via the new flavour.
  - Blocks until the rebuild completes or returns `BackgroundJob` with a poll handle (decision: lean on the existing `ADMIN_BACKFILL` job-pattern for symmetry).
- SDK helper: `client.admin().index_configure(corpus).hnsw_pq(params).send().await`.
**Done when:**
- Round-trip test on the wire.
- Integration test: enable PQ → run RECALL → results equivalent to pure HNSW within recall budget.

### Task 25.9 — Migration: pure HNSW corpus → PQ

**Reads:** §09.07 §8
**Writes:** `crates/brain-index/src/rebuild.rs` (extend)
**What to build:**
- `rebuild_with_pq(corpus_id, params: PqParams)`:
  - Drains the pending buffer (uses the two-tier `flush_with_rebuild` already in place).
  - Reads a `training_sample` random non-tombstoned slot snapshot.
  - Trains the codebook.
  - Encodes every active vector.
  - Builds a fresh `PqHnswIndex` from the codes.
  - Swaps the new epoch in.
- Reverse path `rebuild_without_pq(corpus_id)` for the de-activation case.
**Done when:**
- Test: 50K-vector pure-HNSW corpus → activate PQ → search results overlap with pre-activation top-10 by ≥ 95%.
- Test: 50K-vector PQ corpus → deactivate → search results overlap with pre-deactivation top-10 by ≥ 99% (re-rank against arena floor).

### Task 25.10 — Recall benchmark

**Reads:** §09.07 §10; `crates/brain-index/benches/` for the existing benchmark style
**Writes:** `crates/brain-index/benches/pq_recall.rs`
**What to build:**
- Builds a 100K random-vector pure-HNSW corpus.
- Snapshots ground-truth top-10 for 1K queries via brute-force scan.
- Activates PQ on the same corpus.
- Measures recall@10 across the 1K queries.
- Reports pure-HNSW recall, PQ-without-rerank recall, PQ-with-rerank recall (the gate).
**Done when:**
- Bench prints all three numbers; PQ-with-rerank ≥ 95%; pure HNSW ≥ 95-98%.

### Task 25.11 — Acceptance-suite re-run

**Reads:** [`spec/19_benchmarks/06_complete_acceptance.md`](../../spec/19_benchmarks/06_complete_acceptance.md)
**Writes:** A new variant of the acceptance harness that flips PQ on for memory + statement corpora before the run.
**What to build:**
- A `PQ_ENABLED=true` knob in the harness driver.
- Same suite, same assertions, two profiles (HNSW-only, HNSW+PQ).
**Done when:**
- Both profiles pass the full suite.
- Latency report shows PQ within 1.2× of HNSW on p99 across all RECALL/QUERY tests.

## Stop conditions

If task 25.10's recall comes in below 90% after re-rank, **stop the phase and re-evaluate**:

- The rerank_factor may need to grow (currently `4·K` — try `8·K` and re-measure).
- The codebook may be under-trained (more iterations, larger sample).
- If after tuning recall still fails, escalate to user: HNSW+PQ may not be viable on Brain's vector distribution and pure IVF+PQ (deferred wedge) becomes the only path.

## Risk: hnsw_rs internals

`hnsw_rs::Hnsw<T, D: Distance<T>>` is generic, but its construction-time and search-time call signatures for `Distance::eval` are uniform — both sides typed `&[T]`. Task 25.3 deals with this by:

- Construction (`eval` called with two PQ codes): use SDC via the `Arc<SdcTable>`.
- Search (`eval` called with a queried code and a stored code): use ADC via the thread-local LUT.

The thread-local trick is the load-bearing complexity. If `hnsw_rs`'s search path turns out to use multi-thread fan-out internally and the thread-local doesn't propagate, **task 25.4 surfaces a deviation** and we either:

- Patch `hnsw_rs` upstream (PR a `set_query_context` hook).
- Replace `hnsw_rs` for the PQ path with a thin in-tree HNSW impl that accepts a `(query, code)` callback — ~600 LOC, doable but slips the timeline.

Decision deferred to task 25.4 implementation. Document the chosen path in the commit message.

## Verification

- All tasks pass `cargo test` + `cargo clippy --workspace -- -D warnings`.
- `cargo bench` on the PQ benches shows the budgeted per-candidate distance cost.
- Acceptance suite passes both profiles (HNSW, HNSW+PQ).
- Spec edits (§09.00 §2, §01.07, §00.04 Q7) applied and the new §09.07 referenced from §09.00's "See also" tail.

## Spec edits proposed alongside this phase

(Surgical, additive — full text shown for user review before applying.)

1. **`spec/09_indexing/00_purpose.md` §2**: Soften the "PQ + IVF hybrids — overkill for Brain" line to acknowledge HNSW+PQ as an opt-in extension. Add a forward-link to §09.07.
2. **`spec/01_architecture/07_wedges_and_roadmap.md`** — IVF+PQ wedge section: mark "Partial: HNSW+PQ in v1.x (§09.07); pure IVF deferred."
3. **`spec/00_overview/04_open_questions_archive.md`** Q7: append "**Partially resolved (v1.x):** HNSW+PQ shipped per [§09.07](../09_indexing/07_hnsw_pq.md). Pure IVF remains deferred."

No edits to §08 (arena), §13 (retrievers), or §05 (operations) — PQ is transparent below the search interface.
