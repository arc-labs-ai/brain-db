# 14.03 Distributed Tracing

How Brain integrates with distributed tracing systems.

## 1. The standard

Brain uses [OpenTelemetry](https://opentelemetry.io/) — the industry standard. Traces are exported in OTLP format to any compliant backend (Jaeger, Tempo, Honeycomb, Datadog, etc.).

## 2. The span hierarchy

A typical request produces a hierarchy:

```
[client.request] (span)
  └── [brain.encode] (span)
        ├── [brain.embed] (span)
        │     └── [brain.embedder.cache_lookup] (span)
        ├── [brain.arena.write] (span)
        ├── [brain.wal.append] (span)
        ├── [brain.metadata.write] (span)
        └── [brain.hnsw.insert] (span)
```

Each span has:

- A name.
- Start / end timestamps.
- Status (success / error).
- Optional attributes (key-value pairs).
- A parent span ID (for the tree structure).

## 3. The instrumented operations

Brain creates spans for:

- Each request.
- Major phases (planning, execution).
- Storage operations (arena, WAL, metadata).
- HNSW operations.
- Embedder calls.
- Cross-shard fan-outs.
- Background worker cycles.

This gives operators a complete picture of where time is spent.

## 4. Sampling

Tracing every request is expensive. Brain samples:

```toml
[tracing]
sampler = "ratio"
sample_ratio = 0.01         # 1% of requests
```

Other sampler options:

- `always_on`: 100% (debugging).
- `always_off`: 0% (disabled).
- `rate_limited`: max N traces per second.
- `parent_based`: respect upstream's sampling decision.

## 5. The "head sampling" vs "tail sampling"

Brain implements head-based sampling: the decision is made at request start.

For tail-based sampling (decide based on latency or errors after the fact), use a tracing collector that supports it (Tempo, Honeycomb).

## 6. The export

Traces are exported via OTLP to a configured collector:

```toml
[tracing.export]
endpoint = "http://otel-collector:4317"
protocol = "grpc"             # or "http"
batch_max_size = 512
batch_timeout_ms = 5000
```

The collector handles forwarding to the backend (Jaeger, Tempo, etc.).

## 7. Span attributes

Common attributes:

- `brain.shard`: shard UUID.
- `brain.operation`: operation name.
- `brain.agent_id`: agent.
- `brain.request_id`: request ID.
- `brain.duration_ms`: duration.
- `brain.status`: success/error.
- `brain.error_code`: if error.

These follow OpenTelemetry semantic conventions where applicable.

## 8. The propagation

Trace context propagates from the client SDK to the substrate:

```
Client request:
  Headers / metadata: traceparent: 00-<trace_id>-<span_id>-01

Substrate:
  Reads traceparent.
  Creates child span with the parent's trace_id and span_id.
```

Standard W3C traceparent format. Most tracing libraries support it.

## 9. The cross-shard propagation (v2)

In clustered v2, a cross-node call propagates trace context:

```
Node A's span (parent)
  Cross-node call carries trace context
    Node B's span (child, on different node)
```

The trace shows the call across nodes — useful for diagnosing latency in distributed setups.

## 10. The sampling propagation

Sampling decisions propagate. If the client decided to sample (or not), the substrate respects it.

This avoids the "client samples, substrate doesn't" inconsistency.

## 11. Span events

Within a span, point-in-time events:

```rust
span.add_event("hnsw.search.start", attrs);
span.add_event("hnsw.search.end", attrs);
```

Events are like logs but tied to a span. Used for fine-grained timing within a span.

## 12. The performance overhead

Tracing has overhead:

- Span creation: ~1 µs.
- Attribute setting: ~100 ns per attribute.
- Export: batched, async, ~1% CPU at modest sample rates.

For 1% sampling: < 0.1% overhead. For 100%: 1-2% overhead.

## 13. The "trace not exported" path

If the export pipeline is down or backed up:

- The substrate buffers spans (default 10K).
- If buffer fills: oldest spans are dropped, with metric.
- Operations continue normally.

```
brain_tracing_spans_dropped_total
brain_tracing_export_errors_total
```

## 14. The "high-cardinality" warning

Some attributes can have very high cardinality:

- Memory IDs.
- Request IDs.

These are useful for one-off investigation but explode index size in tracing backends.

Brain's defaults include some high-cardinality attributes (memory_id, request_id) because they enable powerful debugging. For deployments where cardinality is a concern, these can be excluded:

```toml
[tracing.attributes]
exclude = ["brain.memory_id", "brain.request_id"]
```

## 15. The async runtime tracing

Glommio's async tasks each have their own context. The substrate ensures spans are properly attached to tasks.

Specific concern: spans don't accidentally cross task boundaries. The Rust `tracing` crate handles this if used correctly.

## 16. The "trace ID in logs"

Logs include the trace ID:

```json
{"ts":"...","level":"info","trace_id":"abc...","span_id":"def..."}
```

Operators can pivot from a trace to logs (in Tempo / Loki / etc.) by trace ID. This is the "logs in context" pattern.

## 17. The "no trace" fallback

If tracing isn't configured:

- Brain runs normally.
- No spans are created or exported.
- Performance is unchanged.

Tracing is opt-in. The substrate doesn't require a tracing backend.

## 18. The "useful traces" examples

Examples of what traces help diagnose:

- "Why is this RECALL slow?" → trace shows time in embedder vs HNSW vs metadata fetch.
- "Where's the bottleneck?" → trace shows the sequential dependencies.
- "Did the client retry?" → trace shows multiple spans on the same request.

Traces complement metrics (which show aggregates) by showing individual requests.

---

*Continue to [`04_dashboards.md`](04_dashboards.md) for dashboards.*
