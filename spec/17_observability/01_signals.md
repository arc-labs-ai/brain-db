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

## 4. Server and shard metrics

```
brain_up                                      # 1 if accepting requests
brain_build_info{version=, commit=}
brain_config_info{shard_count=, arena_capacity_bytes=, hnsw_m=, embedder_model=}
brain_shards_total
```

`brain_up` is liveness only. `brain_build_info` and `brain_config_info` are
info-style gauges whose value is always `1`; the labels carry the payload, which
is what lets an operator join "this behaviour" to "this build and this config"
in a query.

## 5. Storage metrics

```
brain_arena_used_bytes{shard=}        # bytes backing allocated slots (occupied + tombstoned)
brain_arena_capacity_bytes{shard=}
brain_arena_slots_used{shard=}        # allocated (occupied + tombstoned)
brain_arena_slots_free{shard=}        # reclaimed, on the free list
brain_wal_size_bytes{shard=}
brain_wal_segments{shard=}
brain_metadata_size_bytes{shard=}
```

Note that `used` and `slots_used` both **include tombstoned** slots: they
measure what is allocated, not what is live. The difference between them and
the live memory count is the reclamation backlog, which is the thing worth
alerting on.

## 6. HNSW metrics

```
brain_hnsw_node_count{shard=}                 # active nodes
brain_hnsw_tombstone_count{shard=}            # tombstoned nodes
brain_hnsw_tombstone_ratio{shard=}            # tombstoned / (active + tombstoned), 0..1
```

The ratio is exposed as its own gauge rather than left to PromQL because it is
the alert threshold operators actually use, and computing it from two gauges
sampled at different instants produces spurious spikes.

## 7. Query and retrieval metrics

```
brain_query_total{shard=}                             # recalls served
brain_query_latency_ms{shard=}                        # end-to-end recall latency (histogram)
brain_query_outcome_total{shard=, outcome=}           # single | many | none
brain_query_rerank_invoked_total{shard=}              # recalls the rerank stage reordered
brain_query_fusion_k{shard=}                          # effective adaptive fusion-k, per served recall (histogram)

brain_retriever_invocations_total{shard=, retriever=} # semantic | lexical | graph
brain_retriever_candidates_total{shard=, retriever=}
brain_retriever_latency_ms{shard=, retriever=}        # per-lane wall-clock (histogram)
```

`brain_query_outcome_total` is the one to watch for retrieval quality: a rising
`none` share is the signal that reads are degrading, and it is visible without
any per-query inspection. `rerank_invoked_total` stays at zero on a default
deployment, since rerank ships disabled.

## 8. Extractor and derivation metrics

```
brain_extractor_drops_total{shard=}                      # encode-side enqueues dropped (channel full)
brain_extractor_cycle_duration_seconds{shard=}           # histogram
brain_extractor_tier_runs_total{shard=, tier=, outcome=} # per-tier outcome per memory
brain_extractor_items_written_total{shard=, kind=}       # typed-graph rows persisted, by item kind
brain_extractor_schema_filtered_total{shard=, reason=}   # dropped: predicate/relation_type not in an active schema
brain_extractor_apply_dropped_total{shard=, reason=}     # real items the apply pass could not persist
brain_extractor_predicate_consolidated_total{shard=}     # open-vocab surface forms folded onto a near neighbour
brain_extractor_resolver_outcome_total{shard=, tier=}    # which resolver tier satisfied each mention
brain_extractor_llm_micro_usd_spent_total{shard=}        # LLM-tier spend, in 1e-6 USD
```

Two of these are easy to confuse and are deliberately separate:
`schema_filtered_total` counts candidates dropped **by policy** (their type is
not declared), which is normal and expected; `apply_dropped_total` counts real
items the apply pass **could not persist**, which is not. A nonzero rate on the
second deserves investigation.

`llm_micro_usd_spent_total` is the cost surface. It is a counter in dollar-micro
units, so a spend rate is `rate(...)/1e6` dollars per second.

```
brain_auto_edge_edges_written_total{shard=}              # logical SimilarTo edges persisted
brain_auto_edge_neighbours_found_per_cycle{shard=}       # above-threshold neighbours per cycle (histogram)
brain_auto_edge_cycle_duration_seconds{shard=}
brain_auto_edge_drops_total{shard=}

brain_temporal_edge_edges_written_total{shard=}          # logical FollowedBy edges persisted
brain_temporal_edge_gap_seconds{shard=}                  # predecessor→memory gap distribution (histogram)
brain_temporal_edge_skipped_total{shard=, reason=}       # enqueues that produced no edge
brain_temporal_edge_cycle_duration_seconds{shard=}
brain_temporal_edge_drops_total{shard=}

brain_causal_edge_edges_written_total{shard=}            # logical Caused edges persisted
brain_causal_edge_skipped_total{shard=, reason=}
brain_causal_edge_predicate_whitelist_resolved{shard=}   # causal predicates resolved against the active schema
brain_causal_edge_cycle_duration_seconds{shard=}
brain_causal_edge_drops_total{shard=}

brain_statement_embed_cycles_total{shard=}
brain_statement_embed_rows_embedded_total{shard=}        # statements inserted into the statement HNSW
brain_statement_embed_rows_skipped_total{shard=}         # dropped without embedding (tombstoned, superseded)
brain_statement_embed_errors_total{shard=}               # embedder batch failures
brain_statement_embed_batch_duration_seconds{shard=}
```

Each derivation worker exposes the same four shapes — written, skipped, cycle
duration, and **drops**. `drops_total` is the one that matters structurally: it
counts enqueues lost because the worker's channel was full, which means the
write succeeded and its derivation silently did not happen. It should be zero.

`*_skipped_total` is the benign counterpart — an enqueue that legitimately
produced no edge — and carries a `reason` label so the two never have to be told
apart by guesswork.

## 9. Worker metrics

Every worker, whatever it does, exposes the same set:

```
brain_worker_cycles_total{shard=, worker=}
brain_worker_processed_total{shard=, worker=}
brain_worker_errors_total{shard=, worker=}
brain_worker_panics_total{shard=, worker=}      # a subset of errors_total
brain_worker_cycle_duration_ms{shard=, worker=} # most recent successful cycle
brain_worker_last_run_unixtime{shard=, worker=} # last attempted cycle: success, error, or caught panic
brain_worker_pending_work{shard=, worker=}      # last-observed estimate of outstanding work
```

`brain_worker_panics_total` is broken out of `errors_total` rather than folded
into it because the two call for different responses: an error is a cycle that
failed and will retry, a panic is a bug. **Nonzero is worth alerting on.**

`last_run_unixtime` advances on a *failed* cycle too. A worker that is erroring
every cycle still looks alive on this gauge, which is intentional — the gauge
answers "is the scheduler running it", and `errors_total` answers "is it
working". A stalled worker is the one where this timestamp stops moving.

## 9a. Connection and frame metrics

```
brain_connections_active                        # currently in flight
brain_connections_total                         # lifetime accepts
brain_connections_closed_total{reason=}
brain_connections_rejected_total{reason=}       # shed at accept by the admission gate (global or per-IP cap)
brain_frame_recv_total
brain_frame_send_total
brain_frame_size_bytes{direction=}              # recv | send (histogram)
```

`connections_rejected_total` counts connections refused *before* they became
connections, which is why it is separate from `closed_total` — a shed connection
never opened and would otherwise be invisible.

## 9b. Tracing metrics

```
brain_tracing_spans_dropped_total               # export buffer was full
brain_tracing_export_errors_total               # OTLP export failed or timed out
```

The observability pipeline reports its own failures. A dropped span is a hole in
the trace that nothing else would reveal.

## 10. Resource metrics

Standard Linux process metrics, read from `/proc/self`:

```
process_cpu_seconds_total          # from /proc/self/stat
process_memory_resident_bytes      # VmRSS
process_memory_virtual_bytes       # VmSize
process_open_fds                   # entries in /proc/self/fd
```

These are read on scrape rather than tracked continuously, which is why they are
standard-named and unprefixed: they describe the process, not Brain.

## 11. The "rate" derivations

Most metrics are counters. Rates (req/s, errors/s) are derived in PromQL:

```
rate(brain_request_total{op="encode",status="success"}[5m])
sum(rate(brain_request_total[5m])) by (op)
```

The dashboards (next file) show these.

## 12. Histogram buckets

Default histogram bucket boundaries, in milliseconds:

```
1, 2.5, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, +Inf
```

Thirteen bounds plus the `+Inf` overflow bucket, so fourteen counts per
histogram. The bounds are **fixed in the binary**, not configurable: histogram
bounds that differ between deployments cannot be aggregated across them, and the
whole point of exposing a histogram rather than a quantile is that it aggregates.

Observations are stored scaled by 1000 so a sub-millisecond measurement does not
quantise to zero before it reaches its bucket.

## 13. Cardinality

Labels with high cardinality (per-space, per-session) create explosion. Brain avoids them in metrics:

- ✓ Shard label: low cardinality (~16 values).
- ✓ Operation label: low cardinality (~10 values).
- ✓ Retriever / tier / worker / reason labels: low cardinality, fixed sets.
- ✗ `space_id`: high cardinality (up to 10⁶ per namespace); **never** a metric label.
- ✗ `session_id`: unbounded, client-chosen; never a metric label.
- ✗ `namespace`: bounded but tenant-identifying; not a metric label.

Per-space observability uses logs and the audit trail, not metrics. This is the
one rule in this file that is not a preference: a `space_id` label on a counter
multiplies every series by the tenant count, and on a shard serving a million
spaces that is a metrics-system outage, not a slow dashboard.

## 14. Sampling

Some metrics are sampled rather than every-event:

- HNSW search visits: every Nth search.
- Recall quality estimate: hourly sample.

This bounds metric volume.

## 15. Labels: shard ID

The `shard` label uses the UUID:

```
brain_arena_slots_used{shard="abc123-uuid"} 1234
```

For dashboards, operators may add a friendly name via metric relabeling:

```
brain_arena_slots_used{shard="abc123-uuid", shard_name="prod-shard-0"} 1234
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
  "space_id": "agent-001",
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
- `space_id`: agent (when applicable).
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
  "space_id": "...",
  "session": "...",
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
{"ts":"...","level":"info","operation":"encode","shard":"<uuid>","space_id":"...","request_id":"...","duration_ms":8,"status":"success","msg":"encode completed"}
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
"msg": "Encoded memory abc-123 in session xyz at 12:34:56 with salience 0.8"
```

Good:
```
"msg": "encode completed",
"memory_id": "abc-123",
"session_id": "xyz",
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
  "space_id": "...",
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
- `brain.space_id`: agent.
- `brain.request_id`: request ID.
- `brain.duration_ms`: duration.
- `brain.status`: success/error.
- `brain.error_code`: if error.

These follow OpenTelemetry semantic conventions where applicable.

## 46. The propagation

Trace session propagates from the client to Brain:

```
Client request:
  Headers / metadata: traceparent: 00-<trace_id>-<span_id>-01

Brain:
  Reads traceparent.
  Creates child span with the parent's trace_id and span_id.
```

Standard W3C traceparent format. Most tracing libraries support it.

## 47. The cross-shard propagation (future)

In a future clustered release, a cross-node call propagates trace session:

```
Node A's span (parent)
  Cross-node call carries trace session
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

Glommio's async tasks each have their own session. Brain ensures spans are properly attached to tasks.

Specific concern: spans don't accidentally cross task boundaries. The Rust `tracing` crate handles this if used correctly.

## 54. The "trace ID in logs"

Logs include the trace ID:

```json
{"ts":"...","level":"info","trace_id":"abc...","span_id":"def..."}
```

Operators can pivot from a trace to logs (in Tempo / Loki / etc.) by trace ID. This is the "logs in session" pattern.

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
