# 13.05 Streaming Responses

How the SDK handles streaming responses — primarily SUBSCRIBE, but also large RECALL or any future streaming opcodes.

## 1. The streaming model

A streaming response is a sequence of frames on the same stream ID, each containing a response chunk. The SDK exposes this as an async iterator:

```rust
let mut stream = client.subscribe()
    .agent("agent-id")
    .start()
    .await?;

while let Some(event) = stream.next().await? {
    process_event(event);
}
```

The user gets events one at a time. The SDK handles framing, buffering, and flow control.

## 2. The stream's lifecycle

```
1. Client sends SUBSCRIBE frame with stream_id=N.
2. Server begins streaming events on stream_id=N.
3. Each event arrives as a frame; the SDK puts it in the stream's buffer.
4. Each `next()` call returns an event from the buffer.
5. When the user is done, `stream.close()` (or drop) signals end.
6. Server finishes, sends final frame, closes stream.
```

The stream is async; clients can pause without losing events (the server respects the client's window).

## 3. Backpressure

The SDK supports backpressure:

- The SDK maintains a buffer of received events (default size 1000).
- If the buffer fills, the SDK stops reading from the connection (TCP backpressure).
- The substrate observes the slow client via TCP window; pauses sending.

For applications processing slowly, this prevents memory blowup.

## 4. The flow-control window

Per the wire protocol's stream multiplexing, each stream has a window:

```rust
struct StreamWindow {
    available: u32,    // Frames the substrate can send before waiting for ack
    consumed: u32,
}
```

The SDK acks frames after the user reads them; the window updates.

Users don't see this directly, but the mechanism enforces backpressure end-to-end.

## 5. Cancellation

To cancel a stream:

```rust
stream.close().await?;
// Or in async iterators with drop:
drop(stream);
```

The SDK sends a `STREAM_CLOSE` frame. The substrate stops sending on this stream.

If the user simply drops the stream object (in Rust), the destructor sends the close. In Python, an async context manager pattern is used:

```python
async with client.subscribe(...) as stream:
    async for event in stream:
        process(event)
# Auto-closes on exit
```

## 6. Filters

SUBSCRIBE accepts filters:

- Per agent.
- Per context.
- Per event kind.
- Per memory kind.

These are sent in the initial SUBSCRIBE frame; the substrate filters server-side.

```rust
client.subscribe()
    .agent("agent-id")
    .contexts(["important"])
    .events([EventKind::MemoryCreated])
    .start()
    .await?;
```

## 7. The "starting position"

A SUBSCRIBE can start:

- From now (default): only events after subscription.
- From a specific LSN: replay events starting from there.
- From the beginning: all events ever.

```rust
client.subscribe()
    .agent("agent-id")
    .start_from(StartPosition::Lsn(12345))
    .start()
    .await?;
```

For from-LSN subscribes, the substrate must have the WAL records still on disk (per [11.07 WAL Retention](../11_background_workers/07_wal_retention.md)). If too old, the SDK gets a "LSN not available" error.

## 8. Reconnection during streaming

If the connection drops mid-stream:

- In-buffer events are still available.
- New events are not received until reconnect.
- The SDK can reconnect and resume:
  - The substrate's stream state may not survive.
  - The SDK re-subscribes from the last received LSN.

This is opt-in:

```rust
let mut stream = client.subscribe()
    .agent("agent-id")
    .resume_on_disconnect(true)
    .start()
    .await?;
```

Without resume, a disconnect ends the stream and the user reconnects manually.

## 9. The ordering guarantee

Within a stream, events arrive in WAL order. Different agents' events on the same stream may interleave but each is sequential.

The substrate doesn't reorder events.

## 10. Large RECALL responses

For RECALL with very large K (1000+) and `include_text`, the response may exceed the wire protocol's frame size limit (~16 MiB).

The SDK handles this transparently:

- The substrate streams the response across multiple frames.
- The SDK assembles them.
- The user sees a single response object.

For extremely large responses, the SDK could expose them as streams instead of single objects:

```rust
let mut results = client.recall("cue").k(10000).stream().await?;
while let Some(r) = results.next().await? {
    process_result(r);
}
```

`stream()` mode delivers results as they arrive; the user processes them incrementally.

## 11. The stream is iterable

Each language's idiomatic iteration:

```rust
// Rust
while let Some(item) = stream.next().await? {
    process(item);
}
```

```python
# Python
async for item in stream:
    process(item)
```

```typescript
// TypeScript
for await (const item of stream) {
    process(item);
}
```

```go
// Go
for {
    item, err := stream.Recv()
    if err == io.EOF { break }
    if err != nil { return err }
    process(item)
}
```

## 12. The error stream

Errors during streaming are delivered as errors, not as events:

```rust
while let Some(item) = stream.next().await {
    match item {
        Ok(event) => process(event),
        Err(e) => {
            log_error(e);
            break;
        }
    }
}
```

A non-recoverable error ends the stream. A recoverable error (transient) might be auto-retried by the SDK if `resume_on_disconnect` is enabled.

## 13. The keep-alive on streams

For long-lived subscriptions, the substrate sends keep-alive frames:

- Empty event frames every 30 seconds.
- The SDK ignores them (they're for liveness only).

If keep-alives stop arriving (default timeout 90 seconds), the SDK considers the connection dead.

## 14. The "stream close acks"

When the SDK closes a stream:

- Sends `STREAM_CLOSE` frame.
- Awaits the substrate's `STREAM_CLOSED` ack.
- If ack doesn't arrive within timeout, log a warning but proceed.

## 15. The metrics for streams

The SDK exposes:

- Active streams count.
- Events received per stream.
- Buffer size (current).
- Lag (time between event creation and reception).

For long-lived streams, monitoring these helps detect issues.

## 16. The "fan-out" stream

For multi-shard subscriptions (a multi-shard agent), the SDK fans out:

- Subscribes to each shard.
- Merges events from all subscribers.
- Presents as a single stream.

Ordering across shards is best-effort (timestamp-based); strict per-shard ordering is preserved.

## 17. The "single-use" stream

A stream is single-use:

- Once closed, can't be resumed.
- For reconnection, create a new stream.

This simplifies the SDK's state management.

---

*Continue to [`06_idiomatic_languages.md`](06_idiomatic_languages.md) for language-specific idioms.*
