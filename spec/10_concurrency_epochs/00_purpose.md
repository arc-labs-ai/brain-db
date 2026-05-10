# 10.00 Purpose

This document specifies how Brain handles concurrency. Most concurrent systems suffer from one of two problems: too much locking (slow) or too little (incorrect). Brain's strategy is to **eliminate** sources of contention rather than synchronize them, by using:

- **Single-writer-per-shard** — within a shard, there's only one writer, so writer-vs-writer contention doesn't exist.
- **Epoch-based reclamation** — readers don't lock; they pin to an epoch and the writer ensures their view stays valid.
- **Atomic publication** — writers prepare changes, then atomically swap them in.

These mechanisms together let readers run lock-free against a stable snapshot while writers proceed independently.

## What this document covers

- The core concurrency principles.
- The single-writer-per-shard discipline and its rationale.
- Epoch-based reclamation: why and how.
- The atomic publication protocol.
- The role of `ArcSwap` and `crossbeam-epoch`.
- Cooperative yielding for fairness.
- Failure modes specific to concurrency.

## What this document does not cover

- **Cross-shard coordination.** Defined in [12. Sharding + Clustering](../12_sharding_clustering/).
- **Background workers' specific concurrency.** Defined in [11. Background Workers](../11_background_workers/).
- **Storage-layer transactions.** Defined in [05. Storage](../05_storage_arena_wal/) and [07. Metadata](../07_metadata_graph/).

## 1. The big picture

Each shard runs on a dedicated OS thread, pinned to a CPU core. On that thread, Glommio runs a cooperative-async executor that schedules many tasks:

- Connection handlers.
- Request executors.
- The single writer task.
- Background workers.

Tasks share the shard's data structures: the arena, the metadata, the HNSW. Concurrency is between these tasks; they all run on the same thread (no preemption between them; cooperative yields).

The single-writer-per-shard discipline means:

- Mutations to shard state happen in one task (the writer).
- Many tasks read concurrently.
- Readers don't lock; they pin to a stable view.
- The writer publishes new views atomically.

## 2. The decisions and consequences

### Decision: single-writer per shard.

Consequence: no writer-vs-writer locks; no two-phase commit; simple control flow.

Trade-off: write throughput is bounded by single-writer speed (~10K writes/sec sustained). Scaling is by adding more shards.

### Decision: lock-free reads via atomic publication.

Consequence: readers never block on writers; reads have predictable latency.

Trade-off: brief delay between write and visibility (~10 ms publication interval).

### Decision: epoch-based reclamation.

Consequence: writers can free memory safely when no readers hold references.

Trade-off: complexity of epoch tracking; brief delay before reclaim.

### Decision: cooperative yielding.

Consequence: fair scheduling between tasks on a shard.

Trade-off: tasks must explicitly yield; runaway tasks can starve others.

## 3. Why not "just use locks"

We could have used:

- Mutexes around shared data structures.
- Reader-writer locks.
- Lock-free data structures throughout.

Each has costs:

- Mutexes serialize; reads block on writes.
- RW locks have starvation issues; writer-bias adds complexity.
- Lock-free data structures are notoriously hard to get right.

The Brain approach avoids the issues by:

- Pinning writes to one task per shard (no writer contention).
- Letting readers run on a snapshot (no read-vs-write contention).
- Using well-understood primitives (`ArcSwap`, epoch-based reclamation) where coordination is needed.

## 4. The "shared-nothing within a shard" claim

Within a shard, tasks share data via:

- The shard's data structures (arena, metadata, HNSW), accessed via reference.
- Channels (writer's input queue).
- Atomic primitives (publication, epoch counters).

There's no traditional shared mutable state — everything is immutable from a reader's perspective; the writer mutates only its own copies before publishing.

## 5. The "no locks in the read path" guarantee

The hot path for reads (RECALL, PLAN, REASON):

```
1. Open redb read transaction (lock-free; MVCC).
2. ArcSwap.load() to get current ANN state (atomic; lock-free).
3. Compute results.
4. Drop transactions and references.
```

No mutex. No await on a lock. Just atomic loads and lock-free operations.

The latency of these reads is dominated by the actual work (search, lookup), not by synchronization overhead.

## 6. The writer's path

The writer's hot path:

```
1. Receive operation from the queue.
2. (Optionally) batch with other operations.
3. Acquire write transaction (uncontended on this shard).
4. Apply changes.
5. fsync WAL.
6. Commit transaction.
7. Update in-memory derived state (HNSW, etc.).
8. Periodically: advance epoch, publish new ANN snapshot.
9. Send ack back to executor.
```

There's no lock acquisition. The writer is the only mutator; serialization is implicit.

## 7. The epoch protocol's role

For derived state (HNSW), the writer mutates in place and periodically publishes via ArcSwap. Old versions of the state may still be in use by in-flight readers; they're freed via the epoch protocol.

Without epochs, freeing too early would crash readers; freeing too late would leak memory. The epoch protocol gives the writer a clear signal of when it's safe to free.

## 8. The three layers of concurrency control

Brain has three layers, each with its own discipline:

| Layer | Concurrency control |
|---|---|
| Storage (arena, WAL) | Single-writer; lock-free reads via mmap |
| Metadata (redb) | redb's MVCC |
| ANN (HNSW) | ArcSwap publication + epoch-based reclamation |

Each is well-suited to its layer's access pattern. Together they form a coherent system.

## 9. The "no global locks" principle

There's no global lock that any operation must take. Even substrate-wide events (like a snapshot) don't take global locks — they coordinate per-shard.

This means a misbehaving operation on one shard doesn't affect other shards. Failure modes are contained.

## 10. The compositional reasoning

For correctness reasoning:

- A reader sees a consistent view of one shard's state at one point in time.
- A writer's changes are atomic per operation (per-WAL-record).
- Cross-shard operations are not atomic; each shard is independent.

These rules compose: a complex operation across multiple shards is "consistent per shard, eventually consistent globally". Agents that need stronger semantics across shards must coordinate at the application layer.

---

*Continue to [`01_principles.md`](01_principles.md) for the core principles.*
