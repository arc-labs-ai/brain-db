# 17.02 Reference Dashboards

Reference Grafana dashboards Brain ships with.

## 1. The dashboards

Brain ships eight Grafana dashboard definitions, as JSON, in
`config/monitoring/dashboards/`:

- **Overview**: high-level health.
- **Per-shard**: detailed view of a single shard.
- **Storage**: arena, WAL, metadata.
- **HNSW**: index health.
- **Workers**: background worker status.
- **Network**: connections and protocol.
- **Errors**: error rates and breakdowns.
- **Capacity**: utilization and growth.

They use the metrics catalogued in [`01_signals.md`](01_signals.md). Every
panel below names the series it reads, because a panel is a claim about a
metric name and a label set: a query naming a label that does not exist returns
empty and renders as a blank panel, with no error anywhere to notice. The
three label facts that catch most panels are that the **request family carries
no `shard` label**, the **connection and frame families carry none either**, and
the **process families carry none** — per-shard breakdowns are only available
on the families recorded inside a shard.

## 2. Overview dashboard

Top-level "is everything OK?" view.

Panels:

- **Up**: `brain_up`, plus Prometheus's own `up` for the scrape result. Neither
  carries a `shard` label — `up` is per scrape target.
- **Build / version**: `brain_build_info`. Value is always 1, so read it as a
  table showing the `version` and `git_commit` labels, not as a stat.
- **Shards**: `brain_shards_total`.
- **Active connections**: `sum(brain_connections_active)`.
- **In-flight requests**: `sum(brain_request_active)`.
- **Request rate by op**: `sum by (op) (rate(brain_request_total[5m]))`.
- **Latency p50 / p99**:
  `histogram_quantile(0.99, sum by (le) (rate(brain_request_duration_ms_bucket[5m])))`.
  Add `op` to the `by` clause for a per-operation breakdown.
- **Error rate**: `sum(rate(brain_request_total{status="error"}[5m]))`. The
  status values are `success`, `error` and `timeout` exactly — there is no
  `error_<code>` form to match with a regex.
- **Resident memory**: `process_memory_resident_bytes`.

There is **no memory-count gauge** to put on this dashboard. The nearest
emitted stand-in is `brain_arena_slots_used`, which counts allocated slots
including tombstoned ones; a live count comes from `ADMIN_STATS`, not from
metrics.

This is the "first dashboard to look at" when something feels wrong.

## 3. Per-shard dashboard

Detailed view of one shard. Variable: `shard` (selectable in dropdown), whose
values are shard indices counting from zero — not UUIDs.

What can genuinely be broken down per shard is narrower than it looks, because
the `shard` label only exists on families recorded inside a shard:

- Arena, WAL and metadata footprint: `brain_arena_*`, `brain_wal_size_bytes`,
  `brain_wal_segments`, `brain_metadata_size_bytes`.
- HNSW health: `brain_hnsw_node_count`, `brain_hnsw_tombstone_count`,
  `brain_hnsw_tombstone_ratio`.
- Read path: `brain_query_total`, `brain_query_latency_ms`,
  `brain_query_outcome_total`, and the `brain_retriever_*` lanes.
- Write path: `brain_writer_submit_total`, `brain_writer_stage_duration_ms`,
  `brain_writer_apply_errors_total`.
- Workers: the `brain_worker_*` family, labelled `(shard, worker)`.
- Derivation backlog: `brain_extraction_queue_depth`,
  `brain_extraction_queue_oldest_age_seconds`.

Request rate, latency and error rate **cannot** be split by shard: those are
recorded at the connection layer before a request reaches one. Use
`brain_query_*` and `brain_writer_*` for per-shard read and write volume
instead, which is also the more useful split — they attribute latency to a
retriever lane or a write stage rather than to an opcode.

## 4. Storage dashboard

Storage health across all shards.

Panels:

- **Arena utilization**:
  `brain_arena_used_bytes / clamp_min(brain_arena_capacity_bytes, 1)`. The
  `clamp_min` keeps the ratio defined if the capacity gauge is momentarily
  absent.
- **Arena growth rate**: `delta(brain_arena_used_bytes[1h])`. These are gauges,
  so `delta` — not `rate` or `increase`, which assume a counter and
  extrapolate.
- **WAL size and segment count per shard**: `brain_wal_size_bytes`,
  `brain_wal_segments`.
- **Metadata size**: `brain_metadata_size_bytes`.
- **Free list size**: `brain_arena_slots_free`, against
  `brain_arena_slots_used`.
- **Reclamation activity**:
  `rate(brain_worker_processed_total{worker="slot_reclamation"}[5m])` — the
  worker counter, since there is no reclamation-rate metric of its own.

For capacity planning and storage health. Note that `arena_used_bytes` and
`arena_slots_used` both **include tombstoned** slots: the gap between them and
the live count is the reclamation backlog.

## 5. HNSW dashboard

Index health.

Panels:

- **Node count**, per shard and total: `brain_hnsw_node_count`.
- **Tombstone count**: `brain_hnsw_tombstone_count`.
- **Tombstone ratio** (act above 30%): `brain_hnsw_tombstone_ratio`. Exposed as
  its own gauge rather than divided in PromQL, because two gauges sampled at
  different instants produce spurious spikes on the ratio.
- **Growth rates**: `delta(brain_hnsw_node_count[1h])` and the same on the
  tombstone count. `delta` because both are gauges — `increase()` treats a
  decline as a counter reset and reports nonsense when maintenance compacts
  the graph.
- **Maintenance cadence**:
  `brain_worker_cycles_total{worker="hnsw_maintenance"}` and
  `brain_worker_last_run_unixtime{worker="hnsw_maintenance"}`.

Four panels an HNSW dashboard wants cannot be built yet: recall estimate,
search latency, search visits per query, and rebuild progress. All four belong
to families declared deferred (`crates/brain-server/src/metrics/mod.rs:31-40`)
and absent from the `/metrics` body, because each needs sampling
instrumentation inside the index. Until they land, **tombstone ratio is the
index-quality signal** — tombstones are the mechanism behind most recall decay
— and the maintenance worker's cadence is the rebuild signal.

## 6. Workers dashboard

Background workers' status.

Panels, all labelled `(shard, worker)`:

- **Last run age**: `time() - max by (worker) (brain_worker_last_run_unixtime)`.
- **Cycle duration**: `brain_worker_cycle_duration_ms` — a gauge holding the
  most recent successful cycle, not a histogram, so no `histogram_quantile` on
  it.
- **Pending work**: `brain_worker_pending_work`.
- **Cycle rate**: `sum by (worker) (rate(brain_worker_cycles_total[5m]))`.
- **Error rate**: `sum by (worker) (rate(brain_worker_errors_total[5m]))`, with
  `brain_worker_panics_total` beside it — a panic is a bug rather than a
  retryable failure and deserves its own panel.
- **Derivation output**: the `*_edges_written_total` counters for the auto,
  temporal and causal edge workers, and
  `brain_extractor_items_written_total`.

The `worker` label values are full snake_case names —
`hnsw_maintenance`, `statement_embed`, `wal_retention` — and a worker disabled
in a deployment contributes no rows at all, so a panel filtered to it renders
empty rather than flat-zero.

Watch the last-run panel with one thing in mind: the timestamp advances on a
*failed* cycle too, so a worker erroring every cycle still looks alive here.
Read it against the error rate.

## 7. Network dashboard

Connection and protocol metrics.

Panels:

- **Active connections**: `brain_connections_active`.
- **Accept rate**: `rate(brain_connections_total[5m])`.
- **Close rate by reason**:
  `sum by (reason) (rate(brain_connections_closed_total[5m]))`. `reason` takes
  `bye`, `protocol_error`, `timeout`, `eof` or `fatal`; `bye` is orderly
  client turnover and the other four are not, which is the whole value of the
  breakdown.
- **Rejected rate**: `rate(brain_connections_rejected_total[5m])` — connections
  shed at accept by the admission gate, labelless because nothing has been
  negotiated yet that could classify them.
- **Frame throughput**: `rate(brain_frame_recv_total[5m])` and the send
  counterpart.
- **Frame size distribution**:
  `histogram_quantile(0.99, sum by (le, direction) (rate(brain_frame_size_bytes_bucket[5m])))`.
  `direction` is `send` or `recv`. This histogram sums raw bytes, so its `_sum`
  is the true byte total and is worth a throughput panel of its own.

There is no active-stream gauge and no connection-lifetime histogram. None of
this family carries a `shard` label — connections terminate at the connection
layer, which fronts every shard.

## 8. Errors dashboard

Error breakdown.

Panels:

- **Request errors by op**:
  `sum by (op) (rate(brain_request_total{status="error"}[5m]))`, and the same
  for `status="timeout"`, which is a distinct terminal status rather than a
  kind of error.
- **Write-path errors**:
  `sum by (phase, error) (rate(brain_writer_apply_errors_total[5m]))`. This is
  the family that says *which* write failed and why.
- **Worker errors**: `sum by (worker) (rate(brain_worker_errors_total[5m]))`.
- **Derivation signal loss**:
  `rate(brain_extractor_apply_dropped_total[5m])` by `reason` — real items the
  apply pass could not persist. Keep it apart from
  `brain_extractor_schema_filtered_total`, which counts candidates dropped
  because their type is not declared and is normal.
- **Channel drops**: the `*_drops_total` counters across the extractor and the
  three edge workers. These mean a write succeeded and its derivation did not.
- **Recent error details**: a logs panel, which needs Loki or Elastic.

**Errors by code is not available from metrics, and errors by shard is not
either.** `status` has three values and no code dimension, and the request
family has no `shard` label. A per-code breakdown comes from the logs, where
`error_code` is a field. `brain_writer_apply_errors_total{error=}` is the one
emitted error-kind axis, and it covers the write path only.

## 9. Capacity dashboard

For capacity planning.

Panels:

- **Arena growth with forecast**:
  `predict_linear(brain_arena_used_bytes[6h], 86400)` beside the live gauge.
- **HNSW growth with forecast**: the same on `brain_hnsw_node_count`.
- **CPU utilisation**: `rate(process_cpu_seconds_total[5m])`.
- **RAM**: `process_memory_resident_bytes`, with
  `process_memory_virtual_bytes` beside it. Mind the word order — resource
  first, then quality; the transposed spelling returns nothing.
- **Open file descriptors**: `process_open_fds`.
- **LLM spend**: `sum(brain_extractor_llm_micro_usd_spent_total)`, a counter in
  dollar-micro-units, so a spend rate is `rate(...)/1e6` dollars per second.
- **Derivation admission headroom**:
  `brain_derivation_admission_available_bytes` against
  `brain_derivation_admission_budget_bytes`. Sustained zero availability means
  derivation is waiting on memory rather than on the provider.
- **Disk utilisation**: from node_exporter, not from Brain.

The forecast panels use `predict_linear`, which fits a line to a gauge and
extrapolates — correct for these footprint gauges, and not interchangeable with
`rate()`. There is no memory-count series to forecast; `brain_arena_slots_used`
is the capacity-relevant stand-in, since a tombstoned slot occupies arena space
until reclamation runs.

## 10. The dashboards are starting points

The provided dashboards aren't the only views. Operators customize:

- Add panels for their specific concerns.
- Filter to specific deployments.
- Combine with their own metrics (application-level).

Brain provides defaults; operators tailor.

## 11. The Grafana version

The JSON carries `schemaVersion: 39`, which Grafana v10 and later read
directly; older Grafana may need adjustment on import.

The files live in the source tree at `config/monitoring/dashboards/`, one JSON
per dashboard named for it. They can be:

- Imported manually through the Grafana UI.
- Provisioned through Grafana's file-based provisioning.
- Loaded by a sidecar container, in Kubernetes.

## 12. Dashboard variables

Common variables across dashboards:

- `$shard`: which shard. Populate it from a family that actually carries the
  label — `label_values(brain_hnsw_node_count, shard)` works;
  `label_values(up, shard)` returns nothing, because `up` is Prometheus's own
  scrape series and carries `job` and `instance` only.
- `$op`: which operation, from `label_values(brain_request_total, op)` — ten
  values.
- `$time_range`: time window, Grafana's built-in.

These let one dashboard serve multiple shards and operations without
duplication. A variable's name should match the label it binds, since the query
templates it literally: `{shard="$shard"}` against a family whose label is
`shard` matches, and anything else silently matches nothing.

## 13. The "alerting" panels

Some panels have associated alert rules (next file). Grafana shows alert state on the panel:

```
[Latency p99 panel]
  Current: 23ms
  Alert: OK (threshold: 50ms)
```

Visual feedback when something's off.

## 14. The "annotations"

Major events can be annotated on the dashboards:

- Deployments (from the CI system).
- Configuration changes.
- Manual interventions.

Annotations help correlate metric changes with actions:

```
[Latency dashboard]
  ↓ deployment v1.5 (annotation)
  ←latency rose here
```

## 15. The dashboards in CI

The dashboard JSON is checked into source control. Changes go through code
review.

CI verifies that each expected dashboard parses, carries a `title`, `uid`,
`schemaVersion` and `panels` array, and that every identifier in a panel
expression starts with `brain_` or `process_`.

That gate catches a mistyped prefix. It does **not** catch a plausible-looking
name that the server never emits, because it checks the prefix taxonomy rather
than the emitted set — so `process_resident_memory_bytes` and
`brain_schema_version` would both pass it while returning empty at scrape time.
The alert rules are held to the stronger check: that suite parses the emitted
names out of the exposition and rejects any rule naming a series that does not
exist. **Confirm a new panel's metric names against
`crates/brain-server/src/metrics/format.rs` by hand**, and check the labels
too — a wrong label name returns empty exactly like a wrong metric name, and no
test looks at labels at all.

## 16. The "default visualization"

Each metric type has a recommended visualization, and more importantly a
required query shape:

| Type | Panel | Query |
|---|---|---|
| Counter (`_total`) | time-series | `rate(...)` or `increase(...)` |
| Gauge | gauge, stat, or time-series | the bare series; `delta(...)` for change |
| Histogram | heatmap, or quantile time-series | `histogram_quantile` over `rate(..._bucket)` |
| Info gauge | table | the bare series, read for its labels |

Getting this wrong is the most common way a panel misleads rather than
breaking. `rate()` on a gauge reports garbage whenever the gauge falls;
`increase()` on a gauge reads a decline as a counter reset. A
`histogram_quantile` over raw buckets, without `rate()`, answers "since process
start" rather than "now" — and the `by` clause belongs to the inner
aggregation, `sum by (le) (rate(..._bucket[5m]))`, not after the quantile
function, where it will not parse.

Two emitted families look like histograms and are not:
`brain_worker_cycle_duration_ms` is a gauge holding the most recent successful
cycle, and `brain_causal_edge_predicate_whitelist_resolved` is a gauge counting
resolved predicates. Neither has buckets.

## 17. Reading a dashboard against an incident

A dashboard's value in a postmortem is the annotated timeline, not the panel
layout. Capture the panels that moved, with their time range, into the incident
record: a stat panel's current value is worthless a day later, while
`brain_extraction_queue_oldest_age_seconds` over the incident window is the
evidence for what the system was doing.

## 18. The deployment tooling

For deployments using the standard Prometheus + Grafana stack, Brain ships the
two artifacts that are specific to it and nothing beyond them:

- The dashboard JSON, in `config/monitoring/dashboards/`.
- The alert rules, in `config/monitoring/alerts/brain-rules.yml`.

Scrape configuration is a single target: the `[server] metrics_addr` listener,
on its own port, serving `/metrics` unauthenticated. Point Prometheus at it with
whatever mechanism the deployment already uses — a static target, a
ServiceMonitor, a scrape annotation. Brain provides no Helm chart, no operator
CRDs and no sidecar; a chart that shipped with Brain would encode one
deployment's choices into an infrastructure-agnostic substrate.

---

*Continue to [`03_alerts.md`](03_alerts.md) for alerts.*
