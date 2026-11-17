# 19.06 Combined Acceptance Gate

What "the typed graph is done" means. Concrete tests that must pass.

## Functional acceptance

### Schemaless mode

- [ ] With no schema declared, all baseline schemaless-mode acceptance tests pass.
- [ ] Clients (no typed-graph types) work normally when no schema is declared.
- [ ] Data directories created with no schema work with or without later declaring one.
- [ ] RECALL latency under retrieval: P50 ≤ 10 ms, P99 ≤ 50 ms (warmed shard).

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

### Tenant LLM credentials

- [ ] `LLM_CREDENTIAL_PUT`: registers at the named scope and nowhere else; a second PUT reports `replaced = true`.
- [ ] `LLM_CREDENTIAL_GET`: returns provider, model and fingerprint; never the key.
- [ ] `LLM_CREDENTIAL_DELETE`: reports whether a row existed, so a revoke during an incident is distinguishable from a no-op.
- [ ] `LLM_CREDENTIAL_VERIFY`: `Ok` / `Unauthorized` / `ModelUnavailable` / `Unreachable` / `NoCredential`, each reached.
- [ ] A key without the `LLM_CREDENTIAL` permission bit is refused on all four ops.
- [ ] `LLM_CREDENTIAL` is not delegable: a delegated caller never holds it.
- [ ] A plaintext key is refused over a transport that does not protect it.
- [ ] ENCODE into a namespace with no registered credential is refused with `LlmCredentialMissing` (0x0066), terminal, before the write lands.
- [ ] ENCODE into a namespace whose credential cannot be *resolved* is admitted, and the HyPE stage reports `StageOutcome::Failed`.
- [ ] A credential store whose rows none of the loaded sealing keys can open fails the boot audit rather than serving.

> Every extraction item below presumes the namespace has a registered provider credential. Without one the write never reaches extraction — ENCODE is refused at admission.

- [ ] Pattern extractor: runs in the extractor worker; output visible after the memory is drained (not at ENCODE-ack).
- [ ] Classifier extractor: runs in the extractor worker after the pattern tier; output visible within one worker cycle of ENCODE.
- [ ] LLM extractor (cache hit): returns from cache without LLM call.
- [ ] LLM extractor (cache miss): calls LLM, caches result, schema-validates.
- [ ] LLM extractor (invalid output): retries once, then drops with audit.
- [ ] LLM extractor (over budget): skips with metric.
- [ ] Extractor idempotency: re-running on same memory produces identical results (modulo LLM cache TTL).

### Read (RECALL) and query introspection

- [ ] Free-text RECALL: semantic + lexical retrievers invoked, fused, shaped to a membership answer.
- [ ] Entity-anchored RECALL: graph retriever invoked, weighted appropriately.
- [ ] Time-filtered RECALL: temporal filter applied post-fusion.
- [ ] Type-filtered RECALL: kind/predicate filter applied.
- [ ] Confidence filter: respected.
- [ ] Membership shape: RECALL returns Single / Many / None from the relevance band over the full pool, not a caller top-K; `max_results` caps a `Many` only.
- [ ] EXPLAIN (debug op): returns plan without execution.
- [ ] TRACE (debug op): returns plan + execution metadata + per-retriever ranks.

### Provenance and versioning

- [ ] Every statement has evidence list and extractor metadata.
- [ ] FORGET cascade: statements depending on forgotten memory get evidence list updated; orphaned statements get tombstoned.
- [ ] Confidence aggregation: matches the formula in the spec.
- [ ] Schema-version flagging: statements from old schema are marked stale after schema update.
- [ ] Re-extraction: produces new statements supersession-chained from old.

## Performance acceptance

> **v1.0 status — OPTIONAL / off the critical path.** The performance, throughput,
> and storage-performance targets in this section are **advisory for v1.0**, not a
> lock gate. They require reference hardware (16-core x86_64, 64 GiB, NVMe) and a
> quiet measured run that v1.0 deliberately does **not** block on. v1.0 is locked
> on the **functional** and **operational** correctness gates above and below —
> which are hardware-independent and run in CI on any box. The numbers here stay
> as engineering targets to measure post-lock; a miss is a tuning follow-up, never
> a v1.0 blocker.
>
> This status is load-bearing for how every number below should be read. Each is a
> **reference-hardware** figure. Several of them — the embedding-path latency and
> throughput rows in particular — are per-core model-inference costs that move with
> ISA and clock, so reproducing them on unlike hardware is not expected and a miss
> there is not evidence of a regression. Where a figure is portable, it says so.

### Latency (P50 / P99, single shard, warm)

Targets below assume the default text-input path, whose embed step is ~5–10 ms on reference hardware. The `ENCODE_VECTOR_DIRECT` (pre-supplied vector) path has separate, lower targets — see [`../01_architecture/05_hardware_and_targets.md`](../01_architecture/05_hardware_and_targets.md) §7.1.

> **The GPU rows below are not exercisable on this build.** The embedding layer
> accepts `Device::Cpu` with `DType::F32` only; any other device or dtype is
> rejected at model load with `EmbedError::UnsupportedDevice`
> (`crates/brain-embed/src/model/load.rs:48-52`,
> `crates/brain-embed/src/config.rs:14-31`). No GPU dispatcher ships — the
> `Dispatcher` trait exists partly so one can be added without re-spelling the
> surface, but none is implemented. The GPU targets are retained as the
> acceptance criteria that path must meet **when** it exists, and they are not
> part of the v1.0 gate; a run cannot pass or fail them today. They are flagged
> here rather than deleted because the figures are the specification of that
> work, and an unexercisable row left unflagged reads as an untested one.

- [ ] ENCODE (text, CPU embedding): P50 ≤ 12 ms, P99 ≤ 25 ms.
- [ ] ENCODE (text, GPU embedding): P50 ≤ 3 ms, P99 ≤ 8 ms. *(no GPU path — see note)*
- [ ] ENCODE_VECTOR_DIRECT (pre-supplied vector, no embedding): P50 ≤ 1 ms, P99 ≤ 5 ms.
- [ ] ENCODE + classifier extractors (post-encode async): P50 ≤ 5 ms added to extractor-completion event, P99 ≤ 20 ms.
- [ ] STATEMENT_CREATE: P50 ≤ 1 ms, P99 ≤ 5 ms.
- [ ] RELATION_CREATE: P50 ≤ 1 ms, P99 ≤ 5 ms.
- [ ] RECALL (retrieval, default candidate pool): P50 ≤ 10 ms, P99 ≤ 50 ms.
- [ ] RECALL (entity-anchored, 2-hop graph): P50 ≤ 15 ms, P99 ≤ 100 ms.
- [ ] Entity resolution (tiers 1–2, exact+fuzzy): P50 ≤ 1 ms.
- [ ] Entity resolution (tier 3, embedding HNSW): P50 ≤ 10 ms.

### Throughput

Per-shard sustained, distinguishing CPU vs GPU embedding paths (matching [`../01_architecture/05_hardware_and_targets.md`](../01_architecture/05_hardware_and_targets.md) §7.1.4):

- [ ] ENCODE throughput (text, CPU embedding): ≥ 100/s per shard, **on reference hardware**. See the note below: this row is a per-core model-inference gate, not a storage one.
- [ ] ENCODE throughput (text, GPU embedding): ≥ 1K/s per shard. *(no GPU path — see the note under "Latency")*
- [ ] ENCODE_VECTOR_DIRECT throughput (storage-only): ≥ 100K/s per shard.
- [ ] STATEMENT_CREATE throughput: ≥ 10K/s per shard.
- [ ] RECALL throughput (mixed workload): ≥ 1K/s per shard.

> **What the CPU-embedding ENCODE row actually gates.** A text ENCODE runs one
> embedder forward pass per request. Nothing batches it — the CPU dispatcher puts
> each request through the model independently and assembles no batches
> (`crates/brain-embed/src/dispatcher/cpu.rs:1-7`) — and the pass is confined to
> the shard's own core, because the embed is handed to a blocking thread whose
> intra-op pool inherits that thread's CPU affinity
> (`crates/brain-ops/src/embedding.rs:16-61`). So the per-shard ceiling for this
> row *is* the per-core embedder rate, and it does not improve with more cores:
> more cores buy more shards, each with its own lane. Aggregate ENCODE throughput
> scales with shard count; this row does not.
>
> That makes 100/s per shard a statement about one core's model-inference
> throughput on the 16-core x86_64 reference box, and **only** about that box. The
> figure is deliberately not restated as a hardware-independent floor: the
> embedding layer's own bench asserts **no absolute gate** for exactly this
> reason, treating the machine-local Criterion baseline as the real regression
> signal because "embedding cost depends on the model, available cores, and thread
> configuration" (`crates/brain-embed/benches/throughput.rs:3-6`). A deployment on
> other hardware should read this row as "measure your per-core embedder rate;
> that is your per-shard text-ENCODE ceiling", not as a number its box is expected
> to hit.
>
> Measured datapoint, recorded as evidence rather than as a gate: on a 10-CPU
> aarch64 development container, the embed path sustains **≈66 encodes/s per
> pinned shard** — 266/s aggregate across 4 shards, 586/s across 9 — measured by
> the `embed_contention` harness (`crates/brain-server/benches/embed_contention.rs`),
> which pins one executor per shard and drives real forward passes through a bare
> CPU dispatcher. The per-shard figure is flat across shard counts, which is the
> expected signature of a per-core bound: aggregate scales, per-shard does not.
> ≈66/s/shard is below the 100/s target, and on that hardware this row is
> unreachable — as the advisory status at the head of this section already
> anticipates, since it is not the reference box. The ≈15 ms per pass that ≈66/s
> implies also agrees with the embedder-miss line in
> [`02_performance_targets.md`](02_performance_targets.md) §3, so the two
> measurements corroborate rather than contradict each other.

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
- [ ] Metrics: the `brain_worker_*`, `brain_extractor_*`, `brain_retriever_*` and `brain_query_*` families emitted and consistent. Every metric carries the `brain_` prefix; a family without it is not Brain's.
- [ ] Deferred metric families are absent rather than wrong: `brain_hnsw_recall_estimate`, `brain_hnsw_search_visits`, `brain_embedder_duration_ms` and `brain_executor_latency_ms` are not emitted, and nothing reports a placeholder value in their place (`crates/brain-server/src/metrics/mod.rs:31-40`). A dashboard built on them shows no data, which is the correct signal; see [`03_recall_quality.md`](03_recall_quality.md) §8 for what the recall one would have gated.
- [ ] Audit log: all derivations and supersessions logged; queryable.

## Schema-on / schema-off transitions acceptance

- [ ] A data directory created with no schema serves normally, and declaring a schema later activates extractors without touching the memories already stored.
- [ ] Backfill over existing memories produces statements consistent with the schema.
- [ ] Switching a deployment from "with schema" to "without schema": Brain continues to serve; typed-graph data preserved but unused.

## Documentation acceptance

- [ ] Spec sections 00–19 are complete and internally consistent.
- [ ] All file/directory references in the spec resolve.
- [ ] At least one end-to-end tutorial exists, walking from blank deployment to a working query.
- [ ] Every normative claim cites the code that proves it, and a claim whose mechanism no longer exists is corrected or removed rather than left standing. A spec that disagrees with the binary is worse than a thinner one, because a reader cannot tell which parts to trust.
- [ ] The spec states how the system behaves, not how it came to behave that way: no deliberation, no open-question or decision-log content, no internal plan or issue references, and no superseded-design notes. Design history is tracked outside `spec/`, so a spec file is readable as a description of the current system with nothing to subtract.

## What the typed graph explicitly does NOT include

- Learned query router.
- SPLADE / sparse-neural retrieval.
- Transaction-time bitemporal.
- Multi-tenant schemas.
- Active derivation rules.
- Federation / cross-node.
- GUI admin.

These are future major-version work.

## How to run the acceptance suite

The functional, operational and schema-transition gates are all Rust tests in the
workspace sweep — there is no separate shell harness to invoke:

```bash
# Everything CI runs, in one command: fmt, clippy, the full test sweep,
# doctests, docs, and a release-build smoke.
just prod-verify

# The sweep alone (nextest does not run doctests, so both are needed).
cargo nextest run --workspace
cargo test --workspace --doc

# The two asserting performance benches — the same pair the nightly
# perf workflow runs. `cargo bench --workspace` would additionally run
# the reporting-only benches, two of which need model weights on disk.
just prod-bench
```

**v1.0 lock gate.** v1.0 is locked when the **functional** and **operational**
acceptance sections pass — the workspace regression plus the end-to-end and
schema-transition tests inside it — on **any** supported box. Those gates are
hardware-independent and run in CI, which is what makes them a usable lock
criterion: anyone can reproduce a pass or a failure.

The **performance acceptance** section (and the storage-*performance* items) is
**optional for v1.0**. Those targets want reference hardware (16 cores, 64 GB RAM,
NVMe SSD, Linux 6.6+) and a quiet measured run, and they are engineering targets
to capture post-lock rather than lock criteria. The reason is not that performance
does not matter; it is that a gate nobody can reproduce is not a gate. Several of
these figures are per-core model-inference costs (see the ENCODE-throughput note
above), so on any box but the reference one they would fail for reasons unrelated
to the code under test — and a release gate that fails on correct code teaches
people to override gates.

What guards performance in the meantime is narrower and real: two benches that
assert their own p50 targets in-process and run nightly, failing the workflow on a
regression (§19.02 §2.11, §19.04 §17).
