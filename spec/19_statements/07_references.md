# 19.07 References

Cross-links from §19 to the rest of the spec.

## Sibling knowledge-layer sections

| Target | §19 file referencing |
|---|---|
| [`../17_knowledge_model/00_purpose.md`](../17_knowledge_model/00_purpose.md) | Three-layer model; statements are layer 3. |
| [`../17_knowledge_model/02_three_statement_kinds.md`](../17_knowledge_model/02_three_statement_kinds.md) | Fact / Preference / Event semantics. |
| [`../18_entities/00_purpose.md`](../18_entities/00_purpose.md) | Subjects are EntityIds. |
| [`../18_entities/01_resolution.md`](../18_entities/01_resolution.md) | Subject resolution at extraction time. |
| [`../20_relations/00_purpose.md`](../20_relations/00_purpose.md) | Statements complement relations — claims about an entity vs typed edges between entities. |
| [`../21_schema_dsl/`](../21_schema_dsl/) | Predicate vocabulary declarations + object-type constraints. |
| [`../22_extractors/00_purpose.md`](../22_extractors/00_purpose.md) | Three-tier extractor system — primary producer of statements. |
| [`../25_provenance_versioning/00_purpose.md`](../25_provenance_versioning/00_purpose.md) | Audit + versioning model that supersession + retract feed into. |
| [`../26_knowledge_storage/00_purpose.md`](../26_knowledge_storage/00_purpose.md) | redb storage catalog; statement tables are in [`./03_storage.md`](./03_storage.md) at fine grain. |
| [`../27_knowledge_workers/00_purpose.md`](../27_knowledge_workers/00_purpose.md) | Background workers — embedding refresh, supersession sweep, confidence recompute, FORGET cascade. |

## Wire-protocol counterparts

| Target | §19 file referencing |
|---|---|
| [`../28_knowledge_wire_protocol/06_statement_frames.md`](../28_knowledge_wire_protocol/06_statement_frames.md) | Wire shape for every statement opcode — CREATE / GET / SUPERSEDE / TOMBSTONE / RETRACT / HISTORY / LIST. |
| [`../28_knowledge_wire_protocol/02_subscribe_events.md`](../28_knowledge_wire_protocol/02_subscribe_events.md) | `StatementCreated`, `StatementSuperseded`, `StatementTombstoned` event shapes. |
| [`../28_knowledge_wire_protocol/03_errors.md`](../28_knowledge_wire_protocol/03_errors.md) | Error code mapping: `STATEMENT_NOT_FOUND`, `STATEMENT_OBJECT_TYPE_MISMATCH`, `STATEMENT_CONTRADICTS_EXISTING`. |
| [`../28_knowledge_wire_protocol/04_validation.md`](../28_knowledge_wire_protocol/04_validation.md) §3.2 | Field-level validation rules. |
| [`../28_knowledge_wire_protocol/14_admin_frames.md`](../28_knowledge_wire_protocol/14_admin_frames.md) §7 | `ADMIN_LIST_STALE_STATEMENTS` — operator-facing cleanup. |

## Substrate dependencies

| Target | §19 file referencing |
|---|---|
| [`../03_wire_protocol/`](../03_wire_protocol/) | Wire framing layer. |
| [`../05_storage_arena_wal/`](../05_storage_arena_wal/) | Single-writer-per-shard discipline that makes multi-table atomicity work. |
| [`../06_ann_index/`](../06_ann_index/) | HNSW parameters — statement HNSW reuses the same hnsw_rs crate. |
| [`../09_cognitive_operations/05_forget.md`](../09_cognitive_operations/05_forget.md) | FORGET op's cascade fires the statement-evidence cleanup in [`./05_evidence.md`](./05_evidence.md) §6. |

## Code references

Phase 17 implementation lives in (paths to be filled in as sub-tasks land):

| Concern | Code path |
|---|---|
| `Statement`, `StatementObject`, `EvidenceRef` value types | `crates/brain-core/src/knowledge/statement.rs` (phase 17.2) |
| `StatementKind` enum (Fact / Preference / Event) | `crates/brain-core/src/knowledge/kinds.rs` (phase 16 prep — live) |
| `StatementId`, `PredicateId` id types | `crates/brain-core/src/knowledge/ids.rs` (phase 16 prep — live) |
| Confidence aggregation | `crates/brain-core/src/knowledge/confidence.rs` (phase 17.9) |
| redb tables — statements + 6 indexes + chain + overflow | `crates/brain-metadata/src/tables/knowledge/statement.rs` (phase 16 prep — live) |
| redb tables — predicates registry | `crates/brain-metadata/src/tables/knowledge/predicate.rs` (phase 16 prep — live) |
| Predicate registry ops + built-in registration | `crates/brain-metadata/src/predicate_ops.rs` (phase 17.3) |
| Statement CRUD + supersession + tombstone | `crates/brain-metadata/src/statement_ops.rs` (phase 17.4) |
| Statement HNSW | `crates/brain-index/src/statement_hnsw.rs` (phase 17.5; populator phase 21) |
| Wire-op request / response structs | `crates/brain-protocol/src/knowledge/statement_req.rs` + `_resp.rs` (phase 17.6) |
| Wire-op handlers + event emission | `crates/brain-ops/src/ops/knowledge_statement.rs` (phase 17.7) |
| SDK Fact / Preference / Event builders | `crates/brain-sdk-rust/src/knowledge/statement.rs` (phase 17.8) |
| Integration tests | `crates/brain-server/tests/knowledge_statement*.rs` (phase 17.10) |
| Resolver-driven auto-create (phase 22+) | TBD |

## Process references

| Document | Purpose |
|---|---|
| [`../../.claude/plans/phase-17.md`](../../.claude/plans/phase-17.md) | Phase 17 master plan (this implementation). |
| [`../../.claude/plans/phase-16-task-07.md`](../../.claude/plans/phase-16-task-07.md) | Phase 16.7 plan that pre-built statement event variants + error codes. |
| [`../../docs/phases/phase-17-statements.md`](../../docs/phases/phase-17-statements.md) | Phase 17 sub-task index. |

## §19 internal file map

For navigation:

| File | Purpose |
|---|---|
| [`./00_purpose.md`](./00_purpose.md) | Schema, kinds, indexes (high-level). |
| [`./01_supersession.md`](./01_supersession.md) | Supersession chain mechanics. |
| [`./02_contradiction.md`](./02_contradiction.md) | Contradiction detection + surface. |
| [`./03_storage.md`](./03_storage.md) | redb table layout. |
| [`./04_confidence.md`](./04_confidence.md) | Evidence-driven confidence aggregation. |
| [`./05_evidence.md`](./05_evidence.md) | Inline vs overflow, FORGET cascade. |
| [`./06_open_questions.md`](./06_open_questions.md) | Known gaps + deferrals. |
| [`./07_references.md`](./07_references.md) | This file. |
