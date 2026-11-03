# 12.05 Runtime (Concurrency and Failure Modes)

> **TL;DR.** How the planner / executor behave under concurrency (cross-task and cross-shard fan-out, in-flight scheduling) and how they handle failures (validation errors, embedder errors, search errors, partial results, timeouts).

## Concurrency

How concurrent executions interact within a shard and across shards.

## 1. The Glommio executor model

Each shard runs on a dedicated OS thread, pinned to a CPU core. On that thread, a Glommio executor schedules many async tasks.

Tasks include:
- Connection handlers.
- Request executors (one per in-flight request).
- The writer task.
- Background workers (decay, consolidation).

Glommio multiplexes them via cooperative async scheduling. No OS-level session switches; just future-state machine resumption.

## 2. The single-writer pattern

Within a shard:
- Many request executors run concurrently (reads in particular).
- One writer task processes mutations sequentially.

Read-only request executors (RECALL, PLAN, REASON) don't touch the writer. They open read transactions on storage and run independently.

Write-bearing request executors (ENCODE, FORGET, LINK) send their write operations to the writer via a channel and await acks.

## 3. Channel-based writer communication

```rust
// Executor side:
let ack = self.send_to_writer(op).await?;

// Writer side:
loop {
    let op = self.queue.recv().await;
    let ack = self.process(op).await;
    op.ack_tx.send(ack).await;
}
```

The channel is bounded (default 1024). If full, executors await until space opens. This provides backpressure naturally.

## 4. Group commits

The writer task batches operations:

```rust
async fn writer_loop(&mut self) {
    loop {
        // Drain at least 1 op
        let mut batch = vec![self.queue.recv().await];
        
        // Try to gather more, with a brief timeout
        let timeout = sleep(Duration::from_micros(100));
        loop {
            select! {
                op = self.queue.try_recv() => batch.push(op?),
                _ = &mut timeout => break,
                _ = (batch.len() >= 64) => break,
            }
        }

        // Process the batch as one group commit
        self.process_batch(batch).await;
    }
}
```

The 100 µs window plus 64-op cap balance latency vs throughput:
- Light load: ~100 µs added latency per write.
- Heavy load: 64 ops per commit, ~5 µs per write.

## 5. Cooperative yielding

Within long-running steps, executors yield:

- Every ~100 µs of CPU work.
- At every I/O await point.
- Explicitly via `tokio::task::yield_now()` (or Glommio equivalent).

Yields let other tasks make progress. Without them, a single heavy request could starve others.

## 6. The reader-writer interaction

Readers (executors handling RECALL, etc.):
- Open redb read transactions on storage.
- Run searches and lookups concurrently.
- Don't block writers (MVCC).

Writers:
- Process operations sequentially.
- Hold redb write transactions briefly.
- Don't block readers (MVCC).

The single-writer-per-shard discipline means there's no writer-vs-writer contention.

## 7. Cross-shard concurrency

Cross-shard queries fan out:

```rust
async fn cross_shard_recall(&self, plan: RecallPlan) -> Result<...> {
    let futures = plan.shards.iter().map(|s| self.search_shard(s));
    let results = futures::future::try_join_all(futures).await?;
    self.merge(results)
}
```

Each shard's search runs on its own Glommio executor (different thread, different core). Truly parallel.

The merge runs on the originating executor (the one handling the request).

## 8. Cross-shard communication

For an in-process deployment (single binary), cross-shard calls are direct method calls — no serialization, no network.

For a clustered deployment, cross-shard calls go over the network. The wire protocol carries the requests and responses. Latency is higher (~1 ms typical intra-datacenter).

## 9. The orchestrator pattern

For complex queries (PLAN, REASON), the orchestrator:

```rust
async fn orchestrate(&self, plan: PlanPlan) -> Result<...> {
    // Step 1: parallel embeddings
    let (start_emb, goal_emb) = futures::join!(
        self.embedder.embed(&plan.starting_state),
        self.embedder.embed(&plan.goal_text)
    );

    // Step 2: parallel RECALLs
    let (start_recall, goal_recall) = futures::join!(
        self.recall(start_emb),
        self.recall(goal_emb)
    );

    // Step 3: traversal (sequential)
    let traversal = self.traverse(start_recall, goal_recall, &plan).await?;

    // Step 4: scoring
    let scored = self.score_paths(traversal);

    Ok(scored)
}
```

Sequential steps are awaited in turn; parallel steps run concurrently.

## 10. The "fan out then gather" cost

Fan-out across N shards:

```
Latency = max(per_shard_latency) + merge_overhead
```

The latency is dominated by the slowest shard. If one shard is overloaded, the whole query waits.

Brain has timeouts per-shard (default 5 sec per shard call). If a shard times out, partial results are returned.

## 11. Background work scheduling

Background workers (decay, consolidation, maintenance) also run on the same Glommio executors. They yield generously to keep request latency low.

Brain prioritizes:
- High: request executors.
- Medium: writer task.
- Low: background workers.

This is enforced through scheduling hints (Glommio supports task priorities).

## 12. The "isolation" guarantee

Each shard's state is isolated:

- A shard can't read another shard's storage directly.
- Cross-shard queries go through the wire protocol or a distributed-call interface.
- A shard's failure doesn't cascade to other shards.

This isolation means shards are good failure boundaries.

## 13. Connection-level concurrency

A single TCP connection can carry multiple in-flight requests (different stream IDs). The connection layer demultiplexes and sends responses on the right stream.

Per-connection limits (max concurrent streams) prevent runaway request submission. Default: 1024 streams per connection.

## 14. The "stop the world" rare events

Some operations briefly stop the world on a shard:

- Arena growth: the writer pauses briefly while the arena is mmapped to a new size.
- Snapshot creation: the writer pauses while the metadata is checkpointed.
- HNSW rebuild swap: a microsecond-level swap of the HNSW reference.

These events are rare and brief. They don't affect throughput meaningfully.

## 15. Measurement

Brain measures concurrency:

- Active executor task count per shard.
- Writer queue depth.
- Per-request waiting time (in queue, not yet executing).

These metrics help operators tune capacity.

## 16. The "small concurrency" advantage

Brain's per-shard concurrency is intentionally limited:

- A handful of cores per shard.
- A few hundred to a few thousand in-flight requests per shard.

This gives:
- Predictable latency (no thread pool exhaustion).
- Easy resource accounting.
- Simple debugging.

For higher throughput, add more shards. Sharding is the scaling lever.

## 17. The "no shared mutable state across shards" rule

A core invariant: shards don't share mutable state. Each shard's:
- Storage is independent.
- HNSW is independent.
- Writer task is independent.

Cross-shard queries communicate via messages (function calls or RPC), never shared memory.

This makes shards independent failure and scaling units. It also makes the codebase simpler — no cross-shard locks, no global state.

---

## Failure Modes

What can go wrong at the planner/executor level and how Brain responds.

## 1. Plan-time error: invalid request

**Failure mode.** The request specifies invalid parameters (K too large, max_depth out of range, etc.).

**Detection.** Planner validates against per-request rules.

**Response.** Error response with a specific error code (`InvalidRequest`).

**Operator action.** None; this is client error.

## 2. Plan-time error: agent quota exceeded

**Failure mode.** Agent has too many memories, sessions, or in-flight requests.

**Detection.** Planner checks quotas.

**Response.** Error response (`QuotaExceeded`).

**Operator action.** Adjust quotas if appropriate; otherwise the agent must reduce usage.

## 3. Embedder unavailable

**Failure mode.** The embedder service is down, returning errors, or timing out.

**Detection.** Embedder calls return errors.

**Response.** The executor returns `EmbedderUnavailable` to the client.

**Operator action.** Investigate the embedder process. It may be CPU-starved, OOM, or have crashed.

## 4. Embedder slow

**Failure mode.** The embedder is responding but slowly (queue building up).

**Detection.** Per-call latency metrics show p99 above thresholds.

**Response.**
- Backpressure: requests with embedder calls fail fast with `EmbedderOverloaded`.
- Cache hits bypass the embedder; their requests still succeed.

**Operator action.** Scale up embedder capacity or shed load.

## 5. Storage unavailable

**Failure mode.** The storage layer is failing — disk full, file not found, redb returning errors.

**Detection.** Storage calls return errors.

**Response.**
- Reads: error to client (`StorageUnavailable`).
- Writes: error to client; the WAL itself may have been written successfully (durable but unacknowledged).

**Operator action.** Investigate. The shard may need to be marked offline until disk issues are resolved.

## 6. Writer queue full

**Failure mode.** Too many writes pending; the writer's input channel is at capacity.

**Detection.** `send_to_writer` returns `WriterOverloaded`.

**Response.** Error to client; clients can retry with backoff.

**Operator action.** Investigate why writes are slow (disk, fsync, batching). Add capacity if sustained.

## 7. Shard not found

**Failure mode.** The router returns a shard ID, but the shard isn't running on the expected machine.

**Detection.** The cross-shard call returns `ShardNotFound`.

**Response.** The executor returns the error to the client. (For multi-shard queries, partial results may be returned.)

**Operator action.** Check the cluster's routing table; the shard may have moved or failed.

## 8. Cross-shard call timeout

**Failure mode.** A cross-shard call takes too long.

**Detection.** Per-call timeout (default 5 sec).

**Response.** The executor returns the timeout to the client; partial results from other shards are still returned.

**Operator action.** Investigate the slow shard.

## 9. Plan exceeded budget

**Failure mode.** The planner estimates a query will take > 1 second.

**Detection.** Cost estimation in the planner.

**Response.** Error to client (`QueryTooExpensive`).

**Operator action.** None; this is a client-side problem (perhaps query optimization or accepting smaller K).

## 10. Executor task crashed (panic)

**Failure mode.** A panic during execution.

**Detection.** Glommio catches panics; the task is terminated.

**Response.**
- Brain logs the panic with backtrace.
- The client sees a generic `InternalError` response.
- Other tasks continue.

**Operator action.** Investigate the panic. This is a Brain bug.

## 11. Idempotency table not found

**Failure mode.** The idempotency table is corrupted or unreadable.

**Detection.** Reads return `redb::Error`.

**Response.**
- The executor proceeds without idempotency check (logs a warning).
- A duplicate request from a client may produce a duplicate memory.

**Operator action.** Fix the idempotency table. Investigate the cause.

This is an open issue: should Brain fail-closed (reject the request) or fail-open (proceed without idempotency)? Currently fail-open with warning. See [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

## 12. Read transaction held too long

**Failure mode.** A SUBSCRIBE or maintenance read holds a redb read transaction beyond a sensible duration.

**Detection.** Per-transaction age.

**Response.** Brain kills transactions older than the configured max (default 1 hour).

**Operator action.** Investigate why; may need to fix a stuck client or worker.

## 13. The merge step fails

**Failure mode.** Merging cross-shard results fails (memory issue, mismatched data).

**Detection.** The merge code throws an error.

**Response.** Error response.

**Operator action.** Likely a Brain bug; report.

## 14. Empty result acceptable vs error

For RECALL, empty results aren't an error — they indicate no matches. The response is a success with 0 results.

For ENCODE, success means the memory was created. There's no "empty success" — you got a MemoryId or you got an error.

For PLAN/REASON, empty results may be expected. The response indicates success with 0 paths/evidence.

## 15. Partial results

For cross-shard queries where one shard fails:

```rust
struct PartialResponse {
    successful_shards: Vec<ShardResult>,
    failed_shards: Vec<(ShardId, Error)>,
    partial: bool,
}
```

The response is marked `partial: true`. Clients can decide whether to accept partial or retry the whole query.

For some clients, partial is fine; for others, they'd rather have a clear error. The client controls this via a request flag (`partial_ok=true`).

## 16. The error code system

Errors are categorized:

- `1xx`: Network / transport.
- `2xx`: Validation.
- `3xx`: Authorization / quota.
- `4xx`: Resource not found.
- `5xx`: Substrate internal error.

Each specific error has a code and a human-readable message. Codes are stable; messages may evolve.

## 17. The retry policy guidance

Brain's responses indicate retryability:

- `Retryable: true` — transient error; client can retry.
- `Retryable: false` — permanent error; retrying won't help.

For example:
- `EmbedderOverloaded` is retryable (with backoff).
- `InvalidRequest` is not retryable (it'll fail again).

Clients implementing retry should respect this signal.

## 18. The "shed load" pathway

When Brain is overloaded (high CPU, memory pressure, queue depths), it sheds load:

- Reject incoming requests with `Overloaded` errors.
- Maintain enough capacity for in-flight requests to complete.

This prevents a death spiral. Better to fail fresh requests fast than to slow down all requests.

The shed-load thresholds are configurable. Default: shed when CPU > 90% sustained for 5 sec.

---

*Continue to [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) for unresolved questions.*
