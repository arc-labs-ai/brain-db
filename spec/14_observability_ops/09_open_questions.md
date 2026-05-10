# 14.09 Open Questions

Observability and operations questions unresolved as of this spec version.

---

## OQ-OO-1: Per-tenant metrics

**Issue.** Brain's metrics are per-shard, not per-agent. For multi-tenant deployments, per-tenant visibility is wanted but cardinality is a concern.

**Options.**

a) **No per-tenant metrics (status quo).** Use logs / audit instead.

b) **Top-N tenants only.** Emit metrics for top consumers; aggregate the long tail.

c) **Configurable per-tenant.** Operators specify which tenants get metrics.

**Recommendation.** (b) for v1.x. Tracks the heavy hitters without exploding cardinality.

---

## OQ-OO-2: Built-in alerting

**Issue.** Brain emits metrics; operators configure alerts in Alertmanager. Some users want built-in alerting (no external dependency).

**Options.**

a) **External (status quo).** Standard Prometheus + Alertmanager.

b) **Built-in alerting.** Brain evaluates alert rules and fires notifications directly.

**Recommendation.** Stay with (a). Prometheus + Alertmanager is mature; reinventing it is unnecessary.

---

## OQ-OO-3: Distributed tracing for cross-shard ops (v2)

**Issue.** Cross-node calls in v2 need careful trace propagation.

**Options.** Standard OpenTelemetry propagation.

**Recommendation.** Implement when v2 clustering arrives. Standard tooling.

---

## OQ-OO-4: Adaptive sampling in tracing

**Issue.** Fixed-rate sampling (e.g., 1%) doesn't capture rare-but-interesting traces. Adaptive sampling captures more errors and slow requests.

**Options.**

a) **Fixed rate (status quo).** Simple.

b) **Tail-based sampling.** Sample after the request, based on outcome.

c) **Adaptive.** Higher rate for errors and outliers; lower for normal.

**Recommendation.** (b) via tracing collector (Tempo, Honeycomb support it). (c) is a possible substrate-side enhancement.

---

## OQ-OO-5: Metric persistence across restarts

**Issue.** Counters reset on restart. Tools (Prometheus) handle this, but some metrics (e.g., total memories ever encoded) are lost.

**Options.**

a) **Reset (status quo).** Counters reset; cumulative views via PromQL.

b) **Persist.** Save counter state to disk; restore on startup.

**Recommendation.** Stay with (a). PromQL handles resets; persistence adds complexity.

---

## OQ-OO-6: Health-check details

**Issue.** Health endpoint returns "healthy" or not. More granular info might help orchestrators.

**Options.**

a) **Binary (status quo).**

b) **Detailed.** Returns per-component health (storage, embedder, workers).

**Recommendation.** Add (b) as `/healthz/detailed`. Keeps `/healthz` simple but offers depth.

---

## OQ-OO-7: Self-healing automation

**Issue.** Some issues have known fixes (HNSW rebuild, restart worker). The substrate could auto-fix.

**Options.**

a) **Manual fix (status quo).**

b) **Auto-rebuild.** Substrate auto-rebuilds when threshold hit (already implemented).

c) **Auto-restart workers.** Already implemented for crashed workers.

d) **More automation.** E.g., auto-shed load on memory pressure.

**Recommendation.** (b) and (c) are done. More aggressive automation should be opt-in (operators may not want surprise behavior).

---

## OQ-OO-8: Cost metrics

**Issue.** Operators want per-operation cost (CPU, network, storage). Calculating this is non-trivial.

**Options.**

a) **No cost metrics (status quo).** Operators derive externally.

b) **Approximate cost metrics.** Brain emits resource usage per operation; cost calculated externally.

c) **Cost metrics with pluggable cost models.**

**Recommendation.** (b) is reasonable; tracked for v1.x.

---

## OQ-OO-9: Anomaly detection

**Issue.** Threshold alerts catch known patterns. Anomaly detection (statistical or ML-based) might catch unknowns.

**Options.**

a) **Threshold alerts only (status quo).**

b) **Built-in anomaly detection.**

c) **External tools.**

**Recommendation.** (a) and (c). Brain's metrics work with anomaly detection tools (Datadog Watchdog, etc.). No need for built-in.

---

## OQ-OO-10: Log-to-metric conversion

**Issue.** Some signals are in logs, not metrics. For alerting, metrics are easier.

**Options.**

a) **Manual via log aggregator.** (Loki / Splunk can derive metrics from logs.)

b) **Brain emits the metric directly.**

**Recommendation.** Both. For high-value signals, Brain emits metrics. For everything else, use the log aggregator.

---

*Continue to [`10_references.md`](10_references.md) for references.*
