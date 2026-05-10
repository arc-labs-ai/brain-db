# 08.08 The Execution Engine

The executor takes an `ExecutionPlan` and runs it. This file specifies the executor's architecture and runtime behavior.

## 1. The executor's interface

```rust
struct Executor {
    storage: StorageHandle,
    embedder: EmbedderHandle,
    metrics: MetricsHandle,
}

impl Executor {
    async fn execute(&self, plan: ExecutionPlan) -> Result<Response, ExecError> {
        match plan {
            ExecutionPlan::Encode(p) => self.execute_encode(p).await,
            ExecutionPlan::Recall(p) => self.execute_recall(p).await,
            // ...
        }
    }
}
```

The executor is async (returns futures). Each `execute_*` method orchestrates the steps in the plan.

## 2. Step-by-step execution

Each step in the plan is a discrete async operation:

```rust
async fn execute_recall(&self, plan: RecallPlan) -> Result<Response> {
    // Step 1: embed
    let cue_vector = self.embedder.embed(&plan.embedding.text).await?;

    // Step 2: shard searches in parallel
    let shard_results = futures::future::try_join_all(
        plan.shards.iter().map(|s| self.search_shard(s, &cue_vector))
    ).await?;

    // Step 3: merge
    let merged = self.merge_results(shard_results, &plan.merge);

    // Step 4: text fetch (if needed)
    let with_text = if let Some(t) = plan.text_fetch {
        self.fetch_texts(&merged, t).await?
    } else {
        merged
    };

    // Step 5: response
    Ok(self.build_response(with_text, plan.response))
}
```

Sequential steps yield to other tasks at await points. Parallel steps (multiple shards) use `try_join_all`.

## 3. Cooperative yields

Within steps, long-running work yields:

- During HNSW search: every ~1000 nodes visited, yield.
- During metadata range scans: every ~100 rows, yield.
- During response building: when assembling large payloads, yield periodically.

This keeps the executor's task fair to other tasks on the same core.

## 4. Error propagation

Errors propagate via `Result`:

```rust
let cue_vector = self.embedder.embed(...).await?;  // ? propagates errors
```

Each step is fallible. If any fails, the executor:

1. Cancels other in-flight steps (Glommio handles this naturally).
2. Builds an error response.
3. Returns to the request handler.

## 5. The executor's concurrency

Within a single request:

- Sequential steps: one at a time.
- Parallel steps: multiple at once via `try_join_all` or similar.

Across requests:

- Multiple executors run on the same shard, on the same Glommio executor.
- Glommio's cooperative scheduler interleaves them.

## 6. The "executor per request" model

Each incoming request gets its own executor task. The task lives until the response is sent.

For SUBSCRIBE (long-lived), the executor task lives for the duration of the subscription.

For typical requests, the task lives ~10 ms. The substrate handles thousands of such tasks per second per core.

## 7. Resource handles

The executor uses handles:

- `StorageHandle`: arena, WAL, metadata, HNSW.
- `EmbedderHandle`: embedder service.
- `MetricsHandle`: instrumentation.

Handles are cheap to clone (Arc-based). Each executor task gets its own handles; no contention.

## 8. The shared embedder

The embedder is a shared resource. Multiple executor tasks queue requests to it. The embedder handles batching internally ([04.06 Batching + GPU](../04_embedding_layer/06_batching_gpu.md)).

For a single-CPU embedder: the queue depth determines latency. For a GPU embedder with batching, throughput is higher; latency depends on batch wait time.

## 9. The shared storage

Storage is per-shard; the executor's StorageHandle for a shard talks to that shard's writer/readers.

For reads (RECALL), the executor opens a read transaction (or uses a pre-existing one). Lock-free, MVCC.

For writes (ENCODE, FORGET), the executor sends the operation to the writer task. The writer batches and commits.

## 10. The writer task interaction

The writer task is per-shard; only one. The executor sends operations to it via a channel:

```rust
async fn send_to_writer(&self, op: WriteOp) -> Result<WriteAck> {
    self.writer_tx.send(op).await?;
    self.writer_ack.recv().await?
}
```

The writer task:

1. Receives operations from the channel.
2. Batches them (for group commit).
3. Appends WAL records, fsyncs.
4. Applies to in-memory state (arena, metadata, HNSW).
5. Sends acks back to executors.

Multiple executors can be waiting for acks; each gets its own.

## 11. The cancellation behavior

If a request is canceled (client disconnects), the executor:

- For in-flight reads: cancels them (Glommio cancels the future).
- For in-flight writes: doesn't cancel — the write is already in the writer task's queue, and the writer will complete it. The ack is just dropped.

This means writes can't be canceled mid-flight. They complete and become durable, even if the client gave up. The substrate doesn't have an "undo" for in-flight writes.

## 12. Timeouts

Each request has an implicit timeout (configurable, default 30 sec). If the executor exceeds the timeout, it's canceled and a `Timeout` error is returned.

Timeouts apply to the whole request lifecycle, not individual steps. A request that's making progress slowly times out; one that's stuck on a single step will be canceled when the overall timeout hits.

## 13. The execution log

Each execution produces a log entry:

```
{
    request_id, request_kind, agent_id,
    plan: {...},
    steps: [{name, latency_ms, ...}, ...],
    total_latency_ms, success/error,
}
```

This is visible in the observability layer. Operators can query it for analysis.

## 14. Backpressure

If the executor is overwhelmed (queue depths growing, latency rising), backpressure propagates upstream:

- The connection layer's frame-receive limits prevent runaway requests.
- The embedder's queue has a max length; new requests fail with `EmbedderOverloaded`.
- The writer's queue has a max length; new writes fail with `WriterOverloaded`.

These are explicit "circuit breaker" signals. Clients see structured errors and can retry with backoff.

## 15. The "fast" vs "slow" path within executor

For simple plans (single-shard RECALL with cache hit):

- Direct embedding cache lookup.
- Direct HNSW search.
- Direct metadata read.
- ~1-2 ms total.

For complex plans (cross-shard PLAN):

- Multiple sequential and parallel steps.
- 30-100 ms.

The executor doesn't have separate code paths; it just runs the plan. The plan's complexity determines the latency.

## 16. The "synchronous" alternative

We considered making the executor synchronous (no async/await), running on a dedicated thread per shard:

- Pros: no future overhead, simpler control flow.
- Cons: no cooperative concurrency; one stuck request blocks others on the shard.

Glommio's async-on-thread model gives us thread-per-core (preserving NUMA locality) plus cooperative concurrency. The async overhead is minimal.

## 17. The executor's footprint

A typical executor task:

- Memory: ~10 KB (the plan, intermediate buffers, futures).
- CPU: ~10 ms over its lifetime.
- Yields: ~10-20 yields per request.

For 10K concurrent requests: ~100 MB of executor state. Reasonable.

---

*Continue to [`09_concurrency.md`](09_concurrency.md) for cross-task concurrency.*
