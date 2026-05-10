# 02.00 Purpose

This document defines Brain's data model: the entities stored, their structural relationships, and how they evolve. It is the foundation for every spec that follows.

## What this document covers

- The cognitive vocabulary Brain chose (memory, recall, salience, decay, consolidation, edge) and the alternatives that were rejected. ([`01_cognitive_vocabulary.md`](01_cognitive_vocabulary.md))
- The `Memory` entity — its fields, semantics, invariants, and lifecycle. ([`02_memory_entity.md`](02_memory_entity.md))
- Identifier formats and their stability properties. ([`03_identifiers.md`](03_identifiers.md))
- The `Context` entity — agent-scoped logical groupings. ([`04_context.md`](04_context.md))
- The salience model — the formula that drives ranking and decay. ([`05_salience.md`](05_salience.md))
- The eight typed edges and their semantics. ([`06_edges.md`](06_edges.md))
- Memory kinds: episodic, semantic, consolidated. ([`07_memory_kinds.md`](07_memory_kinds.md))
- The memory lifecycle, from creation through tombstoning to reclamation. ([`08_lifecycle.md`](08_lifecycle.md))
- How the data model can evolve while preserving compatibility. ([`09_schema_evolution.md`](09_schema_evolution.md))

## What this document does not cover

- **Byte-level storage layouts.** Defined in [05. Storage: Arena & WAL](../05_storage_arena_wal/) and [07. Metadata + Graph Store](../07_metadata_graph/).
- **Wire-format encodings.** Defined in [03. Wire Protocol](../03_wire_protocol/).
- **The semantics of cognitive operations on these entities.** Defined in [09. Cognitive Operations](../09_cognitive_operations/).
- **The HNSW graph that indexes these vectors.** Defined in [06. ANN Index](../06_ann_index/).

The split: this spec defines *what the entities are*. Other specs define *how the entities are stored, transmitted, indexed, or operated on*.

## Why a dedicated data-model spec

The data model is the contract between every other component. Putting it in one place — referenced from everywhere — keeps the components consistent. If `MemoryId`'s format is documented here and another spec describes it differently, the other spec is wrong.

This also serves implementers building from scratch: read this spec, implement the data structures, then read the storage and operations specs to know what to do with them.

## Conventions

- **Field types** are written in Rust syntax: `u64`, `[u8; 16]`, `String`, `Vec<EdgeId>`.
- **Sizes** are explicit where they matter for storage layout: a `MemoryId` is 16 bytes, a vector is 1536 bytes (384 × 4).
- **Invariants** are called out as `INVARIANT:` blocks. They are properties the system maintains; if they are observed to be violated, that's a bug.
- **Examples** are illustrative; they don't constrain the implementation.

## Position in the spec series

This is spec 02, immediately after [01. System Architecture](../01_system_architecture/) and before everything else. The dependency chain is:

```
01 (System Architecture) → 02 (Data Model) → 03..16 (everything else)
```

If you came here without reading 01 first, you might be missing the context for *why* the data model is shaped this way. Consider [`../01_system_architecture/03_primitives.md`](../01_system_architecture/03_primitives.md) for the cognitive primitives this data model exists to serve.

---

*Continue to [`01_cognitive_vocabulary.md`](01_cognitive_vocabulary.md) for the vocabulary.*
