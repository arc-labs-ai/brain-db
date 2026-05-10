# 08.10 Planner & Executor Failure Modes

What can go wrong at the planner/executor level and how the substrate responds.

## 1. Plan-time error: invalid request

**Failure mode.** The request specifies invalid parameters (K too large, max_depth out of range, etc.).

**Detection.** Planner validates against per-request rules.

**Response.** Error response with a specific error code (`InvalidRequest`).

**Operator action.** None; this is client error.

## 2. Plan-time error: agent quota exceeded

**Failure mode.** Agent has too many memories, contexts, or in-flight requests.

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
- The substrate logs the panic with backtrace.
- The client sees a generic `InternalError` response.
- Other tasks continue.

**Operator action.** Investigate the panic. This is a substrate bug.

## 11. Idempotency table not found

**Failure mode.** The idempotency table is corrupted or unreadable.

**Detection.** Reads return `redb::Error`.

**Response.**
- The executor proceeds without idempotency check (logs a warning).
- A duplicate request from a client may produce a duplicate memory.

**Operator action.** Fix the idempotency table. Investigate the cause.

This is an open issue: should the substrate fail-closed (reject the request) or fail-open (proceed without idempotency)? Currently fail-open with warning. See [`11_open_questions.md`](11_open_questions.md).

## 12. Read transaction held too long

**Failure mode.** A SUBSCRIBE or maintenance read holds a redb read transaction beyond a sensible duration.

**Detection.** Per-transaction age.

**Response.** The substrate kills transactions older than the configured max (default 1 hour).

**Operator action.** Investigate why; may need to fix a stuck client or worker.

## 13. The merge step fails

**Failure mode.** Merging cross-shard results fails (memory issue, mismatched data).

**Detection.** The merge code throws an error.

**Response.** Error response.

**Operator action.** Likely a substrate bug; report.

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

The substrate's responses indicate retryability:

- `Retryable: true` — transient error; client can retry.
- `Retryable: false` — permanent error; retrying won't help.

For example:
- `EmbedderOverloaded` is retryable (with backoff).
- `InvalidRequest` is not retryable (it'll fail again).

Clients implementing retry should respect this signal.

## 18. The "shed load" pathway

When the substrate is overloaded (high CPU, memory pressure, queue depths), it sheds load:

- Reject incoming requests with `Overloaded` errors.
- Maintain enough capacity for in-flight requests to complete.

This prevents a death spiral. Better to fail fresh requests fast than to slow down all requests.

The shed-load thresholds are configurable. Default: shed when CPU > 90% sustained for 5 sec.

---

*Continue to [`11_open_questions.md`](11_open_questions.md) for unresolved questions.*
