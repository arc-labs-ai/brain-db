# 01. System Architecture

> **TL;DR.** Brain stores four record types (Memory, Entity, Statement, Relation) for AI agents, with explicit provenance, confidence, and bi-temporal validity. Retrieval fuses semantic, lexical, entity-graph, and temporal signals with weighted rank fusion. One Rust core, one wire protocol, one schema, Apache 2.0. This section frames the system: the three problems it solves, the layered architecture, hardware envelope, non-goals, and the five competitive design wedges every decision protects.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Senior systems engineers, ML engineers building agent systems |
| Voice | Hybrid (rationale + normative MUST/SHOULD where applicable) |
| Depends on | (none — this is the foundational document) |
| Referenced by | All other specs |

## What this spec defines

The architecture of **Brain — a memory database for AI agents**. This is the foundational specification — every other document in this series builds on the abstractions, terminology, and component boundaries defined here.

Detail specs (02 through 19) define byte layouts, algorithms, protocols, and operational procedures. If a detail spec contradicts this one, the detail spec is wrong unless explicitly amending this one.

### Audience expectations

The reader is assumed to know:

- Rust at production-engineering level.
- Async I/O concepts (futures, executors, backpressure).
- Basic distributed-systems vocabulary (replication, consistency models, sharding).
- Basic ML vocabulary (embeddings, similarity, transformers).
- Linux as a deployment target.

Brain does not assume the reader has built a vector index, a wire protocol, or an agent system before. Where these are needed for the architecture to make sense, [`02_background.md`](02_background.md) provides the missing context.

## What is Brain

Brain stores typed memories — facts, preferences, events, entities, and relations — for AI agents. Four record types in one database:

- **Memory** — raw experience text + 384-d embedding (`ENCODE` writes; `RECALL` reads)
- **Entity** — canonical nouns (Person, Organization, …) with stable identity
- **Statement** — typed claims about entities (Fact / Preference / Event) with confidence, evidence, and bi-temporal validity (`valid_from`, `valid_to`, `extracted_at`, `record_invalidated_at`)
- **Relation** — typed binary edges between entities with cardinality and provenance

Retrieval fuses semantic, lexical, entity-graph, and temporal signals with weighted rank fusion. One Rust core, one wire protocol, one schema model. Apache 2.0. Embedded / self-hosted / cluster from the same crates.

## Three problems Brain solves

Every agent-memory system in production today fails at one of three things. Brain fixes each at the data layer, not in the prompt.

### 1. Extraction produces junk

The standard "save the conversation to a vector DB" pattern stores hash duplicates, hallucinated user profiles, transient small-talk, and feedback-loop contamination alongside durable signal. A 32-day production audit of Mem0 showed 97.8% of stored memories were junk.

Brain's three-tier extractor pipeline (pattern → GLiNER classifier → LLM with prompt cache) rejects 80-90% of naive candidates before they touch storage. The five-tier supersession ladder (Tier 0 exact match through Tier 4 LLM judge) catches conflicts at write time, not query time.

### 2. Flat text loses temporal and relational signal

`"Alice prefers dark mode"` is a string to most vector DBs. Brain stores it as a typed `Statement(Preference, subject=alice, predicate=prefers, object="dark mode", valid_from=t0, confidence=0.92, evidence=[memory_id])`.

LongMemEval (ICLR 2025) shows existing systems achieve only 5-65% accuracy on temporal reasoning queries. Brain's bi-temporal model + entity graph answers `"who does the user work for?"` by entity traversal, `"what happened last week?"` by time range, and `"what did I believe about Alice on March 1?"` by `as_of` query — none of which similarity-only search can do.

### 3. Existing systems lock you in

- **Mem0** gates graph features behind a $249/mo cloud tier.
- **Zep** requires Neo4j as a dependency and has known latency / concurrency issues.
- **Pinecone serverless** meters per-segment access.
- **Letta** requires its hosted runtime to use the agent framework.

Brain ships **embedded** (in-process), **self-hosted** (single binary), and **cluster** (multi-node, when needed) from the same Apache 2.0 crates. Every feature in the spec is in the open codebase.

## What this document covers

- The conceptual frame: five cognitive primitives (`ENCODE`, `RECALL`, `PLAN`, `REASON`, `FORGET`). ([`03_primitives.md`](03_primitives.md))
- The layered architecture: seven components, their boundaries, design constraints. ([`04_layers.md`](04_layers.md))
- Hardware envelope and the capacity / latency / throughput targets that envelope delivers. ([`05_hardware_and_targets.md`](05_hardware_and_targets.md))
- Scope: explicit non-goals plus comparison with vector stores, graph DBs, and agent-memory frameworks. ([`06_scope_and_comparison.md`](06_scope_and_comparison.md))
- **Design wedges + roadmap**: five competitive differentiators every decision protects, plus future-looking patterns Brain may borrow from other databases. ([`07_wedges_and_roadmap.md`](07_wedges_and_roadmap.md))
- **Glossary**: vocabulary used throughout the spec series. ([`08_glossary.md`](08_glossary.md))

## What this document does not cover

- **Wire-format byte layouts.** Defined in [04. Wire Protocol](../04_wire_protocol/00_purpose.md).
- **On-disk storage formats.** Defined in [08. Storage: Arena & WAL](../08_storage/00_purpose.md) and [10. Metadata + Graph Store](../10_metadata/00_purpose.md).
- **Cognitive operation semantics in depth.** Defined in [05. Operations](../05_operations/00_purpose.md).
- **Concurrency and epoch model.** Defined in [14. Concurrency](../14_concurrency/00_purpose.md).
- **Failure-mode procedures.** Defined in [18. Failure Recovery](../18_failure_recovery/00_purpose.md).

## Audience

Senior systems engineers and ML engineers with systems background. Terms are defined on first use; further-reading links throughout.

A reader who has built distributed systems and used embedding models will find no individually-new ideas here — just an unusual combination, applied to a problem that has historically been solved with brittle ad-hoc scaffolding. The contribution is the synthesis.

## Voice and conventions

Two voices mixed throughout:

- **Third-person factual** ("Brain uses…", "Brain accepts…") for rationale, design discussion, trade-off analysis.
- **Third-person normative** ("the server MUST…", "implementations SHOULD…") for requirements that bind implementations.

Requirements follow [RFC 2119 conventions](https://www.rfc-editor.org/rfc/rfc2119). Code identifiers in `monospace`. Concept names in **bold** when first introduced. Cross-references to other docs use *NN. Document Title*; cross-references within this section use [`file.md`](file.md).

## Release status

Brain is **pre-release (v0.1.0)**. No external users. The wire protocol, redb tables, and schema model are in flux until the v1.0 release; the release gate is the combined acceptance suite at [`../19_benchmarks/06_complete_acceptance.md`](../19_benchmarks/06_complete_acceptance.md).

---

*Continue to [`01_problem.md`](01_problem.md) for the motivating problem.*
