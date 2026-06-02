# 04.06 Streaming Model

The protocol uses streams to multiplex many concurrent operations over a single TCP connection. This file specifies how streams are identified, ordered, cancelled, and ended.

## 1. What a stream is

A **stream** is a sequence of frames sharing a common `stream_id`, all part of the same logical operation. From the client's perspective, a stream is the unit of asking for and receiving an operation's frames.

Examples:

- An `ENCODE` stream: client sends one `ENCODE_REQ` frame with EOS, server sends one `ENCODE_RESP` frame with EOS. Two frames total.
- A `RECALL` stream: client sends one `RECALL_REQ`, server sends N `RECALL_RESP` frames over time, with the last carrying EOS.
- A `SUBSCRIBE` stream: client sends one `SUBSCRIBE_REQ`, server sends `SUBSCRIBE_EVENT` frames as events occur, until unsubscribe.

Streams are independent of each other. Multiple streams may be active concurrently on a connection.

## 2. Stream IDs

The `stream_id` is a 32-bit unsigned integer in the frame header.

### 2.1 Allocation

- **Client-initiated streams** use **odd** stream IDs: 1, 3, 5, ...
- **Server-initiated streams** are reserved for **even** IDs: 2, 4, 6, ... — not currently used.
- **Stream ID 0** is reserved for connection-level frames (HELLO, WELCOME, AUTH, AUTH_OK, PING, PONG, BYE, errors not associated with a stream).

The client manages its own pool of stream IDs. Recommended approach: allocate sequentially, starting at 1, incrementing by 2 per stream. Wrap-around is acceptable but rare.

### 2.2 Reuse

A stream ID can be reused after the stream ends (EOS or cancellation). Reuse is the client's choice; the server tracks which stream IDs are active.

For high-throughput clients, recycling stream IDs is necessary; otherwise the 32-bit space could be exhausted in extreme cases (4 billion streams). For typical workloads, no reuse is needed.

### 2.3 Limits

The server limits concurrent active streams per connection. Default: 1024. Configurable via `WELCOME.server_features.max_concurrent_streams`.

A client attempting a 1025th concurrent stream gets `ERROR(StreamLimitExceeded)`.

## 3. End-of-stream (EOS)

A frame with the `EOS` flag set is the last frame on its stream. After EOS:

- No more frames will be sent on that stream by the side that sent EOS.
- The receiver may proceed with assumptions about completeness.

### 3.1 Single-frame streams

For request/response operations like `ENCODE` and `FORGET`:

- The request frame has EOS (it's the only request frame).
- The response frame has EOS (it's the only response frame).

### 3.2 Streaming responses

For `RECALL`, `PLAN`, `REASON`, `ADMIN_LIST_TOMBSTONED`, `ADMIN_MIGRATE_EMBEDDINGS`:

- The request frame has EOS (single request frame).
- The server may send multiple response frames; only the last one has EOS.

### 3.3 Subscriptions

For `SUBSCRIBE`:

- The request frame has EOS.
- Server-pushed events do not have EOS — the subscription is open-ended.
- The final EOS comes when the client sends `UNSUBSCRIBE_REQ` (on a different stream) and the server emits a final EOS-bearing frame on the original subscription stream.

### 3.4 EOS as a contract

EOS commits the sender to "no more frames on this stream". A stream may stay open with no frames in flight (e.g., a SUBSCRIBE waiting for events) — that's distinct from EOS, which terminates the stream.

## 4. Frame ordering

### 4.1 Within a stream

Frames within a stream are ordered: the receiver observes them in the order the sender sent them. TCP guarantees this within a connection.

For `RECALL`, this means results stream in priority order — the server emits highest-confidence results first when possible.

For `PLAN`, this means plan steps stream in plan order — first step, second step, etc. (Where partial plans are emitted, this is the order they were discovered, not necessarily the order they'd be executed.)

### 4.2 Across streams

Frames across different streams may be interleaved on the wire. The server may emit frame N of stream A, then frame M of stream B, then frame N+1 of stream A.

This is intentional. Forcing a strict serial order across streams would defeat multiplexing.

### 4.3 Implication for clients

Clients must demultiplex by stream_id. A typical client pattern: each stream is a future / async iterator that consumes frames matching its stream_id.

## 5. Stream cancellation

A client can cancel an in-flight stream by sending a `CANCEL_STREAM` frame.

### 5.1 The cancel frame

```rust
CANCEL_STREAM(stream_id=N)  // cancellation
   payload: { target_stream_id: M, reason: ... }
```

The cancel itself is a stream (with its own stream_id N), targeting another stream (M).

### 5.2 Server response

After receiving `CANCEL_STREAM`:

1. Server stops emitting frames on `target_stream_id = M`.
2. Server emits `CANCEL_STREAM_ACK(stream_id=N, EOS)`.
3. Server emits a final frame on `target_stream_id = M` with EOS, even if no useful payload remains.

The cancellation may not be instant; in-flight frames already in the network may still arrive at the client. The client treats them as no-ops past the cancel.

### 5.3 Cancellation is best-effort

For an `ENCODE` stream, cancelling after the WAL fsync but before the response is meaningless — the operation has committed. Cancellation in this state is acknowledged but doesn't undo the encode.

For a `RECALL` mid-execution, cancellation aborts the search; the client gets the results emitted so far plus the final EOS.

For long-running operations (`PLAN`, `REASON`, `ADMIN_MIGRATE_EMBEDDINGS`), cancellation is the recommended way to stop them early.

### 5.4 Implicit cancellation

When the client closes its side of the connection without sending `BYE`, all its open streams are implicitly cancelled. The server cleans up internal state and frees the stream IDs.

`BYE` is the graceful equivalent: the server processes in-flight frames, then closes.

## 6. Backpressure within a stream

The protocol uses TCP-level flow control for backpressure. There's no application-level "slow down" signal.

For `RECALL` and similar streaming operations, this means:

- If the client reads slowly, the server's writes block.
- The server's emit-results loop yields to the runtime when the write blocks.
- Other streams on the connection are unaffected (each is in its own runtime task).

For `SUBSCRIBE`, additional rate control exists via `max_inflight`:

- The subscription's `max_inflight` parameter limits how many unacked events the server has out at once.
- The client implicitly acks events by reading them (TCP-level reading).
- If the client lags, the server stops sending events; queued events build up server-side until either the client catches up or the queue exceeds an internal limit (and events get dropped — see [05. Operations](../05_operations/00_purpose.md) §SUBSCRIBE for the lossy-vs-lossless modes).

## 7. Stream lifecycle

```
[client allocates stream_id N]
     │
     ▼
[client sends opening frame on stream_id N, with stream-opening opcode]
     │
     ├───► [server processes; may emit frames on stream_id N]
     │
     ▼
   ┌─[stream-end conditions:]
   │
   ├──► [client sends EOS frame] → server ack with EOS → stream closed
   ├──► [server emits EOS frame] → stream closed
   ├──► [client cancels via CANCEL_STREAM] → server EOS → stream closed
   ├──► [client closes connection] → all streams implicitly cancelled
   └──► [stream timeout (configured)] → server emits EOS with timeout error → stream closed
```

A stream is closed when EOS has been observed. After closure, the stream_id may be reused.

## 8. Stream timeouts

The server may impose timeouts on streams:

- **Per-operation timeout** — for operations with built-in budgets (`PLAN`, `REASON`), the budget governs.
- **Idle timeout** — for streaming operations that don't progress (server can't make progress, or client isn't reading), an idle timeout (default: 5 min) closes the stream.

Stream timeouts are server-side; the client can independently impose its own client-side timeout (cancelling the stream when expired).

## 9. Stream observability

Each stream is observable in the server's telemetry:

- A trace span is opened on stream open, closed on EOS.
- Metrics: per-opcode latency, stream duration, payload sizes, error rates.
- Logs: stream open/close events at info level.

The stream's `stream_id` is part of trace/log context, allowing correlation between client and server logs.

## 10. Why stream-based multiplexing

Alternatives:

- **One connection per operation.** High overhead per request; defeats the connection-pool model.
- **Pipelined requests on one connection.** Each request blocks the next; head-of-line problem.
- **Multiplex without IDs (server-side queuing).** Confused at high concurrency; harder to debug.

Stream IDs are the right primitive for "many independent operations on one transport". gRPC, HTTP/2, and many other protocols use the same idea, with minor variation.

## 11. Frame interleaving examples

### 11.1 Two RECALL streams

```
C → S: RECALL_REQ(stream_id=1, EOS)
C → S: RECALL_REQ(stream_id=3, EOS)

S → C: RECALL_RESP(stream_id=1, !EOS)  [stream 1 first batch]
S → C: RECALL_RESP(stream_id=3, !EOS)  [stream 3 first batch]
S → C: RECALL_RESP(stream_id=1, !EOS)  [stream 1 next batch]
S → C: RECALL_RESP(stream_id=3, EOS)   [stream 3 done]
S → C: RECALL_RESP(stream_id=1, EOS)   [stream 1 done]
```

The server interleaves results from both streams. Each stream sees its own result set.

### 11.2 ENCODE while RECALL is running

```
C → S: RECALL_REQ(stream_id=1, EOS)

[server starts processing recall...]
S → C: RECALL_RESP(stream_id=1, !EOS)  [partial results]

C → S: ENCODE_REQ(stream_id=3, EOS)    [client starts an encode while recall is in flight]

S → C: ENCODE_RESP(stream_id=3, EOS)   [encode completes faster]
S → C: RECALL_RESP(stream_id=1, EOS)   [recall completes]
```

The encode runs concurrently with the recall; both complete on their own schedules.

### 11.3 Cancellation

```
C → S: RECALL_REQ(stream_id=1, EOS)

S → C: RECALL_RESP(stream_id=1, !EOS)

C → S: CANCEL_STREAM(stream_id=3, EOS) targeting stream 1

S → C: CANCEL_STREAM_ACK(stream_id=3, EOS)
S → C: RECALL_RESP(stream_id=1, EOS)   [final EOS, possibly empty]
```

After the ack, stream 1 is closed. Stream 3 (the cancel) is also closed.

## 12. Edge cases

### 12.1 Cancel for a stream that doesn't exist

The server responds with `CANCEL_STREAM_ACK` indicating the stream wasn't active (no error — cancel is best-effort and idempotent).

### 12.2 EOS on a stream the server didn't initiate

If the client sends EOS on a stream that's solely server-initiated, the server treats it as a `CANCEL_STREAM` for that stream.

### 12.3 Reusing a stream_id while still active

If the client sends a new stream-opening frame with the same `stream_id` as an active stream, the server returns `ERROR(StreamIdInUse)` and ignores the new frame.

### 12.4 Server emits a frame on stream_id 0 with non-connection opcode

Protocol error. The client closes the connection.

---

*Continue to [`07_error_handling.md`](07_error_handling.md) for error handling.*
