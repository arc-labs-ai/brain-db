# 02. Data Model

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Anyone who interacts with Brain at the data level — implementers, SDK authors, operators, application developers |
| Voice | Hybrid (rationale + normative MUST/SHOULD) |
| Depends on | [01. System Architecture](../01_system_architecture/) |
| Referenced by | All other specs |

## What this spec defines

The entities Brain stores and the relationships between them. This is the spec that defines what a "memory" is, what an "edge" is, what a "context" is, what identifiers look like, and how all of these evolve over time.

Every other spec depends on this one. The wire protocol carries the entities defined here; the storage layer persists them; the cognitive operations manipulate them.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this document covers |
| [`01_cognitive_vocabulary.md`](01_cognitive_vocabulary.md) | The vocabulary chosen and the alternatives rejected |
| [`02_memory_entity.md`](02_memory_entity.md) | The `Memory` — the central entity |
| [`03_identifiers.md`](03_identifiers.md) | Identifier formats: MemoryId, AgentId, ContextId, RequestId, ShardId |
| [`04_context.md`](04_context.md) | Contexts as logical scopes |
| [`05_salience.md`](05_salience.md) | The salience model: initial computation, update rules, decay |
| [`06_edges.md`](06_edges.md) | The eight typed edges |
| [`07_memory_kinds.md`](07_memory_kinds.md) | Episodic, Semantic, Consolidated |
| [`08_lifecycle.md`](08_lifecycle.md) | Active → Tombstoned → Reclaimed |
| [`09_schema_evolution.md`](09_schema_evolution.md) | How the data model evolves over time |
| [`10_failure_modes.md`](10_failure_modes.md) | Data-model-level failure modes |
| [`11_open_questions.md`](11_open_questions.md) | Unresolved questions |
| [`12_references.md`](12_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
