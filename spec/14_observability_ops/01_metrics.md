# 14.01 Metrics

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

*Continue to [`02_logs.md`](02_logs.md) for logging.*
