# 14.04 Reference Dashboards

Reference Grafana dashboards Brain ships with.

## 1. The dashboards

Brain ships JSON definitions for these dashboards:

- **Overview**: high-level health.
- **Per-shard**: detailed view of a single shard.
- **Storage**: arena, WAL, metadata.
- **HNSW**: index health.
- **Workers**: background worker status.
- **Network**: connections and protocol.
- **Errors**: error rates and breakdowns.
- **Capacity**: utilization and growth.

These can be imported into Grafana. They use the metrics defined in [01_metrics.md](01_metrics.md).

## 2. Overview dashboard

Top-level "is everything OK?" view.

Panels:

- **Substrate up**: per-shard `up` metric. Green = up, red = down.
- **Total requests/sec**: rate, by operation.
- **Error rate**: percentage of requests in error.
- **Latency p99**: per-operation, time-series.
- **Memory count**: per-shard, time-series (for growth tracking).

This is the "first dashboard to look at" when something feels wrong.

## 3. Per-shard dashboard

Detailed view of one shard. Variable: `shard_id` (selectable in dropdown).

Panels:

- Request rates per operation.
- Latency histograms per operation.
- Error rates per operation.
- Memory / arena / WAL stats.
- HNSW health.
- Worker statuses.
- Top errors (by count).

For deep-diving into a specific shard.

## 4. Storage dashboard

Storage health across all shards.

Panels:

- Arena utilization (used vs capacity).
- Arena growth rate.
- WAL size per shard.
- WAL growth rate.
- Metadata size.
- Slot reclamation rate.
- Free list size.

For capacity planning and storage health.

## 5. HNSW dashboard

Index health.

Panels:

- Per-shard node count.
- Tombstone ratios (ideal: < 30%).
- Recall estimates (ideal: > 95%).
- Search latency.
- Search visits per query.
- Rebuild status (in progress / progress %).
- Rebuild history (count over time).

The HNSW is performance-critical; this dashboard surfaces issues.

## 6. Workers dashboard

Background workers' status.

Panels:

- Per-worker last run age.
- Per-worker cycle duration.
- Per-worker pending work.
- Per-worker error rate.
- Per-worker total cycles.

Operators check this to ensure workers are running and keeping up.

## 7. Network dashboard

Connection and protocol metrics.

Panels:

- Active connections.
- Active streams.
- Frame send / receive rates.
- Frame size distributions.
- Connection lifetime histogram.
- Connection errors by type.

For diagnosing networking issues.

## 8. Errors dashboard

Error breakdown.

Panels:

- Error rate over time.
- Errors by code (heatmap).
- Errors by operation.
- Errors by shard.
- Recent error details (logs panel, requires Loki / Elastic).

Operators use this to identify error patterns.

## 9. Capacity dashboard

For capacity planning.

Panels:

- Memory count growth (with forecast).
- Storage growth.
- Request rate growth.
- CPU utilization.
- Memory (RAM) utilization.
- Disk utilization.
- Time-to-full projections.

The forecast panels use Prometheus's `predict_linear` for simple projections.

## 10. The dashboards are starting points

The provided dashboards aren't the only views. Operators customize:

- Add panels for their specific concerns.
- Filter to specific deployments.
- Combine with their own metrics (application-level).

Brain provides defaults; operators tailor.

## 11. The Grafana version

Dashboards target Grafana v9+ syntax. Older Grafana versions may need adjustment.

The JSON files are in `/etc/brain/dashboards/` after install. They can be:

- Imported manually via Grafana UI.
- Provisioned via Grafana's file-based provisioning.
- Loaded via a sidecar container (in Kubernetes).

## 12. Dashboard variables

Common variables across dashboards:

- `$shard_id`: which shard (from the `up` metric's labels).
- `$operation`: which operation (from `brain_request_total`).
- `$time_range`: time window (Grafana's built-in).

These let one dashboard serve multiple shards / operations without duplication.

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

The dashboard JSON is checked into source control. Changes go through code review.

A test verifies the JSON parses, references valid metrics, and has expected structure. This catches typos before they hit production.

## 16. The "default visualization"

Each metric has a recommended visualization:

- Counters → graph (time-series).
- Histograms → heatmap or time-series of quantiles.
- Gauges → gauge or stat panel.

The default dashboards use these. Operators can override.

## 17. The screenshots

Dashboard screenshots are in `/docs/observability/dashboards/`:

- For documentation.
- For onboarding new operators.
- For comparison after upgrades (visual regression).

Screenshots are also useful in incident postmortems.

## 18. The deployment tooling

For deployments using the standard Prometheus + Grafana stack:

- Brain's Helm chart deploys metrics scraping config.
- The chart can install dashboards via Grafana's sidecar.
- Default ServiceMonitors are provided for Prometheus Operator.

These reduce setup work for Kubernetes deployments.

---

*Continue to [`05_alerts.md`](05_alerts.md) for alerts.*
