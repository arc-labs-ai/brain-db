# 12.04 The Execution Engine

The executor takes an `ExecutionPlan` and runs it. This file specifies the executor's architecture and runtime behavior.

## 1. The executor's interface

There is no `Executor` object. Execution is a set of free functions over a
cheap-clone context:

```rust
// brain-planner: executor/
#[derive(Clone)]
pub struct ExecutorContext {
    pub embedder: Arc<dyn Dispatcher>,
    pub index: SharedHnsw,
    pub metadata: SharedMetadataDb,
    pub writer: Arc<dyn WriterHandle>,
    /// `Some` only inside a txn-flagged op's request scope.
    pub txn: Option<Arc<TxnSnapshot>>,
    pub caller_namespace: NamespaceId,
    pub caller_space: SpaceId,
    // ...
}

pub async fn execute_path(plan: PathPlan, ctx: &ExecutorContext, trace: bool)
    -> Result<PathResult, ExecError>;
pub async fn execute_reason(plan: ReasonPlan, ctx: &ExecutorContext, trace: bool)
    -> Result<ReasonResult, ExecError>;
// plus `execute_path_stream` / `execute_reason_stream` for the streamed forms.
// `trace` is what QUERY_TRACE sets (§02 §15).
```

One `ExecutorContext` is built per shard and cloned per request, where
`brain-ops`'s dispatcher stamps the authenticated caller's namespace and space
onto the clone. Every field is an `Arc` or already cheap to clone, so no
`&mut` and no mutex is involved.

**Only PATH and REASON go through this crate's executor.** ENCODE, RECALL and
FORGET are planned and executed inside their `brain-ops` handlers
(`handlers::encode`, `handlers::recall`, `handlers::forget`), which call into
the retrievers and the writer directly. `ExecutionPlan::Recall` and the RECALL
plan structs survive only as the shape EXPLAIN renders — see
[02. Per-operation planning §5](02_per_op_planning.md).

## 2. Step-by-step execution

Each stage is a discrete async operation, and RECALL's stages are the ones
worth reading as a sequence:

```text
embed the cue  (one vector, reused by every lane)
  -> probe the memory HNSW with the resolved ef        [02 §4]
  -> probe the HyPE, lexical and statement lanes
  -> fuse the lanes (HyPE boosts a direct hit, never leapfrogs one),
     then apply the precision floors, which ship at 0 and so neither
     abstain nor trim until an operator sets them
  -> resolve metadata rows for the survivors
  -> fetch texts when the request asked for them
  -> build the response
```

Sequential stages yield to other tasks at their await points. Where a stage
fans out — several lanes, or several shards for a namespace-scoped recall — it
joins the futures rather than awaiting them in turn.

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

For typical requests, the task lives ~10 ms. Brain handles thousands of such tasks per second per core.

## 7. Resource handles

The executor uses handles:

- `metadata: SharedMetadataDb` — the shard's redb store.
- `index: SharedHnsw` — the published HNSW generation.
- `writer: Arc<dyn WriterHandle>` — the shard's single writer.
- `embedder: Arc<dyn Dispatcher>` — the embedder service.
- `vectors` — the per-shard by-slot vector source.

They are fields of `ExecutorContext` (§1) rather than separate handle types,
and every one is an `Arc` or already cheap to clone. A request gets a clone of
the shard's context with its caller namespace and space stamped on, so there
is no contention and no handle plumbing per call. Metrics are not a handle:
the families are process-global statics ([17.01](../17_observability/01_signals.md)).

## 8. The shared embedder

The embedder is a shared resource. Multiple executor tasks queue requests to it. The embedder handles batching internally ([07. Batching + GPU](../07_embedding/04_batching_gpu.md)).

For a single-CPU embedder: the queue depth determines latency. For a GPU embedder with batching, throughput is higher; latency depends on batch wait time.

## 9. The shared storage

Storage is per-shard; a shard's context talks only to that shard's writer and readers.

For reads (RECALL), the executor opens a read transaction (or uses a pre-existing one). Lock-free, MVCC.

For writes (ENCODE, FORGET), the executor calls the shard's writer handle on its own task. The WAL group commit batches the records; the redb transaction commits them.

## 10. The writer handle interaction

The writer handle is per-shard; only one, and every executor that mutates state calls it:

```rust
let ack = writer.submit(write).await?;
```

One `submit` does:

1. Idempotency lookup — a replay returns the cached ack and stops here.
2. Appends its WAL records and awaits the group commit's fsync.
3. Inserts into the HNSW, before the metadata transaction opens, so a failure here abandons the write with nothing committed.
4. Opens one redb write transaction, applies every phase, stamps the idempotency row, commits — with no await in that whole region.
5. Publishes events and enqueues the derivation stages.

Multiple executors can be inside `submit` at once; each awaits its own ack. Step 4 is the one they cannot share, and the executor's single thread is what keeps them out of it rather than a queue.

## 11. The cancellation behavior

If a request is canceled (client disconnects), the executor:

- For in-flight reads: cancels them (Glommio cancels the future).
- For in-flight writes: doesn't cancel — the write's own detached task runs to completion. The ack is just dropped.

This means writes can't be canceled mid-flight. They complete and become durable, even if the client gave up. Brain doesn't have an "undo" for in-flight writes.

## 12. Timeouts

Each request has an implicit timeout (configurable, default 30 sec). If the executor exceeds the timeout, it's canceled and a `Timeout` error is returned.

Timeouts apply to the whole request lifecycle, not individual steps. A request that's making progress slowly times out; one that's stuck on a single step will be canceled when the overall timeout hits.

## 13. The execution log

Each execution produces a log entry:

```
{
    request_id, request_kind, space_id,
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

Brain considered making the executor synchronous (no async/await), running on a dedicated thread per shard:

- Pros: no future overhead, simpler control flow.
- Cons: no cooperative concurrency; one stuck request blocks others on the shard.

Glommio's async-on-thread model provides thread-per-core (preserving NUMA locality) plus cooperative concurrency. The async overhead is minimal.

## 17. The executor's footprint

A typical executor task:

- Memory: ~10 KB (the plan, intermediate buffers, futures).
- CPU: ~10 ms over its lifetime.
- Yields: ~10-20 yields per request.

For 10K concurrent requests: ~100 MB of executor state. Reasonable.

---

*Continue to [`05_runtime.md`](05_runtime.md) for cross-task concurrency.*
