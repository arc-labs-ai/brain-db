# 17. Observability + Operations

> **TL;DR.** What operators see and do. Metrics in OpenMetrics format, per-shard and per-operation but **never per-tenant** — a tenant label would multiply every series by the tenant count, so per-space questions are answered from logs and the audit trail instead. Structured logs (compact or JSON), OpenTelemetry traces, health endpoints, and the five golden signals. Reference dashboards and alert rules ship with Brain. Admin operations (stats, rebuild, snapshot, restore, worker control, configuration reload, audit query) flow over a separate admin HTTP listener with its own mandatory bearer token. Runbooks for common situations let an SRE diagnose without deep Brain expertise.

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

Brain observes itself, on the admin listener
(`crates/brain-server/src/admin/router.rs:33-56`):

- `/healthz` — liveness, unauthenticated.
- `/readyz` — readiness, which reports whether the shards are serving.
- `/metrics` — the Prometheus exposition, on its own listener and port.
- `POST /v1/diagnostics/debug-snapshot` — a state dump for an escalation.

Operators can check Brain without external tooling. There is no HTML status
page and no `/debug/*` tree; everything above returns JSON or the Prometheus
text format.

## 6. The "five golden signals" coverage

Per Google's SRE book:

- **Latency**: per-operation quantiles, from the request duration histogram.
- **Traffic**: requests per second by operation; per-shard volume from the
  query and writer families rather than the request family.
- **Errors**: error rates by operation. **Not by code** — the status label has
  three values and no code dimension, so a per-code breakdown comes from the
  logs.
- **Saturation**: CPU, resident memory, derivation queue depth, arena
  occupancy, WAL size, and the derivation admission budget.
- **Plus** — for Brain — index quality, via the HNSW tombstone ratio. A direct
  recall estimate needs sampled exact-search comparison and is not emitted.

Each signal has metrics and alerts; the dashboard panels cover most of them.

## 7. The "what to monitor" levels

```
Level 1: Brain health.
  Is Brain up and accepting traffic?

Level 2: Per-shard health.
  Is each shard healthy? Is it processing requests?

Level 3: Per-operation health.
  Are encodes/recalls/etc. fast and successful?

Level 4: Per-tenant health.
  Are specific agents experiencing issues?
```

Levels 1 to 3 are answered by metrics. **Level 4 is not** — no metric carries a
`space_id` or `namespace` label, so per-tenant health is read from the request
logs, which carry the effective identity per entry, and from the audit trail.
Alerts fire at the level appropriate to the issue, which in practice means
levels 1 to 3.

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

- Arena and index growth, as the stand-in for memory count — there is no live
  memory-count gauge, and allocated slots include tombstoned ones.
- Tombstone accumulation, and the tombstone ratio that drives index
  maintenance.
- Derivation backlog depth and age: how far a workspace has drifted from its
  own data.
- Edge and typed-graph write rates, per derivation worker.

These complement system signals (CPU, memory, disk).

## 10. The "automation hooks"

Observability signals can drive automation:

- Auto-scale based on CPU.
- Auto-restart on health failure.
- Auto-rebalance on hot spots.

Brain emits signals; the automation platform (Kubernetes, etc.) acts on them.

## 11. The "audit" requirement

For compliance scenarios, Brain keeps an audit trail in shard metadata and
exposes it on the admin plane at `GET /v1/audit` and `GET /v1/audit/export`.

Be precise about what it records: it is an **extraction and
entity-resolution** audit — which extractor ran against which memory, at which
version, with what outcome, cost and outputs — not a log of every
state-mutating operation. Entries are append-only and time-ordered by a UUIDv7
key, and each carries a BLAKE3 hash of the input text, which exists to detect
an edited memory rather than to chain entries together
(`crates/brain-metadata/src/tables/audit.rs:1-6`, `:86-96`).

That hash is **not** a tamper-evidence chain: no entry references its
predecessor's hash, so the trail is append-only by construction rather than
cryptographically verifiable. A deployment that needs tamper evidence gets it
by exporting to append-only external storage.

## 12. The "in-production debugging"

When something goes wrong:

- Detailed logs from the affected time.
- Traces showing the request path.
- Metrics showing context: load, and errors elsewhere.

These three together usually identify the issue. Brain exposes them readily.

## 13. The role of the client

Clients contribute observability:

- Per-request client-side metrics: latency as the client experienced it,
  which includes the network the server cannot see.
- Errors as the application saw them.

Client and server traces are **separate traces today**: the wire protocol
carries no trace context, so Brain's spans are roots rather than children of
the caller's. Join them by timestamp and by the space identity the server
records, not by a shared trace id.

## 14. The "self-service" diagnostics

Operators can diagnose without engineering help:

- Documented metrics.
- Documented log fields.
- Reference dashboards.
- Common-issue runbooks.

Most issues should be diagnosable by an SRE familiar with general database operations, without deep Brain expertise.

## 15. The escalation path

For issues beyond runbook scope:

- Detailed logs, at TRACE level if needed.
- CPU profiling from **outside** the process, with `perf` against the named
  shard threads. `POST /v1/diagnostics/profile` answers `501` — the in-process
  Glommio profiler is not built, and there is no pprof endpoint.
- `POST /v1/diagnostics/debug-snapshot` for a state dump.
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

The one cost figure Brain emits directly is LLM spend:
`brain_extractor_llm_micro_usd_spent_total`, a per-shard counter in
dollar-micro-units, so a spend rate is `rate(...)/1e6` dollars per second.

Per-tenant cost is **not** a metric — the same cardinality rule applies — but it
is recorded durably: each audit entry carries the extraction's
`cost_micro_usd` and the namespace it is billed to, which is how a per-tenant
total is produced without a tenant label on a series.

Everything else is inferred: CPU from `process_cpu_seconds_total`, derivation
pressure from the admission gauges. Cost itself lives in the operator's metric
system.

---

*Continue to [`01_signals.md`](01_signals.md) for metrics, logs, and traces.*
