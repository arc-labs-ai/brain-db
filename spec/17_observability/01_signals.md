# 17.01 Signals (Metrics, Logs, Traces)

> **TL;DR.** Brain emits three observability signals: Prometheus / OpenMetrics-format metrics (counters, gauges, histograms — per-shard, per-operation, per-tenant), structured JSON logs with consistent fields, and OpenTelemetry traces with span propagation through the connection layer. All three are continuously emitted at < 5% total overhead; operators consume them via Prometheus / Loki / Tempo or equivalents.

## Metrics

The metrics Brain exposes — what they mean, how to read them, and why they exist.

## 1. The format

Brain emits metrics in OpenMetrics format (Prometheus-compatible) on the `/metrics` HTTP endpoint:

```
# HELP brain_request_total Total requests.
# TYPE brain_request_total counter
brain_request_total{shard="<uuid>",op="encode",status="success"} 12345
brain_request_total{shard="<uuid>",op="recall",status="success"} 67890

# HELP brain_request_duration_ms Request duration histogram.
# TYPE brain_request_duration_ms histogram
brain_request_duration_ms_bucket{op="encode",le="0.005"} 100
brain_request_duration_ms_bucket{op="encode",le="0.010"} 9000
brain_request_duration_ms_bucket{op="encode",le="0.025"} 12000
brain_request_duration_ms_bucket{op="encode",le="+Inf"} 12345
```

The endpoint is on a separate port from the data plane (default 9091).

## 2. The metric naming

Brain follows Prometheus conventions:

- All metrics start with `brain_`.
- `_total` suffix for counters.
- `_seconds` or `_ms` suffix for durations.
- `_bytes` suffix for sizes.
- Label keys are lowercase snake_case.

Per the [Prometheus naming guide](https://prometheus.io/docs/practices/naming/).

## 3. Request metrics

Per-operation counters and histograms:

```
brain_request_total{op=, shard=, status=}
brain_request_duration_ms{op=, shard=, status=}
brain_request_active{op=, shard=}        # Currently in-flight
```

Operations: encode, recall, plan, reason, forget, link, unlink, txn_begin, txn_commit, txn_abort, subscribe, admin_*.

Status: success, error_<code>, timeout.

## 4. Memory metrics

Per-shard counts:

```
brain_memory_count{shard=}                    # Active
brain_memory_count_tombstoned{shard=}         # Tombstoned
brain_memory_count_total{shard=}              # Active + tombstoned
brain_memory_kind{shard=, kind=episodic}
brain_memory_kind{shard=, kind=semantic}
brain_memory_kind{shard=, kind=consolidated}
```

These are gauges, sampled periodically (every 30s by default).

## 5. Storage metrics

```
brain_arena_used_bytes{shard=}
brain_arena_capacity_bytes{shard=}
brain_arena_slots_used{shard=}
brain_arena_slots_free{shard=}
brain_wal_size_bytes{shard=}
brain_wal_segments{shard=}
brain_metadata_size_bytes{shard=}
```

Storage utilization. Operators monitor for "approaching capacity".

## 6. HNSW metrics

```
brain_hnsw_node_count{shard=}                 # Active nodes
brain_hnsw_tombstone_count{shard=}            # Stale nodes
brain_hnsw_tombstone_ratio{shard=}            # tombstone / total
brain_hnsw_search_visits{shard=, quantile=}   # Nodes visited per search
brain_hnsw_recall_estimate{shard=}            # Estimated recall (0-1)
brain_hnsw_rebuild_in_progress{shard=}        # 0 or 1
brain_hnsw_rebuild_progress_pct{shard=}
brain_hnsw_rebuild_count_total{shard=}        # Lifetime rebuilds
brain_hnsw_rebuild_duration_sec{shard=, quantile=}
```

For monitoring the index's health.

## 7. Embedder metrics

```
brain_embedder_calls_total{model=}            # Embeddings produced
brain_embedder_cache_hits_total{model=}
brain_embedder_cache_misses_total{model=}
brain_embedder_duration_ms{model=, quantile=}
brain_embedder_queue_depth{model=}
brain_embedder_workers_active{model=}
```

The embedder is often a bottleneck; these metrics surface it.

## 8. Worker metrics

Per worker:

```
brain_worker_cycles_total{shard=, worker=}
brain_worker_processed_total{shard=, worker=}
brain_worker_errors_total{shard=, worker=}
brain_worker_cycle_duration_ms{shard=, worker=, quantile=}
brain_worker_last_run_unixtime{shard=, worker=}
brain_worker_pending_work{shard=, worker=}
```

Workers: decay, access_boost, consolidation, hnsw_maintenance, idempotency_cleanup, slot_reclamation, wal_retention, edge_scrub, counter_reconciliation, statistics_update, embedder_cache_eviction.

## 9. Connection metrics

```
brain_connections_active                      # Active client connections
brain_connections_total                       # Lifetime opens
brain_connections_closed_total{reason=}
brain_streams_active                          # Active multiplexed streams
brain_frame_send_total{op=}
brain_frame_recv_total{op=}
brain_frame_size_bytes{op=, direction=, quantile=}
```

Network-level metrics for diagnosing connectivity / protocol issues.

## 10. Resource metrics

Standard Linux process metrics, plus Brain-specific:

```
process_cpu_seconds_total
process_memory_resident_bytes
process_memory_virtual_bytes
process_open_fds
brain_executor_latency_ms{shard=, quantile=}  # Glommio latency
brain_executor_tasks_active{shard=}
```

## 11. The "rate" derivations

Most metrics are counters. Rates (req/s, errors/s) are derived in PromQL:

```
rate(brain_request_total{op="encode",status="success"}[5m])
sum(rate(brain_request_total[5m])) by (op)
```

The dashboards (next file) show these.

## 12. Histogram buckets

Default histogram buckets (in ms):

```
1, 2.5, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, +Inf
```

Covers ~1ms (fast ops) to 10s (slow ones). Tunable per-deployment if specific buckets are wanted.

## 13. Cardinality

Labels with high cardinality (per-agent, per-context) create explosion. Brain avoids them in metrics:

- ✓ Shard label: low cardinality (~16 values).
- ✓ Operation label: low cardinality (~10 values).
- ✗ Agent ID: high cardinality (millions); not in metrics.

Per-agent observability uses logs, not metrics.

## 14. Sampling

Some metrics are sampled rather than every-event:

- HNSW search visits: every Nth search.
- Recall quality estimate: hourly sample.

This bounds metric volume.

## 15. Labels: shard ID

The `shard` label uses the UUID:

```
brain_memory_count{shard="abc123-uuid"} 1234
```

For dashboards, operators may add a friendly name via metric relabeling:

```
brain_memory_count{shard="abc123-uuid", shard_name="prod-shard-0"} 1234
```

## 16. The `up` metric

Standard Prometheus convention:

```
up{job="brain", shard="<uuid>"} 1
```

`up=0` means the shard isn't responding. Alerting on `up == 0` catches outages.

## 17. The `_info` metrics

Static information:

```
brain_build_info{version="1.0.0", commit="<sha>", build_date="..."} 1
brain_config_info{shard="<uuid>", arena_size="1Gi", hnsw_M="16"} 1
```

These have value=1; the labels carry the info. Useful for cross-referencing.

## 18. Histogram vs summary

Brain uses histograms (server-side aggregation friendly), not summaries (client-side quantiles).

Histograms work better with multi-replica deployments. Summaries don't aggregate across instances.

## 19. The metrics endpoint security

The `/metrics` endpoint is unauthenticated by default — typical for Prometheus scraping. For deployments wanting auth:

```toml
[metrics]
endpoint = "/metrics"
auth = "basic"
auth_users = [{name = "prom", password = "..."}]
```

Production deployments should at least restrict network access (firewall the metrics port).

## 20. The metric schema document

Brain ships with a metrics catalog:

- Every metric documented.
- Bounds and expected ranges.
- Linked alerts.
- Change log across versions.

Operators use this to write custom alerts and dashboards.

---

## Logs

How Brain emits structured logs.

## 21. The log format

JSON-structured logs:

```json
{
  "ts": "2026-05-07T12:00:00.123Z",
  "level": "info",
  "logger": "brain.executor",
  "shard": "<uuid>",
  "operation": "encode",
  "agent_id": "agent-001",
  "request_id": "...",
  "duration_ms": 8,
  "msg": "encode completed"
}
```

One JSON object per line. Readable with `jq`, ingestible by Loki / Elastic / Splunk / etc.

## 22. The log levels

- **TRACE**: per-frame protocol details. Used during deep debugging.
- **DEBUG**: per-request details. Used during development.
- **INFO**: normal lifecycle events (startup, shutdown, worker cycles).
- **WARN**: unusual conditions (retries, slow queries, capacity warnings).
- **ERROR**: errors that need attention (failed operations, etc.).

Default level: INFO. Production deployments may use WARN to reduce volume.

## 23. The destination

By default, logs go to stdout. Operators redirect to:

- A file (`brain > /var/log/brain.log`).
- A log aggregator (via stdout capture in containers).
- syslog.

Config:

```toml
[logging]
output = "stdout"            # or "file", "syslog"
file_path = "/var/log/brain/brain.log"
rotation = "daily"           # for file
```

## 24. The fields

Common fields in all entries:

- `ts`: ISO 8601 timestamp.
- `level`: severity.
- `logger`: which subsystem (brain.executor, brain.worker.decay, etc.).
- `msg`: human-readable message.

Operation-specific fields:

- `operation`: encode, recall, etc.
- `shard`: shard UUID.
- `agent_id`: agent (when applicable).
- `request_id`: request UUID.
- `duration_ms`: latency.

Error-specific fields:

- `error_code`: stable error identifier.
- `error_message`: human-readable.
- `stack`: Rust backtrace (DEBUG/TRACE only).

## 25. The "logger" hierarchy

The logger is a dotted path:

- `brain` — top level.
- `brain.executor` — request handlers.
- `brain.worker.<name>` — workers.
- `brain.storage.arena` — arena layer.
- `brain.storage.wal` — WAL layer.
- `brain.hnsw` — index.
- `brain.embedder` — embedding service.
- `brain.network` — connection layer.

Operators can filter by logger to focus:

```
| jq 'select(.logger | startswith("brain.worker"))'
```

## 26. The per-level guidance

What logs at each level:

```
TRACE:
  - Per-frame send/receive.
  - Per-iteration of a search.

DEBUG:
  - Per-request begin/end.
  - Per-cycle of a worker.
  - Per-checkpoint.

INFO:
  - Brain startup / shutdown.
  - Each worker's hourly summary.
  - Each major event (rebuild started, snapshot created).

WARN:
  - Retries.
  - Slow operations (> p99 expectation).
  - Approaching capacity.
  - Recovered from transient error.

ERROR:
  - Failed operations after retries.
  - Storage errors.
  - Crashes.
```

## 27. The "no PII" rule

Logs should not contain user data by default:

- ✗ Memory text.
- ✗ Cue text.
- ✓ Memory IDs (opaque).
- ✓ Counts and durations.

For debugging that requires user data, use TRACE level (which can be enabled selectively, with auth).

## 28. The audit log

Separate from regular logs, an audit stream:

- Every state-mutating operation.
- Every admin action.
- Hash-chained for tamper evidence.

```
brain-audit.log:
{
  "ts": "...",
  "actor": "agent-001",
  "operation": "encode",
  "memory_id": "...",
  "agent_id": "...",
  "context": "...",
  "auth_method": "token",
  "hash": "sha256:...",
  "prev_hash": "sha256:..."
}
```

The hash chain lets auditors verify the log hasn't been tampered with.

## 29. Audit log retention

Audit logs typically have stricter retention:

- Production: 1-7 years (regulatory).
- Internal: 90 days.

Configurable via `[audit]` section. Logs can be exported to external storage (S3, etc.).

## 30. Log sampling

For very high-volume operations, Brain may sample:

- 100% of errors.
- 10% of warnings.
- 1% of info.

Default is no sampling (full fidelity). Sampling is opt-in for cost-conscious deployments.

## 31. Per-request logging

Each request, by default, emits one INFO log at completion:

```json
{"ts":"...","level":"info","operation":"encode","shard":"<uuid>","agent_id":"...","request_id":"...","duration_ms":8,"status":"success","msg":"encode completed"}
```

For DEBUG, additional logs at start, mid-points, end.

## 32. Log-rate adaptive

Under load, log volume can become a problem. Brain has:

- Rate-limited error logging (don't log "same error" thousands of times).
- Backpressure on the log pipeline (drop with warning if backed up).

These prevent observability from becoming a performance issue.

## 33. The "message" field guidance

Messages should be:

- Short (< 80 chars typical).
- Action-focused ("encode completed", "cache miss", "rebuild started").
- Avoid raw IDs/values in the message — those are in dedicated fields.

Bad:
```
"msg": "Encoded memory abc-123 in context xyz at 12:34:56 with salience 0.8"
```

Good:
```
"msg": "encode completed",
"memory_id": "abc-123",
"context_id": "xyz",
"salience": 0.8
```

## 34. The "trace ID" propagation

If the client provides a trace ID (per OpenTelemetry):

```
"trace_id": "abc...",
"span_id": "def..."
```

These propagate through to Brain's logs. Operators can join logs and traces by trace ID.

## 35. The "structured exception" handling

Errors include structured fields:

```json
{
  "ts": "...",
  "level": "error",
  "msg": "encode failed",
  "operation": "encode",
  "agent_id": "...",
  "error": {
    "code": "QuotaExceeded",
    "message": "Agent has reached its memory quota",
    "details": {
      "current_count": 1000000,
      "limit": 1000000
    }
  }
}
```

Code, message, and details are separate fields. Operators can alert on specific codes.

## 36. The "logger configuration" surface

Per-logger level overrides:

```toml
[logging.loggers]
"brain.network" = "debug"      # See network details
"brain.hnsw" = "warn"          # Reduce HNSW noise
```

For focused debugging.

## 37. The log retention default

Default retention (file rotation):

- Daily rotation.
- Keep 7 days.
- Compress after 1 day.

Configurable. For aggregated logs (Loki, etc.), retention is in the aggregator.

## 38. The "graceful logging shutdown"

On shutdown, Brain flushes pending log buffers:

```
1. Stop accepting new operations.
2. Existing ones complete and emit logs.
3. Logger flushes to disk / network.
4. Process exits.
```

This avoids losing logs at shutdown.

---

## Tracing

How Brain integrates with distributed tracing systems.

## 39. The tracing standard

Brain uses [OpenTelemetry](https://opentelemetry.io/) — the industry standard. Traces are exported in OTLP format to any compliant backend (Jaeger, Tempo, Honeycomb, Datadog, etc.).

## 40. The span hierarchy

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

## 41. The instrumented operations

Brain creates spans for:

- Each request.
- Major phases (planning, execution).
- Storage operations (arena, WAL, metadata).
- HNSW operations.
- Embedder calls.
- Cross-shard fan-outs.
- Background worker cycles.

This gives operators a complete picture of where time is spent.

## 42. Sampling

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

## 43. The "head sampling" vs "tail sampling"

Brain implements head-based sampling: the decision is made at request start.

For tail-based sampling (decide based on latency or errors after the fact), use a tracing collector that supports it (Tempo, Honeycomb).

## 44. The export

Traces are exported via OTLP to a configured collector:

```toml
[tracing.export]
endpoint = "http://otel-collector:4317"
protocol = "grpc"             # or "http"
batch_max_size = 512
batch_timeout_ms = 5000
```

The collector handles forwarding to the backend (Jaeger, Tempo, etc.).

## 45. Span attributes

Common attributes:

- `brain.shard`: shard UUID.
- `brain.operation`: operation name.
- `brain.agent_id`: agent.
- `brain.request_id`: request ID.
- `brain.duration_ms`: duration.
- `brain.status`: success/error.
- `brain.error_code`: if error.

These follow OpenTelemetry semantic conventions where applicable.

## 46. The propagation

Trace context propagates from the client to Brain:

```
Client request:
  Headers / metadata: traceparent: 00-<trace_id>-<span_id>-01

Brain:
  Reads traceparent.
  Creates child span with the parent's trace_id and span_id.
```

Standard W3C traceparent format. Most tracing libraries support it.

## 47. The cross-shard propagation (future)

In a future clustered release, a cross-node call propagates trace context:

```
Node A's span (parent)
  Cross-node call carries trace context
    Node B's span (child, on different node)
```

The trace shows the call across nodes — useful for diagnosing latency in distributed setups.

## 48. The sampling propagation

Sampling decisions propagate. If the client decided to sample (or not), Brain respects it.

This avoids the "client samples, Brain doesn't" inconsistency.

## 49. Span events

Within a span, point-in-time events:

```rust
span.add_event("hnsw.search.start", attrs);
span.add_event("hnsw.search.end", attrs);
```

Events are like logs but tied to a span. Used for fine-grained timing within a span.

## 50. The performance overhead

Tracing has overhead:

- Span creation: ~1 µs.
- Attribute setting: ~100 ns per attribute.
- Export: batched, async, ~1% CPU at modest sample rates.

For 1% sampling: < 0.1% overhead. For 100%: 1-2% overhead.

## 51. The "trace not exported" path

If the export pipeline is down or backed up:

- Brain buffers spans (default 10K).
- If buffer fills: oldest spans are dropped, with metric.
- Operations continue normally.

```
brain_tracing_spans_dropped_total
brain_tracing_export_errors_total
```

## 52. The "high-cardinality" warning

Some attributes can have very high cardinality:

- Memory IDs.
- Request IDs.

These are useful for one-off investigation but explode index size in tracing backends.

Brain's defaults include some high-cardinality attributes (memory_id, request_id) because they enable powerful debugging. For deployments where cardinality is a concern, these can be excluded:

```toml
[tracing.attributes]
exclude = ["brain.memory_id", "brain.request_id"]
```

## 53. The async runtime tracing

Glommio's async tasks each have their own context. Brain ensures spans are properly attached to tasks.

Specific concern: spans don't accidentally cross task boundaries. The Rust `tracing` crate handles this if used correctly.

## 54. The "trace ID in logs"

Logs include the trace ID:

```json
{"ts":"...","level":"info","trace_id":"abc...","span_id":"def..."}
```

Operators can pivot from a trace to logs (in Tempo / Loki / etc.) by trace ID. This is the "logs in context" pattern.

## 55. The "no trace" fallback

If tracing isn't configured:

- Brain runs normally.
- No spans are created or exported.
- Performance is unchanged.

Tracing is opt-in. Brain doesn't require a tracing backend.

## 56. The "useful traces" examples

Examples of what traces help diagnose:

- "Why is this RECALL slow?" → trace shows time in embedder vs HNSW vs metadata fetch.
- "Where's the bottleneck?" → trace shows the sequential dependencies.
- "Did the client retry?" → trace shows multiple spans on the same request.

Traces complement metrics (which show aggregates) by showing individual requests.

---

*Continue to [`02_dashboards.md`](02_dashboards.md) for dashboards.*
