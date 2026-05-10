# 10.08 Concurrency Failure Modes

What can go wrong with the concurrency model and how the substrate handles it.

## 1. Reader sees stale state

**Failure mode.** A reader's snapshot of HNSW (or other published state) is older than the writer's current state.

**Detection.** This isn't a failure — it's the protocol's eventual consistency. Detection isn't necessary.

**Response.** None. The reader returns results consistent with the snapshot. Subsequent reads see fresher state once the next publication happens.

**Operator action.** None. If the staleness window is too wide, tune the publication interval.

## 2. Writer can't make progress

**Failure mode.** The writer task is stuck — not committing, not draining its queue. Pending operations build up.

**Detection.** `writer_queue_depth` metric exceeds a threshold; `last_commit_at` is stale.

**Response.**
- Backpressure propagates: new writes fail with `WriterOverloaded`.
- Reads continue (they don't depend on the writer).

**Operator action.** Investigate the writer. Possible causes: disk error, redb corruption, deadlock (a bug). May need to restart the shard.

## 3. Pin held too long

**Failure mode.** A crossbeam-epoch pin is held by a stuck or buggy task; reclamation can't proceed; freed memory accumulates.

**Detection.** Memory usage grows; `epoch_pin_oldest_age` metric exceeds threshold.

**Response.**
- Logs a warning.
- The garbage stays in memory until the pin is dropped.

**Operator action.** Find the stuck task; restart the shard if necessary. Pins normally drop within microseconds; long pins indicate a bug.

## 4. ArcSwap publication failure

**Failure mode.** A publication ("swap to new state") fails — for example, the new state's allocation failed (OOM during state construction).

**Detection.** The writer task's publish operation returns an error.

**Response.**
- The writer continues with the old state.
- New writes go to the pending buffer; will be included in the next publication.
- Logs an error.

**Operator action.** Address memory pressure. The substrate may degrade gracefully (writes queue up) until memory is freed.

## 5. Cross-task race condition

**Failure mode.** A subtle bug — two tasks that should coordinate don't, leading to inconsistent state.

**Detection.** Hard. Manifests as wrong results, data inconsistency, panics, or memory corruption.

**Response.**
- The substrate has invariant checks that fire on inconsistencies.
- Severe cases (memory corruption) cause panics.
- Less severe cases (slight data drift) are logged.

**Operator action.** Report the bug. Restart the shard.

## 6. Deadlock

**Failure mode.** Two tasks each hold a resource the other needs; both wait forever.

**Detection.** Tasks are stuck; no progress.

**Response.**
- Task-level timeouts fire after the configured limit (default 30 sec).
- The stuck task is cancelled.
- Other tasks continue.

**Operator action.** Investigate. Brain's design uses few locks (single-writer, lock-free); deadlocks should be rare. Each is a bug to fix.

## 7. Reader starvation

**Failure mode.** Many concurrent writers monopolize the executor; readers don't run.

**Detection.** Read latency rises sharply; CPU is high.

**Response.**
- Reads' priority is set high; they should preempt unprioritized work.
- Cooperative yields keep things fair.

If starvation persists, the substrate may shed write load (reject new writes) to let reads catch up.

**Operator action.** Investigate workload mix. Add capacity if the shard is genuinely overloaded.

## 8. Writer starvation

**Failure mode.** Many concurrent reads consume CPU; writers can't make progress.

**Detection.** Write latency rises; queue depth grows.

**Response.**
- The writer task's priority is high.
- Cooperative yields ensure the writer gets time.

**Operator action.** Same as above — workload analysis and possibly capacity increase.

## 9. Single-writer assumption violated

**Failure mode.** Due to a bug, two tasks both try to write to the same shard's storage.

**Detection.**
- redb's begin_write blocks; the second writer waits.
- The substrate's writer-task setup ensures only one writer task per shard, but if the assumption is somehow violated:
  - WAL ordering is broken (LSNs out of order).
  - Recovery may misbehave.

**Response.**
- Assertions in debug builds catch the issue.
- In production, the substrate logs and continues; recovery may detect inconsistency.

**Operator action.** Report the bug. The architecture is intentional; bugs that break it are critical.

## 10. Memory reclamation lag

**Failure mode.** Garbage accumulates because pins are held continuously by busy reader tasks.

**Detection.** Memory usage grows over time; `arc_swap_garbage_size` and similar metrics rise.

**Response.**
- The substrate's pin discipline keeps individual pins short.
- Periodic forced-advance may be triggered if accumulation is excessive.

**Operator action.** None usually. Pin durations should be < 100 µs.

## 11. The `await` deadlock

**Failure mode.** A task awaits something that depends on itself making progress.

**Example bug:**
```rust
let lock = mutex.lock().await;
expensive_op().await;          // expensive_op needs the lock too. Deadlock.
```

**Detection.** Tasks stuck; timeouts fire.

**Response.** Timeout cancels the task.

**Operator action.** Code review and tests catch these. They're bugs to fix.

## 12. Glommio executor crash

**Failure mode.** The Glommio executor itself panics or exits.

**Detection.** All tasks on the shard stop; no progress.

**Response.**
- The substrate's main thread observes the shard going dark.
- Auto-restart of the shard's executor (in some configurations).
- Otherwise: the shard is offline; manual intervention needed.

**Operator action.** Investigate via logs. Restart the substrate process if the shard's executor can't be revived.

## 13. CPU pinning failure

**Failure mode.** Glommio can't pin to the assigned CPU (e.g., the CPU is offline, taskset constraints).

**Detection.** Glommio reports an error during executor creation.

**Response.**
- The shard fails to start.
- Logs an error.

**Operator action.** Adjust CPU pinning configuration. Linux's `lscpu` shows available CPUs.

## 14. NUMA traversal

**Failure mode.** Despite CPU pinning, memory accesses cross NUMA domains. Latency rises ~3×.

**Detection.** Performance regression; NUMA-aware monitoring tools (numastat).

**Response.**
- Brain pins memory allocation to the same NUMA node as the executor (Glommio supports this).
- If misconfigured, the substrate logs warnings.

**Operator action.** Configure shard placement to keep CPU and memory on the same NUMA domain.

## 15. The "too many shards" problem

**Failure mode.** The substrate is configured with more shards than CPU cores; some shards share cores.

**Detection.** Per-shard latency varies; some shards are slower.

**Response.**
- Glommio handles shared cores fairly.
- Throughput per shared shard is roughly half.

**Operator action.** Match shard count to core count. The substrate's recommended ratio is one shard per core.

## 16. Recovery from concurrency-related crashes

**Failure mode.** A panic in a concurrency primitive crashes the substrate.

**Detection.** Process exits.

**Response.**
- On restart, recovery from the WAL brings the state back.
- The crash itself doesn't lose data (durability is independent of in-memory concurrency).

**Operator action.** Investigate the panic. Brain's WAL design ensures crash safety; the recovered state is consistent.

## 17. The "concurrent maintenance" subtlety

**Failure mode.** A maintenance worker (e.g., HNSW rebuild) and a regular operation conflict.

**Detection.** Tested for in CI; production should not see issues.

**Response.**
- Maintenance operations explicitly coordinate with ongoing work.
- Atomic publication via ArcSwap ensures readers either see pre-maintenance or post-maintenance state, never both.

**Operator action.** None.

---

*Continue to [`09_open_questions.md`](09_open_questions.md) for unresolved questions.*
