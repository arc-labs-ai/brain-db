# 00.00 Spec Series Index

> **TL;DR.** The entry point to the Brain specification. Twenty sections covering the data model, wire protocol, storage, indexing, retrieval, workers, observability, and acceptance criteria. This document is the map: status, section index, reading paths by audience, and per-spec structure.

## What Brain is

> **Brain — a memory database for AI agents.**
>
> Stores four record types — Memory, Entity, Statement, Relation — with explicit provenance, confidence, and bi-temporal validity. Hybrid retrieval (semantic + lexical + entity-graph + temporal) fused with weighted rank fusion. One Rust core, one wire protocol, one schema. Embedded / self-hosted / cluster from the same Apache 2.0 crates.

## Status

**Pre-release (v0.1.0).** No external users. The wire protocol, redb tables, and schema model are in flux. The v1.0 release ships when the combined acceptance suite at [`../19_benchmarks/06_complete_acceptance.md`](../19_benchmarks/06_complete_acceptance.md) passes.

## The sections

| # | Section | What it defines |
|---|---|---|
| **00** | [Overview](./00_index.md) | This document. Series structure, glossary, doc map, versioning, open-questions archive, external references. |
| **01** | [System Architecture](../01_architecture/00_purpose.md) | The conceptual whole: cognitive primitives, the seven internal layers, hardware envelope, capacity targets, design wedges, scope and comparison, glossary. |
| **02** | [Data Model](../02_data_model/00_purpose.md) | The four record types — Memory, Entity, Statement, Relation. Identifiers, edges, contexts, salience, kinds, lifecycle, composition, property-graph rationale, failure modes. |
| **03** | [Schema DSL](../03_schema/00_purpose.md) | The `.brain` schema format. Grammar, AST, validator, namespaces, versioning, system schema. |
| **04** | [Wire Protocol](../04_wire_protocol/00_purpose.md) | Binary protocol over TCP. Unified opcode space (substrate `0x00xx` + typed-graph `0x01xx`), 32-byte fixed header, CBOR payloads, handshake, streaming, error handling. |
| **05** | [Operations](../05_operations/00_purpose.md) | Write pipeline (`ENCODE`, `FORGET`, `LINK/UNLINK`, `MATERIALIZE_PROCEDURAL`) and read pipeline (`RECALL`, `PLAN`, `REASON`). `SUBSCRIBE`, `TXN_*`, `ADMIN_*` semantics. |
| **06** | [Client Interface](../06_sdk/00_purpose.md) | Stub. Brain ships no SDK; clients speak the §04 wire protocol directly (CBOR payloads, documented field schemas). |
| **07** | [Embedding Layer](../07_embedding/00_purpose.md) | BGE-small via candle. Inference pipeline, caching, fingerprinting, migration. |
| **08** | [Storage: Arena & WAL](../08_storage/00_purpose.md) | Mmap'd vector arena (1600-byte slots). Per-shard WAL (O_DIRECT, group commit). Recovery, retention. |
| **09** | [Indexing (HNSW)](../09_indexing/00_purpose.md) | HNSW for memory + entity + statement vectors. Parameters, operations, lifecycle. |
| **10** | [Metadata + Graph Store](../10_metadata/00_purpose.md) | All per-shard durable state: redb tables, tantivy indexes, audit tables. |
| **11** | [Extractors](../11_extractors/00_purpose.md) | Three-tier pipeline (pattern → GLiNER classifier → LLM). Triggers, resolver gauntlet, audit, materialize. |
| **12** | [Query Optimizer](../12_query_optimizer/00_purpose.md) | Planner (pure function: query+stats → plan). Cost estimation, executor, runtime concerns, VSA algebra. |
| **13** | [Retrievers](../13_retrievers/00_purpose.md) | Semantic + lexical + graph retrievers. Weighted RRF rank fusion (k=60), query router, post-processing (rerank + traversal + per-hit enrichment). |
| **14** | [Concurrency](../14_concurrency/00_purpose.md) | Lock-free reads, single-writer-per-shard, ArcSwap, crossbeam-epoch, Glommio scheduling. |
| **15** | [Background Workers](../15_background_workers/00_purpose.md) | Memory maintenance (decay, consolidation, HNSW maintenance), substrate sweepers (idempotency, slot reclamation, WAL retention), typed-graph workers (extractor, text indexer, sweepers, state-carrying). |
| **16** | [Sharding & Clustering](../16_sharding/00_purpose.md) | Shard model, routing, single-node, clustered, rebalancing + replication. |
| **17** | [Observability](../17_observability/00_purpose.md) | Signals (metrics + logs + tracing), dashboards, alerts, admin ops, runbooks, capacity planning. |
| **18** | [Failure Recovery](../18_failure_recovery/00_purpose.md) | Crash recovery, corruption detection, partial failures, disaster recovery. |
| **19** | [Benchmarks](../19_benchmarks/00_purpose.md) | Correctness + durability criteria, performance targets, recall quality, methodology, the combined acceptance gate for v1.0. |

## Reading paths by audience

### Application developer using Brain

01 → 02 → 05 → 06. Optional: 03 → 11 if extending the schema.

### Implementer of a client

01 → 02 → 04 → 05 → 06.

### Implementer of the server

01 → 02 → 14 → 08 → 09 → 10 → 07 → 04 → 12 → 05 → 15 → 16 → 17 → 18 → 19. Add 03 → 11 → 13 for the typed-graph surfaces.

### Operator running Brain

01 → 17 → 18 → 16 → 15. Optional: 08, 19.

### Researcher evaluating Brain against alternatives

01 only — specifically [`06_scope_and_comparison.md`](../01_architecture/06_scope_and_comparison.md) and [`07_wedges_and_roadmap.md`](../01_architecture/07_wedges_and_roadmap.md).

## Per-spec structure

Each section is a directory containing:

- `00_purpose.md` — the section landing page (status, scope, reading order).
- Numbered topic files: `01_xxx.md`, `02_xxx.md`, … usually ending with cross-links to [`04_open_questions_archive.md`](./04_open_questions_archive.md) and [`05_external_references.md`](./05_external_references.md).

Each file is meant to be readable on its own with cross-references to others.

## Version

Brain is at v0.1.0 (pre-release). The spec evolves freely toward the v1.0 release; see [`03_versioning.md`](./03_versioning.md) for how the spec, wire protocol, and on-disk formats are versioned.

---

*Continue to [`01_glossary.md`](01_glossary.md) for the shared glossary.*
