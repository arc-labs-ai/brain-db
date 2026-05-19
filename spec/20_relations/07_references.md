# 20.07 References

Cross-links from §20 to the rest of the spec.

## Sibling knowledge-layer sections

| Target | §20 file referencing |
|---|---|
| [`../17_knowledge_model/00_purpose.md`](../17_knowledge_model/00_purpose.md) | Three-layer model; relations are layer 2 edges. |
| [`../18_entities/00_purpose.md`](../18_entities/00_purpose.md) | Endpoints of every relation. |
| [`../18_entities/01_resolution.md`](../18_entities/01_resolution.md) | from/to entities must be resolved at relation-create time. |
| [`../19_statements/00_purpose.md`](../19_statements/00_purpose.md) | Relation vs Statement guidance in §00 §"Relation vs Statement". |
| [`../19_statements/01_supersession.md`](../19_statements/01_supersession.md) | Supersession mechanics relations inherit. |
| [`../21_schema_dsl/`](../21_schema_dsl/) | `RelationType` declarations + cardinality + symmetric flag. |
| [`../22_extractors/00_purpose.md`](../22_extractors/00_purpose.md) | Three-tier extractor system; phase-22 relation extraction. |
| [`../25_provenance_versioning/00_purpose.md`](../25_provenance_versioning/00_purpose.md) | Audit + versioning model relations feed into. |
| [`../26_knowledge_storage/00_purpose.md`](../26_knowledge_storage/00_purpose.md) | redb storage catalog; relation tables in [`./03_storage.md`](./03_storage.md) at fine grain. |
| [`../27_knowledge_workers/00_purpose.md`](../27_knowledge_workers/00_purpose.md) | Background workers — FORGET cascade, supersession sweep. |

## Wire-protocol counterparts

| Target | §20 file referencing |
|---|---|
| [`../28_knowledge_wire_protocol/07_relation_frames.md`](../28_knowledge_wire_protocol/07_relation_frames.md) | Wire shape for every relation opcode — CREATE / GET / SUPERSEDE / TOMBSTONE / LIST_FROM / LIST_TO / TRAVERSE. |
| [`../28_knowledge_wire_protocol/02_subscribe_events.md`](../28_knowledge_wire_protocol/02_subscribe_events.md) | `RelationCreated`, `RelationSuperseded`, `RelationTombstoned` event shapes. |
| [`../28_knowledge_wire_protocol/03_errors.md`](../28_knowledge_wire_protocol/03_errors.md), [`../03_wire_protocol/10_errors.md`](../03_wire_protocol/10_errors.md) | Error code mapping: `RELATION_NOT_FOUND`, `RELATION_TYPE_MISMATCH`, `RelationTypeNotInSchema` (0x004C), `CardinalityViolation` (0x0065). |
| [`../28_knowledge_wire_protocol/04_validation.md`](../28_knowledge_wire_protocol/04_validation.md) §4 | Field-level validation rules. |

## Substrate dependencies

| Target | §20 file referencing |
|---|---|
| [`../03_wire_protocol/`](../03_wire_protocol/) | Wire framing layer. |
| [`../05_storage_arena_wal/`](../05_storage_arena_wal/) | Single-writer-per-shard discipline that makes multi-table atomicity work. |
| [`../09_cognitive_operations/05_forget.md`](../09_cognitive_operations/05_forget.md) | FORGET op's cascade fires [`./05_evidence.md`](./05_evidence.md) §5. |

## Code references

Phase 18 implementation lives in:

| Concern | Code path |
|---|---|
| `Relation` value type | `crates/brain-core/src/knowledge/relation.rs` (phase 18.2) |
| `Cardinality` enum (OneToOne / OneToMany / ManyToOne / ManyToMany) | `crates/brain-core/src/knowledge/kinds.rs` (phase 15.1 prep — live) |
| `RelationId`, `RelationTypeId` id types | `crates/brain-core/src/knowledge/ids.rs` (phase 15.1 prep — live) |
| redb tables — relations + 2 directional + evidence | `crates/brain-metadata/src/tables/knowledge/relation.rs` (phase 15.1 prep — live) |
| redb tables — relation_type registry | `crates/brain-metadata/src/tables/knowledge/relation_type.rs` (phase 15.1 prep — live) |
| Relation-type registry ops + built-ins | `crates/brain-metadata/src/relation_type_ops.rs` (phase 18.3) |
| Relation CRUD + cardinality enforcement + symmetric canonicalisation | `crates/brain-metadata/src/relation_ops.rs` (phase 18.4) |
| Graph traversal | `crates/brain-metadata/src/relation_traversal.rs` (phase 18.5) |
| Wire-op request / response structs | `crates/brain-protocol/src/knowledge/relation_req.rs` + `_resp.rs` (phase 18.6) |
| Wire-op handlers + event emission | `crates/brain-ops/src/ops/knowledge_relation.rs` (phase 18.7) |
| SDK relation builders | `crates/brain-sdk-rust/src/knowledge/relation.rs` (phase 18.8) |
| Integration tests | `crates/brain-server/tests/knowledge_relation*.rs` (phase 18.9) |

## Process references

| Document | Purpose |
|---|---|
| [`../../.claude/plans/phase-18.md`](../../.claude/plans/phase-18.md) | Phase 18 master plan (this implementation). |
| [`../../docs/development/phases/phase-18-relations.md`](../../docs/development/phases/phase-18-relations.md) | Phase 18 sub-task index (original; superseded by the .claude plan). |

## §20 internal file map

For navigation:

| File | Purpose |
|---|---|
| [`./00_purpose.md`](./00_purpose.md) | Schema, type declarations, indexes (high-level). |
| [`./01_cardinality.md`](./01_cardinality.md) | Cardinality variants + supersession rules. |
| [`./02_symmetric.md`](./02_symmetric.md) | Canonical from/to ordering + dual-index reads. |
| [`./03_storage.md`](./03_storage.md) | redb table layout. |
| [`./04_traversal.md`](./04_traversal.md) | BFS algorithm, depth/branching caps, cycle detection. |
| [`./05_evidence.md`](./05_evidence.md) | Flat Vec<MemoryId>; FORGET cascade. |
| [`./06_open_questions.md`](./06_open_questions.md) | Known gaps + deferrals. |
| [`./07_references.md`](./07_references.md) | This file. |
