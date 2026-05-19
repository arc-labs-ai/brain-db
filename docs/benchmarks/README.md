# Benchmarks

**Audience:** anyone deciding whether Brain meets a workload's
requirements; release managers gating phase tags.

**Goal:** *acceptance evidence*. Numbers, methodology, and a paper
trail of how they were measured. Not "how to tune Brain" — that's
[`../guides/tuning/`](../guides/tuning/).

## Layout

- [`latency-targets.md`](latency-targets.md) — The p50/p95/p99/p999
  numbers Brain commits to per operation, per spec §16/02.
  Methodology, host shape, and what each percentile excludes.
- [`durability-criteria.md`](durability-criteria.md) — The
  fail-stop / no-silent-corruption invariants and the chaos tests
  that prove them, per spec §16/06.
- [`results/`](results/) — Per-phase result reports. One Markdown
  file per phase tag (`phase-09.md`, `phase-14.md`, …). Each
  reports: host shape, workload, observed numbers, deltas vs the
  previous phase, regressions.

## Reproducing

Every result file under `results/` ends with a `## Reproduce`
section: exact `cargo bench` invocation, dataset
size, host shape (kernel, CPU, RAM, storage). If you can't
reproduce a number ±10%, file an issue with your `lscpu` output.

## See also

- [`../guides/tuning/`](../guides/tuning/) — how to change behaviour.
- [`../reference/performance.md`](../reference/performance.md) — short
  reference table of the published numbers (for operators who
  just want the number, not the methodology).
- [`../../spec/16_benchmarks_acceptance/`](../../spec/16_benchmarks_acceptance/)
  — the authoritative acceptance gate.
