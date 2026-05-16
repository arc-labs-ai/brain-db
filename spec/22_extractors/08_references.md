# 22.08 References

Cross-links from §22 to the rest of the spec.

## Sibling knowledge-layer sections

| Target | §22 file referencing |
|---|---|
| [`../17_knowledge_model/00_purpose.md`](../17_knowledge_model/00_purpose.md) | Three-layer model — extractors produce layer-2/3 outputs. |
| [`../18_entities/01_resolution.md`](../18_entities/01_resolution.md) | Entity resolver tiers consumed by §22/04. |
| [`../19_statements/`](../19_statements/) | Statement supersession / confidence aggregation consume extractor outputs. |
| [`../20_relations/`](../20_relations/) | Relation cardinality + auto-supersede consume extractor outputs. |
| [`../21_schema_dsl/02_ast.md`](../21_schema_dsl/02_ast.md) §5 | `ExtractorDef` AST node. |
| [`../21_schema_dsl/05_versioning.md`](../21_schema_dsl/05_versioning.md) | `extractor_version` / `schema_version` stamping. |
| [`../21_schema_dsl/06_system_schema.md`](../21_schema_dsl/06_system_schema.md) | Built-in extractors land via the system schema (phase 20.7). |
| [`../25_provenance_versioning/01_audit_tables.md`](../25_provenance_versioning/01_audit_tables.md) | Concrete redb tables for `EXTRACTOR_AUDIT_TABLE` + indexes. |
| [`../27_knowledge_workers/01_extractor_workers.md`](../27_knowledge_workers/01_extractor_workers.md) | Worker scheduling for the three tiers. |

## Wire-protocol counterparts

| Target | §22 file referencing |
|---|---|
| [`../28_knowledge_wire_protocol/05_schema_frames.md`](../28_knowledge_wire_protocol/05_schema_frames.md) §6-§7 | `EXTRACTOR_LIST` / `_DISABLE` / `_ENABLE` opcodes 0x0124-0x0126 (phase 20.8). |
| [`../28_knowledge_wire_protocol/02_subscribe_events.md`](../28_knowledge_wire_protocol/02_subscribe_events.md) | `ExtractionCompletedEvent` / `ExtractionFailedEvent` (already defined in `events.rs` for forward compat). |
| [`../28_knowledge_wire_protocol/03_errors.md`](../28_knowledge_wire_protocol/03_errors.md) | Extractor-side error codes. |

## Substrate dependencies

| Target | §22 file referencing |
|---|---|
| [`../03_wire_protocol/`](../03_wire_protocol/) | Wire framing for `EXTRACTOR_*` opcodes. |
| [`../05_storage_arena_wal/`](../05_storage_arena_wal/) | Audit-row WAL durability. |
| [`../11_workers/`](../11_workers/) | Worker scheduling discipline shared with substrate workers. |
| [`../16_benchmarks_acceptance/02_latency_targets.md`](../16_benchmarks_acceptance/02_latency_targets.md) §2.6 | Extractor perf targets (phase 20.0 added). |

## Code references

Phase 20 implementation lives in:

| Concern | Code path |
|---|---|
| Extractor trait + registry | `crates/brain-extractors/src/lib.rs` (phase 20.1) |
| Pattern extractor | `crates/brain-extractors/src/pattern.rs` (phase 20.2) |
| Classifier extractor + bundled NER | `crates/brain-extractors/src/classifier.rs` + `models/` (phase 20.3) |
| Audit log | `crates/brain-metadata/src/audit_ops.rs` (phase 20.4) |
| Schema fan-out | `crates/brain-metadata/src/schema_apply.rs` (phase 20.5; extends 19.7) |
| ENCODE integration | `crates/brain-ops/src/ops/encode.rs` (phase 20.6 extends) |
| Built-in extractors via system schema | `crates/brain-metadata/src/system_schema/schema.brain` (phase 20.7 extends) |
| Wire request / response structs | `crates/brain-protocol/src/knowledge/extractor_req.rs` + `_resp.rs` (phase 20.8) |
| Wire-op handlers | `crates/brain-ops/src/ops/knowledge_extractor.rs` (phase 20.8) |
| Integration tests | `crates/brain-server/tests/knowledge_extractor_*.rs` (phase 20.9) |

## Process references

| Document | Purpose |
|---|---|
| [`../../.claude/plans/phase-20.md`](../../.claude/plans/phase-20.md) | Phase 20 master plan. |
| [`../../.claude/plans/phase-21.md`](../../.claude/plans/phase-21.md) | Phase 21 LLM extractor master plan. |
| [`../../docs/phases/phase-20-pattern-classifier-extractors.md`](../../docs/phases/phase-20-pattern-classifier-extractors.md) | Phase 20 sub-task index (original; superseded by the .claude plans). |
| [`../../docs/phases/phase-21-llm-extractor.md`](../../docs/phases/phase-21-llm-extractor.md) | Phase 21 sub-task index. |

## §22 internal file map

For navigation:

| File | Purpose |
|---|---|
| [`./00_purpose.md`](./00_purpose.md) | Overview + three-tier model. |
| [`./01_pattern_extractor.md`](./01_pattern_extractor.md) | Regex tier semantics. |
| [`./02_classifier_extractor.md`](./02_classifier_extractor.md) | Pinned-model tier semantics. |
| [`./03_triggers.md`](./03_triggers.md) | `TriggerExpr` evaluation. |
| [`./04_resolver.md`](./04_resolver.md) | Mention → record resolution. |
| [`./05_audit.md`](./05_audit.md) | `ExtractionAuditRow` shape + retention. |
| [`./06_idempotency.md`](./06_idempotency.md) | Replay semantics + cache. |
| [`./07_open_questions.md`](./07_open_questions.md) | Deferrals. |
| [`./08_references.md`](./08_references.md) | This file. |
| [`./09_llm_extractor.md`](./09_llm_extractor.md) | LLM tier: client trait, cache, retry, cost budget. |
