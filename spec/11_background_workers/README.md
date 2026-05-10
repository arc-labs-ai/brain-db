# 11. Background Workers

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Substrate implementers; operators |
| Voice | Hybrid (rationale + normative) |
| Depends on | [05. Storage](../05_storage_arena_wal/), [06. ANN Index](../06_ann_index/), [07. Metadata](../07_metadata_graph/), [10. Concurrency](../10_concurrency_epochs/) |
| Referenced by | [14. Observability + Operations](../14_observability_ops/) |

## What this spec defines

The substrate's background work — the periodic tasks that maintain state, decay, consolidate, and clean up. These run alongside the request-handling pipeline but at lower priority.

## Background workers in this spec

- **Decay** — applies time-based decay to memory salience.
- **Access boost** — applies salience boost from recent accesses.
- **Consolidation** — promotes Episodic memories to Consolidated when criteria met.
- **HNSW maintenance** — rebuilds the index when degraded.
- **Idempotency cleanup** — prunes expired idempotency records.
- **Slot reclamation** — reclaims tombstoned slots after grace period.
- **WAL retention** — deletes old WAL segments after checkpoint.
- **Edge scrub** — removes orphan edges.
- **Counter reconciliation** — verifies denormalized counters.
- **Statistics update** — refreshes per-shard stats.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_worker_architecture.md`](01_worker_architecture.md) | Worker scheduling and infrastructure |
| [`02_decay.md`](02_decay.md) | Decay worker |
| [`03_consolidation.md`](03_consolidation.md) | Consolidation worker |
| [`04_hnsw_maintenance.md`](04_hnsw_maintenance.md) | HNSW maintenance |
| [`05_idempotency_cleanup.md`](05_idempotency_cleanup.md) | Idempotency TTL pruning |
| [`06_slot_reclamation.md`](06_slot_reclamation.md) | Slot reclamation |
| [`07_wal_retention.md`](07_wal_retention.md) | WAL segment retention |
| [`08_misc_workers.md`](08_misc_workers.md) | Edge scrub, counter reconciliation, stats |
| [`09_failure_modes.md`](09_failure_modes.md) | Failure modes |
| [`10_open_questions.md`](10_open_questions.md) | Unresolved questions |
| [`11_references.md`](11_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
