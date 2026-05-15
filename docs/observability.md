# Brain — Observability operator guide

What `brain-server` emits, how to scrape / collect it, and how to
wire dashboards and alerts. Covers the Phase 12 deliverables:

- §14/01 — Prometheus metrics taxonomy.
- §14/02 — JSON-structured logs.
- §14/03 — OpenTelemetry tracing.
- §14/04 — Reference Grafana dashboards (in `dashboards/`).
- §14/05 — Prometheus alert rules (in `alerts/`).

If you're an operator standing up a brain-server for the first time
and want metrics flowing in 10 minutes, follow [§1 quick-start](#1-quick-start).

---

## 1. Quick-start

```bash
# 1. Run brain-server with default config (metrics on :9091).
brain-server --config config/dev.toml

# 2. Verify metrics are exposed.
curl -s http://127.0.0.1:9091/metrics | head -20

# 3. Point Prometheus at the metrics port.
#    Minimal prometheus.yml:
scrape_configs:
  - job_name: brain
    static_configs:
      - targets: ["127.0.0.1:9091"]

# 4. Import the reference dashboards into Grafana 9+.
#    Grafana UI → Dashboards → Import → upload each
#    dashboards/*.json. Set the Prometheus datasource UID prompt
#    to your Prometheus instance.

# 5. Load the alert rules into Prometheus.
prometheus --config.file=prometheus.yml \
           --web.enable-lifecycle \
           ... \
  && curl -X POST http://localhost:9090/-/reload
```

For non-trivial deployments, see [§5 production wiring](#5-production-wiring).

---

## 2. Metrics

`brain-server` exposes Prometheus metrics on
`cfg.server.metrics_addr` (default `127.0.0.1:9091`). The body is
text/plain version 0.0.4 — Prometheus-compatible.

### Families emitted today (Phase 12)

| Family | Type | Labels | Source |
|---|---|---|---|
| `brain_build_info` | gauge=1 | `version`, `git_commit` | spec §14/01 §17 |
| `brain_config_info` | gauge=1 | `shard_count`, `arena_capacity_bytes`, `hnsw_m`, `embedder_model` | spec §14/01 §17 |
| `brain_up` | gauge | — | spec §14/01 §16 |
| `brain_shards_total` | gauge | — | spec §14/01 |
| `brain_connections_active` | gauge | — | spec §14/01 §9 |
| `brain_connections_total` | counter | — | spec §14/01 §9 |
| `brain_request_total` | counter | `op`, `status` | spec §14/01 §3 |
| `brain_request_active` | gauge | `op` | spec §14/01 §3 |
| `brain_request_duration_ms` | histogram | `op` | spec §14/01 §3, §12 |
| `brain_worker_cycles_total` | counter | `shard`, `worker` | spec §14/01 §8 |
| `brain_worker_processed_total` | counter | `shard`, `worker` | spec §14/01 §8 |
| `brain_worker_errors_total` | counter | `shard`, `worker` | spec §14/01 §8 |
| `brain_worker_last_run_unixtime` | gauge | `shard`, `worker` | spec §14/01 §8 |
| `process_cpu_seconds_total` | counter | — | spec §14/01 §10 |
| `process_memory_resident_bytes` | gauge | — | spec §14/01 §10 |
| `process_memory_virtual_bytes` | gauge | — | spec §14/01 §10 |
| `process_open_fds` | gauge | — | spec §14/01 §10 |
| `process_uptime_seconds` | counter | — | — |
| `process_start_time_seconds` | gauge | — | — |

### Deferred families

These are listed in spec §14/01 but require primitives that haven't
landed yet. The taxonomy reserves their names; the runtime emits
them as those primitives ship. Trackers live in
`crates/brain-server/src/metrics/mod.rs`:

- **Storage** (`phase-12/storage-stat-api`): `brain_arena_used_bytes`,
  `_capacity_bytes`, `_slots_used`, `_slots_free`,
  `brain_wal_size_bytes`, `brain_wal_segments`,
  `brain_metadata_size_bytes`.
- **HNSW** (`phase-12/hnsw-sampling`): `brain_hnsw_node_count`,
  `_tombstone_count`, `_tombstone_ratio`, `_search_visits`,
  `_recall_estimate`, `_rebuild_*`.
- **Embedder** (`phase-12/embedder-instrumentation`):
  `brain_embedder_calls_total`, `_cache_hits_total`,
  `_cache_misses_total`, `_duration_ms`, `_queue_depth`,
  `_workers_active`.
- **Memory** (per-kind counts): `brain_memory_count`,
  `_count_tombstoned`, `_kind{kind=}`.
- **Connection extended** (`phase-12/connection-extended`):
  `brain_connections_closed_total{reason}`, `brain_streams_active`,
  `brain_frame_send_total`, `_recv_total`, `_size_bytes`.
- **Glommio executor** (`phase-12/glommio-reactor-metrics`):
  `brain_executor_latency_ms`, `_tasks_active`.

Panels referencing these in the dashboards render "No data" until
they ship. That is intentional — the dashboards represent the spec
contract.

### Histogram buckets

`brain_request_duration_ms` uses the spec §14/01 §12 default bucket
set: `1, 2.5, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000`
ms plus the `+Inf` overflow.

### Cardinality

Per spec §14/01 §13 there are no high-cardinality labels (no
`agent_id`, no `memory_id`). The largest cardinality is
`shard × worker` (~16 × 12 = 192 series for the worker family).

---

## 3. Logs

`brain-server` writes one structured line per event. Format is
selected via `[logging] format = "..."`:

- `compact` — single-line `<ts> <LEVEL> <target>: <message>`.
  Default. Readable in a terminal.
- `json` — newline-delimited JSON per spec §14/02 §1. Production
  default; ingestible by Loki / Elastic / Splunk.

### Filter precedence

`BRAIN_LOG` > `RUST_LOG` > `[logging] level`. The env vars accept
the standard `tracing-subscriber` `EnvFilter` syntax:

```bash
BRAIN_LOG=info,brain_server::network=debug brain-server ...
```

### JSON field mapping

`tracing-subscriber`'s JSON layer emits these top-level keys; spec
§14/02 §4 uses slightly different names. Use Loki / Elastic
field-rename pipelines if you want the spec names:

| `tracing-subscriber` | Spec §14/02 §4 | Notes |
|---|---|---|
| `timestamp` | `ts` | ISO 8601, millisecond precision |
| `level` | `level` | INFO / WARN / ERROR / etc. |
| `target` | `logger` | Dotted Rust module path |
| `fields.message` | `msg` | Event message |
| `fields.<name>` | `<name>` | Structured fields from `tracing::info!(field = ...)` |

### Examples

```json
{"timestamp":"2026-05-15T12:00:00.123Z","level":"INFO","fields":{"message":"admin server listening","addr":"127.0.0.1:9091"},"target":"brain_server::admin"}
```

```json
{"timestamp":"2026-05-15T12:00:08.314Z","level":"INFO","fields":{"message":"close","stream_id":42,"target_shard":0,"op":"encode"},"target":"brain_server::network::connection","span":{"op":"encode","stream_id":42,"target_shard":0,"name":"brain.request"}}
```

The second example shows a span context auto-attached by the
12.3 OTel instrumentation — every request gets a `brain.request`
span; child operations inherit it.

---

## 4. Tracing (OpenTelemetry)

Opt-in. Enable via `[tracing]`:

```toml
[tracing]
enabled = true
sampler = "ratio"        # always_on | always_off | ratio | parent_based
sample_ratio = 0.01      # 1 % of requests
endpoint = "http://otel-collector:4318/v1/traces"
service_name = "brain-server"
```

When disabled (default), the substrate runs unchanged — spec §14/03
§17 "no-trace fallback".

### Wire format

OTLP/HTTP (protobuf over HTTP). Send to any OTel-compliant
collector — Jaeger, Tempo, Honeycomb, Datadog, etc.

To use OTLP/gRPC instead, the dep matrix needs `tonic-client` in
`opentelemetry-otlp`. That's a future trade-off; HTTP is the
lighter footprint for v1.

### Span model

Each request emits a `brain.request` span at the connection layer
with attributes `op`, `stream_id`, `target_shard`. Shard-side
handlers in `brain-ops` instrument with `tracing::info_span!` calls;
those become child spans in the same trace.

### Trace context propagation

Server-side spans only in v1. The wire protocol does not currently
carry a `traceparent` header (spec §03 amendment required per spec
§14/03 §8). Client→server propagation tracker:
`phase-13/wire-traceparent`.

---

## 5. Production wiring

### Prometheus scrape config

```yaml
scrape_configs:
  - job_name: brain
    metrics_path: /metrics
    static_configs:
      - targets: ["brain-server-0:9091", "brain-server-1:9091"]
        labels:
          env: prod
    relabel_configs:
      # Rename instance to a friendly name.
      - source_labels: [__address__]
        regex: 'brain-server-([0-9]+).*'
        replacement: 'shard-${1}'
        target_label: instance
```

For Prometheus Operator / Kubernetes:

```yaml
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: brain-server
spec:
  selector:
    matchLabels:
      app: brain-server
  endpoints:
    - port: metrics
      interval: 30s
```

### Alert rules

Drop `alerts/brain-rules.yml` into Prometheus' `rule_files:` list,
or wrap as a `PrometheusRule` CRD. Reload Prometheus:

```bash
curl -X POST http://prometheus:9090/-/reload
```

### Grafana dashboards

Import each file in `dashboards/` via Grafana UI or the
file-provisioning sidecar. The dashboards expect a Prometheus
datasource; set the prompt to your instance.

### OTel collector

Minimal `otelcol.yaml`:

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318

exporters:
  otlphttp/tempo:
    endpoint: http://tempo:4318
  logging:
    loglevel: debug

service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [otlphttp/tempo, logging]
```

Point `[tracing] endpoint` at this collector. The collector forwards
to your tracing backend.

---

## 6. SLOs and tuning

The shipped alert thresholds are reasonable starting points per spec
§14/05 §11. Tune for your SLOs:

- p99 latency budget — spec §16/02 sets per-op latency targets; the
  default `BrainHighLatency` rule fires at p99 > 100 ms. Reduce for
  stricter SLOs.
- Error-rate threshold — `BrainHighErrorRate` fires at 10 % over
  5 m. For a 99.9 % availability SLO with a 30-day window, you
  typically want 1 % / 10 m as the burn-rate alert.
- HNSW tombstone — spec §14/05 §5 sets 30 %; reduce if your workload
  has heavy churn.

For SLO-based alerting (multi-window burn rates), the metrics support
it but the substrate doesn't ship the rules by default per spec
§14/05 §17 — too deployment-specific.

---

## 7. Troubleshooting

### `/metrics` returns 200 but body is empty

The exposition path requires a non-empty `AdminState`. If you see
just `# HELP` headers with no metric lines, check that
`brain-server` finished startup (look for the
`admin server listening` log event).

### Spans don't appear in the OTel backend

Check in order:
1. `[tracing] enabled = true` in config.
2. `[tracing] endpoint` reaches the collector — `curl -v $endpoint`.
3. Sampler isn't `always_off` (which short-circuits).
4. The collector accepts OTLP/HTTP (not just OTLP/gRPC).

### Logs aren't JSON

`[logging] format` must be `json` (case-insensitive). The
`compact` fallback applies on unrecognised values with a `warn`
event at install time.

### Dashboards show "No data" everywhere

Most likely: the dashboard references deferred metrics (HNSW,
storage, embedder). See [§2 deferred families](#deferred-families).

If even `brain_request_total` is missing: Prometheus is scraping
the wrong port. Verify with `curl <metrics_addr>/metrics | head`.

---

## 8. Pointers

- Spec: `spec/14_observability_ops/` — primary contract for every
  family / log field / alert / span.
- Roadmap: `ROADMAP.md` — Phase 12 / 13 / 14 split.
- Phase doc: `docs/phases/phase-12-observability.md` — what's
  shipped vs deferred.
- Runtime entry points:
  - `crates/brain-server/src/metrics/` — metric primitives,
    exposition.
  - `crates/brain-server/src/bootstrap/logging.rs` — JSON logger
    init.
  - `crates/brain-server/src/bootstrap/tracing.rs` — OTLP exporter
    init.
- Dashboards: `dashboards/*.json` (8 files).
- Alerts: `alerts/brain-rules.yml`.
