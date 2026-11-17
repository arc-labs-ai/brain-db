# 19.02 Performance Targets

> **v1.0 status — advisory, not a lock gate.** Reference-hardware performance verification is **optional for v1.0** — see [`06_complete_acceptance.md`](06_complete_acceptance.md) §"Performance acceptance". The numbers here are engineering targets to measure post-lock, not v1.0 lock criteria.
>
> **Precedence.** This file states one number per operation. [`../01_architecture/05_hardware_and_targets.md`](../01_architecture/05_hardware_and_targets.md) §7 and [`06_complete_acceptance.md`](06_complete_acceptance.md) state the same operations **split by write path** — CPU-embedded text, GPU-embedded text, and `ENCODE_VECTOR_DIRECT` — and they agree with each other. Where a single number here disagrees with that split, the split is authoritative, because the paths differ by more than measurement noise: a text ENCODE pays a model forward pass that a pre-supplied vector does not. The disagreement is concentrated in FORGET / PLAN / REASON / STATEMENT_CREATE / RELATION_CREATE / entity-resolve latency and in the throughput tables.

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

- **ENTITY_RESOLVE (tier 3 — embedding HNSW)** is wired into the resolver (embed-on-create + top-k entity-HNSW search under a scope+type filter); what remains is the reference-hardware measurement, like every other §19.02 target. Target: p50 ≤ 5 ms at 100K entities, ≤ 50 ms at 1M.
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

Extraction is **not** part of ENCODE's latency: every tier (pattern,
classifier, LLM) runs in the per-shard extractor worker, which ENCODE
feeds via a single non-blocking enqueue. The classifier numbers above
are a worker-throughput target, not a component of ENCODE's p99.

### 2.8 typed graph — LLM extractor

LLM extractor latency is dominated by external API round-trip
times. CI benches use mock HTTP servers; production deployments
set their own SLOs and instrument via the audit table.

| Operation | p50 | p99 |
|---|---|---|
| `LlmExtractor::predict` (cache hit) | 1 ms | 5 ms |
| `LlmExtractor::predict` (cache miss, claude-haiku) | 600 ms | 3 s |
| `LlmExtractor::predict` (cache miss, gpt-4o-mini) | 800 ms | 4 s |
| Short-circuit path (no LLM call) | 200 µs | 1 ms |
| `LlmCacheDb::get` round-trip | 300 µs | 1.5 ms |
| `LlmCacheDb::put` round-trip | 800 µs | 3 ms |

The short-circuit row is the cost of a dispatch that resolves without reaching
a provider at all: a degraded `LlmExtractor` returns `SkippedDisabled` with the
captured reason and writes its audit row
(`crates/brain-extractors/src/llm/extractor.rs:209-215`, status at
`crates/brain-extractors/src/framework/extractor.rs:389`). What the figure
measures is the audit write and nothing else, which is why it is the same
200 µs / 1 ms as the audit-row write in §2.7 — the two numbers are the same
measurement reached by two paths, and they should move together.

It is **not** a spend ceiling. A tenant's own provider key is never rationed:
spend accumulates in the durable `llm_spend` ledger and no call is refused for
cost. A `cost_budget` declared on an `llm` extractor in the schema DSL parses
and is logged as unenforced
(`crates/brain-extractors/src/materialize.rs:438-455`). Benchmarking a cost
gate would be benchmarking something that does not run.

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
and the filter chain (see [`../13_retrievers/05_retrieval_query.md`](../13_retrievers/05_retrieval_query.md))
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
| Retrieval, 3-retriever, push-down filters | 10 ms | 50 ms |
| Retrieval, 3-retriever, post-fusion filters only | 15 ms | 70 ms |
| Retrieval, single-retriever (router-degraded) | 7 ms | 30 ms |
| `EXPLAIN` (plan-only, no execution) | 500 µs | 2 ms |
| `TRACE` (plan + per-retriever metadata, includes execution) | inherits retrieval + ~200 µs | inherits + ~1 ms |
| Filter chain (1 K candidates, full chain) | 1 ms | 5 ms |
| RRF fusion (3 lists × 100 items) | 100 µs | 500 µs |

Notes:

- The query end-to-end is approximately `max(per-retriever) + filter + fusion`, not their sum — retrievers run in parallel on the shard's executor.
- Production-scale validation (100 K memories / 1 M statements / 100 K entities) is the acceptance gate; this section validates these targets at the 10 K corpus scale used by the bench harnesses.
- `EXPLAIN` skips the executor entirely — cost is plan construction (router + cost estimate + pre-filter computation).
- Streaming queries (limit > 100; see streaming results in [`../13_retrievers/05_retrieval_query.md`](../13_retrievers/05_retrieval_query.md)) use the SUBSCRIBE wire path; per-emit latency is retrieval-query latency divided across the result chunks.

### 2.11 Perf gates

**No target here is machine-enforced**; every number is measured and compared by hand.

`perf-bench.yml` runs `relation_traverse` (brain-planner) and `lexical_retrieve` (brain-index) on manual dispatch, and `just prod-bench` runs the same pair locally. Both benches report p50 measurements and assert nothing; a run fails only on a panic or a timeout. Reading the numbers, and comparing them against a Criterion baseline on the machine that produced them, is the operator's job. There is no absolute in-process latency gate because the §2.4 traversal targets are specified at 1M relations per shard and the §2.9 lexical targets at 100K memories / 1M statements, both on the §1 reference hardware, while these two harnesses run roughly 10K-scale fixtures. A millisecond bound asserted against those fixtures on arbitrary hardware measures the machine rather than the implementation. The targets in §2.4 and §2.9 are verified by the acceptance suite on reference hardware, which is where a regression against them is caught. The pair is dispatched rather than run per-PR because each takes minutes.

Every number in this section is a target awaiting measurement on reference hardware (16-core / 64 GB / NVMe per §1), consistent with the advisory status recorded at the top of this file. The CI box is a cloud runner, not the reference machine, which is why the pair worth dispatching there is the traversal and index benches — CPU-and-redb bound and comparatively portable — rather than the embedder- or device-bound figures, whose numbers say more about the host than about Brain.

- §2.2 entity targets at 100K entities.
- §2.3 statement targets at 1M statements.
- §2.4 relation targets at 1M relations.
- §2.6 schema targets at 50 definitions.
- §2.7 extractor targets at single-extractor dispatch.
- §2.8 LLM extractor targets at cache-hit + short-circuit + mock-API miss.
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

**The embedder line is the one that decides whether the rest matters.** On a cache miss it is the largest single term, and it is a *per-core* cost: the encode is handed to the shard executor's blocking pool, whose intra-op workers inherit that thread's CPU affinity, so Candle's matmul fan-out is confined to the shard's own core rather than spreading across the box (`crates/brain-ops/src/embedding.rs:16-61`). Nothing batches it either — the CPU dispatcher runs each request through the model independently and assembles no batches (`crates/brain-embed/src/dispatcher/cpu.rs:1-7`).

Three consequences follow, and they govern every throughput figure below:

1. **The ~15 ms miss is a single-core forward-pass figure**, so it scales with that core's capability and with input length — not with core count. A wider machine buys more shards, each with its own embedder lane; it does not make this line shorter.
2. **The cache-hit/miss split is the dominant lever on RECALL p99**, which is why the embedder cache hit-rate target (§15 of Resource Targets) is a performance requirement and not merely a cost one.
3. **Reproducing this breakdown on unlike hardware requires re-measuring this line first.** It is the term most sensitive to ISA and clock, and the `brain-embed` throughput bench deliberately asserts *no* absolute gate for exactly that reason, treating the machine-local Criterion baseline as the real regression signal (`crates/brain-embed/benches/throughput.rs:3-6`).

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

**The ENCODE row is a storage-path figure.** It is reachable by `ENCODE_VECTOR_DIRECT`, where the caller supplies the vector and the cost is WAL plus metadata. A **text** ENCODE additionally pays one unbatched model forward pass on the shard's own core (§6), which puts its ceiling two to three orders of magnitude lower. The path-split throughput targets in [`06_complete_acceptance.md`](06_complete_acceptance.md) §"Throughput" are authoritative for that distinction, and the same caveat applies to the mixed-workload figure in §2 below, whose 25% ENCODE share is read as the direct-vector path.

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

Brain absorbs bursts above sustained by **queueing**, not by shedding:

- 2× sustained for 10 seconds: tolerated, latency rises.
- 5× sustained for 1 second: tolerated, latency rises further.

**There is no request-level load shedding and no rate limiter.** A shard admits up to its in-flight bound (15 at shipped defaults, §7) and then *parks* on its request channel until a permit frees. Parking the receive also parks the cheap requests behind it — Ping, slot allocation — and that is the deliberate trade: in-order service of a saturated shard was preferred over a second scheduling lane with unreasoned starvation behaviour (`crates/brain-server/src/shard/mod.rs:5014-5020`).

What this means for a burst test: excess load appears as **latency**, never as rejections. There is no knee at which Brain starts refusing data-plane work to protect itself, so a burst that exceeds capacity is absorbed as queue depth for as long as clients keep waiting. Operators must detect saturation from latency and queue-depth signals, not from an error rate.

`Overloaded` does exist as a wire code, but it does not mean "the server is shedding your writes". It is raised by the subscription path — a subscriber that falls too far behind the event stream, or a connection that exhausts its stream ids (`crates/brain-ops/src/handlers/subscribe.rs:1009-1073`) — and it carries a 1 s `retry_after_ms` hint (`crates/brain-server/src/network/dispatch.rs:1420`).

Admission control does exist, but one layer out and on connections rather than requests: a listener accepts at most `max_connections` (default **4096**) and `max_connections_per_ip` (default **64**), counting refusals in `brain_connections_rejected_total` (`crates/brain-server/src/network/connection.rs:143-167`).

## 5. The "max throughput" exploration

Beyond targets:

- Max sustained: ~20-50K ops/sec/shard for RECALL (bottleneck: HNSW search).
- Max sustained: ~10-20K ops/sec/shard for ENCODE (bottleneck: WAL fsync).

These define the hard ceilings; targets are conservative below.

## 6. The bottleneck identification

For each operation, the limiting factor:

| Operation | Bottleneck |
|---|---|
| ENCODE (text) | Embedder forward pass — one per request, on the shard's own core |
| ENCODE_VECTOR_DIRECT | WAL fsync (NVMe ~50K IOPS), amortised by group commit |
| RECALL | Embedder (on cache miss) + HNSW search |
| PLAN | Edge traversal (memory-bound) |
| REASON | Edge traversal (memory-bound) |
| FORGET | WAL fsync |
| LINK | WAL fsync |

For writes that supply their own vector, WAL fsync is the dominant cost, and group commit is what lets throughput exceed the raw fsync rate (§7). For a **text** ENCODE it is not: the request pays a model forward pass before any of that, inline and unbatched, so the ceiling is the per-core embedder rate rather than the device's IOPS. This is why the two paths carry separate targets throughout this section — `ENCODE_VECTOR_DIRECT` is a storage benchmark and text ENCODE is substantially a model-inference benchmark.

Reads are CPU-bound, with the same caveat: a cache-missing cue pays the same forward pass.

## 7. The "WAL group commit" effect

Group commit batches concurrent WAL appends into a single `fdatasync`. The committer waits up to **100 µs** after a batch's first record for more records, or flushes early once the buffer reaches **60 KiB** (`crates/brain-storage/src/wal/group_commit.rs:41-59`).

The window is what bounds the amortisation, and it is deliberately short — tens of microseconds, not tens of milliseconds — because it is added latency on every write that opens a batch. What it buys depends entirely on how many records are genuinely in flight inside that window:

- A write arriving alone pays one fsync and 100 µs of waiting for company that never comes.
- Writes arriving concurrently coalesce, up to the point where either the window closes or 60 KiB of records have accumulated.

Concurrency inside the window is therefore the whole mechanism, and the shard's request admission is sized from this configuration so the two cannot drift: the in-flight bound is `max_batch_bytes / 4 KiB` — **15** at shipped defaults, with 4 KiB taken as the dominant `EncodePayload` record size — so the shard admits exactly as many concurrent requests as the fsync-amortisation window can use (`crates/brain-server/src/shard/mod.rs:4637-4650`). Widening the window and the batch budget widens the concurrency with them.

The payoff: a batch of *n* records costs one fsync instead of *n*, so ENCODE throughput can exceed the device's raw fsync rate. The ceiling is not the device, though — see §6: a text ENCODE's dominant cost is upstream of the WAL.

## 8. The connection-pool effect

For a single client connection:
- Limited by request-response sequencing.
- ~1000 ops/sec from one connection (if synchronous).

For 100 connections:
- ~10,000 ops/sec aggregate (if each does ~100/sec).

Many parallel connections are needed for high throughput. Clients handle this.

## 9. The "concurrent" target

Two different quantities get conflated here, and they differ by four orders of magnitude.

**Connections** are cheap and plentiful. The shipped caps are 4096 per node and 64 per source IP (`crates/brain-server/src/network/connection.rs:143-167`); a connection beyond them is refused at accept time and counted.

**In-flight requests** are deliberately scarce. A shard holds at most `max_batch_bytes / 4 KiB` = **15** concurrent dispatches, derived from the WAL group-commit batch (§7), so a node's in-flight ceiling is 15 × shard count — tens, not hundreds of thousands. This is not an oversight to be raised: in-flight concurrency beyond what the fsync window can coalesce adds queueing without adding throughput, and the bound is what keeps the two in step.

So thousands of connections may be open while only tens of requests are executing, and the rest are parked (§4). Capacity planning should size connections and in-flight work separately; a client that needs more throughput per connection pipelines (§10) rather than expecting more concurrency per shard.

## 10. The pipelining throughput

With pipelining (multiple requests in flight per connection):

- Per-connection: ~10K ops/sec (vs ~1K without).
- Aggregate: scales with the number of active connections, up to the per-shard in-flight bound (§9).

Pipelining works because the shard imposes **no ordering** on replies: each dispatch is detached onto the shard's executor and the protocol correlates responses by `stream_id`, so a client may have several requests outstanding and receive them out of order (`crates/brain-server/src/shard/mod.rs:4980-4993`). A client that serialises request-response round trips is paying its full network latency per operation for no reason.

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

- p99 latency rises (the request queue grows).
- CPU utilization climbs.
- Shards begin parking on their request channels (§4).

Saturation is visible as **latency and queue depth only**. Because there is no request-level shedding, the error rate stays flat right through the knee — so an alert built on errors will not fire, and an operator watching one will see a healthy service while every caller waits. Monitor latency percentiles and the derivation/extraction queue-depth gauges, and scale before the queue becomes the service.

## 15. The "single core" reality

Each Glommio shard runs on one core, and the embed offload is confined to that same core by CPU affinity (§12). So:

- One shard's max throughput is bounded by what one core can do, for *every* part of a request including model inference.
- More shards = more throughput; each shard stays limited.

Reasonable per-core throughput: ~10-50K ops/sec for storage- and index-bound operations. A **text** ENCODE or a cache-missing RECALL is orders of magnitude below that, because it includes a forward pass that no amount of cores on the box makes faster (§3, §6).

For very high throughput, scale shards (more cores). This is the one scaling axis that works for the embedding path: shards are the parallelism.

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

Brain's design balances these by keeping the batching window very short relative to the latency targets: 100 µs, against ENCODE p99 budgets measured in tens of milliseconds, so the amortisation costs at most a fraction of a percent of the budget it protects (§7). The alternative — a tens-of-milliseconds window, which would coalesce far more aggressively — would consume the entire p99 target to save fsyncs.

Operators who want a different balance tune `commit_window` and `max_batch_bytes` together, and should expect the shard's in-flight bound to move with them (§9): the two are derived from one another by design.

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
| Embedder model (BGE-small weights, loaded once and shared by every shard) | ~130 MiB |
| Caches (file system, embedder) | 200-500 MB |
| Connections, tasks | 50-200 MB |
| Total per-shard | ~500-1000 MB |

For a 16-shard node: ~8-16 GB. Comfortable on 32 GB+ machines.

The embedder row is once per node, not per shard: the model handle is `Arc`-shared, so clones are cheap and every shard searches the same loaded weights (`crates/brain-embed/src/dispatcher/cpu.rs:15-19`). Sharding multiplies the *inference lanes*, not the resident weights.

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
| Arena | ~1.6 GB (1600 B per slot × 1M, plus a 4 KiB header) |
| Metadata | ~1 GB |
| WAL (active + retained) | ~500 MB - 1 GB |
| HNSW snapshot (if kept) | ~200 MB |
| Total per shard | ~3.5-4 GB |

The arena figure is exact rather than estimated: the file is `4096 + capacity_slots × 1600` bytes, with `SLOT_SIZE_V1 = 1600` (`crates/brain-storage/src/arena/file.rs:54,159`). Of those 1600 bytes the 384-float vector is 1536, so the arena's size is set by vector width and is independent of how long the memory text is.

For 16 shards: ~55-65 GB. Comfortable on 1 TB NVMe, and the storage-acceptance budget of ~10 GB per shard in [`06_complete_acceptance.md`](06_complete_acceptance.md) therefore has substantial headroom rather than being a tight fit.

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

- One Glommio thread per shard (executor), pinned to a fixed CPU when `pin_cpu` is configured and unbound otherwise (`crates/brain-server/src/shard/mod.rs:2916-2918`).
- Per shard, a blocking-pool thread for embedding, plus that thread's own intra-op pool for the model's matmul fan-out. The intra-op pool is built lazily, one per blocking thread, and its workers inherit the blocking thread's CPU affinity — so it confines the fan-out to the shard's own core rather than providing parallelism (`crates/brain-ops/src/embedding.rs:16-38`). A process-global pool was rejected precisely because it would funnel every shard's embeds through one worker and serialise embedding across the node.
- Tokio runtime for the connection layer (handful of threads).

Total: ~20-30 threads for a 16-shard node, light in the sense that matters — the resident thread count is modest and, more importantly, no thread competes with a shard's reactor for its own core.

## 13. The context-switch rate

With Glommio:

- No kernel context switches between requests under normal operation — each shard is one thread, and concurrent requests are detached tasks on that thread's executor rather than threads of their own.
- Task switches are user-space scheduling, with no kernel involvement.

Context-switch rate stays low: ~1000-10000/sec total, against ~100K-1M/sec for a thread-per-request design.

The one place a shard does cross a thread boundary on the hot path is the embed offload (§12), which is deliberate: a forward pass held on the reactor would block every other in-flight request on that shard for its full duration, which is a far larger latency cost than the switch.

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

Brain bounds its own memory through fixed admission budgets rather than through reactive shedding:

- The per-shard in-flight request bound (§9) caps how many requests can hold working memory at once.
- The derivation and LLM stages carry explicit byte budgets, surfaced as the `brain_derivation_admission_budget_bytes` / `_available_bytes`, `brain_derivation_bytes_in_flight` and `brain_llm_bytes_in_flight` gauges.

There is **no** mechanism that watches total memory and sheds data-plane load as a limit approaches, and no data-plane operation is refused on memory pressure (§4). Under a cgroup limit, the outcome of exceeding it is the OOM killer, not graceful refusal — so operators must size the limit above the sum of the budgets above rather than relying on Brain to back off inside it.

Tested via cgroup memory limits + load testing, where the property under test is that the budgets hold steady-state usage below the limit, not that Brain degrades gracefully at it.

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
