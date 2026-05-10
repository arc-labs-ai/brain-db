# 10.06 Use of crossbeam-epoch

[`crossbeam-epoch`](https://github.com/crossbeam-rs/crossbeam) provides epoch-based reclamation for lock-free data structures.

## 1. The library

Part of the crossbeam project. Provides:

- `Atomic<T>`: an atomic pointer with safe reclamation semantics.
- `Owned<T>`: ownership of an unpublished item.
- `Shared<'g, T>`: a shared reference within a guard.
- `Guard`: a reader's pin to an epoch.
- `pin()`: enter an epoch (returns a Guard).

GitHub: [crossbeam-rs/crossbeam](https://github.com/crossbeam-rs/crossbeam).

## 2. The use case in Brain

Brain uses crossbeam-epoch primarily for:

- Internal HNSW node management during incremental cleanup.
- Other lock-free data structures within a single shard's scope (e.g., free lists for slot allocation).

For most Brain code, ArcSwap + Arc handle reclamation. crossbeam-epoch is for cases where Arc isn't ergonomic (e.g., for plain pointers or for fine-grained items within a structure).

## 3. The basic pattern

```rust
use crossbeam_epoch::{self as epoch, Atomic, Owned, Shared};

struct LockFreeStructure {
    head: Atomic<Node>,
}

// Reader
fn read(&self) {
    let guard = epoch::pin();
    let head = self.head.load(Ordering::Acquire, &guard);
    // ... use head ...
}

// Writer (single-writer in our case)
fn write(&self, item: Node) {
    let guard = epoch::pin();
    let new = Owned::new(item);
    let old = self.head.swap(new, Ordering::AcqRel, &guard);
    unsafe {
        guard.defer_destroy(old);   // Deferred free
    }
}
```

The reader pins to an epoch. The writer swaps in a new value and tags the old for deferred destruction. The old is freed once all readers have advanced past the current epoch.

## 4. The slot free list

One concrete use: the arena's slot free list.

When a slot is reclaimed, it's added to the free list. Allocators take from the list.

```rust
struct FreeList {
    head: Atomic<FreeNode>,
}

fn push(&self, slot: SlotId) {
    let guard = epoch::pin();
    let new = Owned::new(FreeNode { slot, next: Atomic::null() });
    loop {
        let head = self.head.load(Ordering::Acquire, &guard);
        new.next.store(head, Ordering::Release);
        if self.head.compare_exchange(head, new, Ordering::AcqRel, Ordering::Acquire, &guard).is_ok() {
            break;
        }
    }
}
```

(In Brain, the writer-per-shard discipline obviates the CAS loop — the writer is the only mutator. Simplified accordingly.)

## 5. The single-writer simplification

With single-writer-per-shard, much of crossbeam-epoch's complexity goes unused:

- No CAS loops needed (no concurrent writers).
- The writer can use simple atomics with the writer-only access pattern.

We use crossbeam-epoch for the **reclamation** aspect — its tracking of "when is it safe to free?" — even if we don't need its full lock-free machinery.

## 6. The epoch advance

The library maintains a global epoch counter. Periodically:

- The library advances the global epoch.
- Threads' "old epoch" values can be advanced.
- Memory tagged for deferred destruction in epochs that all threads have left can be freed.

The advance is automatic (every few hundred operations) but can be triggered explicitly if we want forced cleanup.

## 7. The "guard scope" rule

A Guard's lifetime defines what's safe to access:

```rust
let guard = epoch::pin();
let shared: Shared<'_, T> = atomic.load(Ordering::Acquire, &guard);
// shared is valid here
drop(guard);
// shared is invalid here (compiler enforces via lifetime)
```

The `'g` lifetime on `Shared<'g, T>` is bound by the Guard. The compiler prevents using a `Shared` after its Guard is dropped.

This is part of Rust's safe-by-default API for unsafe-ish concurrent code.

## 8. The "unsafe defer_destroy" caveat

`Guard::defer_destroy` is an unsafe operation:

```rust
unsafe {
    guard.defer_destroy(old);
}
```

The unsafety is because the caller asserts that the data won't be freed multiple times. Brain's writer-per-shard discipline ensures this — only one task is calling defer_destroy on any given item.

We've audited every use of `defer_destroy` in the codebase. There's no place where it could be called twice.

## 9. The cost of pinning

`epoch::pin()`:
- Atomic load + atomic store (track per-thread state).
- ~20-50 ns.

Cheap enough to call per-operation. Brain calls it per-search and per-write.

## 10. The cost of advance

Epoch advance:
- Check all threads' epochs (~10 ns × thread count).
- Atomic increment of global counter.
- Possibly free a batch of deferred items.

The cost is amortized across many operations. Per-advance: ~50-100 ns.

## 11. The "garbage queue"

Each thread maintains a per-thread garbage queue:

- Items defer_destroy'd while pinned in epoch E go to the thread's queue with epoch tag E.
- When the global epoch advances past E (and all threads have advanced), items in queue with tag E can be freed.

This is per-thread to avoid contention. The library handles the bookkeeping.

## 12. The "retire" pattern

For dropping non-pointer resources (e.g., closing a file when no readers reference the descriptor):

```rust
guard.defer(|| close_file(fd));
```

The closure runs after the epoch advances. Equivalent to defer_destroy but for arbitrary cleanup.

## 13. The interaction with Loom

Loom is a Rust concurrency model checker. crossbeam-epoch has Loom-tested versions for verifying correctness in test environments.

Brain's tests run under Loom for the lowest-level concurrent code paths. This catches subtle ordering bugs.

## 14. The "epoch bound" trade-off

Epoch-based reclamation has a delay: items aren't freed immediately when defer_destroy'd. They wait for the epoch to advance.

For typical Brain workloads:
- Average delay: ~10-100 µs (until next epoch advance).
- Worst case: bounded by the slowest reader's pin duration.

For most use cases, this delay is fine. For memory-pressured workloads, the delay can cause peak memory to be slightly higher than steady-state.

## 15. The "no GC pause" claim, revisited

crossbeam-epoch doesn't have stop-the-world pauses:

- Readers proceed without waiting.
- Writers proceed without waiting (with one exception: if reclamation must happen and all readers are pinned, the writer waits — but this is rare).

Compared to traditional GC, no large pauses; just small periodic increments.

## 16. The "memory ordering" awareness

crossbeam-epoch APIs require explicit memory orderings:

```rust
let head = self.head.load(Ordering::Acquire, &guard);
self.head.store(new, Ordering::Release, &guard);
```

The orderings:
- Acquire: prevents reordering of subsequent reads.
- Release: prevents reordering of preceding writes.
- AcqRel: both.
- SeqCst: full barrier (most expensive; rarely needed).

Brain uses Acquire/Release for normal paths; SeqCst for extra-careful cases.

## 17. The "default features" choice

crossbeam-epoch has features for:

- `std`: standard library (default; we use this).
- `nightly`: nightly compiler features (we don't use).
- `loom`: Loom integration for testing (enabled in test builds).

Brain enables std and loom-in-test.

## 18. The library version

Brain pins to a specific version of crossbeam-epoch in Cargo.toml. The library has been stable for years; updates are mostly bug fixes and performance improvements.

## 19. The alternative: hazard pointers

Hazard pointers are an alternative to epochs:

- Each reader publishes a "hazard pointer" to the data it's about to read.
- Writers check all hazard pointers before freeing.

Pros: more precise (only the specific item is protected, not all data in an epoch).

Cons: more complex API; harder to use safely.

Brain chose epochs over hazards for ergonomics. The precision difference doesn't matter for our workload.

## 20. The summary

crossbeam-epoch provides safe lock-free reclamation:

- Readers pin cheaply.
- Writers tag old data for deferred free.
- Memory is freed when no readers can possibly access it.

Brain uses it for fine-grained reclamation within data structures. The high-level publication uses ArcSwap. Together, they form Brain's no-locks reader path.

---

*Continue to [`07_yields.md`](07_yields.md) for cooperative yielding.*
