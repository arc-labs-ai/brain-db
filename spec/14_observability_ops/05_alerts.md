# 14.05 Alerts

The alert rules Brain ships with — what conditions trigger alerts, and what severity.

## 1. The alerting model

Brain ships Prometheus alert rules. Operators consume them via Alertmanager (or equivalent). Each rule has:

- A name.
- A condition (PromQL).
- A duration (how long the condition must hold before firing).
- A severity.
- Labels and annotations (for context).

## 2. Severity levels

- **Critical (P1)**: page someone immediately. Substrate down, data loss imminent.
- **High (P2)**: notify within hours. Persistent errors, degraded performance.
- **Medium (P3)**: review within day. Trends worth attention.
- **Low (P4)**: informational. Enabled for review but not paging.

The substrate's defaults map issues to severity. Operators tune for their environment.

## 3. Critical alerts

### substrate_down

```yaml
- alert: BrainSubstrateDown
  expr: up{job="brain"} == 0
  for: 1m
  severity: critical
  summary: "Brain substrate is unreachable"
  description: "{{ $labels.shard }} not responding for 1 minute"
```

A shard is unreachable. Page immediately.

### high_error_rate

```yaml
- alert: BrainHighErrorRate
  expr: |
    sum(rate(brain_request_total{status=~"error.*"}[5m])) by (shard)
    /
    sum(rate(brain_request_total[5m])) by (shard)
    > 0.10
  for: 5m
  severity: critical
  summary: "Error rate > 10% on {{ $labels.shard }}"
```

More than 10% of requests failing for 5 minutes.

### data_loss_potential

```yaml
- alert: BrainCheckpointFailing
  expr: time() - brain_metadata_last_checkpoint_unixtime > 3600
  for: 5m
  severity: critical
  summary: "No checkpoint in last hour on {{ $labels.shard }}"
```

If no checkpoint completes for an hour, recovery time grows. Eventually data could be lost (more WAL retention pressure).

## 4. High alerts

### high_latency

```yaml
- alert: BrainHighLatency
  expr: histogram_quantile(0.99, sum(rate(brain_request_duration_ms_bucket[5m])) by (le, op)) > 100
  for: 10m
  severity: high
  summary: "p99 latency > 100ms on {{ $labels.op }}"
```

p99 latency above threshold for sustained period.

### worker_stuck

```yaml
- alert: BrainWorkerStuck
  expr: time() - brain_worker_last_run_unixtime > 7200
  for: 5m
  severity: high
  summary: "Worker {{ $labels.worker }} hasn't run in 2 hours"
```

A worker hasn't run a cycle. Investigation needed.

### high_memory_pressure

```yaml
- alert: BrainMemoryPressure
  expr: process_resident_memory_bytes / node_memory_MemTotal_bytes > 0.85
  for: 10m
  severity: high
  summary: "Memory > 85% on {{ $labels.shard }}"
```

The substrate is using > 85% of available RAM.

### disk_filling

```yaml
- alert: BrainDiskFilling
  expr: predict_linear(node_filesystem_free_bytes[1h], 24*3600) < 0
  for: 30m
  severity: high
  summary: "Disk projected to fill in 24h on {{ $labels.shard }}"
```

Linear projection: disk fills within 24 hours.

## 5. Medium alerts

### tombstone_high

```yaml
- alert: BrainHighTombstoneRatio
  expr: brain_hnsw_tombstone_ratio > 0.30
  for: 1h
  severity: medium
  summary: "Tombstone ratio > 30% on {{ $labels.shard }}"
```

HNSW degraded. Maintenance worker should rebuild; check why it isn't.

### recall_quality_degraded

```yaml
- alert: BrainRecallQualityDegraded
  expr: brain_hnsw_recall_estimate < 0.85
  for: 30m
  severity: medium
  summary: "Estimated recall < 85% on {{ $labels.shard }}"
```

HNSW recall has degraded. Rebuild needed.

### embedder_slow

```yaml
- alert: BrainEmbedderSlow
  expr: histogram_quantile(0.99, sum(rate(brain_embedder_duration_ms_bucket[5m])) by (le)) > 50
  for: 10m
  severity: medium
  summary: "Embedder p99 > 50ms"
```

Embedder is slower than expected.

### connections_growing

```yaml
- alert: BrainConnectionsGrowing
  expr: rate(brain_connections_total[5m]) > 100
  for: 30m
  severity: medium
  summary: "Many new connections / sec — possible client churn"
```

Indicates clients reconnecting frequently.

## 6. Low alerts

### config_change

```yaml
- alert: BrainConfigChanged
  expr: changes(brain_config_info[1h]) > 0
  severity: low
```

Configuration changed in the last hour. Informational.

### worker_errors_warning

```yaml
- alert: BrainWorkerErrorWarning
  expr: rate(brain_worker_errors_total[1h]) > 0.01
  for: 1h
  severity: low
```

Workers had errors (low rate). Worth review.

## 7. The "alert grouping"

Alertmanager groups related alerts:

```yaml
group_by: [shard, severity]
group_wait: 30s
group_interval: 5m
```

A burst of alerts (e.g., a shard going down triggers many) becomes a single notification with all alerts.

## 8. The "alert routing"

Different severities go to different destinations:

```yaml
routes:
  - match:
      severity: critical
    receiver: pagerduty
  - match:
      severity: high
    receiver: slack-oncall
  - match:
      severity: medium
    receiver: slack-team
  - match:
      severity: low
    receiver: email
```

Configurable per-deployment.

## 9. The "silencing"

For maintenance windows, operators silence alerts:

```
amtool silence add alertname=BrainSubstrateDown shard=<uuid> -d 2h
```

Silences alerts during planned work.

## 10. The "auto-resolution"

Most alerts resolve when the condition clears:

- Latency drops back → alert resolves.
- Worker runs → alert resolves.
- Connection restored → alert resolves.

Critical alerts may stay "fired" longer for ack tracking.

## 11. The thresholds

Default thresholds are conservative:

- Latency p99 > 100 ms = high.
- Error rate > 10% = critical.
- Tombstone ratio > 30% = medium.

Operators tune to match their SLOs:

```yaml
# Production SLO: p99 < 50 ms.
- alert: ProdLatencyHigh
  expr: histogram_quantile(0.99, ...) > 50
  ...
```

## 12. The "noise"

False positives are a real cost. Brain's defaults err toward fewer alerts:

- Sustained-condition requirements (`for: 5m`).
- Reasonable thresholds.
- Reasonable severities.

Operators monitor "alert noise" and adjust:

```
brain_alerts_fired_total{severity=}
brain_alerts_resolved_total{severity=}
```

## 13. The runbook links

Each alert has a runbook URL:

```yaml
- alert: BrainSubstrateDown
  annotations:
    runbook: "https://docs.brain.io/runbooks/substrate-down"
```

When paged, the responder gets a link to the runbook (next file).

## 14. The "test alerts"

In staging:

- Synthetic alerts to verify routing works.
- "Test fire" via Alertmanager API.

This ensures the alert pipeline is healthy.

## 15. The alert-rule deployment

Alert rules are checked into source control. Changes are reviewed.

Brain's Helm chart provisions rules via PrometheusRule CRDs (Prometheus Operator). For non-K8s, the rules are loaded from files.

## 16. The "alert fatigue"

Common signs:

- Same alert fires repeatedly.
- Alerts ignored.
- Many simultaneous alerts confuse responders.

Mitigations:

- Tighten thresholds.
- Group related alerts.
- Suppress predictable alerts (e.g., during maintenance).
- Review and prune low-value alerts.

The substrate's defaults are reasonable starting points; operators iterate.

## 17. The SLO-based alerting (advanced)

Alternative to threshold alerts: SLO-based alerts.

```
Error budget: 99% of requests should succeed (1% error budget).
If 25% of monthly budget is consumed in 1 hour, alert.
```

This is more nuanced than threshold alerts. The substrate doesn't ship SLO alerts by default (too deployment-specific) but the metrics support implementing them.

---

*Continue to [`06_admin_ops.md`](06_admin_ops.md) for admin operations.*
