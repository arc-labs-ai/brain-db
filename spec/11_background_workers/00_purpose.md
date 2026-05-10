# 11.00 Purpose

This document specifies the substrate's background workers — the asynchronous tasks that maintain state without being directly triggered by client requests.

## What this document covers

- The architecture of the worker infrastructure.
- Each worker's role, scheduling, and behavior.
- Failure handling in workers.
- The interaction between workers and the request-handling pipeline.

## What this document does not cover

- **The data the workers operate on.** Defined in [02. Data Model](../02_data_model/) and [05–07. Storage layers].
- **The concurrency rules workers follow.** Defined in [10. Concurrency](../10_concurrency_epochs/).
- **The metrics workers expose.** Defined in [14. Observability + Operations](../14_observability_ops/).

## 1. Why background workers

Some maintenance can't (or shouldn't) happen in the request path:

- **Decay** of salience: doesn't make sense to do on every read. Periodic batch update is right.
- **Consolidation** of memories: an aggregate operation that takes minutes; can't block client requests.
- **HNSW rebuild**: takes 5-30 seconds at scale; must run async.
- **Idempotency pruning**: a TTL-driven cleanup; doesn't need to happen on every write.

Background workers handle these.

## 2. The worker model

Each worker is a long-running async task on the shard's Glommio executor. It:

- Wakes up periodically (or on a trigger).
- Does its work.
- Sleeps until the next cycle.

```rust
async fn worker_loop(state: Arc<ShardState>) {
    let interval = Duration::from_secs(60);
    loop {
        if let Err(e) = do_one_cycle(&state).await {
            log::warn!("Worker error: {:?}", e);
        }
        sleep(interval).await;
    }
}
```

## 3. Per-shard workers

Most workers are per-shard. Each shard has its own instance of each worker. The shard-local worker operates only on that shard's data.

Some workers are global (one instance for the whole substrate):

- Cluster topology refresh (in distributed deployments).
- Cross-shard load balancing decisions.

The vast majority are per-shard.

## 4. Worker priority

Workers run at lower priority than request handlers. Glommio's task priority system enforces this:

- High: request handlers, the writer task.
- Medium: cross-shard call handlers.
- Low: background workers.

When the shard is busy with requests, workers wait. When there's spare capacity, they run.

## 5. Worker resource limits

Workers can consume:
- CPU.
- Disk I/O (writes during consolidation, reads during reconciliation).
- Memory (during HNSW rebuild, both old and new index in memory).

The substrate caps:
- Per-worker concurrent operations.
- Total background-work CPU (default 50% of the shard's core).
- Memory consumption (depends on worker; HNSW rebuild has explicit limits).

If a worker would exceed limits, it pauses or splits its work.

## 6. The "idle" state

When a worker has no work, it sleeps. Wake-ups are timer-based (most common) or event-based (e.g., post-write triggers a deferred consolidation check).

Sleep periods are configurable; defaults are conservative.

## 7. Worker observability

Each worker exposes:
- `last_run_at`, `last_run_duration_ms` — for monitoring.
- `pending_work` — what the worker has queued.
- `errors_total` — counter of errors.
- `progress_indicator` — for long-running cycles.

These appear in the substrate's metrics endpoint.

## 8. The work-coalescing pattern

Where possible, workers coalesce work:

- The decay worker processes thousands of memories in a single transaction.
- The idempotency cleanup processes thousands of expired entries in one batch.

Per-record overhead is amortized; the worker is more efficient.

## 9. The sequencing

Workers are independent. Two workers may run concurrently (on different sub-tasks). They don't coordinate among themselves; the underlying storage's transactions handle isolation.

For workers that conflict (e.g., decay and consolidation both update the same memories), the storage layer's transactions serialize them. Conflicts are rare in practice.

## 10. Recovery after restart

After a substrate restart:

- Workers start fresh.
- They immediately begin their first cycle (or after a brief delay for stagger).
- Any in-progress state from before the crash is lost; the worker re-discovers what to do.

This is OK because workers are idempotent: running the decay worker a second time on the same memory just produces the same result.

## 11. The "no work to do" case

When the substrate is empty (no memories), workers don't do much:
- Decay: scans the empty memories table, finds nothing.
- Consolidation: same.
- HNSW maintenance: HNSW is empty.

These all complete quickly with low overhead.

## 12. The "very busy" case

When the substrate is under heavy load:
- Workers wait for capacity.
- Their cycles may extend.
- Some workers may shed work (e.g., decay might process fewer memories per cycle).

The substrate prioritizes serving client requests over keeping workers fully on schedule.

## 13. Worker correctness expectations

Workers should:
- Be **safe to interrupt** at any point (no half-done state corruption).
- Be **idempotent** (running a cycle twice produces the same end state).
- Be **incremental** (large work can be broken into smaller pieces).
- **Yield** generously (per [10.07 Yields](../10_concurrency_epochs/07_yields.md)).

## 14. Per-worker chapters

Each worker has its own file in this spec:

- [02. Decay](02_decay.md)
- [03. Consolidation](03_consolidation.md)
- [04. HNSW Maintenance](04_hnsw_maintenance.md)
- [05. Idempotency Cleanup](05_idempotency_cleanup.md)
- [06. Slot Reclamation](06_slot_reclamation.md)
- [07. WAL Retention](07_wal_retention.md)
- [08. Misc Workers](08_misc_workers.md)

---

*Continue to [`01_worker_architecture.md`](01_worker_architecture.md) for the worker infrastructure.*
