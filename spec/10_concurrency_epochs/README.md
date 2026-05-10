# 10. Concurrency + Epoch Model

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Substrate implementers; storage-layer authors |
| Voice | Hybrid (rationale + normative) |
| Depends on | [05. Storage](../05_storage_arena_wal/), [06. ANN Index](../06_ann_index/), [07. Metadata Store](../07_metadata_graph/), [08. Query Planner](../08_query_planner/) |
| Referenced by | [11. Background Workers](../11_background_workers/), [12. Sharding + Clustering](../12_sharding_clustering/) |

## What this spec defines

How Brain manages concurrent operations on shared state — the epoch-based reclamation protocol, the single-writer-per-shard discipline, the publication mechanism, and how readers and writers stay out of each other's way.

This is the substrate's "concurrency contract" — the rules every layer follows to ensure correctness and performance under concurrency.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_principles.md`](01_principles.md) | The core principles |
| [`02_single_writer.md`](02_single_writer.md) | Single-writer-per-shard |
| [`03_epochs.md`](03_epochs.md) | Epoch-based reclamation |
| [`04_publication.md`](04_publication.md) | The publication protocol |
| [`05_arc_swap.md`](05_arc_swap.md) | Use of ArcSwap |
| [`06_crossbeam_epoch.md`](06_crossbeam_epoch.md) | Use of crossbeam-epoch |
| [`07_yields.md`](07_yields.md) | Cooperative yielding |
| [`08_failure_modes.md`](08_failure_modes.md) | Failure modes |
| [`09_open_questions.md`](09_open_questions.md) | Unresolved questions |
| [`10_references.md`](10_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
