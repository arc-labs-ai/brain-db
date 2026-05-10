# 10.05 Use of ArcSwap

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
struct ShardState {
    hnsw: ArcSwap<HnswState>,
    // ...
}

// Read path
fn search(&self, query: &[f32]) -> Vec<...> {
    let hnsw = self.hnsw.load_full();
    // hnsw is an Arc; valid for the duration of this function
    hnsw.search(query, ...)
}

// Write path
fn publish_new_hnsw(&self, new_state: HnswState) {
    let new_arc = Arc::new(new_state);
    self.hnsw.store(new_arc);
}
```

The reader's `load_full()` returns an Arc; while held, the Arc keeps the HnswState alive. Even if the writer publishes a new state, the reader's old state remains valid until dropped.

## 6. The Arc semantics

`Arc<T>`:
- Atomically refcounted shared ownership.
- `Clone`: increments refcount.
- `Drop`: decrements refcount; if reaches zero, drops the inner T.

For Brain's HNSW:
- The writer holds one Arc (for ongoing mutations or as the current "active" state).
- Each in-flight reader holds an Arc (returned by load_full).
- When old states are no longer referenced, they're freed.

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

For coordinated swaps where we need to verify the previous state, `compare_exchange` is available. Used rarely.

## 9. The atomicity guarantee

`store` and `load` are atomic in the C++/Rust memory model sense:

- A store has release semantics.
- A load has acquire semantics.

This means:
- Writes done before the store are visible to readers after the load.
- No partial state is observable.

For Brain's purposes, this is exactly what we need.

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

The library's overhead is < 1% of the substrate's total cost.

## 12. The memory cost

Each ArcSwap holds:
- A pointer to the current Arc.
- ~16 bytes per ArcSwap.

The Arc itself holds the data plus refcount (~16 bytes overhead).

For Brain's number of ArcSwaps (a handful per shard): negligible memory.

## 13. The interaction with cloning

For HNSW, full deep-cloning is expensive. We don't clone the HNSW on every store; we use structural sharing where possible.

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

In Brain, ArcSwaps are typically per-shard; they're accessed only by the shard's executor's tasks. We don't usually share them across shards.

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

For data that doesn't change (e.g., compiled regex patterns), a plain `Arc<T>` is sufficient. We use ArcSwap only when atomic publication is needed.

For data that changes frequently within a single thread (e.g., the writer's local state), no atomicity needed; just a regular variable.

## 19. The "ArcSwap only for shared mutable state" rule

ArcSwap is for state that's shared across tasks AND mutated. If state is shared but immutable, plain Arc works. If state is mutable but private, regular variable works.

This rule keeps usage clear: ArcSwap appearing in code signals "this is shared mutable state with publication semantics".

## 20. The summary

ArcSwap gives Brain:

- Lock-free, wait-free atomic publication.
- Simple API.
- Compatible with Rust's ownership model via Arc.

It's the right primitive for "atomically swap the current version of this data structure". Combined with the single-writer-per-shard discipline and Arc's automatic refcounting, it provides Brain's publication mechanism.

---

*Continue to [`06_crossbeam_epoch.md`](06_crossbeam_epoch.md) for crossbeam-epoch details.*
