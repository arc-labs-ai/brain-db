# 19.02 Performance Targets

> **TL;DR.** Brain v1.0 performance gates: latency (per-operation p50/p95/p99 targets), throughput (per-shard sustained ops/s), and resource budgets (CPU, RAM, disk per shard / per node). All measured by the benchmark suite on reference hardware (16-core x86_64, 64 GiB RAM, NVMe SSD) at the 1M-memory primary scale.

## Latency Targets

The latency targets Brain v1 must meet, on the reference hardware, with the reference workload.

## 1. The setup

- Hardware: 16-core x86_64, 64 GB RAM, NVMe SSD.
- Data: 1M memories per shard.
- Load: steady-state mixed workload (70% recall, 25% encode, 5% other).
- Concurrency: 100 concurrent clients.

## 2. The targets (single-shard)

### 2.1 Cognitive primitives

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

> **Note:** §2.1 latency numbers are the per-shard, mixed-workload single-shard targets. Aggregate node-level capacity targets in [`../01_architecture/05_hardware_and_targets.md`](../01_architecture/05_hardware_and_targets.md) §7 use slightly different CPU/GPU split assumptions (e.g., ENCODE p50 ≤ 12 ms CPU); both are MUST for their respective scopes.

### 2.2 typed graph — entity operations

Measured at 100K entities per shard. Workload assumptions in §1 apply.

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

### 2.3 typed graph — statement operations

Measured at 1M statements per shard. Workload assumptions in §1 apply.

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

CREATE numbers assume the inline evidence path (≤ 8 evidence entries) and ~7 secondary index writes per `statement_create` (see [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md)). Overflow path adds ~5 ms per chunk for the overflow row write.

### 2.4 typed graph — relation operations

Measured at 1M relations per shard. Workload assumptions in §1 apply.

| Operation | p50 | p95 | p99 | p99.9 |
|---|---|---|---|---|
| RELATION_CREATE (any cardinality, ≤ 5 evidence) | 3 ms | 8 ms | 15 ms | 30 ms |
| RELATION_CREATE (ManyToOne auto-supersede) | 4 ms | 10 ms | 20 ms | 40 ms |
| RELATION_GET | 0.5 ms | 1 ms | 2 ms | 5 ms |
| RELATION_SUPERSEDE (explicit) | 4 ms | 10 ms | 20 ms | 40 ms |
| RELATION_TOMBSTONE | 1 ms | 3 ms | 5 ms | 10 ms |
| RELATION_LIST_FROM (entity + type filter, ≤ 100 results) | 2 ms | 5 ms | 10 ms | 20 ms |
| RELATION_LIST_TO (entity + type filter, ≤ 100 results) | 2 ms | 5 ms | 10 ms | 20 ms |
| RELATION_TRAVERSE (depth=1, default branching) | 5 ms | 12 ms | 25 ms | 50 ms |
| RELATION_TRAVERSE (depth=2, default branching) | 15 ms | 30 ms | 50 ms | 100 ms |
| RELATION_TRAVERSE (depth=3, default branching) | 30 ms | 60 ms | 100 ms | 200 ms |

CREATE numbers assume inline evidence + 3–4 secondary index writes per `relation_create` (RELATIONS + BY_FROM + BY_TO + BY_EVIDENCE; ×2 directional writes if symmetric). Cardinality auto-supersede adds ~1 ms for the pre-create lookup + the old-row rewrite + index flip.

TRAVERSE numbers assume default `max_branching_factor = 1000` per [`../13_retrievers/06_post_processing.md`](../13_retrievers/06_post_processing.md). Pathological super-nodes (single relation with > 1000 out-edges) truncate at the cap and emit a tracing::warn for operator visibility.

### 2.5 typed graph — deferred targets

- **ENTITY_RESOLVE (tier 3 — embedding HNSW)** lands when the entity HNSW is wired into the resolver. Target placeholder per the phase-16 doc: p50 ≤ 5 ms at 100K, ≤ 50 ms at 1M. Final numbers set here.
- **ENTITY_RESOLVE (tier 4 — LLM)** lands here with the LLM extractor. Latency is gated by the model + cache hit-rate; target is "tail under 1 s with cache warm, queued under 5 s cold."
- **Statement HNSW semantic search** — gated on the embedding worker populating the HNSW. Brain writes / reads the table inline; the semantic-search target lands with the worker.
- **Cross-shard RELATION_TRAVERSE** — gated on the query router. Brain ships same-shard only.
- **Query routing (RRF fusion across retrievers)** — gated on the query router.
- **Admin** opcodes — gated on the admin surface.

### 2.6 typed graph — schema operations

Measured at 50-definition schema documents (typical user-facing
schema size). Workload in §1 applies.

| Operation | p50 | p95 | p99 | p99.9 |
|---|---|---|---|---|
| SCHEMA_UPLOAD (parse + validate + persist) | 5 ms | 15 ms | 30 ms | 60 ms |
| SCHEMA_VALIDATE (parse + validate only) | 3 ms | 10 ms | 20 ms | 40 ms |
| SCHEMA_GET (by version) | 1 ms | 3 ms | 5 ms | 10 ms |
| SCHEMA_LIST (per-namespace) | 2 ms | 5 ms | 10 ms | 20 ms |

UPLOAD numbers assume small schemas (≤ 50 type definitions). Larger
schemas (~500 definitions) take proportionally longer for the parse +
validate pass. Persistence cost is dominated by the bulk of
secondary writes (entity_types, predicates, relation_types entries).

### 2.7 typed graph — extractor operations

Measured at single-extractor dispatch over a 4 KiB memory body.
Workload in §1 applies.

| Operation | p50 | p95 | p99 | p99.9 |
|---|---|---|---|---|
| Pattern extractor (regex match + project) | 30 µs | 70 µs | 100 µs | 300 µs |
| Classifier extractor (feature + inference) | 5 ms | 10 ms | 15 ms | 30 ms |
| Audit-row write (primary + 3 indexes) | 200 µs | 600 µs | 1 ms | 2 ms |
| `audit_by_memory` (limit 100) | 500 µs | 1 ms | 2 ms | 5 ms |
| `audit_by_extractor` (limit 100) | 500 µs | 1 ms | 2 ms | 5 ms |

Pattern numbers assume ≤ 6 patterns per extractor at typical
specificity. Classifier numbers assume the bundled `brain.basic_ner`
(small distilled BERT or rule-based fallback) — larger classifiers
scale linearly with inference cost.

ENCODE's overall P99 (§2.1, 20 ms) absorbs at most one classifier
extractor synchronously per memory; additional classifier
extractors dispatch through the near-foreground queue and don't
widen ENCODE's budget.

### 2.8 typed graph — LLM extractor

LLM extractor latency is dominated by external API round-trip
times. CI benches use mock HTTP servers; production deployments
set their own SLOs and instrument via the audit table.

| Operation | p50 | p99 |
|---|---|---|
| `LlmExtractor::predict` (cache hit) | 1 ms | 5 ms |
| `LlmExtractor::predict` (cache miss, claude-haiku) | 600 ms | 3 s |
| `LlmExtractor::predict` (cache miss, gpt-4o-mini) | 800 ms | 4 s |
| Cost-budget skip path (no LLM call) | 200 µs | 1 ms |
| `LlmCacheDb::get` round-trip | 300 µs | 1.5 ms |
| `LlmCacheDb::put` round-trip | 800 µs | 3 ms |

LLM extractors run on the background queue and don't
contribute to ENCODE's P99 budget.

### 2.9 typed graph — LexicalRetriever

LexicalRetriever is per-shard and runs against the tantivy
indexes laid out in [`../10_metadata/06_tantivy_layout.md`](../10_metadata/06_tantivy_layout.md).
Filters and BM25 parameters per [`../13_retrievers/02_lexical_retriever.md`](../13_retrievers/02_lexical_retriever.md).
The text-indexer workers maintain the indexes on the near-foreground
priority lane; benches use shard-local scale.

| Operation | p50 | p99 |
|---|---|---|
| Memory @ 100K, single-term | 10 ms | 50 ms |
| Memory @ 100K, multi-term + filter | 15 ms | 70 ms |
| Statement @ 1M, single-term | 10 ms | 50 ms |
| Statement @ 1M, multi-term + filter | 15 ms | 70 ms |
| `IndexWriter::commit` (256-doc batch) | 5 ms | 25 ms |

Query end-to-end latency (LexicalRetriever + SemanticRetriever +
GraphRetriever + RRF fusion) is in §2.10 below; the table above
covers only the per-retriever LexicalRetriever numbers.

### 2.10 typed graph — query

Query latency is dominated by per-retriever wall-time;
RRF fusion (see [`../13_retrievers/01_rrf_fusion.md`](../13_retrievers/01_rrf_fusion.md))
and the filter chain (see [`../13_retrievers/05_hybrid_query.md`](../13_retrievers/05_hybrid_query.md))
add sub-ms overhead. The gate at this section measures three
retrievers in parallel (semantic + lexical + graph at depth 1)
plus the cross-cutting operations.

Per-retriever single-call latency (sourced from the per-retriever
specs in [`../13_retrievers/`](../13_retrievers/)):

| Retriever | Single-call p50 | Single-call p99 |
|---|---|---|
| `SemanticRetriever` (Memory or Statement, push-down filters) | 5 ms | 25 ms |
| `SemanticRetriever` `Both` corpora | 8 ms | 35 ms |
| `LexicalRetriever` (Memory @ 100K, single-term) | 10 ms | 50 ms |
| `LexicalRetriever` (Statement @ 1M, single-term) | 10 ms | 50 ms |
| `GraphRetriever` (`Star` depth=1) | 5 ms | 20 ms |
| `GraphRetriever` (`Star` depth=2) | 10 ms | 40 ms |
| `GraphRetriever` (`Subgraph` depth=2) | 15 ms | 60 ms |

query end-to-end (parallel retrievers + RRF + filter):

| Operation | p50 | p99 |
|---|---|---|
| Hybrid 3-retriever, push-down filters | 10 ms | 50 ms |
| Hybrid 3-retriever, post-fusion filters only | 15 ms | 70 ms |
| Hybrid single-retriever (router-degraded) | 7 ms | 30 ms |
| `EXPLAIN` (plan-only, no execution) | 500 µs | 2 ms |
| `TRACE` (plan + per-retriever metadata, includes execution) | inherits hybrid + ~200 µs | inherits + ~1 ms |
| Filter chain (1 K candidates, full chain) | 1 ms | 5 ms |
| RRF fusion (3 lists × 100 items) | 100 µs | 500 µs |

Notes:

- The query end-to-end is approximately `max(per-retriever) + filter + fusion`, not their sum — retrievers run in parallel on the shard's executor.
- Production-scale validation (100 K memories / 1 M statements / 100 K entities) is the acceptance gate; this section validates these targets at the 10 K corpus scale used by the bench harnesses.
- `EXPLAIN` skips the executor entirely — cost is plan construction (router + cost estimate + pre-filter computation).
- Streaming queries (limit > 100; see streaming results in [`../13_retrievers/05_hybrid_query.md`](../13_retrievers/05_hybrid_query.md)) use the SUBSCRIBE wire path; per-emit latency is hybrid-query latency divided across the result chunks.

### 2.11 Perf gates

All targets above are measured on the dev workstation; production-reference numbers (16-core / 64 GB / NVMe per §1) are revalidated by the CI suite.

- §2.2 entity targets at 100K entities.
- §2.3 statement targets at 1M statements.
- §2.4 relation targets at 1M relations.
- §2.6 schema targets at 50 definitions.
- §2.7 extractor targets at single-extractor dispatch.
- §2.8 LLM extractor targets at cache-hit + cost-budget skip + mock-API miss.
- §2.9 LexicalRetriever targets at 100K memory / 1M statement scale.
- §2.10 query targets at 10K corpus / 3 retrievers.

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

When Brain just started and caches are cold:

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

Typical clients use connection pooling; subsequent requests skip the connection cost.

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

Brain is accepted if all p99 targets are met:

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

## Throughput Targets

The throughput targets Brain v1 must meet.

## 1. Per-shard targets

| Operation | Target (ops/sec/shard) |
|---|---|
| ENCODE | ≥ 5,000 |
| RECALL | ≥ 20,000 |
| PLAN (depth 3) | ≥ 8,000 |
| REASON (depth 3) | ≥ 5,000 |
| FORGET | ≥ 10,000 |
| LINK | ≥ 30,000 |
| UNLINK | ≥ 30,000 |

These are MUST targets at 1M memories per shard, on reference hardware.

## 2. The "mixed workload" target

A realistic mix (70% recall, 25% encode, 5% other):

- Combined throughput: ≥ 10,000 ops/sec/shard.
- Latency: stays within p99 targets.

This is the primary target — it reflects actual deployments.

## 3. The multi-shard target

For a 16-shard node:

- Aggregate throughput: ≥ 100,000 ops/sec.
- Per-shard: ~6,000-8,000 ops/sec average.

(Per-shard reduces because not all operations target one shard; some shards may be idle while others are busy.)

## 4. The "burst" tolerance

Brain handles bursts above sustained:

- 2× sustained for 10 seconds: tolerated, latency may spike.
- 5× sustained for 1 second: shed via Overloaded.

Bursts are common; tolerance prevents transient overloads from cascading.

## 5. The "max throughput" exploration

Beyond targets:

- Max sustained: ~20-50K ops/sec/shard for RECALL (bottleneck: HNSW search).
- Max sustained: ~10-20K ops/sec/shard for ENCODE (bottleneck: WAL fsync).

These define the hard ceilings; targets are conservative below.

## 6. The bottleneck identification

For each operation, the limiting factor:

| Operation | Bottleneck |
|---|---|
| ENCODE | WAL fsync (NVMe ~50K IOPS) |
| RECALL | HNSW search + embedder |
| PLAN | Edge traversal (memory-bound) |
| REASON | Edge traversal (memory-bound) |
| FORGET | WAL fsync |
| LINK | WAL fsync |

WAL fsync is the dominant bottleneck for writes. Reads are CPU-bound.

## 7. The "WAL group commit" effect

Group commit batches multiple ENCODEs into a single fsync:

- 1 ENCODE / sec: 1 fsync.
- 1000 ENCODE / sec: ~50-100 fsyncs (batched into groups of 10-20).
- 10000 ENCODE / sec: ~500 fsyncs (at limit of NVMe).

Group commit lets ENCODE throughput exceed the raw fsync rate.

## 8. The connection-pool effect

For a single client connection:
- Limited by request-response sequencing.
- ~1000 ops/sec from one connection (if synchronous).

For 100 connections:
- ~10,000 ops/sec aggregate (if each does ~100/sec).

Many parallel connections are needed for high throughput. Clients handle this.

## 9. The "concurrent" target

Brain supports:

- 10K concurrent connections per node.
- 100K concurrent in-flight requests.
- 1M streams per second (open + close).

These are MUST. Beyond these, behavior is "best effort".

## 10. The pipelining throughput

With pipelining (multiple requests in flight per connection):

- Per-connection: ~10K ops/sec (vs ~1K without).
- Aggregate: scales with the number of active connections.

Brain's protocol supports pipelining; clients use it.

## 11. The "load step" test

Throughput tests:

- Start at 1K ops/sec.
- Increase by 1K/sec every 10 seconds.
- Stop when latency p99 exceeds target.

The crossover point is Brain's "knee" — sustainable throughput before latency degrades.

## 12. The "sustain" requirement

Throughput must be sustainable, not just peak:

- Run at target for 10 minutes.
- Verify no latency degradation.
- Verify no resource exhaustion.

A node that hits target for 1 minute then collapses doesn't pass.

## 13. The data-freshness effect

When the data is fresh (recent encodes, no rebuild yet):

- HNSW may have many tombstones (slow).
- Throughput drops.

Acceptance tests don't run this scenario (it's a special case). Operators understand and trigger rebuilds proactively.

## 14. The "saturation" indicators

When throughput approaches max:

- p99 latency rises (request queue grows).
- CPU utilization climbs.
- Occasional Overloaded errors.

Monitor and back off before saturation. Operators scale before this point.

## 15. The "single core" reality

Each Glommio shard runs on one core. So:

- One shard's max throughput is bounded by what one core can do.
- More shards = more throughput, but each shard is limited.

Reasonable per-core throughput: ~10-50K ops/sec depending on operation.

For very high throughput, scale shards (more cores).

## 16. The cross-shard hit

For multi-shard fan-out operations:

- The fan-out coordinator does extra work.
- Each shard does its work in parallel.
- Aggregate doesn't quite scale linearly.

For 16 shards: ~10-12× speedup vs 1 shard, not 16×.

Most operations are single-shard, so this is rarely the main path.

## 17. The throughput vs latency trade-off

Higher throughput typically means higher latency:

- More requests in flight.
- More queueing.
- Larger group-commit batches.

Brain's design balances:
- Group commits batch up to 20 ms.
- Latency p99 stays within targets.

For deployments wanting lower latency at lower throughput: configurable batch sizes.

## 18. The reporting

Throughput benchmarks report:

- Sustained ops/sec at each load level.
- Latency at each load level.
- Resource utilization (CPU, memory, disk, network).
- Errors / second (should be 0 unless overloaded).

Combined picture shows where Brain's "sweet spot" is.

## 19. The historical comparison

Each release compares to previous:

```
Release 1.0:
  ENCODE throughput: 5,200 ops/sec (target: 5,000) ✓

Release 1.1:
  ENCODE throughput: 5,500 ops/sec (target: 5,000) ✓ (+5.8%)
```

Improvements are tracked; regressions are flagged.

## 20. The "future horizon"

V1 is single-node; throughput per machine is the limit.

A future clustered major version will scale across machines:

- Linear scaling for read-heavy workloads.
- Sub-linear for write-heavy (due to replication overhead).

The targets in this spec are v1 single-node. Future versions will have their own.

---

## Resource Targets

The resource utilization targets — CPU, memory, disk, network — Brain v1 must meet.

## 1. The setup

Reference workload at 1M memories per shard, sustained 10K ops/sec/shard.

## 2. CPU targets

| Target | Per shard core |
|---|---|
| Sustained at target throughput | ≤ 70% utilization |
| Idle baseline (workers running, no requests) | ≤ 5% |
| Peak burst | ≤ 95% (transient) |

The 70% target leaves headroom for spikes. Sustained > 80% indicates need for scaling.

## 3. Memory targets

For 1M memories per shard:

| Component | RAM |
|---|---|
| HNSW index | ~150-200 MB |
| Embedder model (loaded once per node) | ~150 MB |
| Caches (file system, embedder) | 200-500 MB |
| Connections, tasks | 50-200 MB |
| Total per-shard | ~500-1000 MB |

For a 16-shard node: ~8-16 GB. Comfortable on 32 GB+ machines.

## 4. Memory growth

Memory should grow proportionally to data:

- 100K memories: ~150 MB per shard.
- 1M memories: ~700 MB per shard.
- 10M memories: ~6 GB per shard.

For 10M memories per shard, RAM is significant — operators should size accordingly.

## 5. Disk targets

For 1M memories:

| Component | Disk |
|---|---|
| Arena | ~6 GB (1.6 KB per slot × 1M) |
| Metadata | ~1 GB |
| WAL (active + retained) | ~500 MB - 1 GB |
| HNSW snapshot (if kept) | ~200 MB |
| Total per shard | ~8-10 GB |

For 16 shards: ~150 GB. Manageable on 1 TB+ NVMe.

## 6. Disk I/O bandwidth

Sustained:

- Reads: ~50-100 MB/s/shard (cold reads + WAL replay during recovery).
- Writes: ~20-50 MB/s/shard at target write rate.

Modern NVMe: ~3,000+ MB/s sequential, ~50K IOPS random. Plenty of headroom.

## 7. Disk IOPS

Sustained:

- WAL: ~500-2000 IOPS per shard at peak (with group commit).
- Arena: ~10K IOPS reads, ~5K writes (cached for hot data).
- Metadata: ~5K IOPS.

Total per shard: up to ~30K IOPS. NVMe handles easily; SATA SSDs may limit.

## 8. Network targets

For 10K ops/sec:

- Inbound: ~50-200 Mbps (small requests).
- Outbound: ~100-500 Mbps (responses with text).

For 16 shards / 100K ops/sec aggregate: ~1-5 Gbps.

10 Gbps NIC is plenty.

## 9. The "memory leak" check

Run for 48 hours at target load. Compare memory usage:

- Should stabilize within first hour.
- Should not grow beyond ~10% over the run.

Growing memory indicates a leak; investigate.

## 10. The "disk growth" check

At target write rate for 24 hours:

- Disk usage grows by expected amount (number of memories × ~10 KB).
- WAL stays bounded (retention worker keeps it within configured limit).
- No stale snapshots accumulating.

## 11. The "open files" budget

Per node:

- Arena files (one per shard): 16.
- WAL files (per shard, multiple segments): ~20 per shard = 320.
- redb files: 16.
- Snapshots: 16.
- Connections (file descriptors): up to 10K.
- Internal: ~50.

Total: ~10K-12K file descriptors.

Set ulimit accordingly: 65536 fd is comfortable.

## 12. The "thread count"

Brain uses minimal threads:

- One Glommio thread per shard (executor).
- A few helper threads (embedder, syscall offload).
- Tokio runtime for connection layer (handful of threads).

Total: ~20-30 threads for a 16-shard node. Light.

## 13. The "context switch" rate

With Glommio:

- No thread context switches under normal operation (each shard is one thread).
- Async task switches are user-space (no kernel involvement).

Context switch rate stays low: ~1000-10000/sec total. Vs traditional thread-per-request: ~100K-1M/sec.

This contributes to predictable latency.

## 14. The "io_uring queue" depth

Per shard: ~256 in-flight I/O operations.

This is enough for parallel I/O without exhausting kernel resources.

## 15. The cache-hit-rate targets

Embedder cache:
- Hit rate ≥ 70% in steady state (typical workloads have repeating cues).

File system cache (kernel-managed):
- Hit rate ≥ 90% for active data.

Cache misses are normal but should be the minority.

## 16. The "background work" budget

Workers should consume:
- < 5% CPU on average.
- < 50 MB/s disk I/O on average.
- Should yield generously to operations.

If a worker exceeds: tune interval or batch size.

## 17. The OOM-protection budget

Brain should:
- Not exceed configured memory limit.
- Shed load before approaching the limit.
- Refuse new operations rather than OOM.

Tested via cgroup memory limits + load testing.

## 18. The "fairness" across shards

Resource usage should be roughly even across shards:
- ~Equal CPU.
- ~Equal memory.
- ~Equal disk.

Hot shards (uneven distribution) indicate hashing or workload issues.

## 19. The reporting

Resource benchmarks report:
- CPU utilization over time.
- Memory usage over time.
- Disk I/O rates.
- Network rates.
- Cache hit rates.
- Per-component breakdowns where useful.

Operators use these to validate behavior in their environment.

## 20. The trade-offs

Brain prioritizes predictability over absolute minimum:

- Larger memory budget for caches (faster).
- More background work to maintain index quality (less rebuild surprise).
- Periodic snapshots (more disk for faster recovery).

These costs are intentional. Operators wanting different trade-offs adjust configuration.

---

*Continue to [`03_recall_quality.md`](03_recall_quality.md) for recall quality criteria.*
