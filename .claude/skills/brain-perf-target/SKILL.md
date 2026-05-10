---
name: brain-perf-target
description: Compare bench results against spec §16/02 latency targets; flag regressions. Use after running `just bench <crate>` or before tagging a phase complete.
when-to-use: |
  Triggers:
    - Just ran `just bench <crate>` and want to interpret the result
    - Phase exit checklist
    - User says "did this regress?" / "are we within target?"
    - Pre-merge perf gate for hot-path changes
spec-refs:
  - spec/16_benchmarks_acceptance/02_latency_targets.md
  - spec/16_benchmarks_acceptance/03_throughput_targets.md
  - spec/16_benchmarks_acceptance/07_benchmark_methodology.md
---

# Perf Target Audit

## When to use

After running benchmarks (`just bench <crate>`) or as part of a phase exit. Brain has spec'd latency and throughput targets; this skill compares the measured numbers and reports inside-target / regressed.

## Spec targets (§16/02 — verify before assuming)

| Op | Metric | Target |
|---|---|---|
| ENCODE | p50 latency, single shard | ≤ 500 µs |
| ENCODE | p99 latency, single shard | ≤ 1 ms |
| RECALL (top_k=10) | p50 | ≤ 1 ms |
| RECALL (top_k=10) | p99 | ≤ 5 ms |
| FORGET | p99 | ≤ 1 ms |
| HNSW search (`ef=100`) | p99 | ≤ 3 ms |
| WAL append + fsync | p99 | ≤ 200 µs |

(Verify in the spec — these are illustrative; the spec is authoritative.)

## Hard rules (per spec §16/07)

- Bench in `--release` mode. Debug numbers are meaningless.
- Bench on the **target hardware tier** (see spec §01/05). Numbers from a laptop are not numbers from a tier-1 server.
- Use `criterion` with at least 100 samples and `confidence_level = 0.95`.
- Pin the bench input shape (vector dim, top-k, payload size) to the spec'd benchmark workload.
- Track regressions in a baseline file; CI flags a regression > 10% on any p99.

## Workflow

1. **Run the bench.** `just bench <crate>` (or `cargo bench -p <crate>`).
2. **Locate the targets.** Per op, find the spec'd value in §16/02.
3. **Compare:** measured ≤ target → green; measured > target → red.
4. **For red results:**
   - Profile (`cargo flamegraph -p <crate> --bench <bench>`).
   - Identify the regression source.
   - Either: revert the change, optimize, or surface to the user with "regression beyond spec target — needs design discussion".
5. **For green results:**
   - Update the baseline if the new number is meaningfully better (>5% improvement).
   - Otherwise, leave the baseline unchanged.

## Output format

```
PERF TARGET AUDIT — <crate>::<bench>

Op       Measured p99   Target p99   Verdict
encode   720 µs         1 ms         ✓ inside target (28% headroom)
recall   6.2 ms         5 ms         ✗ REGRESSED (24% over)
forget   480 µs         1 ms         ✓ inside

Action items:
- recall p99 6.2 ms > 5 ms target. See flamegraph at .../recall.svg.
  Hot frame: brain_index::hnsw::search at 4.1 ms (was 3.3 ms in baseline).
  Suggest: re-run with ef_search=100 to confirm parameter regression isn't the cause.
```

## Anti-patterns

- **Bench in debug mode.** Always `--release`.
- **Bench on dev laptop and call it "production".** Spec §01/05 names the target hardware.
- **Cherry-pick a fast run.** Use `criterion`'s statistical output (mean, p99, std-dev), not eyeball.
- **Lower the target to make the bench pass.** STOP and surface — this is a spec change, not an impl change.

## Cross-references

- `rust-perf` — broader hot-path discipline.
- `bench` (built-in) — runs the bench.
- spec §16/02, §16/03, §16/07.

## Source / Adaptations

Project-local. Operationalizes spec §16/02.
