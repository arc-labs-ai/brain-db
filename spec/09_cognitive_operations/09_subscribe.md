# 09.09 SUBSCRIBE

The SUBSCRIBE primitive: stream changes to memories.

## 1. Semantic contract

```
SUBSCRIBE(filter, agent_id, start_lsn, options) → ChangeStream
```

The substrate opens a long-lived stream that delivers change events as they happen.

## 2. The arguments

### filter

What changes to deliver:

```rust
struct SubscribeFilter {
    agent_id: AgentId,
    contexts: Option<Vec<ContextId>>,    // Limit to specific contexts
    kinds: Option<Vec<MemoryKind>>,      // Limit to specific kinds
    event_types: Option<Vec<EventType>>, // Encode, forget, link, etc.
    min_salience: Option<f32>,
}

enum EventType {
    MemoryEncoded,
    MemoryForgotten,
    MemoryUpdated,         // Salience, kind change, etc.
    EdgeAdded,
    EdgeRemoved,
}
```

All conditions are AND-combined. If a filter is None, that dimension is unrestricted.

### start_lsn

Where to start delivery:

- `LatestOnly`: deliver new events only (ignore history).
- `FromLsn(lsn)`: deliver events with LSN >= lsn.
- `FromTimestamp(t)`: deliver events from time t (mapped to LSN).

For `FromLsn`, the substrate checks if the LSN is still in the WAL (not yet checkpointed-out). If too old, returns `LsnTooOld`.

### options

```rust
struct SubscribeOptions {
    batch_size: usize,            // How many events per delivery (default 100)
    timeout_ms: u32,              // Idle timeout (default 30000)
    include_text: bool,
    include_metadata: bool,
    ack_required: bool,           // Default false; true = client acks each batch
}
```

## 3. The stream protocol

The substrate sends batches:

```rust
struct SubscribeBatch {
    events: Vec<ChangeEvent>,
    batch_lsn: u64,                // LSN at the end of this batch
    has_more: bool,
}

enum ChangeEvent {
    MemoryEncoded { memory_id, agent_id, context_id, kind, text?, metadata?, lsn },
    MemoryForgotten { memory_id, lsn },
    MemoryUpdated { memory_id, fields, lsn },
    EdgeAdded { source, target, kind, weight, lsn },
    EdgeRemoved { source, target, kind, lsn },
}
```

The client reads batches as they arrive. Standard streaming-RPC pattern.

## 4. Event delivery guarantees

- Events are delivered in WAL order (per shard).
- Each event is delivered at-least-once.
- For at-most-once, clients can dedupe by LSN (each event has a unique LSN).
- If the substrate restarts mid-stream, the stream is broken; the client reconnects with `start_lsn` set to the last received batch_lsn.

## 5. Cross-shard subscribers

If `agent_id` spans multiple shards, the substrate orchestrates:

- Open one substream per shard.
- Merge events into a single client-facing stream.
- Order events by LSN within each shard; cross-shard order isn't strictly defined.

For most agents (single shard), this is irrelevant.

## 6. The "tail of the WAL" semantic

SUBSCRIBE is essentially "tail the WAL with a filter applied". The substrate:

1. For `start_lsn` history: replays existing WAL records.
2. For new events: pushes them as they're appended to the WAL.

The boundary between historical and live is invisible to the client; the stream feels continuous.

## 7. The ack protocol

If `ack_required: true`:

- The client must ack each batch.
- The substrate buffers up to N unacked batches (default 10).
- When the buffer is full, the substrate stops sending new batches until the client acks.

This provides backpressure. The client can't be overwhelmed.

If `ack_required: false`, the substrate sends as fast as it can; the client must keep up. If it falls behind, the WAL may roll past (events lost).

## 8. The disconnection / reconnection

If the client disconnects:

- The substrate cleans up the subscription's state.
- The client's filter and position are NOT remembered.
- On reconnect, the client provides `start_lsn` again to resume.

This is by design — server-side subscription state is expensive. Clients track their position.

## 9. Latency

Event-to-delivery latency:

- p50: ~10 ms after the WAL fsync.
- p99: ~50 ms.

The latency is mostly batching delay (events accumulate up to `batch_size` before sending).

For low-latency requirements, use small `batch_size` (e.g., 1). Each event is sent immediately; throughput is lower.

## 10. Throughput

A SUBSCRIBE stream can deliver:

- ~10K events/sec to a single client (typical).
- ~50K events/sec with large batches and large frames.

Per-shard event generation is bounded by encode/forget rates (~5K/sec sustained per shard). So SUBSCRIBE keeps up easily.

## 11. The "include_text" option

If `include_text: true`, MemoryEncoded events include the full memory text. This is heavy:

- Per-event size: text bytes.
- For 1 KB texts: 1 MB per 1000 events.

Useful for applications that need to mirror the data to another store. Most applications don't enable text.

## 12. Filter selectivity

If the filter is selective (e.g., min_salience=0.9), most events are filtered out. The substrate still scans the WAL but discards non-matching events; the client only sees the matches.

For very selective filters (< 1% pass rate), the substrate logs a warning — the WAL scan is doing wasted work.

## 13. The "live + replay" pattern

A common pattern for a downstream consumer:

1. Take a snapshot of the current state via `ADMIN_SNAPSHOT_CREATE`.
2. Note the snapshot's LSN.
3. SUBSCRIBE with `start_lsn = snapshot_lsn + 1`.

The downstream consumer thus has the full state plus a live update stream.

This is the recommended pattern for replication-like use cases.

## 14. Use cases

- **Live agent dashboards**: show the agent's recent activity in a UI.
- **Audit logs**: stream every event to an external log.
- **Replication**: keep a hot standby in sync.
- **Reactive workflows**: trigger external systems on specific events.
- **Data warehouse export**: ETL events to analytics systems.

## 15. The "no historical" mode

For applications that just want live events (not history), use `start_lsn: LatestOnly`. The substrate skips WAL replay and only delivers events from "now" onward.

This is the common case for live dashboards.

## 16. The "checkpointed out" issue

WAL segments older than the checkpoint are eligible for deletion. If the client requests `start_lsn` from a deleted segment, the substrate returns `LsnTooOld`.

The client should:
1. Use a snapshot to get the historical state.
2. SUBSCRIBE from the snapshot's LSN forward.

## 17. Failure modes

### LsnTooOld

The requested start_lsn is in a deleted WAL segment.

### Unauthorized

The client doesn't have permission for the requested agent's data.

### TooManySubscribers

The substrate has hit a max-subscriber limit (configurable, default 100 per shard).

### FilterTooComplex

The filter has too many conditions or too-complex expressions. (Reserved for future filter complexity; in v1, all filters fit.)

## 18. The streaming connection lifecycle

The connection is one-way (substrate to client) after the SUBSCRIBE request. The client:

- Sends SUBSCRIBE.
- Receives batches.
- (Optionally) sends acks.
- Closes the connection when done.

On the substrate side: the connection task pulls events from the WAL tail, applies the filter, frames batches, and sends.

## 19. Resource cost on the substrate

Each subscriber:

- Holds a read transaction (or refreshes periodically).
- Has buffered batches (~100 events × ~few KB each = ~MB).
- Consumes some CPU for WAL scanning and filtering.

For 100 subscribers per shard: ~100 MB of buffer state, ~10% CPU overhead. Acceptable.

For many more subscribers: scale shards or limit the subscriber count.

## 20. SUBSCRIBE vs polling

SUBSCRIBE is more efficient than polling:

- No "is there anything new?" queries.
- Push-based delivery.
- Latency near zero.

For applications that need updates "now and then" (every few seconds), polling RECALL with a recency filter is fine. For applications that need every event, SUBSCRIBE is the right tool.

---

*Continue to [`10_admin.md`](10_admin.md) for admin operations.*
