# 10.04 The Publication Protocol

How the writer makes new state visible to readers atomically.

## 1. The publication concept

When the writer changes a data structure, readers shouldn't see partial state. They should see either the old state or the new state — never an inconsistent in-between.

**Publication** is the moment when the writer makes the new state available. After publication, future readers see the new state; readers who started before continue with the old.

## 2. Where publication is needed

In Brain:

- **HNSW state**: when the writer adds nodes or rebuilds the graph, readers must see consistent snapshots.
- **Routing tables**: when shards are added/removed, requests must be routed consistently.
- **Configuration**: when settings change, operations should see one config or the other.

Each of these uses ArcSwap for atomic publication.

## 3. The ArcSwap pattern

```rust
use arc_swap::ArcSwap;

struct Shard {
    hnsw: ArcSwap<HnswState>,
    // ...
}

// Reader:
fn search(&self, query: &[f32]) -> Vec<...> {
    let hnsw = self.hnsw.load();        // Atomic load of Arc
    hnsw.search(query, ...)
}

// Writer:
fn publish_new_hnsw(&self, new_hnsw: Arc<HnswState>) {
    self.hnsw.store(new_hnsw);          // Atomic store
}
```

The `load` returns an `Arc<HnswState>`; the reader holds it for the duration of the search. Even if the writer publishes a new state during the search, the reader's reference is still valid.

The `store` replaces the Arc atomically. Subsequent loads see the new state.

## 4. The "build then publish" pattern

The writer:

1. Builds the new state in isolation (no other task sees it).
2. Wraps it in an Arc.
3. Atomically swaps it in via ArcSwap.store.
4. Drops its own reference to the old Arc (the readers may still hold references; Arc's refcount handles this).

When the last reader drops its reference, the old state is dropped.

## 5. The cost of publication

ArcSwap.store:

- Atomic write of a pointer.
- ~50 ns.

ArcSwap.load:

- Atomic read of a pointer.
- ~10-50 ns.

Cheap enough to be in any code path.

## 6. The frequency of publication

For HNSW, publication happens:

- Periodically (every 10 ms by default).
- After every major change (e.g., a maintenance rebuild's swap).
- On-demand for read-after-write (the writer publishes immediately).

Periodic publication amortizes the build cost. Each publication captures recent inserts.

## 7. The pending buffer pattern

Between publications, the writer accumulates changes in a "pending buffer":

```rust
struct WriterState {
    published: Arc<HnswState>,
    pending: Vec<PendingInsert>,
}
```

On each insert, the writer adds to `pending`. Periodically, it merges:

```rust
fn merge_pending(&mut self) {
    let mut new_state = (*self.published).clone();   // Deep clone
    for insert in self.pending.drain(..) {
        new_state.apply(insert);
    }
    let new_arc = Arc::new(new_state);
    self.hnsw_swap.store(new_arc.clone());
    self.published = new_arc;
}
```

This is "publication via copy-on-write".

## 8. The "deep clone" cost

Cloning an HNSW state for ~1M nodes is expensive (~150 MB of memory + the clone time). We don't actually deep-clone for routine inserts.

Instead, the HNSW supports incremental publication:

- The writer mutates a private "draft" state.
- The draft shares immutable parts with the published state (structural sharing via Arc).
- When ready to publish, the new state is wrapped in an Arc and swapped.

This is the "persistent data structure" pattern. The HNSW implementation (or Brain's wrapper around hnsw_rs) supports this.

## 9. The implementation reality

In practice, with hnsw_rs:

- The HNSW is mutable internally.
- Brain wraps it with `Arc<HnswWrapper>` where HnswWrapper has `&mut` access protected by the writer-only discipline.
- Publication is simpler: when the writer is done with a batch, it advances an epoch (so readers know "more than X is now visible").

We use ArcSwap mainly for major swaps (full rebuilds) where we have an entirely new HNSW state to publish.

For routine inserts, the writer mutates the existing HNSW; readers see the new nodes after epoch advance.

## 10. The "rebuild swap" path

When the maintenance worker rebuilds the HNSW:

1. Build the new HNSW in the background (takes seconds).
2. Wrap in an Arc.
3. ArcSwap.store the new Arc.
4. Old HNSW's Arc count drops; when no readers hold it, it's freed.

The old HNSW may live a few hundred milliseconds after the swap (until in-flight searches complete). Memory peaks during this window (both old and new are live).

## 11. The "atomic swap" semantics

ArcSwap's swap is **lock-free** and **wait-free**:

- A swap doesn't block any reader.
- A read doesn't block any swap.
- Multiple readers can load concurrently.
- A swap completes in bounded time, regardless of reader count.

This is critical for predictable latency.

## 12. The "many publications" cost

If the writer publishes very frequently (e.g., on every insert), the cost is:

- Per-publish: ~50 ns for the swap, plus the Arc clone overhead.
- Per-read: same cost (load is symmetric to store).

But the writer would be allocating and freeing Arcs constantly — heap pressure.

So we don't publish per-insert. Publications are batched: after a window of inserts, one publish.

## 13. The publication ordering

Publications on different shards are independent. Each shard's publications happen on its own schedule.

Within a shard, publications are ordered by the writer's progress. There's a clear "before" and "after" for each publication.

## 14. The reader's view during publication

A reader holds a reference loaded before a publication:

- The reader's view doesn't change during the publication.
- The publication updates the "current" reference; the reader's old reference is unchanged.
- After the reader drops its reference, the old state's Arc count drops.

## 15. The "publication can fail" question

If the writer can't allocate the new state (OOM during clone), the publication is skipped. The old state remains active. The writer logs the error and tries again later.

In practice, OOM in this path is very rare. We provision generously.

## 16. The "no GC pause" guarantee

Publication doesn't have a "stop-the-world" pause. Readers continue running during publication. The swap is atomic, but it's a single instruction; no waiting.

This contrasts with traditional GC pauses in some systems. Brain doesn't have GC pauses.

## 17. The role in the read-after-write hint

When a client requests `consistency: ReadAfterWrite`, the substrate ensures the read sees the latest publications:

- Wait for the writer to publish all pending writes.
- Then proceed with the read.

The wait is on the publication, not on the writer's overall state. The client sees any publication ≥ the LSN of its previous write.

## 18. The publication LSN

Each publication has an associated LSN — the WAL LSN at the time of publication. Readers can:

- Check the published LSN.
- Compare to their requirement (e.g., "I want at least LSN X").
- Wait if needed.

The substrate tracks per-shard "published LSN" as an atomic. Readers check it cheaply.

## 19. The "publish nothing" case

Sometimes the writer has nothing new to publish (no recent operations). The publish-cycle skips: no allocation, no swap.

This keeps idle shards lightweight.

## 20. The summary

Publication via ArcSwap:

- Atomic, lock-free, wait-free.
- Cheap in steady state.
- Provides clear before/after semantics.
- Plays well with Arc-based memory management.

It's the right primitive for "swap the entire current view of a data structure". For finer-grained reclamation (within a structure), we use crossbeam-epoch.

---

*Continue to [`05_arc_swap.md`](05_arc_swap.md) for ArcSwap details.*
