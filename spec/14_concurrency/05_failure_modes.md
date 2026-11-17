# 14.05 Concurrency Failure Modes

What can go wrong with the concurrency model and how Brain handles it.

## 1. Reader sees stale state

**Failure mode.** A reader's snapshot of HNSW (or other published state) is older than the writer's current state.

**Detection.** This isn't a failure — it's the protocol's eventual consistency. Detection isn't necessary.

**Response.** None. The reader returns results consistent with the snapshot. Subsequent reads see fresher state once the next publication happens.

**Operator action.** None. If the staleness window is too wide, tune the publication interval.

## 2. Writer can't make progress

**Failure mode.** Writes are stuck — not committing, not draining the WAL channel. In-flight requests hold their admission permits and new ones queue behind the shard's receive.

**Detection.** Write-stage timers (`RedbApply`, `RedbCommit`) stall; the shard's durable watermark stops advancing while the WAL tail keeps moving.

**Response.**
- Backpressure propagates by parking: once every permit is out, the shard stops taking from its receive channel and senders block on a full channel. Nothing is rejected to make room.
- Reads continue only until they too queue behind the parked receive; a saturated shard serves in arrival order.

**Operator action.** Investigate the shard. Possible causes: disk error, redb corruption, an await introduced into the transaction body (which deadlocks the shard outright). May need to restart the shard.

## 3. A superseded index generation held too long

**Failure mode.** A stuck or very slow reader keeps its `Arc` to a superseded published index, so that generation cannot be dropped.

**Detection.** Resident memory holds roughly one extra copy of the graph beyond what the live set needs.

**Response.**
- The old generation stays resident until the last reader drops its `Arc`.
- Nothing accumulates past that. Each publish leaves at most one superseded generation behind, and a new publish cannot stack a third, so a stuck reader costs a bounded amount rather than growing garbage.

**Operator action.** Find the stuck task; restart the shard if necessary. A search holds its reference only for the duration of the search, so a long-lived reference indicates a bug.

## 4. ArcSwap publication failure

**Failure mode.** A publication ("swap to new state") fails — for example, the new state's allocation failed (OOM during state construction).

**Detection.** The writer task's publish operation returns an error.

**Response.**
- The writer continues with the old state.
- New writes go to the pending buffer; will be included in the next publication.
- Logs an error.

**Operator action.** Address memory pressure. Brain may degrade gracefully (writes queue up) until memory is freed.

## 5. Cross-task race condition

**Failure mode.** A subtle bug — two tasks that should coordinate don't, leading to inconsistent state.

**Detection.** Hard. Manifests as wrong results, data inconsistency, panics, or memory corruption.

**Response.**
- Brain has invariant checks that fire on inconsistencies.
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

**Operator action.** Investigate. Brain's design uses few locks (one writer per shard, lock-free reads); deadlocks should be rare. The one that is structurally reachable is §9 — an await inside the write transaction. Each is a bug to fix.

## 7. Reader starvation

**Failure mode.** Concurrent writes monopolize the executor; reads don't run.

**Detection.** Read latency rises sharply; CPU is high.

**Response.**
- The admission bound caps how many writes can be in flight on a shard at once, so the executor always has reader tasks to pick up between write yields.
- Cooperative yields keep things fair.

Brain does not shed write load to let reads catch up. Once the permits are exhausted the shard parks its receive, which queues reads and writes alike in arrival order; the queue is the backpressure signal, and no request is dropped.

**Operator action.** Investigate workload mix. Add capacity if the shard is genuinely overloaded.

## 8. Writer starvation

**Failure mode.** Many concurrent reads consume CPU; writes can't make progress.

**Detection.** Write latency rises; the shard's receive channel backs up.

**Response.**
- Cooperative yields ensure writes get time.
- The WAL fsync is a real yield, so a write waiting on durability does not hold the core.

**Operator action.** Same as above — workload analysis and possibly capacity increase.

## 9. An await inside the write transaction

**Failure mode.** A code change introduces an await point between `begin_write` and `commit`. A second in-flight request reaches the same region while the first is suspended inside it.

**Detection.** The shard wedges: redb's write lock is held by a task that cannot be resumed until the holder of the lock makes progress, and on a single-threaded executor nothing else will release it. Every subsequent request on that shard parks forever.

**Response.** There is no runtime guard. The serialization of writes is the absence of that await point, so violating it is not a degraded mode — it is a hang.

**Operator action.** Report the bug and restart the shard. Code review of the transaction body is the enforcement mechanism; the body is deliberately kept short and synchronous so a reviewer can see the whole of it at once.

## 9a. Durable watermark pinned below the WAL tail

**Failure mode.** A reserved LSN range is never resolved, so the shard's contiguous committed prefix stops advancing even though writes keep committing above it.

**Detection.** The watermark lags the WAL tail without bound; checkpoints keep recording the same `durable_lsn`, so replay windows grow on every restart.

**Response.**
- Every exit from the write path after the WAL append resolves its range: success commits it, and a `Drop` guard abandons it on any other path, including cancellation.
- If the submitting task disappears between the drain's reservation and its reply, the drain abandons the range itself rather than leaving it reserved.
- Checkpoint and subscribe-event records are never reserved, so they cannot pin the frontier.

**Operator action.** A persistently lagging watermark is a bug — some path reserved a range and neither committed nor abandoned it. Report it; the shard remains correct (recovery replays more than it needs to) but recovery time degrades.

## 10. Memory peak across a publish

**Failure mode.** Not a failure — a cost. Between a publish and the moment the last reader drops the previous generation, both the old and the new index are resident. Resident memory peaks across that window.

**Detection.** Memory sawtooths around each flush rebuild, with the peak about one extra graph above the steady state.

**Response.**
- Reclaiming whole generations is the design: there is no per-node free to pace, and no deferred queue that could fall behind.
- The window is bounded by the longest in-flight search, not by a reclamation cadence.

**Operator action.** Size the shard for the peak, not the steady state. A shard whose working set only fits without the extra copy is under-provisioned.

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
- Brain's main thread observes the shard going dark.
- Auto-restart of the shard's executor (in some configurations).
- Otherwise: the shard is offline; manual intervention needed.

**Operator action.** Investigate via logs. Restart Brain process if the shard's executor can't be revived.

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
- If misconfigured, Brain logs warnings.

**Operator action.** Configure shard placement to keep CPU and memory on the same NUMA domain.

## 15. The "too many shards" problem

**Failure mode.** Brain is configured with more shards than CPU cores; some shards share cores.

**Detection.** Per-shard latency varies; some shards are slower.

**Response.**
- Glommio handles shared cores fairly.
- Throughput per shared shard is roughly half.

**Operator action.** Match shard count to core count. Brain's recommended ratio is one shard per core.

## 16. Recovery from concurrency-related crashes

**Failure mode.** A panic in a concurrency primitive crashes Brain.

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
