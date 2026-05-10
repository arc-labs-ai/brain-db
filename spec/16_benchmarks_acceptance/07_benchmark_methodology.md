# 16.07 Benchmark Methodology

How Brain's benchmarks are run — to ensure consistency, reproducibility, and meaningful results.

## 1. The reference environment

All published benchmarks use:

- **Hardware**: 16-core x86_64, 64 GB RAM, 1 TB NVMe SSD.
- **OS**: Ubuntu 24.04 LTS.
- **Kernel**: 6.6 LTS.
- **CPU governor**: performance (not powersave).
- **Hugepages**: disabled (irrelevant for file-backed mmap).
- **NUMA**: single node (avoid cross-node memory access).

Variations from this are noted.

## 2. The data generation

A standardized synthetic dataset:

- 1M memories per shard.
- Each memory: 1 KB of text.
- Vector embeddings: derived from the text via the production embedder.
- Edges: 5 per memory on average (Pareto distribution: most have few, some have many).

The dataset generator is open-source and deterministic (seed-based).

## 3. The workload generator

A separate process drives load:

- Configurable RPS, mix, concurrency.
- Records latencies via histograms (HdrHistogram).
- Reports per-operation and aggregate stats.

Runs on separate hardware to avoid contention with the substrate.

## 4. The "warm-up" phase

Before measurement:

- 5 minutes of full target load.
- Caches warm up.
- Allocator settles.
- HNSW search heuristics establish.

Cold-start results are reported separately (interesting but not the primary number).

## 5. The "measurement" phase

10 minutes at steady-state load:

- Record latency distributions.
- Record throughput.
- Record resource utilization (CPU, RAM, disk, network).

10 minutes is long enough for:
- Stable percentiles.
- Background work to occur.
- Outlier requests to appear in tail percentiles.

## 6. The "cool-down" optional phase

After measurement:
- Stop load.
- Verify substrate returns to idle baseline.
- Capture any background work.

This phase isn't measured but ensures the substrate is healthy.

## 7. The repeat count

Each benchmark runs at least 3 times:
- Use median across runs.
- Report standard deviation.

Single runs are noisy; multiple runs catch outliers.

## 8. The "isolation" practice

During benchmarks:
- No other workloads on the machine.
- No background tasks (cron, etc.).
- Network: dedicated.

Contention from outside corrupts results.

## 9. The ramp-up tests

For finding the "knee":

- Start at 1K ops/sec.
- Increase by 1K every 10 seconds.
- Continue until p99 latency exceeds target.
- The crossover is the sustainable throughput.

This identifies the substrate's capacity ceiling.

## 10. The "burst" tests

Beyond steady-state:

- Apply a burst (5× sustained for 10 seconds).
- Measure latency during and after.
- Verify recovery to steady state.

Tests the substrate's burst tolerance.

## 11. The "long-running" tests

For stability:

- 48-hour continuous load.
- Verify:
  - No memory growth.
  - No latency drift.
  - No errors accumulating.

Catches issues that take time to manifest.

## 12. The "chaos" tests

Inject failures during load:

- Kill the substrate; verify recovery.
- Slow disk I/O; verify graceful degradation.
- Network blips; verify reconnects.

Verifies failure-handling under load.

## 13. The instrumentation

The substrate's metrics are captured during benchmarks:

- Latency histograms.
- Throughput counters.
- Resource usage.
- Error counts.

Stored to Prometheus or files for later analysis.

## 14. Per-operation isolation

For per-operation throughput:

- Test ENCODE only.
- Test RECALL only.
- Etc.

For mixed:
- Test the realistic mix.

Both are useful: per-op for bottleneck analysis, mix for real-world.

## 15. The data scale tests

Repeat key benchmarks at different scales:
- 100K memories.
- 1M memories.
- 10M memories.

Identify scaling characteristics:
- Linear (good).
- Sub-linear (great, indicates good cache effects).
- Super-linear (bad, investigate).

## 16. The reporting format

Results report:

```
Benchmark: encode_throughput
Environment: 16-core, 64 GB, NVMe
Data: 1M memories
Run 1: 5,242 ops/sec, p99 24.3 ms
Run 2: 5,189 ops/sec, p99 25.1 ms
Run 3: 5,267 ops/sec, p99 23.8 ms
Median: 5,242 ops/sec, p99 24.3 ms
Std dev: 0.7%
Status: ✓ PASSES (target: ≥ 5,000 ops/sec)
```

Clear and complete.

## 17. The CI integration

Benchmarks run nightly in CI:
- Standard environment (cloud VM).
- Standard dataset.
- Standard configuration.

Results are stored in a benchmark database. Trends are tracked.

Significant regressions block release.

## 18. The "comparison" benchmarks

Brain compares against:
- Pinecone (cloud vector DB).
- Weaviate.
- pgvector (Postgres extension).

These run in similar environments with the same workload generator (modulo their different APIs).

Results show where Brain is faster, where competitive, where slower. Brain is honest about its strengths and weaknesses.

## 19. The "real production" check

Before releases, real-customer-like deployments are tested:
- Beta partners' workloads.
- Staging environments at customer sites.
- Synthetic but realistic scenarios.

CI tests the basics; real workloads validate the spec.

## 20. The "publish everything" principle

Brain's benchmark methodology is open:
- Hardware spec public.
- Code public.
- Datasets public.
- Results public.

Anyone can reproduce; results are trusted.

---

*Continue to [`08_acceptance_test_suite.md`](08_acceptance_test_suite.md) for the test suite.*
