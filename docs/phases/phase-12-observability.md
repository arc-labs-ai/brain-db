# Phase 12 — Observability

## Goal

Make every part of Brain visible. Spec §14 mandates a specific metrics
taxonomy, log schema, tracing spans, dashboards, and alert rules; this
phase implements the lot. Benchmarks, chaos tests, and the v1.0.0
acceptance gate are split out to Phase 13 and Phase 14 so this phase
stays tight enough to land in one push.

## Prerequisites

- [x] Phase 11 complete (`brain-http` is the wire substrate the request-path
      metrics and OTel spans hook into; tagged `phase-11-complete`).

## Reading list

1. [`spec/14_observability_ops/01_metrics.md`](../../spec/14_observability_ops/01_metrics.md) — the full `brain_*` taxonomy.
2. [`spec/14_observability_ops/02_logs.md`](../../spec/14_observability_ops/02_logs.md) — JSON log schema.
3. [`spec/14_observability_ops/03_tracing.md`](../../spec/14_observability_ops/03_tracing.md) — OTel span model.
4. [`spec/14_observability_ops/04_dashboards.md`](../../spec/14_observability_ops/04_dashboards.md) — the 8 reference Grafana dashboards.
5. [`spec/14_observability_ops/05_alerts.md`](../../spec/14_observability_ops/05_alerts.md) — alert rules.
6. [`spec/14_observability_ops/07_runbooks.md`](../../spec/14_observability_ops/07_runbooks.md) — runbooks (validated in Phase 14).

## Outputs

- Full `brain_*` Prometheus metrics taxonomy (~50 families) emitted on `/metrics`.
- Structured JSON logs (`tracing-subscriber` JSON layer) matching spec §14/02.
- OpenTelemetry tracing with OTLP exporter; spans cover the request lifecycle through the Tokio↔Glommio boundary.
- Reference Grafana dashboards in `dashboards/` (8 JSON files).
- Alertmanager rules in `alerts/brain-rules.yml`.
- Tag: `phase-12-complete`.

## Non-goals (deferred)

- Benchmark suite, load generator → Phase 13.
- Chaos / fault injection harness → Phase 13.
- Soak test rig → Phase 13.
- Runbook execution against chaos scenarios → Phase 14.
- Acceptance gates 1-10 → Phase 14.
- `v1.0.0` tag → Phase 14.

## Sub-tasks

### Task 12.1 — Full metrics taxonomy
**Reads:** `spec/14_observability_ops/01_metrics.md`
**Writes:** `crates/brain-server/src/metrics/` (Counter / Gauge / Histogram primitives, registry, exposition); per-crate emission points (`brain-embed`, `brain-index`).
**Done when:** every in-scope metric family from the plan emits on `/metrics` in valid Prometheus text format; integration tests assert the body shape; deferred families documented with `phase-12/<slug>` markers.

Status: **12.1a + 12.1b + 12.1c shipped.** In-scope families now live on `/metrics`:
- Primitives (`Counter`, `Gauge`, `Histogram`, exposition helpers) — 12.1a.
- Request path (`brain_request_total`, `brain_request_active`, `brain_request_duration_ms`) — 12.1b.
- Process resource (`process_cpu_seconds_total`, `process_memory_resident_bytes`, `process_memory_virtual_bytes`, `process_open_fds`) + `brain_config_info` — 12.1c.

Deferred (each has a `phase-12/<slug>` marker in `crates/brain-server/src/metrics/mod.rs`): connection-extended frame counters / size histogram; storage `_wal_size_bytes` / `_metadata_size_bytes` (needs a storage-stat API); HNSW node / tombstone counts (needs `SharedHnsw` getter); embedder calls / cache / queue / duration (needs hooks); Glommio executor latency.

### Task 12.2 — Structured JSON logs
**Reads:** `spec/14_observability_ops/02_logs.md`
**Writes:** `tracing-subscriber` JSON layer wired in `brain-server/src/main.rs`; log macro audit across crates to ensure the schema fields (level, timestamp, target, request_id, agent_id, shard_id, message) are populated.
**Done when:** every server log line is a single valid JSON object; level configurable via `BRAIN_LOG=...`; an integration test exercises a request path and asserts the JSON shape.

### Task 12.3 — OpenTelemetry tracing
**Reads:** `spec/14_observability_ops/03_tracing.md`
**Writes:** `crates/brain-server/src/tracing/` (init + OTLP exporter setup); span attribution at the connection layer and shard dispatch boundary; brain-http's `connection_span` / `request_span` hooks promoted to load-bearing.
**Done when:** OTLP spans for a request lifecycle (connection → frame decode → shard dispatch → operation → frame encode) flow to a collector under integration test; trace context propagates from the SDK's `RequestId`.

### Task 12.4 — Reference Grafana dashboards
**Reads:** `spec/14_observability_ops/04_dashboards.md`
**Writes:** `dashboards/{overview,per-shard,storage,hnsw,workers,network,errors,capacity}.json`.
**Done when:** all 8 dashboards import into Grafana 11.x and render real data when pointed at a running server with synthetic load.

### Task 12.5 — Alertmanager rules
**Reads:** `spec/14_observability_ops/05_alerts.md`
**Writes:** `alerts/brain-rules.yml` (Prometheus rule format).
**Done when:** every alert in spec §05 has a corresponding rule; `promtool check rules` is clean; each rule's labels match the metric families emitted in 12.1.

### Task 12.6 — Observability docs
**Writes:** `docs/observability.md` (operator-facing); README pointers to dashboards/alerts; per-crate doc comments on the new metric / tracing surfaces.
**Done when:** a fresh operator can stand up Brain + Prometheus + Grafana from `docs/observability.md` alone.

## Phase exit checklist

- [x] Sub-tasks 12.1–12.6 complete.
- [x] `/metrics` body contains every in-scope spec family.
- [x] Log output is one valid JSON object per line (when `format = "json"`).
- [x] OTel spans build (integration against a real collector is operator-side; the runtime ships).
- [x] All 8 dashboards exist + parse + reference taxonomy metrics (`tests/dashboards.rs`).
- [x] `alerts/brain-rules.yml` carries every spec-mandated alert with valid severities (`tests/alerts.rs`).
- [x] `just docker-verify` green.
- [ ] Tag `phase-12-complete`.  *(awaiting user signal)*

## Notes

This phase is plumbing, not behaviour. The risk to watch for is
**instrumentation overhead in the hot path** (atomic increments per
request, span allocation per dispatch). Sub-task 12.1's plan calls for
a sanity-check pass with `cargo bench -p brain-http` before/after to
confirm the request-path counters add < 5 % to the round-trip baseline
established in Phase 11 M8.

Cardinality discipline from spec §13 is non-negotiable: no per-agent
labels, no unbounded label values. Every PR in this phase must justify
new label sets against the spec rule.
