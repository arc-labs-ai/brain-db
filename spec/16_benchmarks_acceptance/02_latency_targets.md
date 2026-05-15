# 16.02 Latency Targets

The latency targets Brain v1 must meet, on the reference hardware, with the reference workload.

## 1. The setup

- Hardware: 16-core x86_64, 64 GB RAM, NVMe SSD.
- Data: 1M memories per shard.
- Load: steady-state mixed workload (70% recall, 25% encode, 5% other).
- Concurrency: 100 concurrent clients.

## 2. The targets (single-shard)

### 2.1 Substrate (cognitive primitives)

| Operation | p50 | p95 | p99 | p99.9 |
|---|---|---|---|---|
| ENCODE | 8 ms | 15 ms | 25 ms | 50 ms |
| RECALL (K=10, no text) | 5 ms | 12 ms | 20 ms | 40 ms |
| RECALL (K=10, with text) | 7 ms | 18 ms | 30 ms | 60 ms |
| PLAN (depth 3) | 4 ms | 10 ms | 18 ms | 35 ms |
| REASON (depth 3) | 8 ms | 20 ms | 35 ms | 70 ms |
| FORGET | 3 ms | 8 ms | 15 ms | 30 ms |
| LINK | 2 ms | 5 ms | 10 ms | 20 ms |
| UNLINK | 2 ms | 5 ms | 10 ms | 20 ms |

These are MUST targets for v1.

### 2.2 Knowledge layer — entity operations (phase 16)

Measured at 100K entities per shard. Phase-16 perf gate; substrate workload assumptions in §1 apply.

| Operation | p50 | p95 | p99 | p99.9 |
|---|---|---|---|---|
| ENTITY_CREATE | 1 ms | 3 ms | 5 ms | 10 ms |
| ENTITY_GET | 0.5 ms | 1 ms | 2 ms | 5 ms |
| ENTITY_UPDATE | 1 ms | 3 ms | 5 ms | 10 ms |
| ENTITY_RENAME | 1 ms | 3 ms | 5 ms | 10 ms |
| ENTITY_MERGE | 5 ms | 15 ms | 25 ms | 50 ms |
| ENTITY_UNMERGE | 5 ms | 15 ms | 25 ms | 50 ms |
| ENTITY_TOMBSTONE | 1 ms | 3 ms | 5 ms | 10 ms |
| ENTITY_LIST (limit=100, prefix filter) | 2 ms | 5 ms | 10 ms | 20 ms |
| ENTITY_RESOLVE (tier 1 — exact / alias) | 0.5 ms | 1 ms | 2 ms | 5 ms |
| ENTITY_RESOLVE (tier 2 — trigram fuzzy) | 5 ms | 15 ms | 30 ms | 60 ms |

### 2.3 Knowledge layer — statement operations (phase 17)

Measured at 1M statements per shard. Phase-17 perf gate; substrate workload assumptions in §1 apply.

| Operation | p50 | p95 | p99 | p99.9 |
|---|---|---|---|---|
| STATEMENT_CREATE (Fact, 3 evidence) | 2 ms | 5 ms | 10 ms | 25 ms |
| STATEMENT_CREATE (Preference, auto-supersede) | 3 ms | 8 ms | 15 ms | 30 ms |
| STATEMENT_CREATE (Event) | 2 ms | 5 ms | 10 ms | 25 ms |
| STATEMENT_GET | 0.5 ms | 1 ms | 2 ms | 5 ms |
| STATEMENT_SUPERSEDE (explicit) | 3 ms | 8 ms | 15 ms | 30 ms |
| STATEMENT_TOMBSTONE | 1 ms | 3 ms | 5 ms | 10 ms |
| STATEMENT_RETRACT | 1 ms | 3 ms | 5 ms | 10 ms |
| STATEMENT_HISTORY (chain ≤ 10 versions) | 1 ms | 3 ms | 5 ms | 10 ms |
| STATEMENT_LIST (subject filter, current_only, ≤ 100 results) | 2 ms | 5 ms | 10 ms | 20 ms |
| STATEMENT_LIST (predicate filter, ≤ 100 results) | 3 ms | 8 ms | 15 ms | 30 ms |
| `statements_contradicting()` internal | 2 ms | 5 ms | 10 ms | 20 ms |

CREATE numbers assume the inline evidence path (≤ 8 evidence entries) and ~7 secondary index writes per `statement_create` ([§19/03 §2](../19_statements/03_storage.md)). Overflow path adds ~5 ms per chunk for the overflow row write.

### 2.4 Knowledge layer — deferred targets

- **ENTITY_RESOLVE (tier 3 — embedding HNSW)** lands when the entity HNSW is wired into the resolver (phase 21). Target placeholder per the phase-16 doc: p50 ≤ 5 ms at 100K, ≤ 50 ms at 1M. Final numbers set in phase 21.
- **ENTITY_RESOLVE (tier 4 — LLM)** lands in phase 21 with the LLM extractor. Latency is gated by the model + cache hit-rate; target is "tail under 1 s with cache warm, queued under 5 s cold."
- **Statement HNSW semantic search** — phase 21 when the embedding worker populates the HNSW. Phase 17 writes / reads only; semantic search target lands with the worker.
- **Relation / query / admin** opcodes — separate phases (18 / 22-23 / 22) and separate target rows.

### 2.5 Phase perf gates

- **Phase 16 (sub-task 16.9)** — §2.2 entity targets at 100K entities.
- **Phase 17 (sub-task 17.10)** — §2.3 statement targets at 1M statements.

Phases verify on the dev workstation; production-reference numbers (16-core / 64 GB / NVMe per §1) are revalidated in phase 14's CI suite.

## 3. The breakdown

For RECALL p99 = 20 ms, time is spent:

- Wire / framing / authentication: ~0.5 ms.
- Embedder (cue → vector): ~5 ms (with cache hit) / ~15 ms (miss).
- HNSW search: ~2-5 ms (depends on K and ef_search).
- Metadata fetch (top-K): ~2-5 ms.
- Text fetch (if requested): ~3-5 ms.
- Response framing: ~0.5 ms.

Total: 13-30 ms across cases. The 20 ms p99 is achievable.

## 4. The cold-cache numbers

When the substrate just started and caches are cold:

- p99 latency ~2× normal.
- Returns to normal within ~5 minutes of warm-up.

These numbers don't apply during cold-cache; the warm-up phase is excluded from the targets.

## 5. The "first request" latency

The very first request after startup:

- May include connection setup, embedder model loading.
- Up to 10× the steady-state p99.

This is acceptable; subsequent requests are normal.

## 6. The factor "load level"

At low load (~10% of capacity): latency near p50.

At high load (~80% of capacity):

- p99 may rise 2-3×.
- This is acceptable trading.
- Operators monitor and scale.

## 7. The "tail" sensitivity

p99.9 latency is sensitive to:

- GC pauses (not applicable; Rust has no GC).
- I/O scheduling.
- Concurrent background work.

Brain's design minimizes tail variance:

- Per-core executors (no thread switching).
- Predictable I/O (io_uring).
- Smooth background work (incremental, yielding).

## 8. The "K" effect

For RECALL, K (number of results) affects latency:

- K=1: ~5 ms p99.
- K=10: ~20 ms p99.
- K=100: ~40 ms p99.
- K=1000: ~100 ms p99.

Larger K = more candidates to score; longer.

The 20 ms target is for K=10 (typical).

## 9. The "with text" effect

When the response includes memory text:

- Each memory's text fetch adds ~0.3-0.5 ms.
- For K=10: ~3-5 ms additional.

Hence the "20 ms (no text) → 30 ms (with text)" target gap.

## 10. The "filter" effect

Filters affect HNSW search:

- No filters: ~3 ms p99 search.
- Single filter (e.g., agent): ~5 ms.
- Multiple filters with low selectivity: may significantly increase search time (HNSW visits more candidates).

Targets assume typical filter selectivity (~10-50% of memories pass filter).

## 11. The "depth" effect on PLAN/REASON

Higher depth = longer:

- Depth 1: trivial; ~2 ms.
- Depth 3 (target): ~18 ms.
- Depth 10: ~50 ms.

Targets are at depth 3. Deeper queries are allowed but with higher latency.

## 12. The connection-establishment latency

A new TCP connection:

- TCP handshake: ~1 ms (local network) / ~10 ms (cross-region).
- TLS handshake (if used): additional ~1-3 ms.
- First request: above + normal request latency.

Typical SDKs use connection pooling; subsequent requests skip the connection cost.

## 13. The variability

Latencies vary run-to-run:

- ±10% is normal.
- ±30% indicates instability; investigate.

Benchmarks run multiple times; report median + std dev.

## 14. The "above 10M memories" trade

At 10M memories:

- HNSW search increases to ~5-10 ms.
- p99 RECALL: ~30-40 ms (vs 20 ms at 1M).

This is acceptable; Brain works at scale, with degraded latency.

The targets are for 1M; 10M is "supported but slower".

## 15. The acceptance check

The substrate is accepted if all p99 targets are met:

```
For each operation:
  Run a 10-minute load test.
  Capture p99 latency.
  Verify ≤ target.
```

The full benchmark suite runs nightly in CI.

## 16. The trade-offs

Brain optimizes for predictable latency, not minimum:

- A simpler design might achieve lower p50.
- Brain's design ensures p99 is not too far from p50.
- Tail is what users feel.

This is a deliberate choice. Some users prefer lower averages; Brain prefers tighter distributions.

## 17. The reporting format

Latency benchmarks report:

- Histogram (full distribution).
- Quantiles (p50, p95, p99, p99.9, p99.99).
- Mean, median, std dev.
- Min, max.

Quantiles are the primary metric. Mean is reported but secondary.

## 18. The "background work" effect

When background workers (decay, consolidation, rebuild) run:

- They yield CPU to operations.
- Operations should not see significant latency increase.
- A rebuild may add some pressure (~10-20% latency increase) but is rare.

Acceptance tests run with workers active.

## 19. The "warm-up" definition

For benchmarks:

- 5 minutes of warm-up at full load.
- Then 10 minutes of measurement.

Warm-up establishes:
- File system cache.
- Embedder cache.
- HNSW search heuristics.
- Allocator state.

Without warm-up: cold-start latencies dominate.

## 20. The realism

Targets reflect realistic AI agent workloads:

- Bursts of activity.
- Mix of operations.
- Production-scale data.

For non-realistic workloads (single-op, micro-benchmarks), latency may be much lower. Such results aren't reported as primary.

---

*Continue to [`03_throughput_targets.md`](03_throughput_targets.md) for throughput targets.*
