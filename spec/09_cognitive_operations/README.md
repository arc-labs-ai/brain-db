# 09. Cognitive Operations

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | SDK authors; agent integrators |
| Voice | Hybrid (rationale + normative) |
| Depends on | [02. Data Model](../02_data_model/), [08. Query Planner](../08_query_planner/) |
| Referenced by | [13. SDK Design](../13_sdk_design/) |

## What this spec defines

The semantics of the substrate's cognitive primitives — the high-level operations that agents call to interact with their memory.

These are:

- **ENCODE** — store a memory.
- **RECALL** — find memories similar to a cue.
- **PLAN** — find paths through the graph from a starting state to a goal.
- **REASON** — find supporting and contradicting memories for a query.
- **FORGET** — delete a memory.
- **LINK** / **UNLINK** — create or remove edges between memories.
- **TXN_BEGIN** / **TXN_COMMIT** / **TXN_ABORT** — transactional bracket.
- **SUBSCRIBE** — stream changes to memories.
- **ADMIN_*** — operational primitives (snapshots, stats, maintenance triggers).

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_semantics_overview.md`](01_semantics_overview.md) | The big-picture semantics |
| [`02_encode.md`](02_encode.md) | ENCODE operation |
| [`03_recall.md`](03_recall.md) | RECALL operation |
| [`04_plan.md`](04_plan.md) | PLAN operation |
| [`05_reason.md`](05_reason.md) | REASON operation |
| [`06_forget.md`](06_forget.md) | FORGET operation |
| [`07_link_unlink.md`](07_link_unlink.md) | LINK and UNLINK |
| [`08_transactions.md`](08_transactions.md) | Transactional brackets |
| [`09_subscribe.md`](09_subscribe.md) | SUBSCRIBE |
| [`10_admin.md`](10_admin.md) | Admin operations |
| [`11_consistency.md`](11_consistency.md) | Consistency model |
| [`12_open_questions.md`](12_open_questions.md) | Unresolved questions |
| [`13_references.md`](13_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
