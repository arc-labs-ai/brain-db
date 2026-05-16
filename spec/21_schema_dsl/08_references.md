# 21.08 References

Cross-links from §21 to the rest of the spec.

## Sibling knowledge-layer sections

| Target | §21 file referencing |
|---|---|
| [`../17_knowledge_model/00_purpose.md`](../17_knowledge_model/00_purpose.md) | Three-layer model; the schema declares the user-facing shape of layers 2 + 3. |
| [`../18_entities/00_purpose.md`](../18_entities/00_purpose.md) | Entity-type declarations consumed by entity_ops validation. |
| [`../19_statements/00_purpose.md`](../19_statements/00_purpose.md) | Predicate declarations consumed by statement_ops validation. |
| [`../20_relations/00_purpose.md`](../20_relations/00_purpose.md) | Relation-type declarations consumed by relation_ops validation; cardinality + symmetric flags. |
| [`../22_extractors/00_purpose.md`](../22_extractors/00_purpose.md) | Extractor declarations consumed by the extractor workers (phase 20+). |
| [`../25_provenance_versioning/00_purpose.md`](../25_provenance_versioning/00_purpose.md) | `schema_version` field on every write. |
| [`../26_knowledge_storage/00_purpose.md`](../26_knowledge_storage/00_purpose.md) | `schema_versions` + `schema_active_versions` redb tables. |
| [`../27_knowledge_workers/00_purpose.md`](../27_knowledge_workers/00_purpose.md) | (Future) migration worker — out of scope per v1. |

## Wire-protocol counterparts

| Target | §21 file referencing |
|---|---|
| [`../28_knowledge_wire_protocol/05_schema_frames.md`](../28_knowledge_wire_protocol/05_schema_frames.md) | Wire shape for `SCHEMA_UPLOAD / _GET / _LIST / _VALIDATE` opcodes 0x0120-0x0123. |
| [`../28_knowledge_wire_protocol/02_subscribe_events.md`](../28_knowledge_wire_protocol/02_subscribe_events.md) | `SchemaUpdated` event emitted on successful upload. |
| [`../28_knowledge_wire_protocol/03_errors.md`](../28_knowledge_wire_protocol/03_errors.md) | `SCHEMA_VALIDATION_FAILED` error code (per-error list). |

## Substrate dependencies

| Target | §21 file referencing |
|---|---|
| [`../03_wire_protocol/`](../03_wire_protocol/) | Wire framing. |
| [`../05_storage_arena_wal/`](../05_storage_arena_wal/) | Single-writer-per-shard discipline for atomic schema upload. |

## Code references

Phase 19 implementation lives in:

| Concern | Code path |
|---|---|
| Schema AST | `crates/brain-protocol/src/schema/ast.rs` (phase 19.2) |
| DSL parser (pest) | `crates/brain-protocol/src/schema/parser.rs` (phase 19.3) |
| Validator | `crates/brain-protocol/src/schema/validator.rs` (phase 19.4) |
| Schema-version redb tables | `crates/brain-metadata/src/tables/knowledge/schema_version.rs` (phase 15.1 — widening in 19.5) |
| Schema store ops | `crates/brain-metadata/src/schema_store.rs` (phase 19.5) |
| Wire request / response structs | `crates/brain-protocol/src/knowledge/schema_req.rs` + `_resp.rs` (phase 19.6) |
| Wire-op handlers + event emission | `crates/brain-ops/src/ops/knowledge_schema.rs` (phase 19.6) |
| System schema source | `crates/brain-metadata/src/system_schema/schema.brain` (phase 19.7) |
| System schema loader | `crates/brain-metadata/src/system_schema/mod.rs` (phase 19.7) |
| SDK schema builder + entries | `crates/brain-sdk-rust/src/knowledge/schema.rs` (phase 19.8) |
| Derive macros (if scope permits) | `crates/brain-sdk-macros/` (phase 19.9 — may split / defer) |
| Integration tests | `crates/brain-server/tests/knowledge_schema*.rs` (phase 19.10a) |

## Process references

| Document | Purpose |
|---|---|
| [`../../.claude/plans/phase-19.md`](../../.claude/plans/phase-19.md) | Phase 19 master plan (this implementation). |
| [`../../docs/phases/phase-19-schema-dsl.md`](../../docs/phases/phase-19-schema-dsl.md) | Phase 19 sub-task index (original; superseded by the .claude plan). |

## §21 internal file map

For navigation:

| File | Purpose |
|---|---|
| [`./00_purpose.md`](./00_purpose.md) | Overview + example + simplified grammar. |
| [`./01_grammar.md`](./01_grammar.md) | Formal EBNF. |
| [`./02_ast.md`](./02_ast.md) | Typed AST. |
| [`./03_validator.md`](./03_validator.md) | Static validation + error model. |
| [`./04_namespaces.md`](./04_namespaces.md) | Multi-namespace isolation. |
| [`./05_versioning.md`](./05_versioning.md) | Per-namespace version counter + persistence. |
| [`./06_system_schema.md`](./06_system_schema.md) | Built-in `brain:` types loaded from a parsed static schema. |
| [`./07_open_questions.md`](./07_open_questions.md) | Deferrals (incl. migration plan — out of v1 scope). |
| [`./08_references.md`](./08_references.md) | This file. |
