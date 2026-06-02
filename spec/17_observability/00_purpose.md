# 17. Observability + Operations

> **TL;DR.** What operators see and do. Metrics in OpenMetrics format (per-shard, per-operation, per-tenant), structured JSON logs, OpenTelemetry traces, health endpoints, the five golden signals plus recall quality. Reference dashboards and alert rules ship with Brain. Admin operations (stats, rebuild, snapshot, restore, worker control, configuration reload, audit query) flow over the same wire protocol with admin opcodes. Runbooks for common situations let an SRE diagnose without deep Brain expertise.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Operators; SRE teams |
| Voice | Hybrid (rationale + normative) |
| Depends on | All architecture docs |
| Referenced by | — |

## What this spec defines

How operators run, monitor, and maintain a Brain deployment. Brain's metrics, logs, traces, dashboards, alerts, and runbooks.

This document specifies how Brain is observed and operated — the metrics, logs, traces, dashboards, and procedures that operators use to keep deployments healthy.

## What this document covers

- The metrics Brain exposes.
- The structured logs it emits.
- Distributed tracing integration.
- Reference dashboards and alert rules.
- Administrative operations.
- Runbooks for common situations.
- Capacity planning guidance.

## What this document does not cover

- **The internal mechanisms** that produce the observability signals. Defined in the relevant architecture chapters.
- **The deployment infrastructure** (Kubernetes, etc.). Brain is infrastructure-agnostic.
- **Alerting platform** specifics (PagerDuty, etc.). Brain emits OpenMetrics; the alerting platform is operator's choice.

## 1. The operator's perspective

An operator runs Brain in production. They need to:

- Understand current state (is everything healthy?).
- Detect issues early (before user impact).
- Diagnose problems quickly (where is the issue?).
- Take corrective action.
- Plan capacity (when to add resources?).

This document specifies the tools available.

## 2. The "operate via signals" approach

Brain is observed primarily through:

- **Metrics**: numerical time series in Prometheus / OpenMetrics format. For dashboards, alerts, and trends.
- **Logs**: structured JSON event records. For investigation and audit.
- **Traces**: per-request causal chains in OpenTelemetry format. For deep debugging.

These are emitted continuously, not on demand. Operators consume them via standard tools (Prometheus, Loki, Tempo, Grafana, etc.).

## 3. The "no required platform" rule

Brain doesn't require specific observability platforms. It emits:

- Metrics in Prometheus / OpenMetrics format.
- Logs as structured JSON.
- Traces in OpenTelemetry format.

These are standards that work with any modern stack.

## 4. The observability budget

Observability has a cost (CPU, memory, network for emission). Brain's overhead:

- Metrics: < 1% CPU.
- Logs (at INFO level): < 1% CPU.
- Traces (sampled): < 1% CPU.

For a fully-instrumented deployment: < 5% overhead. Acceptable.

## 5. Self-observability

Brain observes itself:

- Health endpoints (`/healthz`, `/readyz`).
- Self-diagnostics (`/debug/...`).
- Status page (HTML at `/`).

Operators can check Brain without external tooling.

## 6. The "five golden signals" coverage

Per Google's SRE book:

- **Latency**: per-operation p50/p95/p99 metrics.
- **Traffic**: requests per second, by operation and shard.
- **Errors**: error rates, by operation and code.
- **Saturation**: CPU, memory, queue depth, disk usage.
- **Plus** — for Brain — recall quality, salience drift, etc.

Each signal has metrics, alerts, and dashboard panels.

## 7. The "what to monitor" levels

```
Level 1: Brain health.
  Is Brain up and accepting traffic?

Level 2: Per-shard health.
  Is each shard healthy? Is it processing requests?

Level 3: Per-operation health.
  Are encodes/recalls/etc. fast and successful?

Level 4: Per-tenant health.
  Are specific agents (scoped by org_id / user_id / namespace_id / agent_id) experiencing issues?
```

Operators monitor at all levels; alerts fire at the level appropriate to the issue.

## 8. The "ops runbook" approach

For common situations, Brain provides runbooks:

- Brain doesn't start.
- High latency on a shard.
- Memory pressure.
- Disk full.
- Recovery from corruption.

Each runbook has steps to diagnose and resolve. Operators don't need to derive procedures from first principles.

## 9. The "data" focus

Brain is a data system. Observability emphasizes data-related signals:

- Memory count growth.
- Tombstone accumulation.
- Vector quality (recall metrics).
- Edge graph density.

These complement system signals (CPU, etc.).

## 10. The "automation hooks"

Observability signals can drive automation:

- Auto-scale based on CPU.
- Auto-restart on health failure.
- Auto-rebalance on hot spots.

Brain emits signals; the automation platform (Kubernetes, etc.) acts on them.

## 11. The "audit" requirement

For compliance scenarios:

- Every state-mutating operation is audit-logged.
- Audit logs are tamper-evident (append-only, hash-chained).
- Logs are exportable for external storage.

Audit is opt-in; not all deployments need it.

## 12. The "in-production debugging"

When something goes wrong:

- Detailed logs from the affected time.
- Traces showing the request path.
- Metrics showing context (load, errors elsewhere).

These three together usually identify the issue. Brain exposes them readily.

## 13. The role of the client

Clients contribute observability:

- Per-request client-side metrics (latency from client perspective).
- Trace propagation (so server-side traces include client context).
- Errors visible to applications.

End-to-end observability spans client-and-server.

## 14. The "self-service" diagnostics

Operators can diagnose without engineering help:

- Documented metrics.
- Documented log fields.
- Reference dashboards.
- Common-issue runbooks.

Most issues should be diagnosable by an SRE familiar with general database operations, without deep Brain expertise.

## 15. The escalation path

For issues beyond runbook scope:

- Detailed logs (TRACE level if needed).
- Heap / CPU profiling (Brain supports pprof-style).
- Direct admin access for inspecting state.
- Engineering escalation if the issue is a Brain bug.

Brain provides debugging facilities; doesn't hide.

## 16. The "capacity planning" need

To plan capacity:

- Current utilization metrics.
- Growth trends.
- Forecasts.

Operators need data over time. Brain emits, the metrics platform retains, the operator analyzes.

## 17. The "cost visibility"

For cost-conscious deployments:

- Per-operation cost (CPU, embedder calls).
- Per-tenant cost (if multi-tenant).
- Cost anomalies.

Brain's metrics enable cost dashboards. Cost itself is the operator's metric system.

---

*Continue to [`01_signals.md`](01_signals.md) for metrics, logs, and traces.*
