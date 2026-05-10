# 00.00 Spec Series Index

The complete index of the Brain specification series.

## The 17 specs

| # | Title | What it defines |
|---|---|---|
| **00** | [Master Overview & Glossary](../00_master_overview/) | This document. The series structure, shared glossary, doc map, versioning. |
| **01** | [System Architecture](../01_system_architecture/) | The conceptual whole: cognitive primitives, layers, hardware envelope, non-goals. The foundation for everything else. |
| **02** | [Data Model](../02_data_model/) | Entities and their relationships: memories, contexts, edges, identifiers, lifecycle. |
| **03** | [Wire Protocol](../03_wire_protocol/) | The binary protocol over TCP: framing, opcodes, handshake, error codes. |
| **04** | [Embedding Layer](../04_embedding_layer/) | Tokenization, inference, batching, caching, GPU support, model migration. |
| **05** | [Storage: Arena & WAL](../05_storage_arena_wal/) | The vector arena (mmap'd flat file) and the write-ahead log. |
| **06** | [ANN Index (HNSW)](../06_ann_index/) | The HNSW index: layers, parameters, insertion, search, deletion, maintenance. |
| **07** | [Metadata + Graph Store](../07_metadata_graph/) | Salience, contexts, edges, idempotency, secondary indexes — all in redb. |
| **08** | [Query Planner + Execution Engine](../08_query_planner/) | Strategy selection, plan caching, ANN/attractor/graph executors, VSA algebra. |
| **09** | [Cognitive Operations](../09_cognitive_operations/) | The full semantics of `ENCODE`, `RECALL`, `PLAN`, `REASON`, `FORGET`. |
| **10** | [Concurrency + Epoch Model](../10_concurrency_epochs/) | The lock-free read path: epoch-based reclamation, single-writer-per-shard, channels. |
| **11** | [Background Workers](../11_background_workers/) | Decay, consolidation, HNSW maintenance, snapshot. Scheduling and isolation. |
| **12** | [Sharding + Clustering](../12_sharding_clustering/) | Routing, rebalancing, the cluster control plane. |
| **13** | [SDK Design](../13_sdk_design/) | Language-level client interfaces: Rust, Python, TypeScript, Go. |
| **14** | [Observability + Operations](../14_observability_ops/) | Metrics, tracing, logs, configuration, deployment, security. |
| **15** | [Failure Modes + Recovery](../15_failure_recovery/) | Crash recovery, corruption detection, partial failures, disaster recovery. |
| **16** | [Benchmarks + Acceptance Criteria](../16_benchmarks_acceptance/) | The test suite that validates the targets in [01. System Architecture §06](../01_system_architecture/06_targets.md). |

## Dependency graph

```
                                 00. Master Overview
                                          │
                                          ▼
                                 01. System Architecture
                                          │
                                          ▼
                                 02. Data Model
                                          │
                  ┌───────┬───────┬──────┬┴──────┬──────┬──────┐
                  ▼       ▼       ▼      ▼       ▼      ▼      ▼
              03.Wire 04.Embed 05.Stor 06.ANN  07.Meta 09.Cog  10.Conc
                                  │      │       │      │       │
                                  └──┬───┴───────┘      │       │
                                     ▼                  │       │
                              08. Planner ◄─────────────┘       │
                                     │                          │
                                     ▼                          │
                              11. Background ◄──────────────────┘
                                     │
                                     ▼
                              12. Sharding
                                     │
            ┌────────────────────────┼────────────────────────┐
            ▼                        ▼                        ▼
        13. SDK             14. Observability        15. Failure
                                                              │
                                                              ▼
                                                     16. Benchmarks
```

Solid arrows: spec A depends on spec B (cannot be read independently of B).

## Reading paths by audience

### Implementer of the substrate

01 → 02 → 10 (concurrency model) → 05 (storage) → 06 (ANN) → 07 (metadata) → 04 (embedding) → 03 (protocol) → 08 (planner) → 09 (operations) → 11 → 12 → 14 → 15 → 16.

### Implementer of a client SDK

01 → 02 → 03 → 09 → 13. Optional: 14 (so the SDK exposes the right observability hooks).

### Operator running Brain in production

01 → 14 → 15 → 12 → 11. Optional: 05 (for storage planning), 16 (for understanding the test suite).

### Application developer using Brain

01 → 02 → 09 → 13. Optional: 04 (if the agent needs to understand embedding-model lifecycle).

### Researcher evaluating Brain against alternatives

01 only. Specifically [01. System Architecture §08](../01_system_architecture/08_comparison.md).

## Per-spec structure

Each spec is a directory containing:

- A **`README.md`** — index and reading order for that spec.
- Numbered topic files: **`00_purpose.md`**, **`01_xxx.md`**, **`02_xxx.md`**, ..., usually ending with **`open_questions.md`** and **`references.md`**.

Each file is meant to be readable on its own with cross-references to others.

## Version

This is format version 1 of the spec series. Subsequent versions will be released as the implementation proceeds and feedback accumulates. See [`03_versioning.md`](03_versioning.md) for the versioning scheme.

---

*Continue to [`01_glossary.md`](01_glossary.md) for the shared glossary.*
