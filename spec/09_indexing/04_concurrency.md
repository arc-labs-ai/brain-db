# 09.04 Concurrency Model

How reads and writes coexist on the HNSW. The general concurrency story is in [14. Concurrency](../14_concurrency/00_purpose.md); this file covers the HNSW-specific aspects.

## 1. The single-writer discipline

Within a shard there is exactly one HNSW writer handle. It is not `Clone`, so the type system permits no second one, and every insert and tombstone goes through it. Requests on a shard overlap — the shard admits each dispatch under a permit and detaches it onto the shared executor — but an insert runs to completion without an await, so on a single-threaded executor nothing interleaves with it. There is no concurrent insert path within a shard.

Across shards, writers run in parallel — each shard has its own writer.

Reads (searches) are concurrent and lock-free.

## 2. Why single-writer

Multi-writer HNSW would require:
- Lock around the entry point (every insert may update it).
- Lock around each node's edge list (concurrent inserts may modify the same neighbor's edges).
- Coordination to ensure consistency.

The locking adds overhead and complexity; throughput doesn't scale linearly with writer count anyway because of contention on the entry point and dense regions of the graph.

Single-writer-per-shard:
- No writer-vs-writer ordering to arrange within a shard's HNSW.
- Throughput scales with shard count.
- The writer runs on the shard's dedicated executor (Glommio); no context switching.

## 3. Lock-free reads

Searches read the HNSW concurrently with the single writer. The reads are lock-free.

The mechanism uses **arc-swap** to publish the HNSW reference:

```rust
struct SharedHnsw {
    main: Arc<ArcSwap<MainEpoch>>,       // the published graph
    pending: Arc<RwLock<PendingBuffer>>, // not yet folded into it
}

/// An immutable HNSW snapshot for a single published epoch.
struct MainEpoch {
    index: HnswIndex,
    epoch_id: u64,
}

fn search(&self, query: &[f32], k: usize) -> Vec<...> {
    let main = self.main.load();           // atomic load of the Arc
    let from_main = main.index.search(query, k, ...);
    let from_pending = self.pending_search(query, k, ...);
    merge_dedupe_descending(from_main, from_pending, k)
}
```

`ArcSwap` provides atomic load/store of `Arc<T>`. Loads are wait-free; stores are wait-free.

A reader's `load()` returns an `Arc<MainEpoch>` that's valid until dropped. The reader can use it without coordination — even if a flush concurrently publishes a new epoch, the reader's reference keeps the old graph alive until the reader drops it, at which point Arc's refcount frees it. A published `MainEpoch` is never mutated, so there is nothing for a reader to race.

The pending buffer is the one piece a search locks, and it takes a read guard over a flat `Vec` it then scans.

## 4. The publication of inserts

A published graph is immutable, so an insert cannot modify one. Nor does it build a new one: swapping the whole graph per insert would rebuild and free it on every write.

So the writer appends to the **pending buffer** instead, and a search merges that buffer with the published graph. Visibility therefore does not wait on publication — §6 covers what a reader sees — and publication is free to be rare.

Publication is a **flush**: a new graph is built from the authoritative vectors plus the buffered ones, and `flush_with_rebuild` stores the new epoch and drains the folded entries from the buffer in one step. The build runs with no lock held, and the drain reconciles by identity rather than by id, so an entry re-inserted with a newer vector while the build ran survives instead of being dropped against a graph that folded the older one.

Nothing in that sequence costs the reader a wait: a search either loads the old epoch and finds the vector in pending, or loads the new epoch and finds it in the graph.

## 5. What advances an epoch

A flush is driven by the **HNSW maintenance worker**, not by the write path. The worker wakes every 300 s, collects the index's entry and tombstone counts, and decides:

- tombstone ratio > 0.30 → rebuild now.
- tombstone ratio > 0.15 → record the intent and move on. Nothing is rebuilt at this threshold; it exists so the condition is observable before it is acted on.
- otherwise → nothing to do.

So a new epoch is published when the graph has degraded enough to be worth rebuilding, and on an operator's explicit `RebuildHnsw` / `RebuildIndex` op. It is a cost-and-structure cadence, not a visibility one: no write waits for it and no read needs it.

## 5a. A tombstone frees nothing until the next generation

Reclamation needs no epoch protocol of its own, and it does not operate per node — in either direction. A FORGET sets one bit in the index's tombstone bitmap. The node stays in the graph with its vector and its edges, and search skips it on the way out of the candidate list. Nothing is freed, deferred or otherwise: there is no per-node free path in the index at all.

Memory comes back only when the generation is replaced. A rebuild constructs a fresh graph from the live set, which omits the tombstoned memories and starts from an empty bitmap, and the old graph is freed once the last in-flight search holding its `Arc` drops the reference. Clearing a bitmap in place would therefore be meaningless — the graph it describes is already gone.

The pending tombstone overlay follows the graph rather than the bitmap: the same call that publishes the new graph drops exactly the overlay ids the build folded as tombstoned, and keeps any tombstone that arrived after the snapshot for an id folded as active. So the overlay never shrinks ahead of the graph that makes it redundant.

The capacity consequence is worth stating plainly: **a FORGET-heavy workload holds the forgotten memories' index footprint until the next rebuild.** With the maintenance worker's cadence and thresholds above, that is up to 300 s after the tombstone ratio crosses 0.30 — and indefinitely below it, because the 0.15 branch rebuilds nothing. A deployment that deletes heavily and wants the space back sooner runs the rebuild op rather than waiting for the ratio.

Nothing in that is a correctness concern: a tombstoned memory is invisible to search from the moment the bit is set (§6). It is a footprint concern, and it is the price of not paying for eager deletion on a graph that doesn't support it.

The general concurrency model, including how publication relates to the shard's other lock-free reads, is in [14. Concurrency](../14_concurrency/03_lock_free_primitives.md).

## 6. The "see new memory" timing

For a read to see a newly-encoded memory, the encode must complete the writer's insert — and nothing more. **There is no epoch to wait for on the read path.**

The index is served as two parts merged at search time: the published main graph, swapped via `ArcSwap`, and a **pending buffer** of full-precision vectors the writer appends to directly. A search scores the main graph, brute-forces the pending buffer at exact cosine, and merges the two, preferring the pending entry on a collision because its vector is the later write. A tombstone goes into the pending tombstone set and filters the very next search.

So the read-after-write delay is **zero**, and there is no hint to pass. The epoch advance still happens, on its own cadence, but what it does is fold pending entries into the main graph — a cost-and-structure concern, not a visibility one. A client never has to know it exists.

The brute-force scan over pending is what makes this affordable: its size is bounded by how often the flush runs, not by corpus size, so the cost stays with the writer instead of leaking into every reader's mental model.

## 7. Concurrent reads of the same HNSW

Multiple search tasks can read the same HNSW state concurrently. Each gets its own `Arc<MainEpoch>` reference; reads are independent.

The HNSW data structure itself is read-only during search — all distance computations are pure functions on the graph and the query.

## 8. The HNSW's internal locking

hnsw_rs has its own internal locking for multi-threaded inserts. Brain doesn't use that mode (single-writer obviates it), but the locks still exist. They are taken only where Brain calls into hnsw_rs with a vector — the graph build a flush runs, never the write path, which touches the pending buffer instead.

Patching hnsw_rs to remove the unused locks was considered and rejected — the locks are no-ops in single-threaded use, and patching adds maintenance burden. The overhead of unused locks is measurable but small (~5% slowdown on tight insert loops), and a flush is the only place that loop runs.

## 9. The id_map concurrency

The forward and reverse id maps live **inside** the graph they belong to:

```rust
struct MainEpoch {
    index: HnswIndex,   // Hnsw + IdMap + tombstone bitmap
    epoch_id: u64,
}
```

They are built with that graph and never touched again, so a search resolving an internal id reads maps that cannot be concurrently written, and no separate synchronisation applies to them. The whole `MainEpoch` is published atomically, so the graph and the maps that interpret its ids can never be observed out of step.

A pending entry carries its `MemoryId` directly and needs no id mapping at all — which is why a write can be visible without any map being updated.

## 10. The pending buffer

```rust
struct PendingBuffer {
    entries: Vec<PendingEntry>,        // full-precision vectors
    tombstoned: HashSet<MemoryId>,     // overlay over the published graph
}

struct PendingEntry {
    memory_id: MemoryId,
    vector: [f32; VECTOR_DIM],
    tombstoned: bool,
}
```

Each insert appends to `entries`, or overwrites the entry already there for that id and clears its tombstone, so re-encoding an id leaves exactly one buffered vector — the latest. A tombstone adds the id to `tombstoned` and marks any buffered entry.

A search reads the buffer as well as the published graph, so there is no staleness window to bound: the buffer is not a queue of work the reader is waiting on, it is the newest tier of the index. The flush (§4) is what folds it into the graph, and the buffer's size is what the flush cadence bounds — the cost of the brute-force scan, not the visibility of a write.

## 11. Read-after-write needs no hint

When a client encodes and immediately recalls, the memory is there. The insert happens inside the write submission — before the metadata commit and before the ack — and the search merges the pending buffer, so there is nothing to flush and nothing to wait for.

The encode response still carries an `lsn`, but it is for **chaining a subscription** (`encode → subscribe --from-lsn lsn+1`), not for gating a read. There is no per-query wait and no opt-in.

What a client may genuinely need to wait for is the write's *derived* state — its edges, its extracted entities, its HyPE questions — and that is observed through `StageCompleted` events or `ENCODE(wait: derived)`, which name the stage being waited on. See [`../05_operations/07_consistency.md`](../05_operations/07_consistency.md) §3.

## 12. The single-shard execution model

All operations on a shard run on the shard's dedicated executor (Glommio). This means:
- Writing and reading tasks run on the same OS thread.
- "Concurrent" operations are actually cooperative async — they yield to each other, which is also why a synchronous insert cannot be interleaved with.

This avoids cross-thread coordination but means heavy operations on a shard can starve other operations. Brain uses cooperative yields liberally to keep things fair.

## 13. Cross-shard concurrency

Operations on different shards run on different executors (different OS threads, different cores). They're truly parallel.

Cross-shard queries (e.g., a fan-out RECALL) submit per-shard searches in parallel and merge results.

## 14. Concurrency invariants

The HNSW's concurrency invariants:

1. A search returns results consistent with some state of the index — a published graph plus the buffer as it stood when the search read it.
2. Inserts are linearizable at the insert, not at publication: the vector is in the buffer when the writer returns, and the next search finds it there.
3. Removes are linearizable on the same terms: the id enters the tombstone overlay when the writer returns, and the next search filters it.
4. No partial states: a published graph is immutable, and a flush publishes the new one and drains the buffer under the same write guard, so a search never sees a vector in neither tier or counted twice.

Invariants 1 and 4 are maintained by publish-via-swap: readers always hold a complete graph, the one published before they loaded their reference. Invariants 2 and 3 are maintained by the buffer, which is why neither depends on when the next flush runs.

---

*Continue to [`05_filtering.md`](05_filtering.md) for filtering.*
