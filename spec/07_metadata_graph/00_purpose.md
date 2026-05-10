# 07.00 Purpose

This document specifies the metadata + graph store. Together with the vector arena ([05. Storage](../05_storage_arena_wal/)) and the WAL, these comprise the substrate's persistent state.

## What this document covers

- The role of the metadata store in the architecture.
- The choice of redb as the embedded engine.
- The table layout: memories, edges, contexts, idempotency, contexts.
- How variable-length data (text) is stored.
- Transaction semantics within the metadata store.
- Concurrency between metadata operations and the rest of the substrate.

## What this document does not cover

- **Vector storage.** Defined in [05. Storage: Arena & WAL](../05_storage_arena_wal/).
- **The query planner that uses the metadata.** Defined in [08. Query Planner](../08_query_planner/).
- **Background workers (consolidation, decay, etc.).** Defined in [11. Background Workers](../11_background_workers/).

## 1. The role of the metadata store

The metadata store holds:

- **Memory metadata** — for each memory, its kind, context, salience, model fingerprint, slot ID, timestamps, etc.
- **Edges** — relationships between memories: CAUSED, FOLLOWED_BY, DERIVED_FROM, SIMILAR_TO, CONTRADICTS, SUPPORTS, REFERENCES, PART_OF.
- **Contexts** — named buckets memories belong to.
- **Idempotency** — for replay protection: maps RequestId to the resulting MemoryId.
- **Text** — memory text content (for ENCODE) and consolidated content.
- **Bookkeeping** — checkpoints, model fingerprints registry, agent metadata.

The store is per-shard: each shard has its own redb file. Cross-shard queries fan out to multiple shards and merge.

## 2. Why a separate store

The substrate could have stored metadata in the WAL alone, replaying it on startup to build in-memory structures. Why a separate persistent store?

- **Random access.** Some operations (looking up a memory's metadata) are random-access; a B-tree is fast for this. Replaying the WAL is sequential.
- **Reduced startup time.** With persistent metadata, recovery only replays records since the last checkpoint, not the entire history.
- **Compact representation.** The WAL contains the history of mutations; the metadata store contains only the current state. The metadata store is much smaller.

The cost: an additional persistent structure to keep consistent with the WAL. The WAL is still the source of truth; the metadata store is a derived representation maintained for fast random access.

## 3. ACID requirements

The metadata store provides:

- **Atomicity** — multi-key updates within a transaction either all happen or none do.
- **Consistency** — invariants (e.g., edge endpoints exist) are preserved.
- **Isolation** — concurrent reads see a consistent snapshot.
- **Durability** — committed transactions survive crashes (with the WAL ensuring redb's commits sync correctly).

These are needed because:

- Encoding a memory with edges is multi-key (memory record + edge records); must be atomic.
- Searches concurrent with edits must see a consistent view.
- Crashes shouldn't leave half-applied state.

## 4. The redb dependency

[redb](https://github.com/cberner/redb) is the engine:

- Pure Rust (no native dependencies, no cgo).
- ACID transactions.
- B-tree indexed.
- MVCC for concurrency.
- Good documentation, active maintenance.

We chose redb over alternatives in [`01_redb_choice.md`](01_redb_choice.md).

## 5. Per-shard deployment

Each shard's metadata store is a single redb file:

```
data/<shard_uuid>/metadata.redb
```

This file contains all the tables described in this spec. Different shards have different files; no cross-file or cross-shard queries.

## 6. Latency targets

- Memory metadata read (cached): < 1 µs.
- Memory metadata read (cold): < 10 µs.
- Single-row write within a transaction: < 10 µs.
- Transaction commit (with redb's internal sync): 0.1-1 ms.

These match the storage layer's latency budget. The metadata store contributes meaningful but bounded latency to writes; reads are negligible.

## 7. Size targets

For a typical 1M-memory shard:

| Table | Approx size |
|---|---|
| memories | ~150 MB (150 bytes × 1M) |
| edges | ~200 MB (8 edges/memory × 25 bytes/edge) |
| text | ~1 GB (1 KB/memory) |
| contexts | < 1 MB (few thousand contexts max) |
| idempotency | ~50 MB (50 bytes × 1M, with TTL) |
| **Total** | **~1.4 GB per 1M memories** |

Plus the vector arena (~1.5 GB) and HNSW (~150 MB), the total per-shard storage is ~3 GB per 1M memories. Operationally, plan for ~5 GB to give headroom.

## 8. The interface to the rest of the substrate

The metadata store is accessed through a per-shard wrapper:

```rust
struct MetadataStore {
    db: redb::Database,
}

impl MetadataStore {
    fn get_memory(&self, id: MemoryId) -> Option<MemoryMetadata>;
    fn put_memory(&mut self, txn: &mut WriteTxn, m: &MemoryMetadata);
    fn list_edges(&self, source: MemoryId, kind: EdgeKind) -> Vec<Edge>;
    // ...
}
```

The wrapper hides redb's specifics. Higher layers (executors, planners) talk to this wrapper.

## 9. The metadata is not the source of truth

The WAL is the source of truth ([05.00 §4](../05_storage_arena_wal/00_purpose.md)). The metadata store is a derived representation:

- After WAL fsync, the metadata is updated.
- On crash, recovery replays the WAL to bring the metadata back into sync with the WAL.

If the metadata store is corrupted but the WAL is intact, recovery rebuilds the metadata from the WAL. This is slower but correct.

If the WAL is corrupted, recovery is more difficult (the metadata may not be consistent with itself). Backup/restore from snapshot is the answer.

## 10. The text storage decision

Memory text is stored in the metadata store (in a dedicated table) rather than alongside the vector in the arena. This is because:

- Text is variable-length; the arena's fixed-size slots aren't a good fit.
- Text isn't read on every search; metadata-store random access is fine.
- The metadata store's transactional semantics naturally protect text alongside memory metadata.

Detailed in [`07_text_storage.md`](07_text_storage.md).

---

*Continue to [`01_redb_choice.md`](01_redb_choice.md) for the engine choice.*
