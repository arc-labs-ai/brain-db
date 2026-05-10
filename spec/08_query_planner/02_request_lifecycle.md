# 08.02 Request Lifecycle

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

The payload is rkyv-decoded into a typed request:

- `Request::Encode(EncodeRequest)`, etc.

Decoding is a zero-copy operation; rkyv's archived form is read directly. The actual deserialization (to Rust types) only happens for fields the planner needs.

If decoding fails (malformed payload), an error response is sent.

## 5. Phase 4: Enforce quotas

The substrate enforces:

- Per-agent quotas (memory count, requests per second).
- Per-context quotas.
- Global limits (concurrent requests, memory pressure).

If a quota is exceeded, an error response (`QuotaExceeded`) is sent.

Quota checks consult per-shard or per-agent counters; very fast.

## 6. Phase 5: Plan

The planner runs (see [`01_planner_overview.md`](01_planner_overview.md)).

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

The response payload is rkyv-encoded. The 32-byte frame header is filled in:

- Magic, version, opcode (the response opcode), flags.
- Stream ID matching the request.
- Payload length and CRC.
- Header CRC.

Frame size limits apply: payloads beyond ~16 MiB must be chunked. For RECALL with K=10 and short texts, payloads are typically < 100 KB; far below the limit.

For larger responses (text-heavy, K=1000), the executor may stream multiple frames on the same stream ([03.09 Streaming](../03_wire_protocol/09_streaming.md)).

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

Each operation is independently planned and executed; the transaction abstraction is at a higher level (see [09. Cognitive Operations](../09_cognitive_operations/) §Transactions).

## 17. The retry from the client

If a client doesn't get a response (network drop), it may retry the request. The substrate's idempotency table ([07.06](../07_metadata_graph/06_idempotency.md)) handles retries:

- Phase 4 (or 5, depending on implementation) checks the idempotency table.
- If a duplicate, the cached response is returned (skip phases 6-7).
- The same response is framed and sent.

The client can't distinguish a fresh response from a replayed one; both are correct.

## 18. The connection lifecycle (above this)

Connection handling (TCP accept, TLS handshake, protocol handshake) happens before any request is received. The connection is shared across multiple requests; each request has its own stream ID.

After the connection is established, requests flow through the lifecycle described here. The connection persists across many requests.

---

*Continue to [`03_recall_planning.md`](03_recall_planning.md) for RECALL planning.*
