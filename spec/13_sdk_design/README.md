# 13. SDK Design

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | SDK authors; agent integrators |
| Voice | Hybrid (rationale + normative) |
| Depends on | [03. Wire Protocol](../03_wire_protocol/), [09. Cognitive Operations](../09_cognitive_operations/) |
| Referenced by | — |

## What this spec defines

The design of client SDKs for Brain. Includes both the abstract contract every SDK should fulfill and concrete idioms for specific languages.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_principles.md`](01_principles.md) | Design principles |
| [`02_core_api.md`](02_core_api.md) | Core API shape |
| [`03_connection.md`](03_connection.md) | Connection management |
| [`04_retries.md`](04_retries.md) | Retry policy |
| [`05_streams.md`](05_streams.md) | Streaming responses |
| [`06_idiomatic_languages.md`](06_idiomatic_languages.md) | Language-specific idioms |
| [`07_observability.md`](07_observability.md) | SDK-level observability |
| [`08_testing.md`](08_testing.md) | Test support |
| [`09_versioning.md`](09_versioning.md) | SDK versioning |
| [`10_open_questions.md`](10_open_questions.md) | Unresolved questions |
| [`11_references.md`](11_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
