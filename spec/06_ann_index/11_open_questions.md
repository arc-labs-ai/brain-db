# 06.11 Open Questions

ANN-index-level questions unresolved as of this spec version.

---

## OQ-AN-1: Pre-filter index for highly-selective filters

**Issue.** Post-search filtering wastes compute when filters are very selective. A pre-filter approach would search only matching candidates.

**Options.**

a) **Status quo.** Post-search filter; expand ef_search if too few results.

b) **Per-filter HNSW indexes.** Maintain separate HNSW indexes per kind, per popular context, etc. Significant memory overhead.

c) **Inline filter awareness.** A patched HNSW that prunes during traversal based on the filter. Requires modifying hnsw_rs.

**Recommendation.** Defer. Post-search works for most filters; more sophisticated approaches add complexity.

---

## OQ-AN-2: Partial rebuild

**Issue.** Full rebuild is heavy for very large shards. A partial rebuild that only repairs degraded regions would be cheaper.

**Options.**

a) **Full rebuild only.** Status quo.

b) **Region-based partial rebuild.** Identify dense-tombstone regions, rebuild just those.

c) **Incremental cleanup during inserts.** Each insert opportunistically cleans up nearby tombstones.

**Recommendation.** Defer. Full rebuild scales OK for our targets (5–30 sec for 1M memories). For 10M+ shards, this becomes important.

---

## OQ-AN-3: Vector compression in HNSW

**Issue.** HNSW stores its own copy of each vector (~1.5 KB each). With compression (PQ, scalar quantization), the in-HNSW copy could be much smaller.

**Options.**

a) **f32 (status quo).** Simple, fast SIMD, full precision.

b) **f16 in HNSW.** 2× memory savings, slightly slower distance.

c) **PQ-compressed HNSW.** 4-16× savings, more accuracy loss.

**Recommendation.** Defer. v1 prioritizes simplicity. Hits storage walls would push toward (b) or (c).

---

## OQ-AN-4: GPU acceleration of search

**Issue.** Search is CPU-bound. GPUs could compute many distances in parallel, especially for very large indexes.

**Options.**

a) **CPU only (status quo).** Simple; portable; sufficient for typical workloads.

b) **GPU search.** Use a GPU-aware ANN library. Different architecture (FAISS-GPU, ScaNN).

**Recommendation.** Stay CPU-only for v1. GPU-ANN libraries are powerful but operationally heavy. Revisit if we have customer workloads where ANN is the bottleneck and CPU isn't enough.

---

## OQ-AN-5: Hybrid index types

**Issue.** Some workloads might benefit from non-HNSW index types (IVF for very large indexes, brute-force for very small).

**Options.**

a) **HNSW only.** Status quo; simpler.

b) **Configurable index type per shard.** Operator chooses based on shard size and access pattern.

c) **Auto-selection.** Substrate picks the index type based on shard size.

**Recommendation.** Stay HNSW-only. The substrate's small-shard fast path uses brute force already; truly large shards (>100M) aren't in v1's target.

---

## OQ-AN-6: Multi-query batching

**Issue.** For workloads with many concurrent queries, batching them through HNSW could improve throughput.

**Options.**

a) **Per-query (status quo).** Each query independently.

b) **Batch queries.** Multiple queries gathered in a window, processed together. The HNSW visits some shared nodes once across queries.

**Recommendation.** Defer. The per-query model works well; batching would add complexity for marginal gain.

---

## OQ-AN-7: Continuous incremental cleanup

**Issue.** The maintenance worker's full rebuild is a "stop the world" operation. Continuous cleanup during normal operations would smooth performance.

**Options.**

a) **Periodic full rebuild (status quo).**

b) **Continuous cleanup.** Each insert/search opportunistically cleans up nearby tombstones; full rebuild becomes a fallback.

**Recommendation.** Future enhancement. The mechanics are well-understood (some HNSW variants do this); implementation is non-trivial.

---

## OQ-AN-8: Cross-shard ANN

**Issue.** Cross-shard queries fan out to each shard, run HNSW search on each, merge results. The merge isn't quite right — the K results from each shard may not be the global top-K.

**Options.**

a) **K from each shard (status quo).** Slightly inflated K guards against missing global top-K.

b) **Iterative refinement.** Start with K from each shard; if the merged K-th result has score lower than any shard's K+1-th, re-query that shard with K' > K.

**Recommendation.** Currently just inflate K (we use K * over_factor for cross-shard). Iterative refinement is a possible enhancement.

---

## OQ-AN-9: Approximate top-K rather than top-K

**Issue.** Some applications care more about "diverse results" than "top K by similarity". The substrate's current top-K returns highly-similar (often near-duplicate) results.

**Options.**

a) **Top-K (status quo).**

b) **MMR (Maximal Marginal Relevance).** Trade off similarity vs diversity in result selection.

c) **Cluster-then-pick.** Cluster candidates; return one from each cluster.

**Recommendation.** This belongs in the query planner / cognitive operations layer, not the ANN layer. The ANN returns top-K; the planner can ask for more candidates and apply post-processing.

---

*Continue to [`12_references.md`](12_references.md) for references.*
