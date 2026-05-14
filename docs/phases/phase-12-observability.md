# Phase 12 — Observability, Benchmarks, Acceptance

## Goal

Make Brain production-ready. Full Prometheus metrics, structured JSON logs, OpenTelemetry tracing, reference dashboards, alert rules, benchmark suite, chaos test harness, and the v1 acceptance gate.

## Prerequisites

- [x] Phase 10 complete.

## Reading list

1. [`spec/14_observability_ops/01_metrics.md`](../../spec/14_observability_ops/01_metrics.md)
2. [`spec/14_observability_ops/02_logs.md`](../../spec/14_observability_ops/02_logs.md)
3. [`spec/14_observability_ops/03_tracing.md`](../../spec/14_observability_ops/03_tracing.md)
4. [`spec/14_observability_ops/04_dashboards.md`](../../spec/14_observability_ops/04_dashboards.md)
5. [`spec/14_observability_ops/05_alerts.md`](../../spec/14_observability_ops/05_alerts.md)
6. [`spec/14_observability_ops/07_runbooks.md`](../../spec/14_observability_ops/07_runbooks.md)
7. [`spec/15_failure_recovery/07_chaos_testing.md`](../../spec/15_failure_recovery/07_chaos_testing.md)
8. [`spec/16_benchmarks_acceptance/`](../../spec/16_benchmarks_acceptance/) — all files.

## Outputs

- Full `brain_*` Prometheus metrics taxonomy.
- Structured JSON logs (slog or tracing-subscriber JSON layer).
- OpenTelemetry tracing (OTLP exporter optional).
- Reference Grafana dashboards (JSON).
- Alertmanager rules (YAML).
- Benchmark suite using `criterion`.
- Chaos test harness.
- Acceptance gate: every gate in `spec/16_benchmarks_acceptance/08_acceptance_test_suite.md` passes.
- Tag: `phase-12-complete`.
- Tag: `v1.0.0`.

## Sub-tasks

### Task 12.1 — Full metrics taxonomy
**Reads:** `spec/14_observability_ops/01_metrics.md`
**Writes:** `crates/brain-server/src/metrics.rs` and per-crate emission points.
**Done when:** Every spec'd metric is emitted; `/metrics` endpoint returns the full set in Prometheus format.

### Task 12.2 — Structured JSON logs
**Reads:** `spec/14_observability_ops/02_logs.md`
**Writes:** integrate `tracing-subscriber` JSON layer in `brain-server`.
**Done when:** Log output is one JSON object per line, schema per spec; configurable level via env.

### Task 12.3 — OpenTelemetry tracing
**Reads:** `spec/14_observability_ops/03_tracing.md`
**Writes:** `crates/brain-server/src/tracing.rs`
**Done when:** Spans cover request lifecycle; OTLP exporter sends to a configured collector.

### Task 12.4 — Reference Grafana dashboards
**Reads:** `spec/14_observability_ops/04_dashboards.md`
**Writes:** `dashboards/*.json`
**Done when:** Dashboards (overview, per-shard, storage, HNSW, workers, network, errors, capacity) imported and rendering against a running server.

### Task 12.5 — Alert rules
**Reads:** `spec/14_observability_ops/05_alerts.md`
**Writes:** `alerts/brain-rules.yml`
**Done when:** PrometheusRule YAML covers every alert in spec §05.

### Task 12.6 — Runbook validation
**Reads:** `spec/14_observability_ops/07_runbooks.md`
**Writes:** `docs/runbooks/*.md` (one per runbook).
**Done when:** Each runbook is a working procedure; tested by following the steps in a chaos scenario.

### Task 12.7 — Benchmark suite
**Reads:** `spec/16_benchmarks_acceptance/02_latency_targets.md`, `03_throughput_targets.md`, `07_benchmark_methodology.md`
**Writes:** `benches/*.rs` per crate, plus `benches/load_generator.rs`
**Done when:** Each operation has a criterion benchmark; targets met on reference hardware.

### Task 12.8 — Chaos test harness
**Reads:** `spec/15_failure_recovery/07_chaos_testing.md`
**Writes:** `tests/chaos/*.rs`
**What to build:**
- Process kill at random points.
- I/O fault injection (FUSE-based or in-process).
- Network failure simulation.
- Resource exhaustion.
- Concurrency stress (loom for select paths).
- Bit-flip corruption injection.
- Each scenario verifies the spec'd recovery behavior.

### Task 12.9 — Acceptance gate
**Reads:** `spec/16_benchmarks_acceptance/08_acceptance_test_suite.md`
**Writes:** `acceptance/run.sh` + per-gate test files.
**What to build:**
- Script that runs every gate (1-10) and reports pass/fail.
- Gates: Unit, Integration, E2E, Smoke, Performance, Chaos, Soak (48h), Compliance, Documentation, Security.
**Done when:** Gates 1-10 pass on the reference environment.

### Task 12.10 — Soak test
**Reads:** `spec/16_benchmarks_acceptance/08_acceptance_test_suite.md`
**Writes:** `tests/soak.rs`
**Done when:** 48h continuous load, no memory leak, no latency drift, no errors. (Run on dedicated infra; not a CI test.)

### Task 12.12 — Documentation pass
**Writes:** README updates, `docs/guides/`, etc.
**Done when:** Every public API documented; getting-started works; operator guide covers install, config, monitor, recover.

### Task 12.12 — Release prep
**Writes:** `CHANGELOG.md`, version bumps, release notes.
**Done when:** Tagged as `v1.0.0`; changelog complete.

## Phase exit checklist

- [ ] All sub-tasks complete.
- [ ] All 10 acceptance gates pass.
- [ ] Soak test result recorded.
- [ ] `cargo doc` builds without warnings.
- [ ] Release notes written.
- [ ] Tag `phase-12-complete` and `v1.0.0`.

## Notes

This is the longest phase. Don't try to do it in one push. Work each sub-task to completion (the "done when") before starting the next. Many require running infrastructure (Prometheus, Grafana, real load tests) — set those up as separate tasks if needed.
