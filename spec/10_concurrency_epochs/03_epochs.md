# 10.03 Epoch-Based Reclamation

How Brain safely frees data structures that may have in-flight readers.

## 1. The problem

When the writer changes a data structure (e.g., the HNSW), readers in flight may still be using the old version. We can't free the old version immediately — readers would crash.

We can't keep all old versions forever — memory leak.

We need to know "when is it safe to free this old version?"

The answer: when no readers hold references to it. Epoch-based reclamation (EBR) gives us this signal.

## 2. The epoch concept

Time is divided into **epochs**. The substrate has a global epoch counter that the writer advances periodically.

When a reader starts, it **pins** to the current epoch. While pinned, the reader is "in" that epoch.

When the writer wants to free old data, it tags it with the current epoch. The data can be freed once all readers have advanced past that epoch.

The check is: "is there any pinned reader in epoch E or earlier?" If yes, can't free yet. If no, free.

## 3. The crossbeam-epoch library

Brain uses [`crossbeam-epoch`](https://github.com/crossbeam-rs/crossbeam) for this:

- `Guard`: a reader's pin.
- `Atomic<T>`: a pointer that can be safely swapped under guards.
- `Owned<T>`: data that the writer owns; can be tagged for deferred freeing.

The library handles the epoch counter, the per-thread tracking, and the safe-free logic.

## 4. When Brain uses epochs

Most of Brain's reads don't need epochs:

- redb's MVCC handles metadata reads (its own GC).
- ArcSwap + Arc handle the HNSW publication (Arc's refcount is sufficient).
- Mmap'd arena data is stable as long as the file is open.

Where epochs are useful:

- Within the HNSW data structure, for fine-grained reclamation of internal nodes during rebuild.
- For any other place we need lock-free reclamation of data that isn't refcounted.

In v1, epoch-based reclamation is mostly used inside the HNSW for incremental cleanup; the high-level publication is via ArcSwap.

## 5. The HNSW's internal use

When the HNSW maintenance worker removes a node:

1. Mark the node as removed (a flag).
2. Don't free its memory yet — searches in flight may still visit it.
3. Tag the node for deferred freeing in the current epoch.
4. After all readers have advanced past this epoch, the node is freed.

This is `crossbeam-epoch`'s standard pattern. The library handles the bookkeeping.

## 6. The reader's pin

A search:

```rust
fn search(&self, query: &[f32]) -> Vec<...> {
    let guard = epoch::pin();   // Pin to current epoch
    // ... do the search ...
    drop(guard);                // Unpin
}
```

The `pin()` is cheap — just an atomic increment. The `drop` is symmetric.

While pinned, the search holds a "reservation" that prevents the writer from freeing data the search might be reading.

## 7. The writer's pacing

The writer advances the epoch periodically:

- After every batch of writes.
- At a maximum of 100 µs intervals (configurable).
- When a long-pinned reader is detected (to encourage advancing).

Each advance is a single atomic increment. Cheap.

If a reader is pinned for too long (a bug or stuck task), the writer waits — it can't safely free data tagged for that epoch. After a configurable threshold (default 1 sec), the substrate logs a warning.

## 8. The safety property

The epoch protocol's invariant:

```
For all data D tagged for free in epoch E:
  D is freed only after all readers have advanced past E.
```

Equivalently:

```
A reader pinned in epoch E
  observes only data that hasn't been freed in epoch E or earlier.
```

This is the core safety property. It's enforced by the library; we just use the API correctly.

## 9. The performance characteristics

Epoch operations are cheap:

- `pin()`: ~10 ns (atomic increment).
- `drop(guard)`: ~10 ns.
- Epoch advance: ~50 ns (atomic increment + checks).
- Deferred free: ~100 ns (queue addition + later actual free).

These are fast enough to be in the hot path. We can pin per-search without measurable overhead.

## 10. The "long pin" problem

If a reader pins and never unpins (a stuck task, a bug), the writer can't free data freed in epochs after that pin. Memory grows.

Detection:

- Each pin records its epoch.
- The writer monitors the oldest pinned epoch.
- If an epoch is pinned for too long, the writer logs a warning.

Mitigation:

- The substrate's reader tasks have time limits. If exceeded, they're aborted.
- The pin is implicitly released when the task ends.

For v1, we accept that a pathologically-stuck task could cause memory growth until it's killed. Fine in practice.

## 11. The interaction with ArcSwap

Brain uses both ArcSwap (for HNSW publication) and crossbeam-epoch (for internal cleanup). They coexist:

- ArcSwap publishes a new HnswState; old HnswStates are freed when no Arc references remain.
- Within an HnswState, internal nodes (when removed) use epoch-based reclamation.

The two mechanisms are at different levels. ArcSwap is for "swap entire structure"; epochs are for "incremental cleanup within a structure".

## 12. The "fence" semantics

When the writer wants to ensure all in-flight readers have completed their current operations (e.g., before a major rebuild swap), it can do an explicit fence:

```rust
fn fence(&self) {
    let target_epoch = self.advance_epoch();
    // Wait until all readers have advanced past target_epoch
    while self.oldest_pin() < target_epoch {
        sleep(Duration::from_micros(100));
    }
}
```

This forces a barrier. Used sparingly; most operations don't need it.

## 13. The "deferred free vs immediate free"

For data that's known to have no readers (e.g., during a brief writer-only operation), immediate free is fine:

```rust
let data = Box::new(...);   // Allocate
// Use it
drop(data);                  // Free immediately
```

For data that may have readers, deferred free via the epoch protocol:

```rust
let data = epoch::Owned::new(...);
let guard = epoch::pin();
// Publish or use under guard
unsafe { guard.defer_destroy(data); }
```

The choice depends on whether other tasks may have references.

## 14. The "weak guarantees" of epochs

Epoch-based reclamation gives weak guarantees compared to garbage collection:

- It doesn't track which exact data structures are reachable.
- It free things when "no readers in old epoch" — even if no reader is actually using a specific item.

This works for our use cases: items are tagged for free when they're known to be unreachable from new operations; the question is only whether old operations might still see them.

## 15. The "epoch counter wrap" consideration

The epoch counter is 64-bit. It can advance once per 100 µs. So:

- 100K advances per second.
- 64-bit counter: 2^64 / 1e5 = 5.8 × 10^15 seconds = ~180 million years.

Wraparound is not a concern.

## 16. The "TLA+ verified" question

`crossbeam-epoch` has a documented design but isn't formally verified. Brain uses it as a black box; we trust the implementation.

For our level of correctness needs (no data corruption, no use-after-free), the library's testing is sufficient. We don't independently verify it.

## 17. The alternatives considered

We considered:

- **Hazard pointers**: similar to epochs but more complex to use.
- **RCU (read-copy-update)**: kernel-style; not idiomatic in user-space Rust.
- **Reference counting everywhere**: simpler but slower (every read touches an atomic).
- **Stop-the-world GC**: too disruptive.

Epoch-based reclamation is the best fit: low overhead, well-understood, mature library available.

## 18. The "test discipline"

Concurrency bugs in epoch usage are subtle. Testing involves:

- Loom tests for the lowest-level usage patterns.
- Stress tests with many concurrent readers and writers.
- Sanitizer runs (TSan, ASan, MSan) during CI.

A bug here can cause use-after-free crashes; the testing discipline catches them before release.

## 19. The "user code never directly uses epochs" rule

Brain's higher-level code (executors, planners) doesn't see epochs. The HNSW abstraction handles it internally. This keeps the surface small and the high-level code simple.

## 20. The summary

Epochs let Brain free old data structures safely without locks:

- Readers pin to an epoch (cheap).
- Writer tags old data with current epoch.
- Old data is freed when no readers remain in old epochs.

The mechanism is well-suited to internal use within data structures (HNSW). For higher-level publication, ArcSwap is simpler and equally effective.

---

*Continue to [`04_publication.md`](04_publication.md) for the publication protocol.*
