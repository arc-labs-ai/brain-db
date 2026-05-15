# Phase 18: Relation Layer

## Goal

Implement the Relation table, typed edges between entities, cardinality enforcement, symmetric relations, and 1-2 hop traversal.

## Prerequisites

- Phase 17 complete.

## Reading list

- `20_relations/00_purpose.md`

## Outputs

- `Relation`, `RelationType`, `Cardinality` types.
- redb tables: `relations`, `relations_by_from`, `relations_by_to`, `relations_by_evidence`, `relation_types`.
- Wire opcodes 0x50-0x56.
- SDK relation API.
- 1-3 hop traversal implementation (with depth cap).

## Sub-tasks

### 18.1 Relation record types

**Reads:** `20_relations/00_purpose.md`.
**Writes:** `crates/brain-core/src/relation.rs`.
**Done when:** `Relation`, `RelationType`, `Cardinality` types compile + rkyv.

### 18.2 redb relation operations and indexes

**Reads:** `20_relations/00_purpose.md` (indexes section).
**Writes:** `crates/brain-metadata/src/relation_ops.rs`.
**Done when:** create/get/list_from/list_to/tombstone work; both directional indexes maintained.

### 18.3 Cardinality enforcement

**Reads:** `20_relations/00_purpose.md` (cardinality section).
**Writes:** `crates/brain-core/src/relation_cardinality.rs`.
**Done when:** many-to-one prevents two current relations from same `from`; one-to-one prevents in both directions; many-to-many always allowed.
**Pitfalls:** "Current" means non-superseded, non-tombstoned. Test thoroughly.

### 18.4 Symmetric relation storage

**Reads:** `20_relations/00_purpose.md` (symmetric relations).
**Writes:** in `relation_ops.rs`.
**Done when:** symmetric relations stored once (with deterministic from/to ordering by EntityId byte-order), but readable from either side via the index.
**Pitfalls:** When `symmetric=true`, swap from/to before write if needed. Document the canonicalization.

### 18.5 1-3 hop traversal

**Reads:** `20_relations/00_purpose.md` (graph queries).
**Writes:** `crates/brain-core/src/traversal.rs`.
**Done when:** `traverse(start, types, depth, direction)` returns reachable entities + path metadata. Max depth 5 (cap), default 3.
**Pitfalls:** Cycle detection via visited set. Branching factor cap (e.g., 1000 per level) to prevent runaway. Tests for 1-hop, 2-hop, cycle, branching.

### 18.6 Wire opcodes 0x50-0x56

**Reads:** `28_knowledge_wire_protocol/00_purpose.md`.
**Writes:** `crates/brain-protocol/src/knowledge/relation.rs`, `crates/brain-server/src/handlers/knowledge/relation.rs`.
**Done when:** all 7 opcodes work; TRAVERSE returns structured paths.

### 18.7 SDK relation API

**Reads:** `29_knowledge_sdk/00_purpose.md` (relation API).
**Writes:** `crates/brain-sdk-rust/src/knowledge/relation.rs`.
**Done when:** typed relation API with derive macro works for a non-trivial relation type.

### 18.8 Tests

**Writes:** `tests/knowledge_relations.rs`.
**Done when:** create+list+traverse tests for asymmetric and symmetric. Cardinality violation tests. Cycle test.

## Done-when (phase)

- Create relation between entities, list from either direction.
- Cardinality respected, symmetric works.
- 1-3 hop traversal returns correct paths.
- Traversal terminates on cycles within bounds.

## Pitfalls

- Don't conflate `LINK` (substrate, memory-to-memory) with `RELATION_CREATE` (knowledge layer, entity-to-entity). Keep them separate.
- TRAVERSE timeout for deep/wide graphs; respect query budget.
