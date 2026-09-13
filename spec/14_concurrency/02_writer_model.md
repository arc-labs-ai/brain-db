# 14.02 Writer Model

> **TL;DR.** Brain's writer-side concurrency contract: one writer per shard (no writer-vs-writer contention by construction), epoch-based reclamation for memory safety under lock-free reads, and atomic publication (prepare changes off-graph, ArcSwap them in).

## Single-Writer per Shard

The single most important concurrency invariant: each shard has exactly one writer task.

## 1. The discipline

For each shard:

- Exactly one writer task is responsible for all mutations.
- All mutating operations (ENCODE, FORGET, LINK, UNLINK) flow through this task.
- The writer is on the shard's dedicated executor.

Other tasks on the shard handle reads, connection management, etc., but they don't mutate shard state directly. They send write operations to the writer via a channel.

## 2. Why this works

The writer-per-shard model gives several benefits:

- **No writer-vs-writer locking.** Within a shard, there's only one writer. No contention.
- **Implicit serialization.** The writer processes operations sequentially. WAL records are appended in a single, well-defined order.
- **Simple control flow.** No "what if two writers see different states" reasoning.
- **Predictable throughput.** Writer's per-second rate is clear (limited by group commit and storage).

## 3. The writer's responsibilities

```rust
struct Writer {
    shard_id: ShardId,
    queue: Receiver<WriteOp>,
    storage: StorageHandle,
    publisher: Publisher,
}

impl Writer {
    async fn run(&mut self) {
        loop {
            let batch = self.collect_batch().await;
            let result = self.process_batch(batch).await;
            self.send_acks(result).await;
        }
    }
}
```

Per-batch:

1. Collect operations from the queue.
2. Apply them in WAL order.
3. fsync the WAL (durability barrier).
4. Apply to in-memory derived state (metadata, HNSW).
5. Send acks to the originators.

## 4. The queue

```rust
let (tx, rx) = bounded::<WriteOp>(1024);
```

Bounded queue with backpressure. If the queue is full:

- Senders block (in async terms, await space).
- After timeout, they error out (`WriterOverloaded`).

The bound (1024) is tunable. Default is conservative; sustained queue depth >100 is a warning sign.

## 5. The batching window

The writer batches multiple operations:

```rust
async fn collect_batch(&self) -> Vec<WriteOp> {
    let first = self.queue.recv().await;
    let mut batch = vec![first];

    let timeout = sleep(Duration::from_micros(100));
    loop {
        select! {
            op = self.queue.try_recv() => {
                if let Ok(op) = op {
                    batch.push(op);
                    if batch.len() >= 64 { break; }
                }
            }
            _ = &mut timeout => break,
        }
    }
    batch
}
```

The 100 µs window + 64 op cap balance latency vs throughput.

For light load: ~100 µs added latency, batches of 1-2 ops.

For heavy load: 64-op batches every ~6 ms (160 batches/sec × 64 = 10K ops/sec).

## 6. Group commit

A batch is processed as a single group commit:

1. Append all WAL records (one per operation).
2. Single fsync (one disk write for the whole batch).
3. Apply all operations to derived state.
4. Send acks for all operations.

Group commit amortizes the fsync cost. Without it, each operation pays ~0.3 ms for fsync; with batches of 32, that's ~10 µs per operation.

## 7. The "writer is the source of truth" rule

The writer is the single source of truth for shard state. Other tasks observe state through:

- redb read transactions: see committed data.
- ArcSwap.load() on the HNSW: see published data.
- Direct mmap reads: see arena bytes.

No task other than the writer modifies the canonical state. There are caches and derived structures, but they all derive from what the writer has produced.

## 8. The cross-task communication

Other tasks send write operations to the writer:

```rust
async fn execute_encode(&self, plan: EncodePlan) -> Result<EncodeResponse> {
    // ... preparation ...

    let (ack_tx, ack_rx) = oneshot::channel();
    let op = WriteOp::Encode { plan, ack: ack_tx };
    self.writer_tx.send(op).await?;

    let result = ack_rx.await?;
    Ok(self.build_response(result))
}
```

The executor task waits on the ack. It's blocked (async-blocked, not OS-blocked) until the writer processes the operation.

For batch group commits: many executors await acks; the writer broadcasts after the commit.

## 9. The writer's failure modes

If the writer task crashes (panic):

- Brain logs the panic.
- The shard is marked unhealthy.
- New write operations fail.
- Reads continue (the writer wasn't doing them).
- Operator action: investigate, restart the shard.

Restart involves:
- Replaying the WAL.
- Re-creating the writer task.
- Resuming.

In-flight operations that were waiting on acks see the connection close; clients can retry.

## 10. The "no two writers" invariant

Brain enforces single-writer per shard:

- Per-shard struct contains the writer's queue.
- Only the writer task has the receiver end.
- No other task can directly mutate shard state.

In debug builds, assertions check this:

```rust
debug_assert!(thread_id() == writer_thread_id, "Mutation outside writer task");
```

In release builds, the structural guarantees (private receivers, etc.) prevent it.

## 11. The "writer doesn't read directly" subtlety

The writer task can read shard state too — it needs to look up things to apply operations. It does this via the same primitives readers use (redb read transactions, etc.) — within the writer task.

So the writer is also a reader, but it's "the" reader that's also doing writes.

## 12. The cooperative-yield within writer

The writer task yields cooperatively:

- Between batches.
- During large batches (every ~10 ops).
- During I/O (fsync, etc.).

Yields let other tasks (readers, background workers) run. Without them, a busy writer would starve everything else on the shard.

## 13. The writer's resource bound

The writer task's resource use:

- CPU: ~30-50% of the shard's core under sustained load.
- Memory: ~few MB (in-flight ops, batches).
- I/O: bounded by WAL fsync rate.

Other tasks share the remaining CPU. For a 16-thread server with 16 shards: each shard uses about half a core for writes; the rest is for reads.

## 14. The exception: external writers (replication)

For HA / replication, a "follower" shard takes writes from a "leader" shard, not from clients. The follower's writer applies replicated operations.

Brain doesn't currently have replication. A future addition would extend the writer model:

- A leader writer takes client requests.
- A follower writer takes replicated operations.
- Both apply to the same shard's state — but they're never both active at once (one is leader, one is follower).

## 15. The implications for clients

Clients:

- Doesn't manage writer state.
- Submits requests; awaits responses.
- Doesn't need to know about writers.

The single-writer is an internal detail. Clients see a "submit operation" interface.

## 16. The implications for testing

Tests can:

- Spawn a single-shard server.
- Send concurrent operations.
- Verify ordering: operations are linearizable per shard.

Tests can't:

- Create multiple writers per shard.
- Bypass the writer.

Brain's API doesn't expose direct mutation; it always goes through the writer.

## 17. The "writer pause" pattern

Some operations need to briefly pause the writer:

- Snapshot creation: writer pauses while files are linked.
- Schema migration: writer pauses while migrations run.

The pause is implemented as: an "admin" message in the writer's queue takes priority and runs synchronously, blocking other operations. After it completes, the writer resumes normal processing.

The pause is brief (typically < 100 ms). Clients see a temporary latency spike.

## 18. The writer-ready check

When a shard is starting up (recovery in progress), the writer isn't yet ready. Operations submitted to a not-ready writer get queued; when ready, they're processed.

If the queue grows too long during startup, new operations are rejected with `ShardNotReady`.

## 19. The throughput math

Single-writer throughput per shard:

- 10K ops/sec sustained (limited by WAL fsync + redb commits).
- 30-50K ops/sec burst (with full batching, no fsync stall).

Scaling beyond per-shard limits: more shards. Per-shard performance is bounded; Brain's total throughput is N × per-shard.

## 20. The summary

Single-writer-per-shard is the keystone of Brain's concurrency model. It:

- Eliminates writer-vs-writer contention.
- Provides a clear ordering for operations.
- Simplifies reasoning about consistency.
- Bounds per-shard throughput predictably.

The trade-off — bounded per-shard throughput — is acceptable because Brain scales by adding shards.

---

## Epoch-Based Reclamation

How Brain safely frees data structures that may have in-flight readers.

## 1. The problem

When the writer changes a data structure (e.g., the HNSW), readers in flight may still be using the old version. The old version can't be freed immediately — readers would crash.

Brain can't keep all old versions forever either — that's a memory leak.

Brain requires a way to know "when is it safe to free this old version?"

The answer: when no readers hold references to it. Epoch-based reclamation (EBR) provides this signal.

## 2. The epoch concept

Time is divided into **epochs**. Brain has a global epoch counter that the writer advances periodically.

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
- For any other place Brain requires lock-free reclamation of data that isn't refcounted.

Epoch-based reclamation is mostly used inside the HNSW for incremental cleanup; the high-level publication is via ArcSwap.

## 5. The HNSW's internal use

When the index maintenance worker removes a node:

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

If a reader is pinned for too long (a bug or stuck task), the writer waits — it can't safely free data tagged for that epoch. After a configurable threshold (default 1 sec), Brain logs a warning.

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

This is the core safety property. It's enforced by the library; Brain just uses the API correctly.

## 9. The performance characteristics

Epoch operations are cheap:

- `pin()`: ~10 ns (atomic increment).
- `drop(guard)`: ~10 ns.
- Epoch advance: ~50 ns (atomic increment + checks).
- Deferred free: ~100 ns (queue addition + later actual free).

These are fast enough to be in the hot path. Brain pins per-search without measurable overhead.

## 10. The "long pin" problem

If a reader pins and never unpins (a stuck task, a bug), the writer can't free data freed in epochs after that pin. Memory grows.

Detection:

- Each pin records its epoch.
- The writer monitors the oldest pinned epoch.
- If an epoch is pinned for too long, the writer logs a warning.

Mitigation:

- Brain's reader tasks have time limits. If exceeded, they're aborted.
- The pin is implicitly released when the task ends.

Brain accepts that a pathologically-stuck task could cause memory growth until it's killed. Fine in practice.

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
- It frees things when "no readers in old epoch" — even if no reader is actually using a specific item.

This works for Brain's use cases: items are tagged for free when they're known to be unreachable from new operations; the question is only whether old operations might still see them.

## 15. The "epoch counter wrap" consideration

The epoch counter is 64-bit. It can advance once per 100 µs. So:

- 100K advances per second.
- 64-bit counter: 2^64 / 1e5 = 5.8 × 10^15 seconds = ~180 million years.

Wraparound is not a concern.

## 16. The "TLA+ verified" question

`crossbeam-epoch` has a documented design but isn't formally verified. Brain uses it as a black box, trusting the implementation.

For Brain's level of correctness needs (no data corruption, no use-after-free), the library's testing is sufficient. Brain does not independently verify it.

## 17. The alternatives considered

Brain considered:

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

## Atomic Publication

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

Cloning an HNSW state for ~1M nodes is expensive (~150 MB of memory + the clone time). Brain does not actually deep-clone for routine inserts.

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

Brain uses ArcSwap mainly for major swaps (full rebuilds) where an entirely new HNSW state is published.

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

So Brain does not publish per-insert. Publications are batched: after a window of inserts, one publication.

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

In practice, OOM in this path is very rare. Brain provisions generously.

## 16. The "no GC pause" guarantee

Publication doesn't have a "stop-the-world" pause. Readers continue running during publication. The swap is atomic, but it's a single instruction; no waiting.

This contrasts with traditional GC pauses in some systems. Brain doesn't have GC pauses.

## 17. The role in the read-after-write hint

When a client requests `consistency: ReadAfterWrite`, Brain ensures the read sees the latest publications:

- Wait for the writer to publish all pending writes.
- Then proceed with the read.

The wait is on the publication, not on the writer's overall state. The client sees any publication ≥ the LSN of its previous write.

## 18. The publication LSN

Each publication has an associated LSN — the WAL LSN at the time of publication. Readers can:

- Check the published LSN.
- Compare to their requirement (e.g., "I want at least LSN X").
- Wait if needed.

Brain tracks per-shard "published LSN" as an atomic. Readers check it cheaply.

## 19. The "publish nothing" case

Sometimes the writer has nothing new to publish (no recent operations). The publish-cycle skips: no allocation, no swap.

This keeps idle shards lightweight.

## 20. The summary

Publication via ArcSwap:

- Atomic, lock-free, wait-free.
- Cheap in steady state.
- Provides clear before/after semantics.
- Plays well with Arc-based memory management.

It's the right primitive for "swap the entire current view of a data structure". For finer-grained reclamation (within a structure), Brain uses crossbeam-epoch.

---

*Continue to [`03_lock_free_primitives.md`](03_lock_free_primitives.md) for ArcSwap details.*
