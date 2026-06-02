# 19.06 Combined Acceptance Gate

What "the typed graph is done" means. Concrete tests that must pass.

## Functional acceptance

### Schemaless mode

- [ ] With no schema declared, all baseline schemaless-mode acceptance tests pass.
- [ ] Clients (no typed-graph types) work normally when no schema is declared.
- [ ] Data directories created with no schema work with or without later declaring one.
- [ ] RECALL latency under hybrid retrieval: P50 ≤ 10 ms, P99 ≤ 50 ms (warmed shard).

### Schema operations

- [ ] Upload valid schema: succeeds, version incremented.
- [ ] Upload invalid schema (syntax error): rejected with specific error location.
- [ ] Upload schema with unresolved type reference: rejected.
- [ ] Upload schema with conflicting predicate kind: rejected.
- [ ] Schema versions queryable by version_id.
- [ ] Schema update with non-breaking change: accepted.
- [ ] Schema update with breaking change: requires explicit migration flag.

### Entity operations

- [ ] Create entity: succeeds, EntityId returned.
- [ ] Resolve entity (exact match): returns existing entity with confidence 1.0.
- [ ] Resolve entity (fuzzy): returns match above threshold with appropriate confidence.
- [ ] Resolve entity (embedding): returns match above threshold.
- [ ] Resolve entity (no match): creates new entity.
- [ ] Ambiguous resolution: writes pending audit, returns Ambiguous outcome.
- [ ] Merge entities: statements/relations re-targeted to survivor.
- [ ] Unmerge (within grace): reverses merge.
- [ ] Unmerge (past grace): rejected.
- [ ] Rename entity: canonical_name updated, old name in aliases, EntityId stable.

### Statement operations

- [ ] Create Fact: succeeds, returns StatementId.
- [ ] Create Preference: succeeds; if a current Preference with same (subject, predicate) exists, old is superseded.
- [ ] Create Event: succeeds; event_at required; valid_from/valid_to must be None.
- [ ] Statement supersession chain: traversable via chain_root.
- [ ] Contradicting Facts: both stored, contradiction detectable.
- [ ] Tombstone: soft delete with grace; statement removed from default queries.
- [ ] Retract: hard delete; statement gone after grace.
- [ ] Statements with same (subject, predicate, object) but different evidence: confidence aggregated correctly.

### Relation operations

- [ ] Create relation: succeeds.
- [ ] Cardinality enforcement: many-to-one prevents two current relations.
- [ ] Symmetric relations: indexed both ways, queryable from either side.
- [ ] Traversal depth 1-3: returns expected paths.
- [ ] Traversal depth >5: rejected as exceeding cap.
- [ ] Cycle detection: traversal terminates.

### Extraction

- [ ] Pattern extractor: runs synchronously, output visible immediately after ENCODE.
- [ ] Classifier extractor: runs near-foreground; output visible within 100 ms.
- [ ] LLM extractor (cache hit): returns from cache without LLM call.
- [ ] LLM extractor (cache miss): calls LLM, caches result, schema-validates.
- [ ] LLM extractor (invalid output): retries once, then drops with audit.
- [ ] LLM extractor (over budget): skips with metric.
- [ ] Extractor idempotency: re-running on same memory produces identical results (modulo LLM cache TTL).

### Query

- [ ] Free-text query: semantic + lexical retrievers invoked, fused.
- [ ] Entity-anchored query: graph retriever invoked, weighted appropriately.
- [ ] Time-filtered query: temporal filter applied post-fusion.
- [ ] Type-filtered query: kind/predicate filter applied.
- [ ] Confidence filter: respected.
- [ ] EXPLAIN: returns plan without execution.
- [ ] TRACE: returns plan + execution metadata + per-retriever ranks.
- [ ] Streaming results: large queries stream, client cancellation works.

### Provenance and versioning

- [ ] Every statement has evidence list and extractor metadata.
- [ ] FORGET cascade: statements depending on forgotten memory get evidence list updated; orphaned statements get tombstoned.
- [ ] Confidence aggregation: matches the formula in the spec.
- [ ] Schema-version flagging: statements from old schema are marked stale after schema update.
- [ ] Re-extraction: produces new statements supersession-chained from old.

## Performance acceptance

### Latency (P50 / P99, single shard, warm)

Targets below assume the default text-input path (CPU embedding ~5–10 ms). The GPU-batched and `ENCODE_VECTOR_DIRECT` (pre-supplied vector) paths have separate, lower targets — see [`../01_architecture/05_hardware_and_targets.md`](../01_architecture/05_hardware_and_targets.md) §7.1.

- [ ] ENCODE (text, CPU embedding): P50 ≤ 12 ms, P99 ≤ 25 ms.
- [ ] ENCODE (text, GPU embedding): P50 ≤ 3 ms, P99 ≤ 8 ms.
- [ ] ENCODE_VECTOR_DIRECT (pre-supplied vector, no embedding): P50 ≤ 1 ms, P99 ≤ 5 ms.
- [ ] ENCODE + classifier extractors (post-encode async): P50 ≤ 5 ms added to extractor-completion event, P99 ≤ 20 ms.
- [ ] STATEMENT_CREATE: P50 ≤ 1 ms, P99 ≤ 5 ms.
- [ ] RELATION_CREATE: P50 ≤ 1 ms, P99 ≤ 5 ms.
- [ ] QUERY (hybrid, default top_n): P50 ≤ 10 ms, P99 ≤ 50 ms.
- [ ] QUERY (entity-anchored, 2-hop graph): P50 ≤ 15 ms, P99 ≤ 100 ms.
- [ ] Entity resolution (tiers 1–2, exact+fuzzy): P50 ≤ 1 ms.
- [ ] Entity resolution (tier 3, embedding HNSW): P50 ≤ 10 ms.

### Throughput

Per-shard sustained, distinguishing CPU vs GPU embedding paths (matching [`../01_architecture/05_hardware_and_targets.md`](../01_architecture/05_hardware_and_targets.md) §7.1.4):

- [ ] ENCODE throughput (text, CPU embedding): ≥ 100/s per shard.
- [ ] ENCODE throughput (text, GPU embedding): ≥ 1K/s per shard.
- [ ] ENCODE_VECTOR_DIRECT throughput (storage-only): ≥ 100K/s per shard.
- [ ] STATEMENT_CREATE throughput: ≥ 10K/s per shard.
- [ ] QUERY throughput (mixed workload): ≥ 1K/s per shard.

### LLM extraction throughput

- [ ] LLM extractor pipeline (with cache, mixed hit rate): meets configured rate without dropping under nominal load.
- [ ] LLM extractor queue depth metric: bounded, never grows unbounded.

## Storage acceptance

- [ ] 1M memories + 500K statements + 10K entities + 5K relations fits in ~10 GB.
- [ ] Tantivy index commits and reopens cleanly across restarts.
- [ ] Entity HNSW search returns correct top-K within 5 ms for 100K entities.
- [ ] LLM cache eviction works without serving stale data.

## Operational acceptance

- [ ] Graceful shutdown: pending queues drain or persist; no data loss.
- [ ] Restart recovery: WAL replays; all derived indexes rebuild or restore from checkpoint.
- [ ] Index rebuild (administrative): can rebuild any derived index from authoritative state.
- [ ] Schema migration (declared): old extractors removed, new run, statements re-extracted.
- [ ] Backfill: completes for 1M memories in bounded time; resumable on interrupt.
- [ ] Metrics: all `worker_*`, `extractor_*`, `retriever_*`, `query_*` metrics emitted and consistent.
- [ ] Audit log: all derivations and supersessions logged; queryable.

## Schema-on / schema-off transitions acceptance

- [ ] older binary → the binary upgrade: clean, no behavioral change without schema.
- [ ] Schema declaration after upgrade: extractors activate.
- [ ] Backfill over existing memories produces statements consistent with the schema.
- [ ] Switching a deployment from "with schema" to "without schema": Brain continues to serve; typed-graph data preserved but unused.

## Documentation acceptance

- [ ] Spec sections 00–19 are complete and internally consistent.
- [ ] All file/directory references in the spec resolve.
- [ ] At least one end-to-end tutorial exists, walking from blank deployment to a working query.
- [ ] Open questions are tagged with rationale and version-target.

## What the typed graph explicitly does NOT include

(See [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).)

- Learned query router.
- SPLADE / sparse-neural retrieval.
- Transaction-time bitemporal.
- Multi-tenant schemas.
- Active derivation rules.
- Federation / cross-node.
- GUI admin.

These are future major-version work.

## How to run the acceptance suite

```bash
# Full workspace regression (correctness + functional gates).
cargo test --workspace

# Performance benchmarks.
cargo bench --workspace

# End-to-end integration against a running binary.
./scripts/knowledge-e2e.sh

# Schema-on / schema-off transitions.
./scripts/schema-on.sh
./scripts/schema-off.sh
```

Acceptance is met when all the above pass on the reference hardware (16 cores, 64 GB RAM, NVMe SSD, Linux 6.6+).
