# 14.03 Lock-Free Primitives (ArcSwap + Arc)

> **TL;DR.** One crate does the lock-free heavy lifting: ArcSwap, for atomic publication of immutable snapshots. Reclamation falls out of `Arc`'s refcount — Brain runs no epoch protocol — and deletion is a tombstone bitmap plus a pending overlay, filtered at search time. This section covers the usage patterns, the lifetimes, and the discipline that keeps them sound.

## ArcSwap

[`arc-swap`](https://github.com/vorner/arc-swap) is a Rust library providing atomic swap of `Arc<T>`. Brain uses it for publication.

## 1. The library

`arc-swap` provides:

- `ArcSwap<T>`: a wrapper around `Arc<T>` with atomic load/store.
- Lock-free, wait-free operations.
- Optimized for read-mostly workloads (load is the hot path).

GitHub: [vorner/arc-swap](https://github.com/vorner/arc-swap).

## 2. The API

```rust
let arcswap: ArcSwap<MyType> = ArcSwap::new(Arc::new(initial_value));

// Read
let arc: Arc<MyType> = arcswap.load_full();

// Write
arcswap.store(Arc::new(new_value));

// Swap (returns the previous Arc)
let old: Arc<MyType> = arcswap.swap(Arc::new(new_value));
```

## 3. The performance

`load_full()`:
- Returns a fresh Arc; the refcount is incremented.
- ~50-100 ns on modern hardware.

`load()` (a "loose" load):
- Returns a `Guard<T>` that's cheaper to acquire but holds a reference for the guard's lifetime.
- ~10-20 ns.

`store()`:
- Atomic write; replaces the Arc.
- The old Arc's refcount drops; freed when refcount reaches zero.
- ~50-100 ns.

For Brain's frequencies (millions of loads/sec, thousands of stores/sec), these costs are negligible.

## 4. Where Brain uses ArcSwap

| Use site | Purpose |
|---|---|
| Per-shard HNSW reference | Publish HNSW state changes |
| Per-shard configuration | Hot-reload settings |
| Routing table | Cluster reconfiguration |

These are read-mostly: reads happen per-request; stores happen rarely (publication, config reload, rebalancing).

## 5. The HNSW use case

```rust
struct SharedHnsw {
    main: Arc<ArcSwap<MainEpoch>>,      // immutable published index + generation
    pending: Arc<RwLock<PendingBuffer>>, // inserts/tombstones not yet folded in
}

// Read path
fn search(&self, query: &[f32]) -> Vec<...> {
    let epoch = self.main.load();
    // epoch is an Arc guard; the index it names is immutable
    epoch.index.search(query, ...)
}

// Write path
fn publish(&self, new_index: HnswIndex) {
    let prev = self.main.load();
    self.main.store(Arc::new(MainEpoch {
        index: new_index,
        epoch_id: prev.epoch_id.wrapping_add(1),
    }));
}
```

The reader's `load()` yields a reference-counted handle; while held, it keeps that generation alive. Even if the writer publishes a replacement, the reader's generation stays valid — and unmodified — until dropped. The published index is never edited in place, which is what makes the refcount sufficient for reclamation.

## 6. The Arc semantics

`Arc<T>`:
- Atomically refcounted shared ownership.
- `Clone`: increments refcount.
- `Drop`: decrements refcount; if reaches zero, drops the inner T.

For Brain's HNSW:
- The writer holds a reference to the published slot solely to keep it alive regardless of reader cloning. It never mutates the published index.
- Each in-flight reader holds a reference to the generation it loaded.
- When a superseded generation is no longer referenced, it's dropped — the graph and its tombstone bitmap together.

## 7. The "load vs load_full" choice

`load_full()`:
- Returns `Arc<T>`.
- Increments refcount.
- Hold as long as you need.

`load()`:
- Returns a `Guard<T>` — a lightweight reference.
- The guard's lifetime bounds how long the reference is held.
- Cheaper acquisition but harder ergonomics.

Brain uses `load_full()` mostly. The 50 ns overhead is invisible compared to the work that follows.

## 8. The "store_full" alternative

ArcSwap also has variants like `swap` (returns the old value) and `compare_exchange` (CAS-style). Brain uses simple `store` mostly.

For coordinated swaps where Brain requires to verify the previous state, `compare_exchange` is available. Used rarely.

## 9. The atomicity guarantee

`store` and `load` are atomic in the C++/Rust memory model sense:

- A store has release semantics.
- A load has acquire semantics.

This means:
- Writes done before the store are visible to readers after the load.
- No partial state is observable.

For Brain's purposes, this is exactly what's required.

## 10. The lock-free progression

ArcSwap is **lock-free**:
- Operations complete in bounded time regardless of contention.
- A slow store doesn't block fast loads.

It's also **wait-free** for readers:
- A load completes regardless of what the writer is doing.

This gives predictable read latency. No "wait for the writer to finish" stalls.

## 11. The cost model

For Brain's typical frequency:
- Reads (load_full): ~10K/sec/shard. ~1 ms total CPU time.
- Stores: ~100/sec/shard (publications). ~0.01 ms total CPU time.

The library's overhead is < 1% of Brain's total cost.

## 12. The memory cost

Each ArcSwap holds:
- A pointer to the current Arc.
- ~16 bytes per ArcSwap.

The Arc itself holds the data plus refcount (~16 bytes overhead).

For Brain's number of ArcSwaps (a handful per shard): negligible memory.

## 13. The interaction with cloning

For HNSW, full deep-cloning is expensive. Brain does not clone the HNSW on every store; Brain uses structural sharing where possible.

When a major change requires a new HNSW (e.g., a maintenance rebuild), the new HNSW is built in the background, then a single ArcSwap.store publishes it. The old HNSW lives until refcounts drop.

## 14. The "stuck reader" risk

If a reader holds an Arc forever (a leak or a stuck task), the underlying state is never freed. Memory grows.

Brain mitigates by:
- Bounded read transaction lifetime.
- Task timeouts.
- Periodic monitoring of refcounts (an unusual count is a warning signal).

For typical workloads, readers complete in milliseconds; the issue doesn't arise.

## 15. The thread-safety properties

ArcSwap is `Send + Sync`. It can be shared across threads.

In Brain, ArcSwaps are typically per-shard; they're accessed only by the shard's executor's tasks. Brain does not usually share them across shards.

For routing tables (which all shards consult), there's a shared ArcSwap. Loads from many threads concurrently are fine.

## 16. The library's stability

ArcSwap is a mature library:
- v1.x stable for years.
- Used by many crates (tokio, etc.).
- Well-tested.

Brain pins to a specific version in `Cargo.toml`.

## 17. The alternatives considered

Alternatives to ArcSwap:

- **Manual `AtomicPtr`**: more control but harder to use safely.
- **`RwLock<Arc<T>>`**: simpler API but with locking overhead.
- **`crossbeam::atomic::AtomicCell<Arc<T>>`**: similar to ArcSwap but less optimized for this case.

ArcSwap is the most ergonomic and most optimized.

## 18. The "no need for ArcSwap" case

For data that doesn't change (e.g., compiled regex patterns), a plain `Arc<T>` is sufficient. Brain uses ArcSwap only when atomic publication is needed.

For data that changes frequently within a single thread (e.g., the writer's local state), no atomicity needed; just a regular variable.

## 19. The "ArcSwap only for shared mutable state" rule

ArcSwap is for state that's shared across tasks AND mutated. If state is shared but immutable, plain Arc works. If state is mutable but private, regular variable works.

This rule keeps usage clear: ArcSwap appearing in code signals "this is shared mutable state with publication semantics".

## 20. The summary

ArcSwap gives Brain:

- Lock-free, wait-free atomic publication.
- Simple API.
- Compatible with Rust's ownership model via Arc.

It's the right primitive for "atomically swap the current version of this data structure". Combined with the one-writer-per-shard discipline and Arc's automatic refcounting, it provides Brain's publication mechanism.

---

## Reclamation and Tombstones

The second half of the index's concurrency story: how superseded state is freed, and how deletions are represented given that the graph cannot efficiently remove a node.

## 1. There is no reclamation library

Brain runs no epoch-based reclamation protocol and uses no reclamation crate. There are no reader pins, no guards to scope, no `defer_destroy`, no garbage queue, no global reclamation counter, and no writer-side wait for readers to leave an interval.

Reclamation is `Arc`'s reference count over whole published generations, as described under ArcSwap above. That is the entire mechanism.

## 2. Why no protocol is needed

Epoch-based reclamation exists to solve a specific problem: a structure is mutated in place while lock-free readers walk it, so an interior node becomes unreachable at a moment when a reader may still be standing on it. The refcount cannot help, because nothing holds a reference to the node itself.

Brain never creates that situation. The published index is immutable. A publish builds a new one and swaps the pointer; it does not edit the structure a reader is inside. Once the unit of reclamation is the whole published generation, the refcount already tracks exactly the thing that must be protected — and tracks it precisely, rather than conservatively bounding it by an interval.

A protocol here would add pins, grace periods, stuck-reader diagnostics and unsafe deferred-destroy calls, in exchange for nothing Brain needs. The design's answer to fine-grained reclamation is to not have fine-grained reclamation.

## 3. What "epoch" means in Brain

The name survives, attached to a different idea. The published main is a `MainEpoch`: an immutable index plus an `epoch_id` generation number, bumped on each publish.

The generation number is reconciliation and observability state — a flush reports which generation it published, and a reader can ask which one it is seeing. Nothing pins to it, nothing waits on it, and no reader consults it to decide whether it may proceed.

It is deliberately a wrapping counter. No code orders two generation numbers across a wrap or holds one as a capability, so wrap-around is a non-event rather than a hazard to size a counter against.

## 4. Tombstones: the bitmap

The graph has no efficient eager deletion, so a forgotten memory is **tombstoned**: its node stays in the graph and is filtered out at search time.

The filter state is a bit-packed bitmap over the internal `u32` ids the graph library returns:

- `Vec<u64>`, 64 bits per word. At the per-shard ceiling of 10M memories this is ~1.25 MB; a `Vec<bool>` would be 10 MB.
- Grown lazily — a fresh bitmap allocates nothing.
- Every operation is idempotent. Setting an already-set bit, or clearing an already-clear one, is a no-op and does not perturb the count.
- The count is a running counter, updated only on real bit transitions, so reading it is O(1). The tombstone ratio is exposed per request, and an O(N/64) population count per call would be wasteful.

The bitmap deliberately knows nothing about `MemoryId`; the translation lives on the index via its id map.

## 5. Tombstones: the pending overlay

Above the published bitmap sits a set of ids tombstoned since the last publish. The overlay wins everywhere:

- A search excludes overlay ids from the main graph's candidates before merging in the pending tier.
- The presence and tombstone predicates consult the overlay before the published bitmap.
- The combined tombstone count adds overlay ids not already counted in main.

So a forget takes effect immediately, without waiting for a publish.

## 6. Tombstoned nodes are never individually freed

There is no per-node free to schedule, and therefore no window in which a reader could visit a node whose memory has been released.

A tombstoned node's memory goes away when its generation does. The next rebuild simply omits the tombstoned memories, so the freshly built index starts with an empty bitmap, and the old nodes are dropped along with the old `MainEpoch` when its last reader lets go.

This is also why the bitmap is snapshot-persisted byte-for-byte rather than recomputed: it is state belonging to one published generation, and it is restored with the generation it describes.

## 7. The cost model

- Taking a reference to the published generation: an atomic read plus a refcount bump.
- Publishing: an atomic pointer swap. Lock-free and wait-free — a publish never blocks a reader, a read never blocks a publish, and a publish completes in bounded time regardless of reader count.
- Freeing a superseded generation: one deallocation of the whole graph, paid by whichever reader drops last.
- Testing a tombstone: one indexed word load and a mask.

The dominant cost of a publish is *building* the new index, not publishing it.

## 8. The cost this model does carry

Between a publish and the moment the last reader drops the previous generation, both indexes are resident. Memory peaks across that window by about one extra copy of the graph.

The window is bounded by the longest in-flight search, and each publish leaves at most one superseded generation behind, so a slow reader costs a bounded amount rather than accumulating garbage. Shards are sized for that peak.

## 9. The type-level discipline

The reader handle is `Clone`; the writer handle deliberately is not. One writer per shard is enforced by the type system rather than by a runtime check.

The writer also holds a reference to the published slot purely to keep it alive regardless of how readers clone themselves. It never reads it to mutate the published index — there is no in-place mutation to make.

## 10. The summary

- Reclamation: `Arc` refcount over immutable published generations. No epoch protocol, no pins, no unsafe deferred destruction.
- Deletion: a tombstone bitmap plus a pending overlay, filtered at search time. Nodes are never individually removed or freed.
- Both choices follow from the same decision — replace the structure, never edit it under readers — and that decision is what makes the concurrency story small enough to state in full.

---

*Continue to [`04_yields.md`](04_yields.md) for cooperative yielding.*
