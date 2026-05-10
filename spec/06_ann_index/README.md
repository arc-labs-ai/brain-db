# 06. ANN Index (HNSW)

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Implementers of the ANN layer; performance engineers tuning recall quality |
| Voice | Hybrid (rationale + normative) |
| Depends on | [01. System Architecture](../01_system_architecture/), [02. Data Model](../02_data_model/), [05. Storage: Arena & WAL](../05_storage_arena_wal/) |
| Referenced by | [08. Query Planner + Execution Engine](../08_query_planner/), [11. Background Workers](../11_background_workers/) |

## What this spec defines

The Hierarchical Navigable Small World (HNSW) graph that indexes vectors for approximate nearest neighbor (ANN) search. It defines:

- The HNSW algorithm at the level needed to understand Brain's specific implementation choices.
- The parameters Brain ships with and the rationale.
- Insertion, search, and deletion procedures.
- Persistence (or lack thereof) — the index is rebuilt from arena + metadata.
- Maintenance: when and how to rebuild parts of the index.
- Concurrency: the lock-free read path, single-writer writes.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_hnsw_primer.md`](01_hnsw_primer.md) | HNSW in 800 words: layers, navigability, construction |
| [`02_parameters.md`](02_parameters.md) | M, ef_construction, ef_search; Brain's defaults |
| [`03_insertion.md`](03_insertion.md) | The insert algorithm |
| [`04_search.md`](04_search.md) | The search algorithm |
| [`05_deletion.md`](05_deletion.md) | Tombstones, lazy cleanup, rebuild triggers |
| [`06_persistence.md`](06_persistence.md) | Rebuild from arena + metadata; optional snapshot |
| [`07_maintenance.md`](07_maintenance.md) | The maintenance worker; topology drift detection |
| [`08_concurrency.md`](08_concurrency.md) | Lock-free reads; single-writer writes; epoch management |
| [`09_filtering.md`](09_filtering.md) | Inline filters: model fingerprint, kind, context |
| [`10_failure_modes.md`](10_failure_modes.md) | What can go wrong |
| [`11_open_questions.md`](11_open_questions.md) | Unresolved questions |
| [`12_references.md`](12_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
