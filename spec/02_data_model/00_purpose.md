# 02. Data Model

> **TL;DR.** The four record types Brain stores and how they relate. Memory (raw text + 384-d embedding, identified by packed `MemoryId`), Entity (canonical noun, UUIDv7), Statement (typed claim — Fact, Preference, or Event — with confidence, evidence, and bi-temporal validity), Relation (typed binary edge between entities with cardinality and provenance). Statements and Relations cite Memories as evidence. Identifiers, contexts, salience, edges, memory kinds, lifecycle, and the property-graph choice live here.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Anyone who interacts with Brain at the data level — implementers, client implementers, operators, application developers |
| Voice | Hybrid (rationale + normative MUST/SHOULD) |
| Depends on | [01. System Architecture](../01_architecture/00_purpose.md) |
| Referenced by | All other specs |

## What this spec defines

The entities Brain stores and the relationships between them. This is the spec that defines what a "memory" is, what an "edge" is, what a "context" is, what identifiers look like, and how all of these evolve over time.

Every other spec depends on this one. The wire protocol carries the entities defined here; the storage layer persists them; the operations manipulate them.

This document defines Brain's data model: the four record types stored, their structural relationships, and how they evolve. It is the foundation for every spec that follows.

## What Brain stores

Brain stores four record types in one unified database:

| Record | What it captures | Identity |
|---|---|---|
| **Memory** | Raw episodic / semantic / consolidated experience text + 384-d embedding | `MemoryId` — packed `(shard, slot, version)` |
| **Entity** | Canonical nouns — Person, Organization, Project, Place… | `EntityId` — UUIDv7 |
| **Statement** | Typed claims about entities (Fact / Preference / Event) with provenance and confidence | `StatementId` — UUIDv7 |
| **Relation** | Typed binary edges between entities (`reports_to`, `works_at`, …) | `RelationId` — UUIDv7 |

All four are stored in the same per-shard redb + HNSW + tantivy stack. They share one wire protocol, one schema model, one extractor pipeline, one query path. There is no separate "typed graph" sitting on top of a "memory layer" — the four record types are co-equal nouns in one DB. Statements and Relations cite Memories as evidence; that's a `Vec<MemoryId>` field, not a layer boundary.

A typical flow:

- `ENCODE("Priya prefers async meetings for engineering syncs")` writes a Memory.
- The extractor pipeline produces an Entity (`Priya`) and a Statement (`Preference(Priya, prefers, "async meetings")`) with the Memory as evidence.
- `RECALL("what does Priya prefer?")` returns the Statement, joined back to the source Memory through the evidence list.

## What this document covers

- The vocabulary Brain chose (memory, recall, salience, decay, consolidation, edge) and the alternatives that were rejected. ([`01_cognitive_vocabulary.md`](01_cognitive_vocabulary.md))
- The `Memory` record — fields, identifiers, kinds (Episodic / Semantic / Consolidated), and lifecycle (Active → Tombstoned → Reclaimed). ([`02_memory.md`](02_memory.md))
- The `Context` — agent-scoped logical groupings. ([`03_context.md`](03_context.md))
- The salience model — the formula that drives ranking and decay. ([`04_salience.md`](04_salience.md))
- The eight typed edges between memories. ([`05_edges.md`](05_edges.md))
- Entity lifecycle, resolution, and merge. ([`06_entity_lifecycle.md`](06_entity_lifecycle.md))
- The Statement record (Fact / Preference / Event) with supersession, contradiction, and confidence aggregation. ([`07_statement.md`](07_statement.md))
- The Relation record (typed binary edges between entities). ([`08_relation.md`](08_relation.md))
- Composition: how the four record types interact, and why Brain uses a property graph (not RDF). ([`09_composition.md`](09_composition.md))
- Data-model failure modes. ([`10_failure_modes.md`](10_failure_modes.md))

## What this document does not cover

- **Byte-level storage layouts.** Defined in [08. Storage: Arena & WAL](../08_storage/00_purpose.md) and [10. Metadata + Graph Store](../10_metadata/00_purpose.md).
- **Wire-format encodings.** Defined in [04. Wire Protocol](../04_wire_protocol/00_purpose.md).
- **The semantics of operations on these records.** Defined in [05. Operations](../05_operations/00_purpose.md).
- **The HNSW graph that indexes vectors.** Defined in [09. Indexing](../09_indexing/00_purpose.md).

The split: this spec defines *what the records are*. Other specs define *how they are stored, transmitted, indexed, or operated on*.

## Why a dedicated data-model spec

The data model is the contract between every other component. Putting it in one place — referenced from everywhere — keeps the components consistent. If `MemoryId`'s format is documented here and another spec describes it differently, the other spec is wrong.

This also serves implementers building from scratch: read this spec, implement the data structures, then read the storage and operations specs to know what to do with them.

## Conventions

- **Field types** are written in Rust syntax: `u64`, `[u8; 16]`, `String`, `Vec<EdgeId>`.
- **Sizes** are explicit where they matter for storage layout: a `MemoryId` is 16 bytes, a vector is 1536 bytes (384 × 4).
- **Invariants** are called out as `INVARIANT:` blocks. They are properties the system maintains; if they are observed to be violated, that's a bug.
- **Examples** are illustrative; they don't constrain the implementation.

## Position in the spec series

This is spec 02, immediately after [01. System Architecture](../01_architecture/00_purpose.md) and before everything else. The dependency chain is:

```
01 (System Architecture) → 02 (Data Model) → 03..24 (everything else)
```

If you came here without reading 01 first, you might be missing the context for *why* the data model is shaped this way. Consider [`../01_architecture/03_primitives.md`](../01_architecture/03_primitives.md) for the cognitive primitives this data model exists to serve.

