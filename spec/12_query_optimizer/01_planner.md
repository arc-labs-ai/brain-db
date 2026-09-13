# 12.01 Planner

> **TL;DR.** The planner picks a strategy for each request and the request flows through a fixed lifecycle: parse + validate → plan → execute → marshal response. Planning is decision-rule based (no cost-based optimization, no rewriting), bounded at < 50 µs, and produces a predictable execution profile.

## Planner Overview

The planner converts a typed request into an execution plan. This file describes its architecture and decision-making.

## 1. The planner's input

A typed request from validation:

```rust
enum Request {
    Encode(EncodeRequest),
    Recall(RecallRequest),
    Plan(PlanRequest),
    Reason(ReasonRequest),
    Forget(ForgetRequest),
    Link(LinkRequest),
    Unlink(UnlinkRequest),
    Admin(AdminRequest),
    Subscribe(SubscribeRequest),
    Txn(TxnRequest),
    // ...
}
```

Each variant carries the request's parameters: cue text, K, filters, idempotency key, etc.

## 2. The planner's output

An execution plan:

```rust
enum ExecutionPlan {
    Encode(EncodePlan),
    Recall(RecallPlan),
    // ... one variant per request kind
}

struct RecallPlan {
    embedding: EmbeddingStep,
    shards: Vec<ShardSearchStep>,
    merge: MergeStep,
    response: ResponseStep,
}
```

The plan is a description of what to do, not the doing itself. The executor takes the plan and runs it.

## 3. The planner's logic

For each request kind, the planner has a function:

```rust
fn plan_recall(req: &RecallRequest, ctx: &PlannerContext) -> RecallPlan {
    // ...
}
```

These functions:

1. Resolve the routing — which shards to involve.
2. Pick parameters — ef_search, over_factor, etc.
3. Decide on transformations — pre-filter, post-filter.
4. Determine response shape — text inclusion, score thresholds.

Each function is straightforward; no global optimizer.

## 4. The planner context

The planner has access to:

- The request itself.
- Per-shard statistics (memory count, tombstone ratio, last-rebuild-at).
- Configuration (default ef_search, max-results, etc.).
- The agent's metadata (quotas, configuration overrides).

It doesn't have access to the actual storage layer. Planning is computation only; no I/O.

## 5. The planner's invariants

For every request:

- Plan time < 100 µs.
- Plan size < 4 KB (in memory).
- Deterministic given the same input + context (no randomness).

These let the planner be invoked synchronously without yielding the executor's task.

## 6. Decision-making style

The planner uses fixed rules and lookup tables, not search:

```rust
fn pick_ef_search(filter_selectivity: f32, k: usize) -> usize {
    // Simple heuristic
    let base = 64;
    let selectivity_factor = (1.0 / filter_selectivity).max(1.0).min(8.0);
    let k_factor = (k as f32 / 10.0).max(1.0).min(4.0);
    (base as f32 * selectivity_factor * k_factor) as usize
}
```

No ML, no probing, no cost minimization across alternatives. Just rules that have been hand-tuned to work well for typical workloads.

## 7. The "fast path" idea

For the most common request shapes, the planner has a fast path that bypasses the general logic:

- Single-shard agent-scoped RECALL with default params: just pick ef=64, route to the agent's shard, search.
- Single ENCODE on a healthy shard: just route, embed, store.

The fast path is ~10 µs. The general planner is ~50-100 µs. Most requests hit the fast path.

## 8. The plan as a value

The plan is an immutable value passed from planner to executor. This:

- Lets the planner and executor be tested independently.
- Lets the executor log the plan for observability ("this request used ef_search=128 due to selective filter X").
- Allows future enhancements like plan caching or replay.

## 9. The planner doesn't do work

Embedding, storage I/O, and HNSW search aren't part of planning. The planner only describes them.

This separation matters because:

- Planning is synchronous (no yielding).
- Execution is asynchronous (yields all over).

If planning did I/O, the planner would be async, and request handling would have more layers of indirection.

## 10. The planner's API

```rust
struct Planner {
    config: PlannerConfig,
}

impl Planner {
    fn plan(&self, req: Request, ctx: &PlannerContext) -> Result<ExecutionPlan, PlanError> {
        match req {
            Request::Encode(r) => Ok(ExecutionPlan::Encode(self.plan_encode(r, ctx)?)),
            Request::Recall(r) => Ok(ExecutionPlan::Recall(self.plan_recall(r, ctx)?)),
            // ...
        }
    }
}
```

Synchronous. Pure given input and context. Easy to unit-test.

## 11. The planner doesn't know about networks

The planner doesn't deal with TCP, with retries, with timeouts. It doesn't know whether the executor will run subqueries on the same machine or across the network. The plan is at a level of abstraction above transport.

The executor maps shard references to actual destinations: same-machine (direct call) or cross-machine (RPC). The plan just says "search shard X".

## 12. Static vs dynamic planning

The planner does **static** planning: it decides everything based on the request and pre-computed context. It doesn't:

- Issue test queries to the storage to gauge cost.
- Run a small portion of the work and decide based on early results.
- Adapt during execution.

These would be "dynamic" planning. The simpler static approach suffices for Brain's workloads.

The executor does **runtime adaptation** for some things (e.g., re-querying with higher ef if too few results). This isn't planner re-planning; it's the executor following an alternative branch defined in the plan.

## 13. The planner and observability

Each plan is logged with structured fields:

- Request type.
- Chosen parameters (ef_search, over_factor, etc.).
- Estimated cost.
- Latency of the planner itself.

Operators query these logs to debug latency anomalies ("why did this request take 50 ms? — the planner picked ef=500 due to a very selective filter").

## 14. Future enhancements

Possible enhancements (deferred):

- Plan caching: same request shape → cached plan.
- Cost-based plan selection across alternatives.
- Adaptive learning: track per-shard recall vs ef and tune.

The simple rules suffice for current workloads.

---

## Request Lifecycle

The full lifecycle of a request, from connection to response.

## 1. The phases

```
1. Receive frame
2. Validate frame
3. Decode payload
4. Enforce quotas
5. Plan
6. Execute
7. Marshal response
8. Frame response
9. Send response
```

Each phase has its own concerns; failures at any phase result in error responses.

## 2. Phase 1: Receive frame

A connection task reads bytes from the TCP socket and assembles a frame:

- Read the 32-byte fixed header.
- Validate header CRC.
- Read the payload bytes (length specified in header).
- Validate payload CRC.

If validation fails, the connection is closed (the protocol is corrupted).

Latency: < 50 µs typically (network-dependent).

## 3. Phase 2: Validate frame

Higher-level frame validation:

- Magic, version, opcode are all valid.
- Payload length is within limits.
- Stream ID is acceptable.
- The session is in a state where this frame is allowed (e.g., not in handshake).

If validation fails, an error response is sent on the same stream.

## 4. Phase 3: Decode payload

The payload is CBOR-decoded into a typed request:

- `Request::Encode(EncodeRequest)`, etc.

Decoding parses the CBOR payload into the typed request the planner needs.

If decoding fails (malformed payload), an error response is sent.

## 5. Phase 4: Enforce quotas

Brain enforces:

- Per-agent quotas (memory count, requests per second).
- Per-context quotas.
- Global limits (concurrent requests, memory pressure).

If a quota is exceeded, an error response (`QuotaExceeded`) is sent.

Quota checks consult per-shard or per-agent counters; very fast.

## 6. Phase 5: Plan

The planner runs (see [`01_planner.md`](01_planner.md)).

Output: an `ExecutionPlan`.

If planning fails (e.g., the request specifies an impossible combination), an error response is sent.

Latency: < 100 µs (typically < 50 µs via fast path).

## 7. Phase 6: Execute

The executor runs the plan. The work depends on the request kind:

- ENCODE: embed cue, allocate slot, append WAL, fsync, apply, ack.
- RECALL: embed cue, search HNSW, lookup metadata, filter, sort.
- PLAN/REASON: compose multiple RECALLs and edge traversals.
- FORGET: tombstone, ack.
- LINK/UNLINK: edit edge tables.
- ADMIN: per-operation logic.

This is the bulk of the request's latency.

## 8. Phase 7: Marshal response

The execution produces internal data structures (memories, edges, etc.). The marshaller converts them to the wire-protocol response shape.

For RECALL: each result is `(memory_id, score, optional_text, optional_metadata)`.

The marshaller doesn't do work; it copies and rearranges. Fast.

## 9. Phase 8: Frame response

The response payload is CBOR-encoded. The 32-byte frame header is filled in:

- Magic, version, opcode (the response opcode), flags.
- Stream ID matching the request.
- Payload length and CRC.
- Header CRC.

Frame size limits apply: payloads beyond ~16 MiB must be chunked. For RECALL with K=10 and short texts, payloads are typically < 100 KB; far below the limit.

For larger responses (text-heavy, K=1000), the executor may stream multiple frames on the same stream ([04. Streaming](../04_wire_protocol/06_streaming.md)).

## 10. Phase 9: Send response

The framed response is queued for the connection task to send. The connection task:

- Writes bytes to the TCP socket.
- Handles backpressure (if the socket buffer is full, waits).
- May coalesce multiple responses on the same connection.

Latency: < 100 µs typically.

## 11. The cooperative-yield discipline

Each phase yields to other tasks:

- Between phases.
- During phase 6 (execute), at every I/O point.

This lets the executor's task share its core with other request handlers. No phase blocks the core for more than a few microseconds without yielding.

## 12. The request-flow timing

For a typical RECALL:

| Phase | Latency |
|---|---|
| 1. Receive frame | 20 µs |
| 2. Validate | 5 µs |
| 3. Decode | 5 µs |
| 4. Quotas | 2 µs |
| 5. Plan | 30 µs |
| 6. Execute | 8-12 ms (embed dominated) |
| 7. Marshal | 50 µs |
| 8. Frame | 20 µs |
| 9. Send | 50 µs |
| **Total** | **~10-15 ms** |

The non-execute phases are <0.5 ms total. Execute dominates.

## 13. The error path

If any phase fails:

```
on error in phase N:
    1. Build error response: ErrorResponse {
         code: <wire-protocol error code>,
         message: <human-readable>,
         stream_id: <matching request>,
       }
    2. Frame and send.
    3. Log the error with structured fields.
    4. Increment per-error-code counter metric.
```

The response uses the same stream as the request, with the error opcode. The client sees a structured error response.

## 14. The success path

```
on success:
    1. Build response (specific to request type).
    2. Frame and send.
    3. Log success with structured fields (latency, size).
    4. Increment per-operation success counter.
```

## 15. The streaming case

For SUBSCRIBE and other streaming responses:

- Phase 8-9 are repeated for each frame in the stream.
- The stream is open until the client closes it or an error occurs.
- Each frame is independently framed and sent.

The lifecycle for a streaming request is the same; the response phase is just iterated.

## 16. The transactional case

For TXN_BEGIN/TXN_COMMIT brackets:

- TXN_BEGIN is its own request lifecycle.
- Operations within the transaction are their own request lifecycles, all carrying the transaction ID.
- TXN_COMMIT ties them together.

Each operation is independently planned and executed; the transaction abstraction is at a higher level (see [05. Operations](../05_operations/00_purpose.md) §Transactions).

## 17. The retry from the client

If a client doesn't get a response (network drop), it may retry the request. Brain's idempotency table ([10. Metadata](../10_metadata/00_purpose.md)) handles retries:

- Phase 4 (or 5, depending on implementation) checks the idempotency table.
- If a duplicate, the cached response is returned (skip phases 6-7).
- The same response is framed and sent.

The client can't distinguish a fresh response from a replayed one; both are correct.

## 18. The connection lifecycle (above this)

Connection handling (TCP accept, TLS handshake, protocol handshake) happens before any request is received. The connection is shared across multiple requests; each request has its own stream ID.

After the connection is established, requests flow through the lifecycle described here. The connection persists across many requests.

---

*Continue to [`02_per_op_planning.md`](02_per_op_planning.md) for per-operation planning.*
