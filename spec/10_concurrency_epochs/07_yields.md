# 10.07 Cooperative Yielding

How tasks on a shared executor yield CPU time to others.

## 1. The cooperative model

Glommio's executor schedules tasks cooperatively:

- Tasks run until they yield.
- At each yield point, the executor picks the next task to run.
- No preemption; no "I'll cut you off after 10 ms".

This means tasks must yield periodically to be fair. A task that doesn't yield monopolizes the CPU.

## 2. Implicit yields

Most yield points are implicit, at await points:

```rust
async fn read() -> Data {
    let txn = db.begin_read().await?;       // Yield possible here
    let data = txn.fetch().await?;          // Another possible yield
    Ok(data)
}
```

When the await polls the future and it returns Pending, the task yields. The executor can run other tasks until this future is ready.

Brain's request handling has many awaits; yields happen naturally throughout.

## 3. Explicit yields

For long-running synchronous work, explicit yields:

```rust
for item in large_collection {
    process(item);
    if iteration_count % 100 == 0 {
        glommio::executor().yield_if_needed().await;
    }
}
```

The `yield_if_needed` checks if other tasks are waiting; if so, it yields.

Brain uses explicit yields in:

- Long iterations (metadata scans, edge enumeration).
- Heavy in-memory computation (HNSW search visits many nodes).
- Background workers (any heavy lifting yields).

## 4. The "yield budget"

Each task has an implicit budget — about 1 ms of work between yields. Beyond that, other tasks may starve.

The substrate's hot paths are designed to fit within this budget. For paths that exceed it (rare), explicit yields are added.

## 5. The HNSW search yields

HNSW search visits many nodes. For a search with ef=64, ~500-1000 nodes are visited.

```rust
fn search(&self, query: &[f32], ef: usize) -> Vec<Result> {
    let mut visited = 0;
    while !done {
        let node = next_candidate();
        let dist = compute_distance(query, node.vector());
        // ...
        visited += 1;
        if visited % 100 == 0 {
            yield_if_needed_blocking();   // Coop yield
        }
    }
}
```

Yielding every 100 nodes ≈ every ~5 µs. Frequent enough for fairness; rare enough to amortize the yield overhead.

## 6. The metadata scan yields

For range scans over many rows:

```rust
for (key, value) in metadata.range(...)? {
    process(key, value);
    if scanned_count % 100 == 0 {
        yield_if_needed().await;
    }
    scanned_count += 1;
}
```

## 7. The writer yields

The writer task yields:

- Between batches (always; recv awaits).
- Within long batches (every ~10 ops).
- During fsync (the syscall is async-friendly).

This means the writer doesn't monopolize the CPU even under sustained write load.

## 8. The "no yields under critical section" rule

Some sections shouldn't yield:

- Inside a redb write transaction (between begin_write and commit, no awaits).
- Holding an internal lock (rare; we avoid these).
- During WAL fsync (the syscall is the yield).

Yielding inside a transaction would let other readers see in-progress state — but redb's MVCC handles this. Yielding mid-transaction in Rust's async model can also be tricky (cancellation could leave dangling state).

Brain's conventions: short, no-await transaction bodies.

## 9. The yield-cost trade-off

Each yield costs ~50-100 ns (saving and restoring task state). For very tight loops, this adds up.

The substrate's calibration: yield often enough for fairness, rarely enough that overhead is < 1% of total cost.

For Brain's workloads, yielding every 100 ops is the right cadence.

## 10. The "stuck task" detection

If a task fails to yield (a bug or runaway loop), other tasks starve:

- Latency rises sharply.
- Other request handlers can't make progress.

Brain has a watchdog:

- Each task's "last ran" is tracked.
- If a task hasn't run in 100 ms, the watchdog logs.
- If 1 sec, the watchdog tries to abort.

In practice, this catches few real bugs but is a useful safety net.

## 11. The "background worker" generosity

Background workers (decay, consolidation, maintenance) yield very generously:

- After every batch.
- After every redb transaction commit.
- Whenever request load is high.

The workers should never affect request latency. Their work is done in the gaps.

## 12. The "priority hint"

Some tasks are higher priority than others. Glommio supports priority hints:

```rust
let task = glommio::spawn_local(future);
task.priority(Priority::High);
```

Brain uses:

- High: request executors, writer task.
- Medium: connection handlers.
- Low: background workers.

The priority affects which task runs first when multiple are ready.

## 13. The "no preemption" gotcha

A task that genuinely doesn't yield (e.g., a tight loop with no awaits) blocks the entire shard:

- No reads are processed.
- No writes are processed.
- The connection layer can't accept new requests.

This is a serious bug. The watchdog catches it eventually, but for the duration, the shard is unresponsive.

We test for this:
- Stress tests with synthetic long-running tasks.
- Profiling to identify long-running synchronous code.
- Code review focusing on awaits in long iterations.

## 14. The "io_uring" and yields

io_uring (the kernel async I/O mechanism Glommio uses) provides natural yield points:

- Submit operation.
- Yield (the operation is in flight).
- Resume when the operation completes.

This is the substrate's main I/O path. Each WAL fsync, each disk read, is an opportunity to yield.

## 15. The "yield then unyield" cost

A yield is two atomic operations (mark task as ready-to-run, atomically pick next task). ~100 ns.

For tight loops, yielding too often (every iteration) is wasteful. Yielding too rarely (every minute) is unfair.

The sweet spot for Brain: every 10-100 iterations, depending on per-iteration cost.

## 16. The "yield in critical sections" bug

A subtle bug class: yielding in a place where the task has invariants violated.

Example:

```rust
let lock = self.lock.lock().await;
self.value += 1;
// PROBABLY NEVER write
//   yield_now().await;
self.value += 1;
drop(lock);
```

If you yield while holding a lock, other tasks may try to acquire and stall. Or if you yield while in a transitional state (mid-update), other tasks may observe the broken state.

Brain doesn't use blocking locks (mostly), but the principle applies to any "invariants temporarily broken" state.

## 17. The "explicit yield" idiom

Brain's idiom for explicit yields:

```rust
glommio::executor().yield_if_needed().await;
```

The `yield_if_needed` only yields if other tasks are pending; if not, it's effectively a no-op. Cheap when the shard is idle.

## 18. The "long-running task" pattern

For tasks that genuinely take a long time (e.g., consolidation, rebuild):

```rust
async fn long_task() {
    while !done {
        do_chunk();
        glommio::executor().yield_now().await;   // Always yields
    }
}
```

`yield_now` always yields, regardless of pending tasks. This forces fairness.

## 19. The "parallelism via shards" reminder

Cooperative yielding within a shard means the shard's tasks share one core. For more parallelism, add more shards.

A 4-shard substrate has 4 cores' worth of work running in parallel. Within each, cooperative yielding ensures fairness.

## 20. The summary

Cooperative yielding is the substrate's fairness mechanism on a shared shard. Tasks yield at:

- Implicit await points (most natural).
- Explicit yield calls (in long synchronous sections).

The combination keeps all tasks responsive. No task starves; latency is bounded.

The cost of yielding (~100 ns per yield) is small enough to not affect throughput meaningfully when applied at appropriate cadence.

---

*Continue to [`08_failure_modes.md`](08_failure_modes.md) for failure modes.*
