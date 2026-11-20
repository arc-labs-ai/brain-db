# 19. Benchmarks + Acceptance Criteria

> **TL;DR.** The acceptance gate for v1.0. Seven gates — correctness, latency (p99 within targets at 1M memories), throughput, recall (>95% at default ef_search), durability (zero data loss in chaos tests), resource budgets, operational readiness — all must pass. Benchmarks use a realistic 70/25/5 workload mix on reference hardware (16-core x86_64, 64 GB RAM, NVMe SSD) at 100K/1M/10M memory scales, with cold-start and warm-up phases. Brain publishes honest numbers including comparison points against Pinecone, Weaviate, Milvus, and pgvector.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Brain implementers; QA teams |
| Voice | Hybrid (rationale + normative) |
| Depends on | All earlier specs |
| Referenced by | — |

## What this spec defines

The performance and correctness criteria Brain v1 must meet. Both numerical targets (throughput, latency) and qualitative criteria (correctness, recoverability).

This document specifies what Brain v1 must do — the criteria for "this is done".

## What this document covers

- Correctness criteria (Brain behaves as specified).
- Performance targets (latency, throughput, resources).
- Recall quality (the HNSW returns useful results).
- Durability criteria (no data loss for committed operations).
- Benchmark methodology (how to measure).
- An acceptance test suite (executable specification).

## What this document does not cover

- **The architecture and behavior.** Defined in earlier specs.
- **Operational concerns.** Defined in [17. Observability](../17_observability/00_purpose.md).

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

This document specifies criteria for v1. Future major versions (clustered, replicated) will have their own.

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
- Vector dimension: **384** — the output of the substrate's own embedder, BGE-small-en-v1.5, L2-normalised (`crates/brain-embed/src/model/forward.rs:24`, `crates/brain-embed/src/lib.rs:1-5`). Brain embeds server-side; it does not take vectors from a hosted embedding API, so the dimension is a property of the shipped model and not a benchmark choice.
- Token cap per memory: 512 (`crates/brain-embed/src/model/tokenize.rs:23`). Text beyond it truncates, so a 1 KB average sits comfortably inside one full forward pass.
- Salience: distributed [0.1, 1.0].
- Edges: average 5 per memory, distributed (some have 0, some 50).

This matches realistic data. The dimension matters to every downstream budget in this section: an arena slot is 1600 bytes (`crates/brain-storage/src/arena/file.rs:54`), of which the 384-float vector is 1536 — so vector width, not text, sets the storage floor.

## 7. The dataset sizes

Tested at scales:
- 100K memories per shard.
- 1M memories per shard.
- 10M memories per shard.

The 1M case is the primary target. 100K verifies "small case works"; 10M verifies "large case scales".

## 8. The "before benchmarking" prep

Each benchmark run:
- Cold-start the node.
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

Brain isn't always best; the comparison is transparent about where.

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
- Distributed deployment (future versions).

These are known limitations. They're documented for transparency.

## 15. The "test, then deploy"

Acceptance tests run pre-release. Pre-deployment, operators run their own tests:

- Their workload.
- Their hardware.
- Their data.

Brain's tests provide a baseline; operators verify for their environment.

## 16. The "ongoing measurement"

Benchmarks aren't just for release. In production:

- Per-deployment metrics (per [17. Observability](../17_observability/00_purpose.md)).
- Compare against benchmark targets.
- Investigate divergences.

Production should match benchmark performance, modulo workload differences.

## 17. The "what counts as v1"

V1 is:
- Single-node.
- The 5 cognitive primitives.
- All documented features.
- Meets all MUST criteria.

Anything beyond is v1.x or a future major version.

---

*Continue to [`01_correctness_and_durability.md`](01_correctness_and_durability.md) for correctness criteria.*
