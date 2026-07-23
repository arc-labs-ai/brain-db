# 05.05 SUBSCRIBE

The SUBSCRIBE primitive: stream changes to memories.

## 1. Semantic contract

```
SUBSCRIBE(filter, agent_id, start_lsn, options) → ChangeStream
```

Brain opens a long-lived stream that delivers change events as they happen.

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
    memory_ids: Option<Vec<MemoryId>>,   // Limit to specific memories (e.g. watching one write's derivation)
}

enum EventType {
    MemoryEncoded,
    MemoryForgotten,
    MemoryUpdated,         // Salience, kind change, etc.
    EdgeAdded,
    EdgeRemoved,
    StageCompleted,        // one async ENCODE derivation stage finished (auto_edge / temporal_edge / extractor / hype)
}
```

All conditions are AND-combined. If a filter is None, that dimension is unrestricted.

`memory_ids` is the narrowest filter dimension: scoping a subscription to one
or a handful of memory ids turns SUBSCRIBE into a point-observer for those
specific writes, rather than a stream over an agent's whole activity. This is
the mechanism behind the write-pipeline-progress pattern documented in
[02_write_pipeline.md](02_write_pipeline.md) §17e: `ENCODE` with `wait: ack`,
then `SUBSCRIBE` with `memory_ids: [that memory_id]`, to watch that one
write's `StageCompleted` events arrive live. On the wire this field is named
`memory_ids` on `SubscriptionFilter` — see
[04. Wire Protocol](../04_wire_protocol/05_frame_layouts.md) §7.

### start_lsn

Where to start delivery:

- `LatestOnly`: deliver new events only (ignore history).
- `FromLsn(lsn)`: deliver events with LSN >= lsn.
- `FromTimestamp(t)`: deliver events from time t (mapped to LSN).

For `FromLsn`, Brain checks if the LSN is still in the WAL (not yet checkpointed-out). If too old, returns `LsnTooOld`.

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

Brain sends batches:

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
- If Brain restarts mid-stream, the stream is broken; the client reconnects with `start_lsn` set to the last received batch_lsn.

## 5. Cross-shard subscribers

If `agent_id` spans multiple shards, Brain orchestrates:

- Open one substream per shard.
- Merge events into a single client-facing stream.
- Order events by LSN within each shard; cross-shard order isn't strictly defined.

For most agents (single shard), this is irrelevant.

## 6. The "tail of the WAL" semantic

SUBSCRIBE is essentially "tail the WAL with a filter applied". Brain:

1. For `start_lsn` history: replays existing WAL records.
2. For new events: pushes them as they're appended to the WAL.

The boundary between historical and live is invisible to the client; the stream feels continuous.

## 7. The ack protocol

If `ack_required: true`:

- The client must ack each batch.
- Brain buffers up to N unacked batches (default 10).
- When the buffer is full, Brain stops sending new batches until the client acks.

This provides backpressure. The client can't be overwhelmed.

If `ack_required: false`, Brain sends as fast as it can; the client must keep up. If it falls behind, the WAL may roll past (events lost).

## 8. The disconnection / reconnection

If the client disconnects:

- Brain cleans up the subscription's state.
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

If the filter is selective (e.g., min_salience=0.9), most events are filtered out. Brain still scans the WAL but discards non-matching events; the client only sees the matches.

For very selective filters (< 1% pass rate), Brain logs a warning — the WAL scan is doing wasted work.

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
- **Live write-pipeline progress**: pair `ENCODE(wait: ack)` with a `memory_ids`-scoped subscription to watch one write's async derivation stages (`auto_edge`/`temporal_edge`/`extractor`/`hype`) complete in real time, without paying the blocking cost of `wait: derived`. See [02_write_pipeline.md](02_write_pipeline.md) §17e.

## 15. The "no historical" mode

For applications that just want live events (not history), use `start_lsn: LatestOnly`. Brain skips WAL replay and only delivers events from "now" onward.

This is the common case for live dashboards.

## 16. The "checkpointed out" issue

WAL segments older than the checkpoint are eligible for deletion. If the client requests `start_lsn` from a deleted segment, Brain returns `LsnTooOld`.

The client should:
1. Use a snapshot to get the historical state.
2. SUBSCRIBE from the snapshot's LSN forward.

## 17. Failure modes

### LsnTooOld

The requested start_lsn is in a deleted WAL segment.

### Unauthorized

The client doesn't have permission for the requested agent's data.

### ActAsDenied

The request carried `act_as` but the connection principal lacks `can_act_as`, or `act_as.namespace` falls outside its `may_act` allowlist. See [04. Wire Protocol](../04_wire_protocol/04_handshake.md) §10a.3 (R1/R2) and [`07_error_handling.md`](../04_wire_protocol/07_error_handling.md) §3.3.

### TooManySubscribers

Brain has hit a max-subscriber limit (configurable, default 100 per shard).

### FilterTooComplex

The filter has too many conditions or too-complex expressions. (Reserved for future filter complexity; currently, all filters fit.)

## 18. The streaming connection lifecycle

The connection is one-way (substrate to client) after the SUBSCRIBE request. The client:

- Sends SUBSCRIBE.
- Receives batches.
- (Optionally) sends acks.
- Closes the connection when done.

On Brain side: the connection task pulls events from the WAL tail, applies the filter, frames batches, and sends.

## 19. Resource cost on Brain

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

## 21. Implementation status: the real streaming path vs. a dead-code poller

§6's WAL-tail description is the actual behavior on the wire path clients use
today, not a future item. `brain-server`'s connection layer
(`SubscriptionRegistry` / `run_subscription_task`) bridges each shard's real
event bus into a persistent per-connection stream: WAL-tail history replay
cuts over to live delivery with the boundary invisible to the client, exactly
as §6 describes, and `UNSUBSCRIBE` / stream cancellation are handled
properly. This is the code path every real client connection goes through.

A second SUBSCRIBE implementation exists inside the `brain-ops` crate
(`handlers/subscribe.rs`) — a bounded, single-shot poll (default 5s) that
registers with the same underlying registry, waits for one matching event,
and returns. It shares the filter-parsing logic (`ParsedFilter`) with the
real path, which is how filter additions like `memory_ids` (above) reach both
implementations from one code change. But it is **not** reachable from a real
client connection — `brain-server` bypasses it and calls the registry
directly. Do not take this handler's shape, or its module doc, as a
description of what a connected client experiences; §1–§20 above (and this
section) are authoritative.

## 22. Per-request identity (`act_as`)

`SubscribeRequest` carries the same optional `act_as` field every other
data-plane op does — see [04. Wire Protocol](../04_wire_protocol/04_handshake.md)
§10a for the full mechanism (the connection-principal-vs-effective-identity
model, the `can_act_as` grant, and invariants R1–R6). When present, the
subscription observes the effective `(namespace, agent_id)` named by `act_as`
rather than the connection principal's own identity, subject to the same R1
(`can_act_as` gate) and R2 (`may_act` allowlist) checks as every other op.
This is what lets a shared service-principal connection pool — one
credential, `act_as` varying per request — subscribe on behalf of whichever
tenant it's currently serving, the same pattern it already uses for ENCODE /
RECALL / PLAN / REASON.

### 22.1 Why this needed a dedicated fix

Unlike those four primitives, SUBSCRIBE's dispatch was, until this fix,
structurally separate from the normal `act_as`-aware dispatch path:
`SUBSCRIBE`, `UNSUBSCRIBE`, and `CANCEL_STREAM` bypassed the shared
op-dispatch / `act_as_of` resolution entirely, and the wire
`SubscribeRequest` carried no `act_as` field at all — a genuine wire-schema
gap, not just a server-logic one. The permission check that gates a
subscription's agent scope hardcoded the caller's own raw connection
identity, with no `act_as` concept reachable from that branch. This is why
SUBSCRIBE needed a dedicated fix rather than "just working" the way a normal
op picks up `act_as` support — the security machinery itself (the R1/R2
checks, the effective-identity resolution) was already fully reusable; only
the branch that reaches it needed to change.

With `act_as` absent (the common case, and the only case before this fix),
SUBSCRIBE's behavior is unchanged: it observes the connection's own identity
exactly as before.

### 22.2 Related gap, not fixed here

`TXN_BEGIN` / `TXN_COMMIT` / `TXN_ABORT` (§8 in
[03_primitives.md](../01_architecture/03_primitives.md)) have the same
missing-`act_as`-field gap, but they go through normal op dispatch, not a
structural bypass like SUBSCRIBE's. Out of scope for this fix; noted here as
a known follow-up.

---

*Continue to [`06_admin.md`](06_admin.md) for admin operations.*
