# 06.08 Concurrency Model

How reads and writes coexist on the HNSW. The general concurrency story is in [10. Concurrency + Epoch Model](../10_concurrency_epochs/); this file covers the HNSW-specific aspects.

## 1. The single-writer discipline

Within a shard, only one task writes to the HNSW: the **writer task**. All inserts and removes go through this task. There is no concurrent insert path within a shard.

Across shards, writers run in parallel — each shard has its own writer.

Reads (searches) are concurrent and lock-free.

## 2. Why single-writer

Multi-writer HNSW would require:
- Lock around the entry point (every insert may update it).
- Lock around each node's edge list (concurrent inserts may modify the same neighbor's edges).
- Coordination to ensure consistency.

The locking adds overhead and complexity; throughput doesn't scale linearly with writer count anyway because of contention on the entry point and dense regions of the graph.

Single-writer-per-shard:
- No locks within a shard's HNSW.
- Throughput scales with shard count.
- The writer is on a dedicated executor (Glommio); no context switching.

## 3. Lock-free reads

Searches read the HNSW concurrently with the single writer. The reads are lock-free.

The mechanism uses **arc-swap** to publish the HNSW reference:

```rust
type SharedHnsw = Arc<HnswState>;

struct ShardState {
    hnsw: ArcSwap<HnswState>,
}

fn search(&self, query: &[f32]) -> Vec<...> {
    let hnsw = self.hnsw.load();  // Atomic load of Arc
    // Use `hnsw` for the duration of the search
    // Other readers and writers don't affect this clone
    hnsw.search(query, ...)
}
```

`ArcSwap` provides atomic load/store of `Arc<T>`. Loads are wait-free; stores are wait-free.

A reader's `load()` returns an `Arc<HnswState>` that's valid until dropped. The reader can use it without coordination — even if the writer concurrently swaps in a new state, the reader's old reference stays alive until the reader drops it (at which point Arc's refcount handles cleanup).

## 4. The publication of inserts

When the writer task inserts a new memory into HNSW:

- **Without publication:** the search would not see the new memory.
- **With publication:** all subsequent searches see the new memory.

But "publishing" a single insert via swapping the entire HNSW is wasteful — we'd be allocating a new state and freeing the old on every insert.

So Brain doesn't swap on every insert. Instead, the writer:

1. Modifies the HNSW in-place.
2. Periodically publishes (advances the epoch).

Between publications, in-flight searches see a snapshot of the HNSW from before the publication. They may not see recent inserts.

## 5. The epoch protocol

The substrate uses **epoch-based reclamation** (via `crossbeam-epoch`):

- Each shard has an epoch counter, advanced periodically.
- Reads pin to an epoch; they see the HNSW state as-of the epoch.
- Writes happen "in the future" (a higher epoch); reads in the current epoch don't see them.
- When all reads have advanced past an epoch, the data freed by the writer in that epoch can be reclaimed.

In practice, the writer doesn't free data per-insert — most inserts are additive (new node added, no removals). The epoch is more relevant for removes, where edges are dropped.

Detailed epoch protocol in [10. Concurrency + Epoch Model](../10_concurrency_epochs/).

## 6. The "see new memory" timing

For a read to see a newly-encoded memory, the encode must:
1. Complete the writer's HNSW insert.
2. Trigger an epoch advance.

The epoch advances on a regular cadence (every few milliseconds), or when the writer explicitly requests it.

For typical workloads, the read-after-write delay is < 10 ms. For latency-sensitive workloads where read-after-write must be immediate, the substrate provides a hint: pass `consistency=ReadAfterWrite` to the read; the substrate forces an epoch advance and waits for it before processing the read.

## 7. Concurrent reads of the same HNSW

Multiple search tasks can read the same HNSW state concurrently. Each gets its own `Arc<HnswState>` reference; reads are independent.

The HNSW data structure itself is read-only during search — all distance computations are pure functions on the graph and the query.

## 8. The HNSW's internal locking

hnsw_rs has its own internal locking for multi-threaded inserts. Brain doesn't use that mode (single-writer obviates it), but the locks still exist.

We've considered patching hnsw_rs to remove the unused locks. Decided against — the locks are no-ops in single-threaded use, and patching adds maintenance burden. The overhead of unused locks is measurable but small (~5% slowdown on tight insert loops).

## 9. The id_map concurrency

The `id_map_forward` and `id_map_reverse` HashMaps are read by searches and written by inserts. They follow the same arc-swap pattern as the HNSW state:

```rust
struct HnswState {
    hnsw: hnsw_rs::Hnsw<...>,
    id_map_forward: HashMap<...>,
    id_map_reverse: HashMap<...>,
}
```

The whole `HnswState` is published atomically. Updating the id_maps means cloning them, modifying the clone, and publishing the new state.

For high-throughput inserts, full HashMap clones every insert would be expensive. We batch: the writer accumulates inserts in a "pending" buffer, then periodically merges them into the published state.

## 10. The pending buffer

Conceptually:

```rust
struct WriterState {
    published: Arc<HnswState>,
    pending: Vec<PendingInsert>,
    pending_since: Instant,
}
```

Each insert appends to `pending`. The writer flushes (rebuilds and publishes a new HnswState) every:

- 1000 pending inserts (size threshold), OR
- 100 ms (time threshold), OR
- Read-after-write hint received (immediate flush).

Searches use the published state (without the pending) until the next flush.

This means most searches see slightly-stale state. The lag is bounded by the flush thresholds.

## 11. The read-after-write hint

When a client encodes and immediately recalls (expecting to find the encoded memory), the substrate may need to flush pending inserts before the recall.

The mechanism:
- The encode response carries the encode's "publication LSN" (after this LSN is published, the new memory is searchable).
- A subsequent recall with `consistency=ReadAfterWrite` causes the substrate to wait for the publication LSN to be reached before searching.

This is a per-query opt-in. By default, recalls don't wait — they accept the eventual-consistency model.

## 12. The single-shard execution model

All operations on a shard run on the shard's dedicated executor (Glommio). This means:
- The writer task and reader tasks run on the same OS thread.
- "Concurrent" operations are actually cooperative async — they yield to each other.

This avoids cross-thread coordination but means heavy operations on a shard can starve other operations. The substrate uses cooperative yields liberally to keep things fair.

## 13. Cross-shard concurrency

Operations on different shards run on different executors (different OS threads, different cores). They're truly parallel.

Cross-shard queries (e.g., a fan-out RECALL) submit per-shard searches in parallel and merge results.

## 14. Concurrency invariants

The HNSW's concurrency invariants:

1. A search returns results consistent with some past state of the HNSW.
2. Inserts are linearizable: at the moment of publication, the new memory is visible.
3. Removes are linearizable: at the moment of publication, the removed memory is invisible.
4. No partial states: a search never sees half-inserted or half-removed nodes.

These invariants are maintained by the publish-via-swap mechanism. Readers always see a complete, consistent HNSW state — the one that was published before they loaded their reference.

---

*Continue to [`09_filtering.md`](09_filtering.md) for filtering.*
