# 08. Query Planner + Execution Engine

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Implementers of the request-handling pipeline |
| Voice | Hybrid (rationale + normative) |
| Depends on | [03. Wire Protocol](../03_wire_protocol/), [05. Storage](../05_storage_arena_wal/), [06. ANN Index](../06_ann_index/), [07. Metadata Store](../07_metadata_graph/) |
| Referenced by | [09. Cognitive Operations](../09_cognitive_operations/) |

## What this spec defines

The pipeline that converts a wire-protocol request into a sequence of operations against the storage layer (arena, WAL, metadata, ANN), and assembles the response.

The two halves:

- **Query planner** — chooses how to satisfy a request. Picks ef_search values, decides between fast paths and slow paths, plans cross-shard fan-out.
- **Execution engine** — runs the plan. Concurrency, batching, error handling.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_planner_overview.md`](01_planner_overview.md) | Planner role and architecture |
| [`02_request_lifecycle.md`](02_request_lifecycle.md) | The lifecycle of a request |
| [`03_recall_planning.md`](03_recall_planning.md) | RECALL planning |
| [`04_encode_planning.md`](04_encode_planning.md) | ENCODE planning |
| [`05_plan_reason_planning.md`](05_plan_reason_planning.md) | PLAN and REASON planning |
| [`06_forget_planning.md`](06_forget_planning.md) | FORGET planning |
| [`07_cost_estimation.md`](07_cost_estimation.md) | Cost model |
| [`08_executor.md`](08_executor.md) | Execution engine architecture |
| [`09_concurrency.md`](09_concurrency.md) | Cross-task and cross-shard |
| [`10_failure_modes.md`](10_failure_modes.md) | Failure modes |
| [`11_open_questions.md`](11_open_questions.md) | Unresolved questions |
| [`12_references.md`](12_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
