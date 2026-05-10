# 06.00 Purpose

This document specifies the ANN index — the data structure that makes similarity search over millions of vectors fast enough for interactive use.

## What this document covers

- The HNSW graph structure and why we use it.
- The parameters Brain ships with and the rationale.
- Insertion, search, and deletion procedures.
- The lack of dedicated persistence and the rebuild path on startup.
- The maintenance worker that detects and repairs topology degradation.
- The concurrency model: lock-free reads, single-writer writes, epoch-managed reclamation.
- Inline filtering by model fingerprint, kind, context.

## What this document does not cover

- **The HNSW algorithm itself** is summarized in [`01_hnsw_primer.md`](01_hnsw_primer.md) but the original paper ([Malkov & Yashunin, "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs"](https://arxiv.org/abs/1603.09320)) is the authoritative source for the algorithm.

- **Vector storage** — the vectors live in the arena; this spec assumes they're available. See [05. Storage: Arena & WAL](../05_storage_arena_wal/).

- **The cognitive operations that use ANN search** — defined in [09. Cognitive Operations](../09_cognitive_operations/). This spec specifies the search primitive; how RECALL/PLAN/REASON use it is over there.

- **The query planner that decides when to use ANN search vs. other strategies** — defined in [08. Query Planner + Execution Engine](../08_query_planner/).

## 1. The role of the ANN index

The ANN index answers one question: "given a query vector q, what are the K most-similar memory vectors in the shard?"

This question must be answered:

- **Quickly.** Better than O(N) scan of all vectors. HNSW achieves O(log N) typical case.
- **Accurately enough.** The "approximate" in ANN means we may miss the literal top-K; we accept high but not perfect recall (typically 90-99% at default settings).
- **Concurrently.** Many queries in flight; insertions happening; reads see consistent state without locks.

## 2. Why HNSW

We selected HNSW over alternatives:

- **Brute-force scan.** O(N). Fine for tiny indexes; doesn't scale.
- **IVF (Inverted File Index).** Cluster-based. Simpler than HNSW. Better for very large indexes; HNSW better for mid-scale (our target).
- **Annoy (random forest of trees).** Simpler. Worse recall at high K.
- **DiskANN.** Designed for SSDs holding indexes too large for RAM. Adds I/O complexity we don't need at our scale.
- **ScaNN.** Google's competitive ANN. Excellent quality. Less mature in Rust ecosystem.
- **PQ + IVF hybrids.** Fancy compression + clustering. Useful at extreme scale; overkill for us.

HNSW wins because:

- It produces high recall (95-99%) at competitive speed.
- It works in-memory; no I/O overhead per query.
- It supports incremental updates (insert/delete without full rebuild).
- It's well-implemented in [`hnsw_rs`](https://github.com/jean-pierreBoth/hnswlib-rs), giving us a Rust crate we don't have to write.

## 3. The crate: hnsw_rs

Brain uses [`hnsw_rs`](https://github.com/jean-pierreBoth/hnswlib-rs) (also known as `hnswlib-rs`). Pure Rust, MIT/Apache 2.0 licensed.

What we get from the crate:

- The HNSW algorithm with insertion and search.
- Multi-layer graph structure.
- Customizable distance functions (we use cosine / dot product).
- Reasonable performance.

What we add on top:

- The single-writer-per-shard discipline ([`08_concurrency.md`](08_concurrency.md)).
- Lock-free reads via epoch-based reclamation.
- Inline filters for fingerprint / kind / context.
- The maintenance worker for topology drift.
- Integration with our arena and metadata.

We don't fork hnsw_rs. We use it as a library and layer concurrency and filtering on top.

## 4. Per-shard HNSW

Each shard has one HNSW index. The index covers all of the shard's active (non-tombstoned) memories.

Cross-shard queries don't combine HNSW indexes; the query is fanned out to each shard, each shard runs its own HNSW search, and results are merged. See [12. Sharding + Clustering](../12_sharding_clustering/) §Cross-Shard Queries.

## 5. Index size

For a shard with N memories at 384-dim vectors:

- HNSW graph nodes: N entries, each with a small list of edges.
- Default `M = 16` edges per layer, plus higher layers (typically <10% of N is in higher layers).
- Memory overhead: ~150 bytes per memory (vector pointer + edges).
- Total: ~150 MB for 1M memories.

This is in addition to the vector storage in the arena (1.5 KB per vector). The HNSW index doesn't duplicate vectors; it references them.

## 6. Latency targets

For a single-shard ANN search:

- p50: 1-3 ms.
- p99: 5-10 ms.

These targets assume:

- Memories range from 100K to 10M.
- ef_search = 64 (default).
- Vectors fit in memory (page cache hit on the arena).

For very large shards (10M+) or cold caches, latency grows. The substrate's targets in [01.06](../01_system_architecture/06_targets.md) are calibrated against the typical case.

## 7. Recall targets

"Recall@10" means: of the true top-10 nearest neighbors, what fraction does the ANN return?

- Default ef_search = 64: recall@10 ≈ 95-98%.
- ef_search = 128: recall@10 ≈ 98-99.5%.
- ef_search = 256: recall@10 ≈ 99.5-100%.

Higher ef_search costs more compute. The default 64 is the balance most workloads accept.

## 8. The interface

The ANN index exposes:

```rust
trait AnnIndex {
    fn insert(&mut self, memory_id: MemoryId, vector: &[f32]) -> Result<(), AnnError>;

    fn search(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        filter: Option<AnnFilter>,
    ) -> Vec<(MemoryId, f32)>;  // Sorted by similarity descending

    fn remove(&mut self, memory_id: MemoryId) -> Result<(), AnnError>;

    fn size(&self) -> usize;
}
```

`insert` and `remove` are called by the writer task. `search` is called concurrently by reader tasks.

## 9. Insert + search must coexist

The substrate sustains writes and reads simultaneously. The HNSW must support:

- A search seeing all memories committed before the search started.
- A search not seeing memories inserted during the search (consistency).
- An insert not blocking the search.

This requires concurrency management, detailed in [`08_concurrency.md`](08_concurrency.md).

## 10. Position in the architecture

The ANN index sits between the storage layer (which holds vectors) and the query planner (which orchestrates search):

```
Query Planner
     │
     │  search(query, k, ef, filter)
     ▼
ANN Index (HNSW)
     │
     │  reads vectors from
     ▼
Vector Arena (mmap'd file)
```

The index doesn't own vectors; the arena does. The index holds the graph structure and references slots in the arena.

---

*Continue to [`01_hnsw_primer.md`](01_hnsw_primer.md) for HNSW basics.*
