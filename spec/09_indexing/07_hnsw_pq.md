# 09.07 HNSW + Product Quantization

> **TL;DR.** An opt-in compression layer for the per-corpus HNSW. Each indexed vector is encoded into `m=8` PQ subquantizer codes (8 bytes total, ~190× smaller than the 384-d `f32` original). HNSW traversal uses Asymmetric Distance Computation (ADC) against a precomputed query-to-codebook lookup table; the top-K′ candidates are re-ranked against full-precision arena vectors to recover recall. Disabled by default; enabled per corpus per shard via `IndexParams.pq`. Recall@10 target ≥ 95% after re-rank at `K′ = 4·K`.

## Status

| Field | Value |
|---|---|
| Status | Draft (active implementation, phase 25) |
| Audience | Implementers of the ANN layer; operators tuning memory ceiling at high vector counts |
| Voice | Hybrid (rationale + normative) |
| Depends on | [09.00 Purpose](00_purpose.md), [09.01 HNSW Basics](01_hnsw_basics.md), [09.03 HNSW Lifecycle](03_hnsw_lifecycle.md), [09.04 Concurrency](04_concurrency.md), [08. Storage: Arena & WAL](../08_storage/00_purpose.md) |
| Referenced by | [01.07 Wedges & Roadmap](../01_architecture/07_wedges_and_roadmap.md) (resolves the IVF+PQ wedge in HNSW-extension form) |

## 1. Motivation

The pure-HNSW configuration stores every full-precision 384-d `f32` vector in arena memory and references it from the HNSW graph. Per-vector cost: ~1.5 KB arena + ~150 B HNSW node ≈ 1.65 KB. A shard holding 1B vectors needs ~1.65 TB resident — economically infeasible.

Product Quantization (PQ) compresses each vector to a fixed-size code by partitioning the dim-`D` space into `m` equal-width subspaces, training one k-means codebook of `2^bits` centroids per subspace, and storing each subspace as the centroid index. For `D=384, m=8, bits=8`: every vector becomes `[u8; 8]` — 192× smaller than the `f32` original. The graph node grows by the 8 bytes; the arena vector is **kept** because the re-rank pass needs full precision.

Brain's HNSW shape is preserved end-to-end. Only the distance kernel and graph payload change.

## 2. When to enable

PQ is **off by default**. Pure HNSW remains the right choice when:

- The corpus fits comfortably in RAM at full precision (typical: ≤ 10M vectors per shard).
- The acceptance suite recall margins are tight and the workload cannot absorb the PQ recall hit.
- Search latency is dominated by HNSW traversal cost rather than vector residency.

PQ is the right choice when:

- The corpus exceeds the per-shard memory budget at full precision.
- The acceptance suite passes after re-rank with the configured `K′` factor.
- The operator has measured the latency impact on representative queries and accepted it.

Enabling PQ on a small corpus is wasteful, not harmful: the codebook overhead (~393 KB per corpus) and the per-search lookup-table build (~8 KB) dominate the savings at low N.

## 3. Parameters

PQ-specific knobs live on a new `PqParams` struct nested inside `IndexParams`:

```rust
pub struct PqParams {
    pub m: u8,              // subquantizers — must divide D evenly
    pub bits: u8,           // bits per code, 4 or 8 (256 centroids per subspace)
    pub training_sample: u32, // vectors drawn from the corpus to train the codebook
    pub kmeans_iters: u8,   // k-means iterations during training (default 25)
    pub rerank_factor: u8,  // re-rank top-K′ where K′ = rerank_factor · K
}

pub struct IndexParams {
    pub m: u8,
    pub ef_construction: u32,
    pub ef_search: u32,
    pub ef_search_max: u32,
    pub pq: Option<PqParams>,  // None → pure HNSW
}
```

### 3.1 Default `PqParams`

| Field | Default | Notes |
|---|---|---|
| `m` | 8 | 384 / 8 = 48-dim subspaces, divides cleanly |
| `bits` | 8 | 256 centroids per subspace; `[u8; m]` code |
| `training_sample` | 65_536 | Sufficient to fit 256 centroids per 48-d subspace |
| `kmeans_iters` | 25 | Empirically converges by 20-30 iters at this scale |
| `rerank_factor` | 4 | `K′ = 4·K`; matches the FAISS default starting point |

### 3.2 Per-corpus override

Memory, entity, and statement corpora each accept their own `PqParams`. The defaults above match all three; the statement corpus (where `M=32` in the underlying HNSW) typically benefits from `rerank_factor=2` because its retrieval target is text-typed and less ranking-sensitive.

## 4. Codebook training

A codebook is a `[[f32; D/m]; 2^bits]` array per subspace — `m × 2^bits × D/m × 4` bytes total. For the defaults: `8 × 256 × 48 × 4 = 393 216` bytes (~384 KB) per corpus per shard.

### 4.1 Inputs

Training requires a representative sample of the corpus. The bootstrap path:

1. Worker drains up to `training_sample` random `f32` vectors from the arena, skipping tombstoned slots.
2. If the corpus holds fewer than `training_sample` non-tombstoned vectors, **PQ activation fails** with `PqError::InsufficientSample`. The shard stays on pure HNSW.

### 4.2 Algorithm

Per subspace `s ∈ [0, m)`:

1. Slice each sample vector to its `s`-th `D/m`-wide chunk, yielding `training_sample × D/m` real-valued points.
2. Run k-means with `2^bits` centroids:
   - Initialise via **k-means++** seeding (one random point, then probability-weighted by squared distance).
   - Iterate `kmeans_iters` times: assign points to nearest centroid, recompute centroids as the mean of their assignments.
   - If a centroid receives zero assignments, re-seed from the point furthest from any centroid (prevents collapsed cells).
3. Store the `2^bits` final centroids as the subspace codebook.

### 4.3 Determinism

Training is deterministic given the same `(sample, seed)`. The seed is the SHA-256 of the sorted sample memory-ids, truncated to 64 bits. Two shards training on the same sample produce identical codebooks. Two shards training on different samples produce different codebooks; the codebooks are not interchangeable across shards.

### 4.4 Lifecycle

A codebook is trained **once per epoch** (see §6). Re-training requires a full re-encode of every vector, which is a `Rebuild` (per [`03_hnsw_lifecycle.md`](03_hnsw_lifecycle.md) §4). Training is therefore amortised over the epoch's lifetime; it is not on the hot insert path.

## 5. Encoding

Encoding a single vector `v: [f32; D]` to its PQ code `c: [u8; m]`:

```
for s in 0..m:
    chunk = v[s*(D/m)..(s+1)*(D/m)]
    c[s] = argmin_k ||chunk - codebook[s][k]||²
```

`O(m · 2^bits · D/m) = O(2^bits · D)` per vector. With defaults: 256 · 384 = 98 304 floating multiplies per encode — sub-microsecond on modern x86_64 with the `wide` SIMD crate already in the stack.

Encoding happens:

- During training (encode every sampled vector once after the codebooks are frozen).
- During build (encode every active arena vector to populate the new HNSW).
- On insert (encode the new vector before adding to the pending buffer; see §6).

The encoded code, not the original `f32`, is what the HNSW graph stores.

## 6. Distance computation

PQ supports two distance modes; Brain uses both.

### 6.1 Asymmetric Distance Computation (ADC)

For search: the query is full-precision, the target is a PQ code. **Higher recall** than SDC because the query side carries no quantisation error.

Pre-computation (once per query, before HNSW traversal):

```
build_lut(query: &[f32; D], codebook: &[[[f32; D/m]; 2^bits]; m]) -> [[f32; 2^bits]; m]
for s in 0..m:
    chunk_q = query[s*(D/m)..(s+1)*(D/m)]
    for k in 0..2^bits:
        LUT[s][k] = ||chunk_q - codebook[s][k]||²
```

LUT size: `m · 2^bits · 4` bytes = 8 KB at defaults. Fits in L1.

Per-candidate distance (in the HNSW inner loop):

```
adc_distance(LUT, code: &[u8; m]) -> f32
    sum = 0.0
    for s in 0..m:
        sum += LUT[s][code[s] as usize]
    return sum
```

`m` table lookups + `m` floating adds per candidate — `~8` ops at defaults. Roughly **3-5× faster than full-precision dot product** because the per-candidate work is `O(m)` instead of `O(D)`.

### 6.2 Symmetric Distance Computation (SDC)

For graph construction: both endpoints are PQ codes (the inserter doesn't have full-precision query context). Pre-computation (once per epoch, at codebook build):

```
build_sdc_table(codebook) -> [[[f32; 2^bits]; 2^bits]; m]
for s in 0..m:
    for i in 0..2^bits:
        for j in 0..2^bits:
            SDC[s][i][j] = ||codebook[s][i] - codebook[s][j]||²
```

SDC table size: `m · 2^bits² · 4` bytes = 2 MB at defaults. Fits in L2.

Per-edge distance during HNSW construction:

```
sdc_distance(SDC, code_a, code_b) -> f32
    sum = 0.0
    for s in 0..m:
        sum += SDC[s][code_a[s] as usize][code_b[s] as usize]
    return sum
```

SDC introduces quantisation error on **both** sides of the comparison. For HNSW construction this is acceptable — the graph is approximate by nature and the construction-time error is dominated by `ef_construction`'s candidate breadth.

### 6.3 Integration with `hnsw_rs`

`hnsw_rs::Hnsw<T, D: Distance<T>>` is generic over the stored type and distance. Brain uses `Hnsw<u8, PqDist>` where:

- `T = u8`; each item is `[u8; m]` packed contiguously.
- `PqDist` is a Brain-owned struct that holds a reference to the SDC table (for construction) and a thread-local LUT (for search).
- Construction calls `PqDist::eval(va, vb)` which falls through to the SDC path.
- Search wraps `Hnsw::search` with a thin shim that installs the LUT into the thread-local before invocation.

No fork of `hnsw_rs` is required.

## 7. Search path

The search interface from §09.00 §8 is unchanged on the outside. Internally:

```
search(query: &[f32; D], k: usize, ef: usize, filter: Option<AnnFilter>) -> Vec<(MemoryId, f32)>
1. if pq is enabled:
       lut = build_lut(query, codebook)
       install lut in thread-local
       k_inflated = k * rerank_factor
       candidates = hnsw.search_with_dist(query_code, k_inflated, ef, PqDist)
       // candidates carry ADC distances — approximate
   else:
       candidates = hnsw.search(query, k, ef, filter)
       return candidates
2. rerank:
       for each candidate:
           full_vec = arena.read(candidate.memory_id)
           if full_vec is None: skip (tombstoned during search)
           candidate.score = exact_cosine(query, full_vec)
       sort candidates descending by score
       return candidates[..k]
3. apply filter (post-filter, identical to the pure-HNSW path)
```

`k_inflated = k · rerank_factor` is the over-fetch buffer. The PQ-ADC top-K′ is a superset of the exact top-K with high probability; the re-rank pass picks the actual top-K from the inflated pool. This is the same recall-recovery pattern FAISS uses.

### 7.1 Latency impact

Per-query budget (default params, 1M-vector corpus):

| Phase | Pure HNSW | HNSW+PQ |
|---|---|---|
| LUT build | — | ~10 µs |
| HNSW traversal (ADC) | ~3 ms | ~1 ms (3× faster) |
| Re-rank (4·K arena reads) | — | ~150 µs (K=10, sequential mmap reads) |
| Filter | ~50 µs | ~50 µs |
| **Total p50** | ~3.1 ms | ~1.2 ms |

PQ search is **faster** than full HNSW search at scale because the per-candidate distance is `O(m)` not `O(D)`, and that outweighs the LUT build + re-rank overhead. The gain shrinks for small K (re-rank fixed cost) or small corpora (HNSW traversal cost is already low).

## 8. Lifecycle

PQ activation lifts the existing HNSW lifecycle from [`03_hnsw_lifecycle.md`](03_hnsw_lifecycle.md) §4 with two additions:

1. **Codebook is part of the epoch.** `MainEpoch<D>` (from `crates/brain-index/src/shared.rs`) gains a `pq: Option<PqArtifacts>` field. The codebook + SDC table are immutable per epoch, swap-with-the-graph via the existing `flush_with_rebuild` callback.

2. **Activation is a one-shot rebuild.** Transitioning a corpus from `pq = None` to `pq = Some(_)` requires:
   - Drain the pending buffer (per the two-tier invariant in §09.04 §3).
   - Run training (§4).
   - Encode every active arena vector to PQ.
   - Build a new HNSW from the PQ codes.
   - Swap the new epoch in via `flush_with_rebuild`.

   Deactivation is the symmetric flow: re-build HNSW from full-precision arena vectors, swap, drop the codebook.

### 8.1 Insert path

Once PQ is active:

```
insert(memory_id, vector: &[f32; D]):
    code = encode(vector, codebook)
    arena.write_slot(memory_id, vector)   // still full-precision for re-rank
    pending_buffer.push(PendingEntry { memory_id, code, vector })
    if pending_buffer.len() >= threshold:
        schedule_flush()
```

The pending buffer stores both `code` and `vector` so search can re-rank pending entries without an arena round-trip.

### 8.2 Bulk insert (rebuild path)

A `Rebuild` event (per §09.03 §4) under PQ:

1. Snapshot arena slots `(memory_id, vector)`.
2. Encode every vector to `code`.
3. Construct a new `Hnsw<u8, PqDist>` from the codes.
4. Atomic swap via `flush_with_rebuild`.

The arena snapshot is **not** re-trained; the codebook from the current epoch is reused. Re-training only happens on a deliberate `RebuildCodebook` op (admin-triggered).

## 9. Concurrency

Read side: identical to the lock-free model in §09.04. The current `MainEpoch` (Arc-swapped) carries either pure HNSW or PQ HNSW; readers don't branch on which.

Write side: the single-writer invariant (§09.04 §2) holds. PQ encoding happens inside the writer task before the pending push; no additional synchronisation.

Codebook updates (epoch rebuild) follow the existing snapshot-then-swap pattern. Readers mid-flight on the old epoch see the old codebook + old graph + old SDC; readers that observe the new epoch see all three new. The two states never interleave because both are reached via the same `ArcSwap::store`.

## 10. Recall budget

The §19.06 acceptance suite is the gate. PQ activation **must** pass the suite under the corpus's normal load profile, with:

- Recall@10 ≥ 95% after re-rank (drops from pure-HNSW's 95-98%).
- Recall@100 ≥ 92% after re-rank.
- p99 latency within 1.2× of the pure-HNSW baseline (the typical case is faster; the bound covers re-rank tail variance).

A PQ corpus that fails any of these targets reverts to pure HNSW at the next epoch swap. The revert is automatic — the failure is observed via the worker that runs `bench_recall.rs` (per §19.04) on a sampling cadence.

## 11. Failure modes

| Failure | Detection | Recovery |
|---|---|---|
| Training sample too small | `PqParams.training_sample > non_tombstoned_count(arena)` | Reject activation with `PqError::InsufficientSample`; stay on pure HNSW |
| k-means non-convergence | Centroid drift > ε after `kmeans_iters` | Allow non-converged codebook; record warning metric; recall budget catches actual degradation |
| Collapsed centroid cell | Cell receives zero assignments two iterations in a row | Re-seed from the furthest unassigned point |
| Encoding error (NaN in input) | `vector.iter().any(|&x| !x.is_finite())` | Reject the insert; emit `IndexError::NotANumber`; do not insert a corrupt code |
| Re-rank read miss (tombstone during search) | Arena slot returns `None` | Skip the candidate; if fewer than K survive, surface the truncated list and a partial-result flag |
| Recall below budget | Sampling bench on each epoch | Auto-revert to pure HNSW; alert |
| Codebook corruption (snapshot replay) | CRC mismatch on persisted codebook bytes | Drop the codebook; trigger re-training from arena sample |

## 12. Persistence

Persisted artefacts per PQ-enabled corpus per shard:

- The codebook: `m · 2^bits · D/m · 4` bytes, written alongside the HNSW snapshot (§09.03 §5).
- The PQ codes: implicit — they live inside the snapshotted HNSW graph nodes; no separate file.
- The full-precision vectors: untouched — the arena holds them as today.

The arena is **the source of truth**. A lost codebook or PQ HNSW can be rebuilt from the arena in `O(training_sample + N · m · 2^bits)` time. The codebook persistence is an optimisation, not a correctness requirement.

## 13. What this spec does not cover

- **Pure IVF** (no graph; coarse-cells-only search). Remains in the wedge backlog (§01.07). The architectural change is much larger; HNSW+PQ resolves the immediate memory pressure at Brain's target scale.
- **Per-shard PQ params.** All shards in a deployment share the same `IndexParams`; if a deployment needs different `m` per shard, that's a future change.
- **Multi-codebook variants** (residual PQ, optimised PQ). Tracked as future spec work; not required for the v1 PQ ship.
- **The admin op surface for activating PQ.** Defined alongside the [§04 wire opcodes](../04_wire_protocol/03_opcodes.md) as part of the implementation phase.

---

*Continues from [`06_failure_modes.md`](06_failure_modes.md). The architectural wedge resolved here is documented in [`01_architecture/07_wedges_and_roadmap.md`](../01_architecture/07_wedges_and_roadmap.md) under "IVF + Product Quantization on top of HNSW".*
