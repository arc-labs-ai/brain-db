# 00.02 Document Map

For each section, this file gives a one-paragraph summary, key dependencies, and what depends on it. "Where is X documented?" lookup table.

---

## 01. System Architecture

**Summary.** The conceptual whole of Brain. The five cognitive primitives (`ENCODE`, `RECALL`, `PLAN`, `REASON`, `FORGET`), seven internal layers, hardware envelope, capacity targets. **Design wedges** — five competitive differentiators every architectural decision is checked against. Scope, non-goals, and comparison to adjacent systems.

**Depends on.** Nothing. **Depended on by.** Everything.

---

## 02. Data Model

**Summary.** The four record types Brain stores — Memory, Entity, Statement, Relation. Identifiers, edges, contexts, salience, memory kinds, lifecycle. The three Statement kinds (Fact / Preference / Event) with their mutation rules. Entity merge / unmerge. Statement supersession, contradiction, confidence (noisy-OR + decay), evidence. Relation cardinality, symmetry, traversal. Composition (how Statements / Relations cite Memories as evidence). Property-graph rationale.

**Depends on.** [01](../01_architecture/00_purpose.md). **Depended on by.** Every other section.

---

## 03. Schema DSL

**Summary.** The `.brain` schema format. Grammar, AST, validator, namespaces, versioning, system schema (default predicates + types).

**Depends on.** [02](../02_data_model/00_purpose.md). **Depended on by.** [11](../11_extractors/00_purpose.md), [13](../13_retrievers/00_purpose.md).

---

## 04. Wire Protocol

**Summary.** The binary protocol over TCP. 32-byte fixed header, self-describing CBOR structured payloads, little-endian `f32` raw-vector trailing section. One unified opcode space — substrate opcodes (`0x00xx`) plus typed-graph opcodes (`0x01xx`). Handshake, streaming, error handling, and typed-graph frames (entity / statement / relation / schema / query / admin). Portable: any language speaks it with a stock CBOR library; Brain ships no client.

**Depends on.** [01](../01_architecture/00_purpose.md), [02](../02_data_model/00_purpose.md). **Depended on by.** [05](../05_operations/00_purpose.md), [06](../06_sdk/00_purpose.md), [07](../07_embedding/00_purpose.md).

---

## 05. Operations

**Summary.** Semantics of `ENCODE`, `RECALL`, `PLAN`, `REASON`, `FORGET`, `LINK/UNLINK`, `SUBSCRIBE`, `TXN_*`, `ADMIN_*`, `MATERIALIZE_PROCEDURAL`. Write pipeline and read pipeline. Each operation: parameters, return values, side effects, idempotency, latency targets, error conditions, edge cases.

**Depends on.** [01](../01_architecture/00_purpose.md), [02](../02_data_model/00_purpose.md), [04](../04_wire_protocol/00_purpose.md). **Depended on by.** [06](../06_sdk/00_purpose.md), [19](../19_benchmarks/00_purpose.md).

---

## 06. Client Interface

**Summary.** A stub. Brain ships no first-party SDK or client library. Clients talk to Brain directly over the wire protocol (§04) — portable binary framing, self-describing CBOR payloads, documented per-opcode field schemas, plus the conformance corpus (§19). Building client libraries in any language is out of scope / future work.

**Depends on.** [04](../04_wire_protocol/00_purpose.md). **Depended on by.** Third-party client implementers.

---

## 07. Embedding Layer

**Summary.** Layer L2. BGE-small via candle (384-dim). Inference pipeline (tokenization, inference, normalization, batching), LRU caching, fingerprinting, model migration via `ADMIN_MIGRATE_EMBEDDINGS`.

**Depends on.** [01](../01_architecture/00_purpose.md), [02](../02_data_model/00_purpose.md). **Depended on by.** [08](../08_storage/00_purpose.md), [05](../05_operations/00_purpose.md), [11](../11_extractors/00_purpose.md).

---

## 08. Storage: Arena & WAL

**Summary.** The vector arena (mmap'd, MAP_SHARED, 1600-byte slots). The per-shard WAL (O_DIRECT append-only, 256 MiB segments, `pwritev2(RWF_DSYNC)` group commit). Durability barrier sequence, recovery, retention.

**Depends on.** [01](../01_architecture/00_purpose.md), [02](../02_data_model/00_purpose.md). **Depended on by.** [09](../09_indexing/00_purpose.md), [14](../14_concurrency/00_purpose.md), [18](../18_failure_recovery/00_purpose.md).

---

## 09. Indexing (HNSW)

**Summary.** HNSW graph: layers, `M`, `ef_construction`, `ef_search`. Insertion, search, deletion, parameter tuning, maintenance worker. Applies to memory, entity, and statement vector indexes.

**Depends on.** [01](../01_architecture/00_purpose.md), [02](../02_data_model/00_purpose.md), [08](../08_storage/00_purpose.md). **Depended on by.** [12](../12_query_optimizer/00_purpose.md), [15](../15_background_workers/00_purpose.md).

---

## 10. Metadata + Graph Store

**Summary.** All per-shard durable state in a single redb database: memory tables (memories, texts, edges, contexts, idempotency, model fingerprints, checkpoints, agents) and — when a schema is declared — typed-graph tables (entities + secondary indexes, statements + secondary indexes, relations + secondary indexes, predicates, entity types, relation types, extractors, schema versions, audit tables). Per-shard tantivy indexes. LLM extractor cache. Memory / entity / statement HNSW co-located on disk. Subscription state is in-memory only, not durable.

**Depends on.** [01](../01_architecture/00_purpose.md), [02](../02_data_model/00_purpose.md), [08](../08_storage/00_purpose.md). **Depended on by.** [12](../12_query_optimizer/00_purpose.md), [05](../05_operations/00_purpose.md), [11](../11_extractors/00_purpose.md), [13](../13_retrievers/00_purpose.md), [15](../15_background_workers/00_purpose.md).

---

## 11. Extractors

**Summary.** Three-tier pipeline (pattern → GLiNER classifier → LLM). Triggers (on ENCODE; bounded context with top-m similar memories + rolling summary). Resolver gauntlet (Tier 0 exact qname / Tier 1 canonical name / Tier 2 alias / Tier 3 fuzzy trigram / Tier 4 HNSW embedding). Per-extraction audit, idempotency cache (LLM responses keyed on `(input_hash, extractor_id, extractor_version, model_id)` with TTL). Anthropic prompt-cache scaffolding. Plugin surface (`EnricherPlugin`, `ConnectorPlugin`).

**Depends on.** [02](../02_data_model/00_purpose.md), [03](../03_schema/00_purpose.md), [07](../07_embedding/00_purpose.md), [10](../10_metadata/00_purpose.md). **Depended on by.** [13](../13_retrievers/00_purpose.md).

---

## 12. Query Optimizer

**Summary.** The planner (pure function from query+stats to plan) and executors. Strategy selection, cost estimation, executor, runtime concerns, VSA algebra module (HRR at D=512).

**Depends on.** [01](../01_architecture/00_purpose.md), [02](../02_data_model/00_purpose.md), [09](../09_indexing/00_purpose.md), [10](../10_metadata/00_purpose.md), [14](../14_concurrency/00_purpose.md). **Depended on by.** [05](../05_operations/00_purpose.md), [13](../13_retrievers/00_purpose.md).

---

## 13. Retrievers

**Summary.** Three retrievers (semantic, lexical, graph). Weighted RRF rank fusion (k=60) with adaptive top-K. Query router (entity-anchored / exact-term / time-filtered / type-filtered / default rules). Filter chain (type / temporal / confidence / tombstone / supersession). Post-processing: optional cross-encoder reranker (bge-reranker-base), per-hit graph enrichment side-channel, relation traversal (depth, branching, cycle detection).

**Depends on.** [02](../02_data_model/00_purpose.md), [07](../07_embedding/00_purpose.md), [09](../09_indexing/00_purpose.md), [10](../10_metadata/00_purpose.md), [11](../11_extractors/00_purpose.md), [12](../12_query_optimizer/00_purpose.md). **Depended on by.** [05](../05_operations/00_purpose.md), [06](../06_sdk/00_purpose.md), [19](../19_benchmarks/00_purpose.md).

---

## 14. Concurrency

**Summary.** Lock-free read path (crossbeam-epoch reclamation), single-writer-per-shard, ArcSwap config swaps, channels / queues, cooperative Glommio scheduling, yield discipline, failure modes.

**Depends on.** [01](../01_architecture/00_purpose.md). **Depended on by.** [08](../08_storage/00_purpose.md), [09](../09_indexing/00_purpose.md), [12](../12_query_optimizer/00_purpose.md), [15](../15_background_workers/00_purpose.md).

---

## 15. Background Workers

**Summary.** Memory maintenance (decay, consolidation, HNSW maintenance), substrate sweepers (idempotency, slot reclamation, WAL retention), typed-graph workers (pattern + GLiNER classifier + LLM extractor, tantivy text indexer, sweepers — supersession, audit, LLM cache, entity GC, stale-extraction-detector — and state-carrying workers — backfill, FORGET cascade, schema migration). Scheduling, resource budgeting, isolation.

**Depends on.** [01](../01_architecture/00_purpose.md), [08](../08_storage/00_purpose.md), [09](../09_indexing/00_purpose.md), [10](../10_metadata/00_purpose.md), [14](../14_concurrency/00_purpose.md). **Depended on by.** [16](../16_sharding/00_purpose.md).

---

## 16. Sharding & Clustering

**Summary.** Shard model, routing, shard assignment, single-node deployment, clustered deployment, rebalancing (snapshot → stream → catch-up → cutover), replication, failure modes.

**Depends on.** [01](../01_architecture/00_purpose.md), [08](../08_storage/00_purpose.md), [15](../15_background_workers/00_purpose.md). **Depended on by.** [06](../06_sdk/00_purpose.md), [17](../17_observability/00_purpose.md).

---

## 17. Observability

**Summary.** Signals (Prometheus metrics, structured JSON logs, OpenTelemetry tracing). Dashboards, alerts, admin ops, runbooks, capacity planning. Security model (TLS, authentication, per-tenant scope binding: `(org_id, user_id, namespace_id, agent_id, permissions)`).

**Depends on.** [01](../01_architecture/00_purpose.md), [16](../16_sharding/00_purpose.md). **Depended on by.** [18](../18_failure_recovery/00_purpose.md).

---

## 18. Failure Recovery

**Summary.** Process / host crash, disk corruption (CRC detect), network partition, embedding-model corruption (fingerprint detect), partial WAL writes, bad client data. Recovery procedures.

**Depends on.** [01](../01_architecture/00_purpose.md), [08](../08_storage/00_purpose.md), [17](../17_observability/00_purpose.md). **Depended on by.** [19](../19_benchmarks/00_purpose.md).

---

## 19. Benchmarks + Acceptance Criteria

**Summary.** Correctness and durability criteria, performance targets (latency p50 / p99 / p99.9, throughput, resource targets), recall quality, methodology, acceptance test suite, the combined acceptance gate for v1.0.

**Depends on.** Everything else. **Depended on by.** Nothing (it is the validation).

---

## Cross-cutting topics

Some topics aren't section-shaped and are scattered across multiple sections.

- **Authentication and per-tenant scope.** Primarily in [17. Observability](../17_observability/00_purpose.md), referenced by [04. Wire Protocol](../04_wire_protocol/00_purpose.md) §handshake.
- **Configuration.** Primarily in [17. Observability](../17_observability/00_purpose.md), referenced everywhere there are tunable parameters.
- **Embedding model fingerprint.** Defined in [07. Embedding Layer](../07_embedding/00_purpose.md), used in [02. Data Model](../02_data_model/00_purpose.md) and [10. Metadata + Graph Store](../10_metadata/00_purpose.md), referenced from [18. Failure Recovery](../18_failure_recovery/00_purpose.md).
- **CRC32C checksum.** Defined in [04. Wire Protocol](../04_wire_protocol/00_purpose.md) for frames, [08. Storage](../08_storage/00_purpose.md) for WAL records.
- **Provenance and audit.** Audit tables in [10. Metadata + Graph Store](../10_metadata/00_purpose.md); per-derivation audit emitted by [11. Extractors](../11_extractors/00_purpose.md).
- **Cross-cutting open questions** live in [`04_open_questions_archive.md`](./04_open_questions_archive.md).

---

*Continue to [`03_versioning.md`](03_versioning.md) for how the spec, wire protocol, and on-disk formats are versioned.*
