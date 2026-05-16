# Phase 19: Schema DSL

> **Status:** ✓ complete. Superseded by the per-sub-task plans in
> [`.claude/plans/phase-19*.md`](../../.claude/plans/) and the
> "Phase 19" entry in [`ROADMAP.md`](../../ROADMAP.md).
>
> **Scope cut:** migration plan computation was omitted per user
> direction (no existing deployments to migrate). Tracked as
> [`spec/21_schema_dsl/07_open_questions.md`](../../spec/21_schema_dsl/07_open_questions.md)
> Q3.

## Goal

Parse and validate the schema DSL. Implement schema upload, versioning, and migration semantics. Replace hardcoded test types with user-declared schemas.

## Prerequisites

- Phase 16 complete.

## Reading list

- `21_schema_dsl/00_purpose.md`

## Outputs

- DSL parser (`pest` or `nom`-based).
- Schema validator.
- Schema versioning (redb table `schema_versions`).
- Wire opcodes 0x20-0x26.
- SDK schema builder (programmatic API).
- Migration plan computation.

## Sub-tasks

### 19.1 DSL grammar and parser

**Reads:** `21_schema_dsl/00_purpose.md` (grammar section).
**Writes:** `crates/brain-protocol/src/schema/parser.rs`.
**Done when:** parser accepts all examples in spec; produces typed AST.
**Pitfalls:** Pin parser library (pest 2.x); test malformed inputs.

### 19.2 Schema validator

**Reads:** `21_schema_dsl/00_purpose.md` (validation rules).
**Writes:** `crates/brain-protocol/src/schema/validator.rs`.
**Done when:** detects unresolved type refs, duplicate definitions, kind/object inconsistency, predicate conflicts, etc.
**Pitfalls:** Error messages with source locations (line, column).

### 19.3 Schema persistence and versioning

**Reads:** `21_schema_dsl/00_purpose.md` (schema versioning).
**Writes:** `crates/brain-metadata/src/schema_store.rs`.
**Done when:** SCHEMA_UPLOAD increments version, stores document, current version retrievable.

### 19.4 Wire opcodes 0x20-0x26

**Reads:** `28_knowledge_wire_protocol/00_purpose.md`.
**Writes:** `crates/brain-server/src/handlers/knowledge/schema.rs`.
**Done when:** all schema opcodes work via wire.

### 19.5 Replace hardcoded types with declared types

**Reads:** phases 16 and 18.
**Writes:** updates to `crates/brain-core/src/entity.rs` and `relation.rs` to consult `EntityType`/`RelationType` lookups.
**Done when:** Entity create validates against declared type from schema; cardinality is read from declared RelationType.

### 19.6 Migration plan computation

**Reads:** `21_schema_dsl/00_purpose.md` (migration semantics).
**Writes:** `crates/brain-core/src/migration.rs`.
**Done when:** on schema upload, computes plan: which extractors changed, which statements need re-extraction, what flagged stale.
**Pitfalls:** Plan computation should be pure; execution (actual re-extraction) is by the migration worker in phase 24.

### 19.7 SDK schema builder (programmatic)

**Reads:** `29_knowledge_sdk/00_purpose.md` (schema management).
**Writes:** `crates/brain-sdk-rust/src/knowledge/schema.rs`.
**Done when:** `SchemaBuilder` API produces valid schema documents (text or AST); derive macros contribute entity/relation type defs.

### 19.8 Tests

**Writes:** `tests/knowledge_schema.rs`.
**Done when:** parser tests (positive and negative), validator tests, upload-then-create-entity flow with type validation, schema versioning across multiple uploads.

## Done-when (phase)

- Schema DSL parses all spec examples.
- Validation errors are clear and locate the source.
- SCHEMA_UPLOAD versions correctly.
- Subsequent CREATE_ENTITY etc. respect the declared types.

## Pitfalls

- Schema upload should be atomic: validate fully before incrementing version.
- Extractor declarations are parsed but not yet *executed* (phases 17, 18).
- Built-in extractors are *declared* in code as a separate "system schema"; the user schema can reference them.
