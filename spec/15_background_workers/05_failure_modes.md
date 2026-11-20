# 15.05 Worker Failure Modes

What can go wrong with the background workers and how Brain responds.

## 1. A worker crashes

**Failure mode.** A worker task panics or returns an error.

**Detection.** Glommio catches the panic; the task ends. Brain's task supervisor notices.

**Response.**
- Brain logs the panic with backtrace.
- The worker is restarted (after a brief backoff, default 30 seconds).
- Other workers continue.

**Operator action.** Investigate the cause. Repeated panics indicate a bug or systematic issue.

## 2. A worker is stuck

**Failure mode.** A worker is alive but not making progress (e.g., waiting on a lock that's held forever).

**Detection.** `worker_last_run_at` metric stagnates.

**Response.**
- Watchdog timer: if the worker hasn't run a cycle in 3× the expected interval, log alert.
- After 10×, optionally restart the worker.

**Operator action.** Investigate. Possible deadlock or storage issue.

## 3. A worker can't keep up

**Failure mode.** Work accumulates faster than the worker can process. Queue depth or eligible-set grows.

**Detection.** `worker_pending_work` metric exceeds threshold.

**Response.**
- The worker continues at its configured rate.
- Brain logs warnings as the backlog grows.

**Operator action.** Increase the worker's batch size, decrease its interval, add capacity, or reduce the workload that creates the backlog.

## 4. A worker error during cycle

**Failure mode.** A specific cycle fails (e.g., a transient redb error).

**Detection.** The worker's cycle returns an error.

**Response.**
- The worker logs the error.
- Sleeps as normal until next cycle.
- Errors_total counter increments.

**Operator action.** None usually (transient). For repeated errors on a specific worker, investigate.

## 5. The HNSW rebuild OOMs

**Failure mode.** A rebuild's memory peak exceeds the limit; the rebuild aborts.

**Detection.** Worker logs "HNSW rebuild aborted: out of memory" or similar.

**Response.**
- The old HNSW remains active.
- The worker doesn't auto-retry immediately; it waits for the next cycle and may try again.
- If memory pressure persists, the rebuild keeps failing.

**Operator action.** Address memory pressure: scale up RAM, reduce shard count, etc.

## 6. The consolidation summarizer is unavailable

Consolidation reaches its model through a `Summarizer`, which reports two distinguishable conditions. There is no circuit breaker: the distinction below is what a breaker would otherwise approximate, and it is exact.

**No backend at all.** The worker checks `Summarizer::enabled()` once per cycle and returns immediately when it is false, so a deployment without consolidation does no clustering work rather than discovering the absence one `summarize` call in. The substrate default returns `SummarizerError::Disabled`, which also ends the pass.

**Backend present but failing.** `SummarizerError::Failed` is logged at `trace` and that one cluster is skipped; the pass continues with the next cluster, and the next cycle tries again. A cluster whose summary comes back empty is skipped the same way. Nothing escalates and nothing pauses — a failing summarizer costs the summaries it could not produce and nothing else.

Consolidation is billed to the tenant whose memories it consolidates: the namespace is passed to `summarize` rather than inferred, and clustering is strictly within one `(namespace, space, session)`, so every call has exactly one owner.

**Operator action.** Restore the summarization backend, or leave it disabled — consolidation is a no-op without one, not a failure.

## 7. (Removed) The decay worker created database contention

Salience now decays when read, so there is no decay worker and no decay writes.

## 8. The reclamation worker leaves dangling state

**Failure mode.** Reclamation crashes mid-way; partial state.

**Detection.** Inconsistencies in metadata (memory deleted but text not, etc.).

**Response.**
- The transaction is atomic — partial commits don't happen.
- If Brain crashes during reclamation, the redb transaction is rolled back.
- Recovery sees the memory as still tombstoned; reclamation is retried.

**Operator action.** None.

## 9. The WAL retention deletes a needed segment

**Failure mode.** A bug or misconfigured retention deletes a segment still needed for recovery.

**Detection.** Recovery fails with "WAL gap detected".

**Response.**
- Brain refuses to start.
- An operator must restore from backup.

**Operator action.** Restore from snapshot. Investigate the misconfiguration. Strict review of retention settings.

## 10. The idempotency sweep races with replays

**Failure mode.** A client retries a request just as the cleanup is deleting the entry. The client's lookup misses; the request is processed as new, possibly producing a duplicate.

**Detection.** Hard to detect in real time. Audit logs may reveal duplicates after the fact.

**Response.**
- The window is microseconds.
- Increasing the TTL reduces the chance.

**Operator action.** None typically. For workloads where this matters, increase TTL.

## 11. Worker metrics not updated

**Failure mode.** A worker is running but its metrics aren't updating (a bug).

**Detection.** Metrics show stale values.

**Response.**
- Logs show the worker is active.
- Metric is wrong but worker is correct.

**Operator action.** Investigate the metric reporting path. The worker's actual function isn't compromised.

## 12. The "all workers paused" scenario

**Failure mode.** Operator pauses all workers (e.g., for migration, debugging).

**Detection.** Operator action; all workers idle.

**Response.**
- Brain continues serving requests.
- Backlog grows: tombstones accumulate, idempotency table grows, salience doesn't decay.
- Performance degrades over time.

**Operator action.** Resume workers when the underlying issue is resolved.

## 13. The "very old shard" recovery

**Failure mode.** A shard hasn't run workers for a long time (was offline). When it comes back online, there's a large backlog.

**Detection.** Worker pending work metrics are very high at startup.

**Response.**
- The workers process the backlog over many cycles.
- Other operations continue at full speed.

**Operator action.** Monitor the backlog drain. May want to temporarily increase worker rates.

## 14. The shard fills with tombstones

**Failure mode.** Mass deletion (FORGET-by-filter) creates many tombstones; the maintenance worker can't keep up; the shard's HNSW recall degrades.

**Detection.** `tombstone_ratio` > 50%; recall metrics drop.

**Response.**
- Maintenance worker triggers immediate full rebuild.
- Rebuild takes a while (10s of seconds for 1M memories).
- Once complete, recall is restored.

**Operator action.** Monitor. If sustained, consider sharding or data-model changes.

## 15. Deadlock between workers

**Failure mode.** Two workers each hold a redb lock; both wait.

**Detection.** Worker `last_run_at` stagnates.

**Response.**
- redb's transaction model prevents this in practice (one writer at a time).
- If it somehow happens (a Brain bug), both workers' transactions time out.

**Operator action.** Report the bug.

## 16. The "config change without restart" path

**Failure mode.** Operator changes a worker's interval without restart; old interval persists.

**Detection.** Configured interval doesn't match actual cycle timing.

**Response.**
- Brain doesn't auto-reload all config.
- Some configs are reloaded on signal (`SIGHUP`); others require restart.
- The configured-vs-actual mismatch is logged.

**Operator action.** Send `SIGHUP` or restart for full config reload.

## 17. The cumulative effect

Workers are interdependent:

- Slot reclamation can't run if metadata is inaccessible.
- HNSW rebuild needs metadata + arena.
- Edge scrub needs the metadata's edge tables.

If one core component is unhealthy, multiple workers fail. Brain's health endpoints show worker statuses; an operator sees correlated failures.

## 18. Recovery's effect on workers

After a Brain restart with WAL replay:

- Workers start fresh.
- Their cursors (e.g., decay cursor) are read from persistent storage if applicable, or reset.
- The first few cycles may catch up on backlog.

This is by design; no special handling needed. Workers are robust to restarts.
