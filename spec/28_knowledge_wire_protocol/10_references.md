# 28.10 References

Cross-links from §28 to the rest of the spec. Use this file when navigating: every external reference made by §28's other files appears here with a one-line summary.

## Substrate dependencies (§03)

§28 inherits transport, handshake, frame layout, and the ERROR / SUBSCRIBE primitives from the substrate:

| Target | Used for |
|---|---|
| [`../03_wire_protocol/00_purpose.md`](../03_wire_protocol/00_purpose.md) | Substrate purpose; the broader wire-protocol picture. |
| [`../03_wire_protocol/02_transport.md`](../03_wire_protocol/02_transport.md) | TCP / TLS framing; backpressure. Reused verbatim. |
| [`../03_wire_protocol/03_frame_header.md`](../03_wire_protocol/03_frame_header.md) | 32-byte header; u16 opcode at bytes 5-6, u8 flags at byte 7. |
| [`../03_wire_protocol/04_payload_encoding.md`](../03_wire_protocol/04_payload_encoding.md) | rkyv body convention shared with §28. |
| [`../03_wire_protocol/05_opcodes.md`](../03_wire_protocol/05_opcodes.md) | Substrate opcode table; the `0x00xx` namespace. |
| [`../03_wire_protocol/06_handshake.md`](../03_wire_protocol/06_handshake.md) | HELLO/WELCOME/AUTH; schema capability advertisement is here (see [`./08_schema_optional_mode.md`](./08_schema_optional_mode.md) §8). |
| [`../03_wire_protocol/07_request_frames.md`](../03_wire_protocol/07_request_frames.md) | Substrate request frame shapes; SUBSCRIBE_REQ. |
| [`../03_wire_protocol/08_response_frames.md`](../03_wire_protocol/08_response_frames.md) | Substrate response frame shapes; the `ErrorResponse` body shared with §28. |
| [`../03_wire_protocol/09_streaming.md`](../03_wire_protocol/09_streaming.md) | Streaming model reused for `ENTITY_LIST`, `STATEMENT_LIST`, `RELATION_LIST_*`, `QUERY`, admin-job ops. |
| [`../03_wire_protocol/10_errors.md`](../03_wire_protocol/10_errors.md) | Substrate `ErrorCode` taxonomy; §28's error mapping target. |
| [`../03_wire_protocol/11_validation.md`](../03_wire_protocol/11_validation.md) | Substrate validation conventions; [`./04_validation.md`](./04_validation.md) layers on top. |
| [`../03_wire_protocol/12_versioning.md`](../03_wire_protocol/12_versioning.md) | Pre-v1.0 compatibility policy; license for the 16.6a wire change. |

## Domain dependencies (knowledge value types)

§28 specifies wire shapes; the value-type semantics live in:

| Target | §28 file referencing |
|---|---|
| [`../17_knowledge_model/00_purpose.md`](../17_knowledge_model/00_purpose.md) | Three-layer model; §00 purpose framing. |
| [`../18_entities/00_purpose.md`](../18_entities/00_purpose.md) | Entity record schema; [`./01_entity_frames.md`](./01_entity_frames.md) §1, §3, §12. |
| [`../18_entities/01_resolution.md`](../18_entities/01_resolution.md) | Resolver tiers; [`./01_entity_frames.md`](./01_entity_frames.md) §9 (ENTITY_RESOLVE). |
| [`../18_entities/02_storage.md`](../18_entities/02_storage.md) | redb layout; normalized name + alias index referenced by [`./01_entity_frames.md`](./01_entity_frames.md), [`./04_validation.md`](./04_validation.md). |
| [`../18_entities/03_merge.md`](../18_entities/03_merge.md) | Merge mechanics; [`./01_entity_frames.md`](./01_entity_frames.md) §7. |
| [`../18_entities/04_unmerge.md`](../18_entities/04_unmerge.md) | Unmerge / grace period; [`./01_entity_frames.md`](./01_entity_frames.md) §8. |
| [`../19_statements/00_purpose.md`](../19_statements/00_purpose.md) | `Statement` value type; [`./06_statement_frames.md`](./06_statement_frames.md) §2, §3, §5. |
| [`../20_relations/00_purpose.md`](../20_relations/00_purpose.md) | `Relation` value type, cardinality rules; [`./07_relation_frames.md`](./07_relation_frames.md) §2, §3, §10. |
| [`../21_schema_dsl/00_purpose.md`](../21_schema_dsl/00_purpose.md) | Schema grammar; [`./05_schema_frames.md`](./05_schema_frames.md) §2. |
| [`../21_schema_dsl/01_grammar.md`](../21_schema_dsl/01_grammar.md) | Full DSL grammar. |
| [`../22_extractors/00_purpose.md`](../22_extractors/00_purpose.md) | Three-tier extractor system; [`./05_schema_frames.md`](./05_schema_frames.md) §6–§7. |
| [`../23_retrievers/00_purpose.md`](../23_retrievers/00_purpose.md) | Hybrid retrievers; relevant to phase 23 `QUERY_*` frames (Sitting C). |
| [`../24_hybrid_query/00_purpose.md`](../24_hybrid_query/00_purpose.md) | RRF fusion; relevant to phase 23. |
| [`../26_knowledge_storage/00_purpose.md`](../26_knowledge_storage/00_purpose.md) | redb knowledge tables overview. |
| [`../27_knowledge_workers/00_purpose.md`](../27_knowledge_workers/00_purpose.md) | Background workers that emit `EXTRACTION_*` events. |
| [`../29_knowledge_sdk/00_purpose.md`](../29_knowledge_sdk/00_purpose.md) | Typed SDK over §28; consumer of every wire shape here. |
| [`../30_extractor_governance/00_purpose.md`](../30_extractor_governance/00_purpose.md) | Enable / disable policy; [`./05_schema_frames.md`](./05_schema_frames.md) §6–§7. |
| [`../31_complete_acceptance/00_purpose.md`](../31_complete_acceptance/00_purpose.md) | v1.0 acceptance criteria; §28 wire round-trip suite. |

## Code references

§28 wire shapes are implemented (where applicable) in the brain workspace. Phase 16.6c landed the entity slice; later phases extend per the per-section status tables.

| Wire shape | Code |
|---|---|
| Frame header / opcode | `crates/brain-protocol/src/{header,opcode}.rs` |
| Entity request structs | `crates/brain-protocol/src/knowledge/entity_req.rs` |
| Entity response structs | `crates/brain-protocol/src/knowledge/entity_resp.rs` |
| Entity handler | `crates/brain-ops/src/ops/knowledge_entity.rs` |
| Entity wire smoke test | `crates/brain-server/tests/knowledge_entity_wire.rs` |
| RequestBody / ResponseBody variants | `crates/brain-protocol/src/{request,response}.rs` |

Later phases extend these files with `knowledge/schema_*`, `knowledge/statement_*`, `knowledge/relation_*`, `knowledge/query_*`, `knowledge/admin_*` siblings.

## Process references

| Document | Purpose |
|---|---|
| [`../../.claude/plans/phase-28-backfill.md`](../../.claude/plans/phase-28-backfill.md) | Backfill plan for §28; tracks Sittings A/B/C. |
| [`../../.claude/plans/phase-16-task-06.md`](../../.claude/plans/phase-16-task-06.md) | Phase 16.6 plan: opcode u16, namespace split, entity wire ops. |
| [`../../.claude/plans/phase-16-task-06-spec-edits.md`](../../.claude/plans/phase-16-task-06-spec-edits.md) | Diff sheet for the §03 / §28 edits applied in 16.6a. |
