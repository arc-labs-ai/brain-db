# 10.01 Principles

The core principles that guide Brain's concurrency design.

## 1. Avoid contention rather than synchronize it

The primary lever: structure the system so concurrent operations don't actually contend on shared mutable state.

- Single-writer-per-shard: no writer-vs-writer contention.
- Sharding: partition the workload; per-shard contention is local.
- Lock-free reads: readers don't share mutable state with writers.

When contention is unavoidable (e.g., a global counter), use atomic operations rather than locks.

## 2. Predictable latency over peak throughput

Brain prioritizes predictable latency over maximum throughput. A 99th percentile latency of 25 ms with 5K RPS is preferred over 99th percentile of 100 ms with 10K RPS.

This means:
- We don't use techniques that add tail latency for throughput gains (e.g., aggressive batching that delays small requests).
- We don't share resources in ways that cause contention spikes.

## 3. Cooperative scheduling on a thread

Within a shard, all tasks share one thread. They cooperate via async-await:

- Tasks yield at await points.
- Long-running synchronous work is rare; when needed, explicit yields keep things fair.

No OS-level context switches. No preemption. Just "I'm done with my chunk, take the thread."

## 4. Immutable views from reader's perspective

Readers operate on stable views:

- redb read transactions: a consistent snapshot.
- ArcSwap.load() on the HNSW: a stable reference; the underlying data won't change.
- The arena (mmap'd; readers see whatever bytes are present at slot offset).

The writer may be modifying things, but the reader's specific view is frozen.

## 5. Atomic publication

When the writer wants readers to see new state, it publishes atomically:

- Build the new state in a separate place.
- ArcSwap.store(new_state) — single atomic operation.
- After the swap, future readers see the new state; readers from before the swap continue with the old.

There's never a "half-published" state.

## 6. Defer reclamation

Old states aren't immediately freed. They're freed when no readers hold references:

- Arc's reference counting handles simple cases.
- Epoch-based reclamation handles cases where references aren't directly tracked (e.g., raw pointers into a data structure).

This avoids "use-after-free" without locking.

## 7. The "no shared mutable state across threads" rule

Different shards run on different threads. They don't share mutable data structures. Communication between shards is via:

- Function calls (in-process; sender is on the source shard, receiver is on the target shard via Glommio's cross-executor calls).
- Network (out-of-process; via the wire protocol).

This means shards are concurrency-independent; one shard's contention doesn't affect another.

## 8. The "no async cancellation in critical sections" rule

Async tasks in Rust can be canceled (the future is dropped). For critical sections (the writer's commit path), we ensure cancellation can't occur mid-section:

- Writer's commit is a single async block with no awaits between begin_write and commit.
- If the task is canceled, it's at a yield point; the in-progress transaction is aborted (no harm done).

This is correctness-preserving despite Rust's cancellation semantics.

## 9. The "explicit ordering for cross-task communication" rule

When two tasks need to coordinate (e.g., writer and a reader), the coordination is via an explicit primitive:

- Channels: ordered, bounded, with backpressure.
- Atomics: ordered (via memory ordering).
- ArcSwap: atomic ref-counted publication.

Never via shared mutable variables without coordination.

## 10. The "no panicking under load" rule

Concurrent code paths must handle high-load without panicking:

- Allocation failures must be handled (return errors, not panic).
- Channel-full conditions return errors, not panic.
- Out-of-budget conditions return errors.

Panics are reserved for genuine bugs (assertion violations, impossible states).

## 11. Simplicity > theoretical elegance

We prefer simple concurrency that's clearly correct over clever schemes that are hard to verify:

- Yes to single-writer-per-shard (simple).
- Yes to ArcSwap (simple primitive, well-understood).
- Yes to redb's MVCC (proven; we don't reimplement).

We avoid:
- Hand-rolled lock-free data structures (hard to verify).
- Complex distributed protocols within a shard.
- Theoretical concurrency techniques without proven implementations.

## 12. The "stable structures over rebuilding" preference

Where data structures are stable across publications (e.g., the arena's slot layout doesn't change between ENCODE operations), readers can use the same reference for many operations.

For data structures that are rebuilt (e.g., HNSW after a maintenance rebuild), the rebuild produces a new structure; the old is freed.

The rebuild path is rare; the steady-state path uses stable structures. Most operations don't trigger rebuilds.

## 13. Testing the concurrency

The concurrency code is exercised:

- Stress tests: many concurrent reads + writes; check for crashes, leaks, data corruption.
- Correctness tests: sequential semantics in a concurrent setting (linearizability checks).
- Loom (Rust's concurrency model checker): for the lowest-level lock-free code.

Concurrency bugs are notoriously hard to find without disciplined testing. Brain invests in this.

## 14. The "boring" preference

Brain's concurrency is intentionally unexciting:

- Standard primitives (Arc, AtomicUsize, channels).
- Established patterns (single-writer, MVCC, epoch-based reclamation).
- No exotic algorithms.

Boring is good. We don't want to be on the cutting edge of concurrency research; we want a solid foundation that works.

## 15. The "performance scales with shards" model

Brain doesn't try to scale a single shard to many cores. Each shard uses one core; throughput per shard is bounded.

For more throughput, add more shards:

- 1 shard: ~5K RPS.
- 4 shards: ~20K RPS.
- 16 shards: ~80K RPS.

This linear scaling is enabled by the single-thread-per-shard model.

The "throughput per core" is high because there's no contention overhead. ~5K RPS per core is competitive with multi-threaded systems that fight contention.

## 16. The "no shared mutable state across shards" reinforced

Reiterating because it's crucial: shards do not share mutable state. Each shard's:

- Arena: independent.
- WAL: independent.
- Metadata store: independent.
- HNSW: independent.
- Writer task: independent.

Cross-shard operations explicitly orchestrate: send a request to shard B, wait for response, etc. Never reach into shard B's memory directly.

This is a hard architectural rule. Violating it would create cross-shard contention and undermine the model.

## 17. The "agents-as-shard-units" framing

For typical deployments, an agent's data lives in one shard. Agents are the natural unit of locality:

- An agent's encodes go to its shard.
- An agent's recalls happen on its shard.
- An agent's edges are within its shard.

This means most operations don't cross shard boundaries. The cross-shard concurrency model is rarely exercised; per-shard concurrency dominates.

## 18. The "concurrency budget" principle

Each shard has a concurrency budget (CPU time, memory, etc.). The substrate doesn't oversubscribe:

- The writer task has a guaranteed slice.
- Readers compete for the rest.
- Background workers yield generously.

If demand exceeds the budget, requests are queued or rejected. Latency rises but predictably.

This is the substrate's "fair scheduling" baseline. More sophisticated scheduling (priorities, weights) is possible but not in v1.

---

*Continue to [`02_single_writer.md`](02_single_writer.md) for the single-writer discipline.*
