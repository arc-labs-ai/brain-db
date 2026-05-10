# 16.00 Purpose

This document specifies what Brain v1 must do — the criteria for "this is done".

## What this document covers

- Correctness criteria (the substrate behaves as specified).
- Performance targets (latency, throughput, resources).
- Recall quality (the HNSW returns useful results).
- Durability criteria (no data loss for committed operations).
- Benchmark methodology (how to measure).
- An acceptance test suite (executable specification).

## What this document does not cover

- **The architecture and behavior.** Defined in earlier specs.
- **Operational concerns.** Defined in [14. Observability + Operations](../14_observability_ops/).

## 1. Why a benchmark spec

Without explicit targets:

- "Done" is subjective.
- Performance regressions go undetected.
- Different teams measure differently.

This spec sets concrete targets so claims can be checked.

## 2. The two kinds of criteria

- **MUST**: required for v1 release. Not meeting one of these blocks release.
- **SHOULD**: aspirational targets. Should aim for; failing one isn't a release blocker but is a known gap.

## 3. The "v1 scope"

This document specifies criteria for v1. v2 (clustered, replicated) will have its own.

v1 is single-node. The targets reflect what's achievable on a single machine with reasonable hardware.

## 4. The reference hardware

Benchmarks run on:

- **Standard**: 16-core x86_64 (e.g., AWS c6i.8xlarge), 64 GB RAM, NVMe SSD.
- **Small**: 4-core, 16 GB RAM, NVMe SSD (for "minimum viable" testing).
- **Large**: 64-core, 256 GB RAM, NVMe SSD (for "high-end" testing).

Targets reference the standard. Other hardware scales proportionally (with caveats).

## 5. The "real workload" definition

Benchmarks use a realistic workload mix:

- 70% RECALL (cue-based search).
- 25% ENCODE (new memories).
- 3% LINK / UNLINK (edges).
- 2% other (PLAN, REASON, FORGET).

This matches typical AI agent workloads.

## 6. The data shape

Memories:
- Average text size: 1 KB (typical for a chat exchange or knowledge bullet).
- Vector dimension: 1536 (OpenAI text-embedding-3-large).
- Salience: distributed [0.1, 1.0].
- Edges: average 5 per memory, distributed (some have 0, some 50).

This matches realistic data.

## 7. The dataset sizes

Tested at scales:
- 100K memories per shard.
- 1M memories per shard.
- 10M memories per shard.

The 1M case is the primary target. 100K verifies "small case works"; 10M verifies "large case scales".

## 8. The "before benchmarking" prep

Each benchmark run:
- Cold-start substrate.
- Warm-up phase (5 minutes of mixed load) to establish caches.
- Measurement phase (10 minutes for steady-state).

Cold-start results are reported separately if relevant.

## 9. The "regression catching"

Benchmarks run on every release candidate:

- Compare to previous release.
- If any p99 latency rises > 20%: investigate.
- If any throughput drops > 10%: investigate.

Catches regressions before users see them.

## 10. The "first principles" justification

Where targets seem aggressive:

- p99 latency 25 ms for RECALL: reasonable given embedder + HNSW search profiles.
- Throughput 10K ops/s/shard: matches NVMe SSD random read rates (~50K IOPS).
- HNSW recall > 95%: standard HNSW performance.

These aren't arbitrary; they're achievable with good engineering.

## 11. The "publish honest numbers" principle

Brain's benchmark results will be published with:
- The hardware used.
- The workload generator.
- The dataset.
- The full configuration.

Anyone can reproduce. No surprises.

## 12. The "comparison to alternatives"

Brain's benchmarks include comparison points:

- Pinecone (cloud vector DB).
- Weaviate, Milvus.
- pgvector (Postgres extension).

Brain shows how it compares: where it's faster, where it's competitive, where the alternatives are better.

Brain isn't always best; we're transparent about where.

## 13. The acceptance "gates"

For v1 release:

```
GATE 1: Correctness — all unit and integration tests pass.
GATE 2: Latency — p99 within targets at 1M memories.
GATE 3: Throughput — minimum throughput met at 1M memories.
GATE 4: Recall — > 95% recall at default settings.
GATE 5: Durability — no data loss in chaos tests.
GATE 6: Resource — within budgets.
GATE 7: Operational — runbooks exercised; metrics complete.
```

All gates must pass for release.

## 14. The "known limitations" disclosure

Brain v1 doesn't claim:
- Sub-millisecond p99 latency.
- 100K+ ops/s/shard throughput.
- Cross-shard transactions.
- Distributed deployment (v2).

These are known limitations. They're documented for transparency.

## 15. The "test, then deploy"

Acceptance tests run pre-release. Pre-deployment, operators run their own tests:

- Their workload.
- Their hardware.
- Their data.

Brain's tests provide a baseline; operators verify for their environment.

## 16. The "ongoing measurement"

Benchmarks aren't just for release. In production:

- Per-deployment metrics (per [14. Observability](../14_observability_ops/)).
- Compare against benchmark targets.
- Investigate divergences.

Production should match benchmark performance, modulo workload differences.

## 17. The "what counts as v1"

V1 is:
- Single-node.
- The 5 cognitive primitives.
- All documented features.
- Meets all MUST criteria.

Anything beyond is v1.x or v2.

---

*Continue to [`01_correctness_criteria.md`](01_correctness_criteria.md) for correctness criteria.*
