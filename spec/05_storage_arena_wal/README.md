# 05. Storage: Arena & WAL

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Storage-layer implementers; operators planning capacity |
| Voice | Hybrid (rationale + normative byte-level requirements) |
| Depends on | [01. System Architecture](../01_system_architecture/), [02. Data Model](../02_data_model/), [04. Embedding Layer](../04_embedding_layer/) |
| Referenced by | [06. ANN Index](../06_ann_index/), [10. Concurrency + Epoch Model](../10_concurrency_epochs/), [11. Background Workers](../11_background_workers/), [15. Failure Modes + Recovery](../15_failure_recovery/) |

## What this spec defines

Layer L5 of the architecture — the storage layer. It defines:

- The **vector arena**: a memory-mapped flat file that holds all of a shard's vectors. Slot-based, fixed-size, alignment-preserving.
- The **write-ahead log (WAL)**: an append-only durable log of every state-mutating operation. The substrate's source of truth for crash recovery.
- The coordination between arena and WAL during writes.
- The recovery procedure on startup.
- The retention and checkpointing policies.

The metadata store (redb-backed B-tree) is a separate spec ([07. Metadata + Graph Store](../07_metadata_graph/)). This spec covers the parts of the storage layer that hold vectors and the durability log.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_arena_overview.md`](01_arena_overview.md) | The vector arena: motivation and structure |
| [`02_arena_layout.md`](02_arena_layout.md) | The byte-level arena layout |
| [`03_arena_growth.md`](03_arena_growth.md) | Arena growth: fallocate, mremap |
| [`04_wal_overview.md`](04_wal_overview.md) | The WAL: motivation and structure |
| [`05_wal_records.md`](05_wal_records.md) | WAL record formats |
| [`06_wal_durability.md`](06_wal_durability.md) | O_DIRECT, RWF_DSYNC, group commit |
| [`07_write_path.md`](07_write_path.md) | The full ENCODE write path |
| [`08_recovery.md`](08_recovery.md) | Crash recovery procedure |
| [`09_checkpointing.md`](09_checkpointing.md) | Checkpoints and WAL retention |
| [`10_snapshots.md`](10_snapshots.md) | Snapshot creation via reflinks |
| [`11_failure_modes.md`](11_failure_modes.md) | Storage-level failure modes |
| [`12_open_questions.md`](12_open_questions.md) | Unresolved questions |
| [`13_references.md`](13_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
