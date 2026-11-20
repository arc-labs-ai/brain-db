# 14. Concurrency + Publication Model

> **TL;DR.** Brain eliminates contention rather than synchronizing it. One writer per shard removes writer-vs-writer locks; readers run lock-free against an `ArcSwap`-published, immutable snapshot and hold it by `Arc`, which also reclaims the old one once the last reader lets go — there is no epoch protocol and no reader pin. Each shard pins to one CPU core under Glommio's cooperative-async executor, where a bounded number of requests are in flight at once and the redb write transaction — which contains no await — serializes them structurally. No mutex on the read path, no global locks anywhere, no shared mutable state — the writer mutates its own copies and atomically publishes.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Storage-layer authors; engine implementers |
| Voice | Third-person factual + RFC2119 normative |
| Depends on | [08. Storage](../08_storage/00_purpose.md), [09. Indexing](../09_indexing/00_purpose.md), [10. Metadata + Graph Store](../10_metadata/00_purpose.md), [12. Query Optimizer](../12_query_optimizer/00_purpose.md) |
| Referenced by | [15. Background Workers](../15_background_workers/00_purpose.md), [16. Sharding & Clustering](../16_sharding/00_purpose.md) |

## What this spec defines

How Brain manages concurrent operations on shared state — the one-writer-per-shard discipline, the publication mechanism, how old state is reclaimed, and how readers and writers stay out of each other's way.

This is Brain's "concurrency contract" — the rules every layer follows to ensure correctness and performance under concurrency.

Most concurrent systems suffer from one of two problems: too much locking (slow) or too little (incorrect). Brain's strategy is to **eliminate** sources of contention rather than synchronize them, by using:

- **One writer per shard** — within a shard, mutations go through one writer handle and one transaction at a time, so writer-vs-writer contention doesn't exist.
- **Immutable publication** — readers don't lock and don't pin; they take an `Arc` to a snapshot that cannot change underneath them.
- **Atomic publication** — writers prepare changes, then atomically swap them in.

These mechanisms together let readers run lock-free against a stable snapshot while writers proceed independently.

## What this document covers

- The core concurrency principles.
- The one-writer-per-shard discipline and its rationale.
- Reclamation by refcount: why no epoch protocol is needed.
- The atomic publication protocol.
- The role of `ArcSwap` and `Arc`.
- Cooperative yielding for fairness.
- Failure modes specific to concurrency.

## What this document does not cover

- **Cross-shard coordination.** Defined in [16. Sharding & Clustering](../16_sharding/00_purpose.md).
- **Background workers' specific concurrency.** Defined in [15. Background Workers](../15_background_workers/00_purpose.md).
- **Storage-layer transactions.** Defined in [08. Storage](../08_storage/00_purpose.md) and [10. Metadata + Graph Store](../10_metadata/00_purpose.md).

## 1. The big picture

Each shard runs on a dedicated OS thread, pinned to a CPU core. On that thread, Glommio runs a cooperative-async executor that schedules many tasks:

- Connection handlers.
- Request executors.
- The write path, dispatched concurrently and committing one transaction at a time.
- Background workers.

Tasks share the shard's data structures: the arena, the metadata, the HNSW. Concurrency is between these tasks; they all run on the same thread (no preemption between them; cooperative yields).

The shard's receive loop does not run one request to completion before taking the next. Each dispatch takes an admission permit and is detached onto the same executor, so the loop returns to its receive immediately and a bounded number of requests are in flight at once. The one-writer-per-shard discipline means:

- Mutations to shard state go through one writer handle, reached by every in-flight request.
- Only one redb write transaction is open at a time, because the block that opens, applies and commits it contains no await point and therefore cannot be interleaved on a single-threaded executor.
- Many tasks read concurrently.
- Readers don't lock; they pin to a stable view.
- The writer publishes new views atomically.

## 2. The decisions and consequences

### Decision: one writer per shard.

Consequence: no writer-vs-writer locks; no two-phase commit; simple control flow.

Trade-off: write throughput is bounded by how fast one shard can commit (~10K writes/sec sustained). Scaling is by adding more shards.

### Decision: concurrent dispatch, structurally serialized commits.

Consequence: a request that is waiting on its WAL fsync does not block the next request from reaching the log, so the group-commit window has more than one record to fold.

Trade-off: a bound is required, and the dispatch path must keep the redb transaction await-free or the serialization guarantee is lost.

### Decision: lock-free reads via atomic publication.

Consequence: readers never block on writers; reads have predictable latency.

Trade-off: brief delay between write and visibility (~10 ms publication interval).

### Decision: reclaim whole published generations by refcount.

Consequence: a writer frees old state safely with no protocol to run — the last reader to drop its `Arc` drops the structure.

Trade-off: the superseded generation stays resident until that last reader lets go, so memory peaks across a publish. Reclamation is all-or-nothing per generation; individual nodes are never freed.

### Decision: cooperative yielding.

Consequence: fair scheduling between tasks on a shard.

Trade-off: tasks must explicitly yield; runaway tasks can starve others.

## 3. Why not "just use locks"

Brain could have used:

- Mutexes around shared data structures.
- Reader-writer locks.
- Lock-free data structures throughout.

Each has costs:

- Mutexes serialize; reads block on writes.
- RW locks have starvation issues; writer-bias adds complexity.
- Lock-free data structures are notoriously hard to get right.

The Brain approach avoids the issues by:

- Routing every mutation on a shard through one writer handle (no writer contention).
- Letting readers run on a snapshot (no read-vs-write contention).
- Using well-understood primitives (`ArcSwap`, `Arc`) where coordination is needed.
- Serializing the commit structurally — an await-free transaction body on a single-threaded executor — rather than with a lock.

## 4. The "shared-nothing within a shard" claim

Within a shard, tasks share data via:

- The shard's data structures (arena, metadata, HNSW), accessed via reference.
- Channels (writer's input queue).
- Atomic primitives (publication, the published-generation counter, the durable watermark).

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

The write hot path, per request:

```
1. Take an admission permit; the shard's receive loop moves on.
2. Append the operation's WAL records; await the group-commit fsync.
3. Apply in-memory derived state (HNSW).
4. Open the redb write transaction, apply every phase, stamp
   idempotency, commit — all synchronous, no await inside.
5. Publish the committed LSN range to the shard's durable watermark.
6. Send the ack; release the permit.
```

There's no lock acquisition. Steps 1–3 of two requests may overlap; step 4 cannot. The transaction body contains no await point, so once a request enters it the executor has nowhere else to go until `commit()` returns — serialization is structural, not enforced.

The admission bound is derived from the WAL group-commit batch budget rather than configured on its own: `max_batch_bytes / 4 KiB`, floored at two. Two is the smallest bound that means anything — one would reproduce serial dispatch, zero would wedge the shard. At the default 60 KiB batch, the bound is 15. Widening the commit window and its batch budget widens the concurrency to match.

When every permit is out, the shard parks on its receive. It does not shed: the request stays in the channel and is served in arrival order. Parking the receive also parks the shard's cheap requests, which is deliberate — in-order service of a saturated shard is preferable to a second scheduling lane with unreasoned starvation behavior.

## 7. How old state is reclaimed

For derived state (HNSW), the writer builds a new immutable index and publishes it via `ArcSwap`. Old versions may still be in use by in-flight readers, so they are freed when the last `Arc` to them is dropped.

Freeing too early would leave readers holding a dangling view; freeing too late would leak. `Arc`'s refcount answers exactly that question, and it can be relied on here because the published structure is replaced wholesale rather than edited under readers. Brain therefore runs no reclamation protocol of its own: no pins, no grace periods, no deferred-destroy queue. See [`02_writer_model.md`](02_writer_model.md) under Reclamation.

## 8. Concurrency control by component

Each component has its own discipline:

| Component | Concurrency control |
|---|---|
| Storage (arena, WAL) | One group committer per shard; lock-free reads via mmap |
| Metadata (redb) | redb's MVCC |
| ANN (HNSW) | ArcSwap publication of immutable generations + `Arc` refcount reclamation |

Each is well-suited to its component's access pattern. Together they form a coherent system.

## 9. The "no global locks" principle

There's no global lock that any operation must take. Even shard-wide events (like a snapshot) don't take global locks — they coordinate per-shard.

This means a misbehaving operation on one shard doesn't affect other shards. Failure modes are contained.

## 10. The compositional reasoning

For correctness reasoning:

- A reader sees a consistent view of one shard's state at one point in time.
- A write's changes are atomic per operation: every phase of one write, plus its idempotency row, commits in a single redb transaction or none of it does.
- Writes on one shard commit one at a time, but not necessarily in LSN order — two concurrent requests may reach the transaction in the opposite order to the one in which they reached the log. Durability bookkeeping therefore tracks a contiguous committed prefix rather than a maximum; see [`02_writer_model.md`](02_writer_model.md).
- Cross-shard operations are not atomic; each shard is independent.

These rules compose: a complex operation across multiple shards is "consistent per shard, eventually consistent globally". Agents that need stronger semantics across shards must coordinate at the application layer.

---

*Continue to [`01_principles.md`](01_principles.md) for the core principles.*
