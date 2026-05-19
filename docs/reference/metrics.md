# Prometheus metrics

Catalogue of every metric Brain emits on `GET /metrics`. Format:
Prometheus text-exposition (OpenMetrics-compatible).

**Source:** `crates/brain-server/src/metrics/`.
**Spec:** §14/01.

The endpoint lives on `[server] metrics_addr` (default 9091).
A canonical scrape job is in
[`../../deploy/compose/prometheus.yml`](../../deploy/compose/prometheus.yml).

---

## Request lifecycle

### `brain_request_total` *(counter)*

Labels: `op`, `status`.

`op` ∈ `{encode, recall, plan, reason, forget, link, unlink, txn_begin, txn_commit, txn_abort}`.
`status` ∈ `{success, error, timeout}`.

> Total requests by operation and terminal status.

### `brain_request_duration_ms` *(histogram)*

Labels: `op`.
Buckets (ms): `1, 2.5, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, +Inf`.

> Request duration histogram by operation.

### `brain_request_active` *(gauge)*

Labels: `op`.

> Requests currently in flight by operation.

---

## Connections + frames

### `brain_connections_active` *(gauge)*

> Currently in-flight client connections.

### `brain_connections_total` *(counter)*

> Total accepted client connections since startup.

### `brain_connections_closed_total` *(counter)*

Labels: `reason`. `reason` is one of the enumerated close reasons
(`graceful`, `idle_timeout`, `peer_reset`, `protocol_error`,
`server_shutdown`, `unauthenticated`, etc. — bounded set, never
user-input).

> Connections closed by reason.

### `brain_frame_send_total` *(counter)*

> Total outbound frames since startup.

### `brain_frame_recv_total` *(counter)*

> Total inbound frames since startup.

### `brain_frame_size_bytes` *(histogram, raw bytes)*

Labels: `direction` ∈ `{send, recv}`.

> Per-frame wire size in bytes (header + payload), by direction.
> `_sum` is a true byte total (the histogram is in raw mode, not
> scaled).

---

## Workers

### `brain_worker_cycles_total` *(counter)*

Labels: `shard`, `worker`.

`worker` is one of: `decay`, `access_boost`, `consolidation`,
`hnsw_maintenance`, `idempotency_cleanup`, `slot_reclamation`,
`wal_retention`, `edge_scrub`, `counter_reconciliation`,
`statistics_update`, `embedder_cache_eviction`.

> Worker cycles completed.

### `brain_worker_processed_total` *(counter)*

Labels: `shard`, `worker`.

> Items processed by the worker.

### `brain_worker_errors_total` *(counter)*

Labels: `shard`, `worker`.

> Worker cycle errors.

### `brain_worker_last_run_unixtime` *(gauge)*

Labels: `shard`, `worker`.

> Unix-time of the worker's last cycle.
> `time() - brain_worker_last_run_unixtime` is the staleness gauge
> used by the worker-stuck alert.

---

## HNSW index

### `brain_hnsw_node_count` *(gauge)*

Labels: `shard`.

> Active HNSW node count.

### `brain_hnsw_tombstone_count` *(gauge)*

Labels: `shard`.

> Tombstoned HNSW node count.

### `brain_hnsw_tombstone_ratio` *(gauge)*

Labels: `shard`.

> `tombstones / (active + tombstones)`. Range `[0, 1]`.

**Deferred** (tracker: phase-12/hnsw-sampling — not in v1.0):

- `brain_hnsw_search_visits` (histogram, sampled per search)
- `brain_hnsw_recall_estimate` (gauge, hourly quality estimate)
- `brain_hnsw_rebuild_in_progress` (gauge, 0/1)
- `brain_hnsw_rebuild_progress_pct` (gauge)
- `brain_hnsw_rebuild_count_total` (counter)
- `brain_hnsw_rebuild_duration_sec` (histogram with quantiles)

---

## Process / runtime

Sourced from `/proc/self/{stat,status,fd}` — Linux-only.

### `process_cpu_seconds_total` *(counter)*

> Total process CPU time (user + system).

(Hardcoded HZ=100 — see tracker `phase-12/sysconf-clock-tick` if
running on a kernel with non-default clock tick.)

### `process_memory_resident_bytes` *(gauge)*

> Resident set size (RSS).

### `process_memory_virtual_bytes` *(gauge)*

> Virtual memory size.

### `process_open_fds` *(gauge)*

> Open file descriptors.

### `process_uptime_seconds` *(counter)*

> Uptime since admin-server start.

### `process_start_time_seconds` *(gauge)*

> Unix timestamp of process start.

---

## Build + configuration info

These are "info metrics" — value is always `1`, the labels carry
the useful information.

### `brain_build_info` *(gauge)*

Labels: `version`, `git_commit`.

### `brain_config_info` *(gauge)*

Labels: `shard_count`, `arena_capacity_bytes`, `hnsw_m`, `embedder_model`.

### `brain_up` *(gauge)*

> 1 if accepting requests, 0 during drain / shutdown.

### `brain_shards_total` *(gauge)*

> Number of configured shards.

---

## Deferred metric families (not in v1.0)

Per `crates/brain-server/src/metrics/mod.rs:32–47`. Tracker IDs
are in code comments.

### Memory counts (tracker: requires per-shard memory-stat API)

- `brain_memory_count`
- `brain_memory_count_tombstoned`
- `brain_memory_count_total`
- `brain_memory_kind{kind=…}`

### Storage (tracker: `phase-12/storage-stat-api`)

- `brain_arena_used_bytes` / `_capacity_bytes`
- `brain_arena_slots_used` / `_slots_free`
- `brain_wal_size_bytes` / `_segments`
- `brain_metadata_size_bytes`

### Embedder (tracker: embedder instrumentation hooks)

- `brain_embedder_calls_total`
- `brain_embedder_cache_{hits,misses}_total`
- `brain_embedder_duration_ms`
- `brain_embedder_queue_depth`
- `brain_embedder_workers_active`

### Executor (tracker: paired with phase-12.3 OTel)

- `brain_executor_latency_ms`
- `brain_executor_tasks_active`

These will land in follow-up patches; the metric names + label
sets are committed in code comments so dashboards and alert rules
can be authored ahead of time.

---

## Common queries

```promql
# p99 ENCODE latency (5-minute window):
histogram_quantile(0.99,
    sum by (le) (rate(brain_request_duration_ms_bucket{op="encode"}[5m])))

# Error rate per op:
sum by (op) (rate(brain_request_total{status="error"}[5m]))
  / sum by (op) (rate(brain_request_total[5m]))

# Workers that haven't run recently (staleness > 2× interval):
time() - brain_worker_last_run_unixtime > 3600

# Tombstone backlog — triggers HNSW maintenance pressure:
brain_hnsw_tombstone_ratio > 0.15
```

## See also

- [`http-api.md`](http-api.md) — the `/metrics` endpoint surface.
- [`../guides/observability.md`](../guides/observability.md) — scraping, dashboards, alerting.
- [`../../monitoring/dashboards/`](../../monitoring/dashboards/) — pre-built Grafana JSON.
- [`../../monitoring/alerts/brain-rules.yml`](../../monitoring/alerts/brain-rules.yml) — alert taxonomy.

**Spec:** §14/01. **Source:** `crates/brain-server/src/metrics/`.
