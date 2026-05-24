# 09. ANN Index (HNSW)

> **TL;DR.** HNSW per shard, via the `hnsw_rs` crate. Memory HNSW defaults `M=16, ef_construction=200, ef_search=64`; entity and statement HNSWs use per-corpus parameters documented in [`01_hnsw_basics.md`](01_hnsw_basics.md) § 25. Three corpora when a schema is declared: memory HNSW, entity HNSW, statement HNSW. The index references slots in the arena rather than copying vectors. Single-writer-per-shard inserts, lock-free reads through ArcSwap + crossbeam-epoch, inline filtering by model fingerprint, kind, and context. p99 search 5-10 ms; recall@10 95-98% at default ef_search.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Implementers of the ANN layer; performance engineers tuning recall quality |
| Voice | Hybrid (rationale + normative) |
| Depends on | [01. System Architecture](../01_architecture/00_purpose.md), [02. Data Model](../02_data_model/00_purpose.md), [08. Storage: Arena & WAL](../08_storage/00_purpose.md) |
| Referenced by | [12. Query Optimizer](../12_query_optimizer/00_purpose.md), [15. Background Workers](../15_background_workers/00_purpose.md) |

## What this spec defines

The Hierarchical Navigable Small World (HNSW) graph that indexes vectors for approximate nearest neighbor (ANN) search. It defines:

- The HNSW algorithm at the level needed to understand Brain's specific implementation choices.
- The parameters Brain ships with and the rationale.
- Insertion, search, and deletion procedures.
- Persistence (or lack thereof) — the index is rebuilt from arena + metadata.
- Maintenance: when and how to rebuild parts of the index.
- Concurrency: the lock-free read path, single-writer writes.

This document specifies the vector index — the data structure that makes similarity search over millions of vectors fast enough for interactive use.

## What this document covers

- The HNSW graph structure and why Brain uses it.
- The parameters Brain ships with and the rationale.
- Insertion, search, and deletion procedures.
- The lack of dedicated persistence and the rebuild path on startup.
- The maintenance worker that detects and repairs topology degradation.
- The concurrency model: lock-free reads, single-writer writes, epoch-managed reclamation.
- Inline filtering by model fingerprint, kind, context.

## What this document does not cover

- **The HNSW algorithm itself** is summarized in [`01_hnsw_basics.md`](01_hnsw_basics.md) but the original paper ([Malkov & Yashunin, "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs"](https://arxiv.org/abs/1603.09320)) is the authoritative source for the algorithm.

- **Vector storage** — the vectors live in the arena; this spec assumes they're available. See [08. Storage: Arena & WAL](../08_storage/00_purpose.md).

- **The operations that use ANN search** — defined in [05. Operations](../05_operations/00_purpose.md). This spec specifies the search primitive; how RECALL/PLAN/REASON use it is over there.

- **The query planner that decides when to use ANN search vs. other strategies** — defined in [12. Query Optimizer](../12_query_optimizer/00_purpose.md).

## 1. The role of the vector index

The vector index answers one question: "given a query vector q, what are the K most-similar memory vectors in the shard?"

This question must be answered:

- **Quickly.** Better than O(N) scan of all vectors. HNSW achieves O(log N) typical case.
- **Accurately enough.** The "approximate" in ANN means Brain may miss the literal top-K; Brain accepts high but not perfect recall (typically 90-99% at default settings).
- **Concurrently.** Many queries in flight; insertions happening; reads see consistent state without locks.

## 2. Why HNSW

Brain selected HNSW over alternatives:

- **Brute-force scan.** O(N). Fine for tiny indexes; doesn't scale.
- **IVF (Inverted File Index).** Cluster-based. Simpler than HNSW. Better for very large indexes; HNSW better for mid-scale (Brain's target).
- **Annoy (random forest of trees).** Simpler. Worse recall at high K.
- **DiskANN.** Designed for SSDs holding indexes too large for RAM. Adds I/O complexity Brain does not need at this scale.
- **ScaNN.** Google's competitive ANN. Excellent quality. Less mature in Rust ecosystem.
- **PQ + IVF hybrids.** Compression + clustering. Useful at extreme scale; pure IVF (no graph) remains overkill for Brain's mid-scale target. HNSW with PQ-compressed graph payload **is** in scope as an opt-in extension — see [`07_hnsw_pq.md`](07_hnsw_pq.md).

HNSW wins because:

- It produces high recall (95-99%) at competitive speed.
- It works in-memory; no I/O overhead per query.
- It supports incremental updates (insert/delete without full rebuild).
- It's well-implemented in [`hnsw_rs`](https://github.com/jean-pierreBoth/hnswlib-rs), providing a Rust crate Brain does not have to write.

## 3. The crate: hnsw_rs

Brain uses [`hnsw_rs`](https://github.com/jean-pierreBoth/hnswlib-rs) (also known as `hnswlib-rs`). Pure Rust, MIT/Apache 2.0 licensed.

What the crate provides:

- The HNSW algorithm with insertion and search.
- Multi-layer graph structure.
- Customizable distance functions (Brain uses cosine / dot product).
- Reasonable performance.

What Brain adds on top:

- The single-writer-per-shard discipline ([`04_concurrency.md`](04_concurrency.md)).
- Lock-free reads via epoch-based reclamation.
- Inline filters for fingerprint / kind / context.
- The maintenance worker for topology drift.
- Integration with Brain's arena and metadata.

Brain does not fork hnsw_rs. Brain uses it as a library and layers concurrency and filtering on top.

## 4. Per-shard HNSW

Each shard has one HNSW index. The index covers all of the shard's active (non-tombstoned) memories.

Cross-shard queries don't combine HNSW indexes; the query is fanned out to each shard, each shard runs its own HNSW search, and results are merged. See [16. Sharding & Clustering](../16_sharding/00_purpose.md) §Cross-Shard Queries.

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

For very large shards (10M+) or cold caches, latency grows. Brain's targets in [01.05 Hardware and Targets](../01_architecture/05_hardware_and_targets.md) are calibrated against the typical case.

## 7. Recall targets

"Recall@10" means: of the true top-10 nearest neighbors, what fraction does the ANN return?

- Default ef_search = 64: recall@10 ≈ 95-98%.
- ef_search = 128: recall@10 ≈ 98-99.5%.
- ef_search = 256: recall@10 ≈ 99.5-100%.

Higher ef_search costs more compute. The default 64 is the balance most workloads accept.

## 8. The interface

The vector index exposes:

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

Brain sustains writes and reads simultaneously. The HNSW must support:

- A search seeing all memories committed before the search started.
- A search not seeing memories inserted during the search (consistency).
- An insert not blocking the search.

This requires concurrency management, detailed in [`04_concurrency.md`](04_concurrency.md).

## 10. Position in the architecture

The vector index sits between the storage layer (which holds vectors) and the query planner (which orchestrates search):

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

*Continue to [`01_hnsw_basics.md`](01_hnsw_basics.md) for HNSW basics.*
