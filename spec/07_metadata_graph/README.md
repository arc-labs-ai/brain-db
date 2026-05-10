# 07. Metadata + Graph Store

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Storage-layer implementers; query planner authors |
| Voice | Hybrid (rationale + normative) |
| Depends on | [02. Data Model](../02_data_model/), [05. Storage: Arena & WAL](../05_storage_arena_wal/) |
| Referenced by | [06. ANN Index](../06_ann_index/), [08. Query Planner](../08_query_planner/), [11. Background Workers](../11_background_workers/) |

## What this spec defines

The metadata store: the persistent home for everything that isn't a vector. Memory metadata, edges between memories, contexts, idempotency records, and bookkeeping for sharding.

The store is built on **redb**, a pure-Rust embedded ACID key-value store.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_redb_choice.md`](01_redb_choice.md) | Why redb; alternatives considered |
| [`02_table_layout.md`](02_table_layout.md) | The tables in the metadata store |
| [`03_memory_table.md`](03_memory_table.md) | The memory metadata table |
| [`04_edge_storage.md`](04_edge_storage.md) | How edges are stored and indexed |
| [`05_context_table.md`](05_context_table.md) | The contexts table |
| [`06_idempotency.md`](06_idempotency.md) | The idempotency table |
| [`07_text_storage.md`](07_text_storage.md) | Where memory text lives |
| [`08_transactions.md`](08_transactions.md) | Transaction semantics |
| [`09_concurrency.md`](09_concurrency.md) | Concurrency model in redb |
| [`10_failure_modes.md`](10_failure_modes.md) | Failure modes and recovery |
| [`11_open_questions.md`](11_open_questions.md) | Unresolved questions |
| [`12_references.md`](12_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
