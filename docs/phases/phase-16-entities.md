# Phase 16: Entity Layer

## Goal

Implement the Entity table, the entity type system, the entity HNSW for embedding-based resolution, and the entity resolver (tiers 1, 2, 3). After this phase, entities can be created, looked up, resolved, renamed, and merged programmatically.

## Prerequisites

- Phase 15 complete.

## Reading list

- `18_entities/00_purpose.md`
- `18_entities/01_resolution.md`
- `18_entities/02_storage.md`

## Outputs

- `EntityId`, `Entity`, `EntityType` types in `brain-core`.
- redb tables in `brain-metadata` populated with empty data.
- Entity HNSW per shard (using same `hnsw_rs` as memory HNSW).
- Resolver implementing tiers 1 (exact), 2 (trigram fuzzy), 3 (embedding similarity). Tier 4 (LLM) stub only — not active.
- Wire opcodes 0x30-0x38 implemented.
- SDK helpers for entity CRUD.

## Sub-tasks

### 16.1 Entity record types and rkyv serialization

**Reads:** `18_entities/00_purpose.md` (schema).
**Writes:** `crates/brain-core/src/entity.rs`.
**Done when:** `Entity`, `EntityType`, `EntityAttributes`, `EntityId` types compile; rkyv archive/deserialize round-trips correctly.
**Pitfalls:** Stable serialization: pin rkyv version, mark all archived structs `#[derive(CheckBytes)]`.

### 16.2 redb entity table operations

**Reads:** `18_entities/02_storage.md`.
**Writes:** `crates/brain-metadata/src/entity_ops.rs`.
**Done when:** `entity_get`, `entity_put`, `entity_list`, `entity_by_canonical_name`, `entity_aliases` reads/writes work. Compound key indexes function.
**Pitfalls:** Normalize names before indexing (lowercase, whitespace-collapse). Test case-sensitivity.

### 16.3 Entity HNSW per shard

**Reads:** `18_entities/02_storage.md`; `spec/06_ann_index/`.
**Writes:** `crates/brain-index/src/entity_hnsw.rs`.
**Done when:** can insert, search, tombstone entity embeddings. Tombstone+rebuild cycle works.
**Pitfalls:** Different parameters than memory HNSW (smaller); document the difference.

### 16.4 Trigram index (entity_trigrams)

**Reads:** `18_entities/02_storage.md` (entity_trigrams table); `18_entities/01_resolution.md` (tier 2).
**Writes:** `crates/brain-metadata/src/trigram.rs`.
**Done when:** trigram extraction from strings, write/read trigram → entity index, Jaccard similarity computation.
**Pitfalls:** Pre-compute trigrams on entity create; small overhead, but resolves are fast.

### 16.5 Resolver tiers 1 + 2 + 3

**Reads:** `18_entities/01_resolution.md`.
**Writes:** `crates/brain-core/src/resolver.rs`.
**Done when:** `resolve_entity` runs tiers 1-3, returns `ResolutionOutcome`. Tier 4 (LLM) is a stub returning "not implemented." Tier 5 creates new entity.
**Pitfalls:** Configurable thresholds. Test with: exact hit, fuzzy near-miss, embedding match, ambiguous (multiple candidates), no match.

### 16.6 Entity create / get / update wire opcodes

**Reads:** `28_knowledge_wire_protocol/00_purpose.md` (entity opcodes 0x30-0x38).
**Writes:** `crates/brain-protocol/src/knowledge/entity.rs`, `crates/brain-server/src/handlers/knowledge/entity.rs`.
**Done when:** wire opcodes 0x30-0x33 (CREATE, GET, UPDATE, RENAME) work end-to-end.
**Pitfalls:** Validate entity type exists; validate attributes against type schema; validate unique constraints.

### 16.7 Entity merge with grace-period rollback

**Reads:** `18_entities/00_purpose.md` (merge section).
**Writes:** `crates/brain-core/src/merge.rs`, `crates/brain-server/src/handlers/knowledge/entity_merge.rs`.
**Done when:** opcode 0x34 (MERGE) and 0x35 (UNMERGE) work. Merge updates redirect; unmerge within grace period restores.
**Pitfalls:** Atomic operation: all redirects in one redb transaction. Test concurrent merges.

### 16.8 SDK helpers for entity CRUD

**Reads:** `29_knowledge_sdk/00_purpose.md` (entity API section).
**Writes:** `crates/brain-sdk-rust/src/knowledge/entity.rs`.
**Done when:** typed entity SDK works for at least one example entity type (Person, with derive macro).
**Pitfalls:** Derive macro: generates schema metadata, serialization, constructor. Test the macro with a non-trivial entity type.

### 16.9 Tests

**Reads:** existing substrate test patterns.
**Writes:** `tests/knowledge_entities.rs`.
**Done when:** unit tests for resolver tiers, integration test for create-merge-unmerge-rename cycle, performance test for resolver under load.
**Pitfalls:** Fuzz the resolver with adversarial inputs (Unicode, very long strings, empty strings).

## Done-when (phase)

- All sub-tasks pass tests.
- Entity create / get / update / merge / rename: all work via wire + SDK.
- Resolver returns correct outcomes for the documented test cases.
- Entity HNSW search: P50 ≤ 5 ms for 100K entities, ≤ 50 ms for 1M.
- substrate-only mode regression: still passes.

## Pitfalls

- Entity types are user-declared but this phase has no schema DSL yet (phase 19). Use a hardcoded `Person` type for testing.
- Don't activate any extractors. Phase 20+.
- Resolver tier 4 (LLM) is a stub. Don't wire it up.
