# 18.07 References

Cross-links from §18 to the rest of the spec.

## Sibling knowledge-layer sections

| Target | §18 file referencing |
|---|---|
| [`../17_knowledge_model/00_purpose.md`](../17_knowledge_model/00_purpose.md) | Three-layer model; entities are layer-2. |
| [`../19_statements/00_purpose.md`](../19_statements/00_purpose.md) | Statements with `subject = Entity`; merge re-routes them. |
| [`../20_relations/00_purpose.md`](../20_relations/00_purpose.md) | Relations between entities; merge re-routes them. |
| [`../21_schema_dsl/`](../21_schema_dsl/) | Entity type declarations; attribute schema; per-type conflict policies. |
| [`../22_extractors/00_purpose.md`](../22_extractors/00_purpose.md) | Three-tier extractor system; the source of most resolver inputs. |
| [`../25_temporal_model/`](../25_temporal_model/) | Time semantics; relevant to merge grace period + GC grace. |
| [`../26_knowledge_storage/00_purpose.md`](../26_knowledge_storage/00_purpose.md) | redb layout overview; entity tables are documented in [`./02_storage.md`](./02_storage.md) at fine grain. |
| [`../27_knowledge_workers/00_purpose.md`](../27_knowledge_workers/00_purpose.md) | Background workers — embedding refresh, GC sweep, audit cleanup, chain collapse. |

## Wire-protocol counterparts

| Target | §18 file referencing |
|---|---|
| [`../28_knowledge_wire_protocol/01_entity_frames.md`](../28_knowledge_wire_protocol/01_entity_frames.md) | Wire shape for every entity opcode — CREATE / GET / UPDATE / RENAME / MERGE / UNMERGE / RESOLVE / LIST / TOMBSTONE. |
| [`../28_knowledge_wire_protocol/02_subscribe_events.md`](../28_knowledge_wire_protocol/02_subscribe_events.md) | `ENTITY_CREATED`, `ENTITY_UPDATED`, `ENTITY_RENAMED`, `ENTITY_MERGED`, `ENTITY_UNMERGED`, `ENTITY_TOMBSTONED` event shapes. |
| [`../28_knowledge_wire_protocol/03_errors.md`](../28_knowledge_wire_protocol/03_errors.md) | Error code mapping: `ENTITY_NOT_FOUND`, `ENTITY_TYPE_MISMATCH`, `ENTITY_AMBIGUOUS`, `ENTITY_MERGE_CONFLICT`. |
| [`../28_knowledge_wire_protocol/04_validation.md`](../28_knowledge_wire_protocol/04_validation.md) | Field-level validation rules — name length, alias count, attribute size, etc. |
| [`../28_knowledge_wire_protocol/14_admin_frames.md`](../28_knowledge_wire_protocol/14_admin_frames.md) | `ADMIN_LIST_PENDING_RESOLUTIONS`, `ADMIN_RESOLVE_AMBIGUITY`, `ADMIN_GET_AUDIT` for operator-facing entity workflows. |

## Substrate dependencies

| Target | §18 file referencing |
|---|---|
| [`../03_wire_protocol/`](../03_wire_protocol/) | The transport / framing layer that §28 (and therefore §18's wire counterparts) inherits from. |
| [`../05_storage_arena_wal/`](../05_storage_arena_wal/) | Single-writer-per-shard discipline that makes merge atomicity work. |
| [`../06_ann_index/`](../06_ann_index/) | HNSW parameters; entity HNSW reuses the substrate's hnsw_rs crate with smaller params per [`./02_storage.md`](./02_storage.md). |
| [`../07_metadata_graph/`](../07_metadata_graph/) | redb conventions reused by entity tables. |

## Code references

Phase 16 implementation lives in:

| Concern | Code path |
|---|---|
| `Entity`, `EntityType`, `EntityAttributes` value types | `crates/brain-core/src/knowledge/entity.rs` |
| `EntityId`, `EntityTypeId`, `MergeId`, `AuditId` id types | `crates/brain-core/src/knowledge/ids.rs` |
| Resolver tiers 1-3 (5: pure functions) | `crates/brain-core/src/knowledge/resolver.rs` |
| Trigram extraction + Jaccard similarity | `crates/brain-core/src/knowledge/trigrams.rs` |
| redb tables — entities, entity_by_canonical_name, entity_aliases, entity_trigrams | `crates/brain-metadata/src/tables/knowledge/entity.rs` |
| redb tables — entity types registry | `crates/brain-metadata/src/tables/knowledge/entity_type.rs` |
| Free-function entity CRUD — entity_put, entity_get, entity_update, entity_rename, entity_tombstone | `crates/brain-metadata/src/entity_ops.rs` |
| Trigram redb ops — index_entity_trigrams, remove_entity_trigrams, lookup_candidates_by_trigram | `crates/brain-metadata/src/trigram_ops.rs` |
| Entity HNSW | `crates/brain-index/src/entity_hnsw.rs` |
| Wire-op handlers (CREATE / GET / UPDATE / RENAME) | `crates/brain-ops/src/ops/knowledge_entity.rs` |
| Wire smoke test | `crates/brain-server/tests/knowledge_entity_wire.rs` |

Merge / unmerge / GC code lands phase 16.7 (or 16.8 for GC worker), at file paths to be added to this table at that point.

## Process references

| Document | Purpose |
|---|---|
| [`../../.claude/plans/phase-16-task-06.md`](../../.claude/plans/phase-16-task-06.md) | Phase 16.6 plan (opcode u16, namespace split, entity wire ops). |
| [`../../.claude/plans/phase-28-backfill.md`](../../.claude/plans/phase-28-backfill.md) | §28 backfill plan with cross-references back to §18 mechanics. |

## §18 internal file map

For navigation:

| File | Purpose |
|---|---|
| [`./00_purpose.md`](./00_purpose.md) | Entity schema, types, resolver overview, merge / rename / GC outlines. |
| [`./01_resolution.md`](./01_resolution.md) | Resolver tier-by-tier details. |
| [`./02_storage.md`](./02_storage.md) | redb table layouts, HNSW config, read / write paths. |
| [`./03_merge.md`](./03_merge.md) | Merge mechanics (this file's primary cross-ref target for [`../28_knowledge_wire_protocol/01_entity_frames.md`](../28_knowledge_wire_protocol/01_entity_frames.md) §7). |
| [`./04_unmerge.md`](./04_unmerge.md) | Unmerge mechanics + grace period semantics. |
| [`./05_garbage_collection.md`](./05_garbage_collection.md) | Tombstone semantics + optional GC sweep. |
| [`./06_open_questions.md`](./06_open_questions.md) | Entity-specific open questions. |
| [`./07_references.md`](./07_references.md) | This file. |
