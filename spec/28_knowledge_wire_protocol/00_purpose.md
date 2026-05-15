# Wire Protocol — Knowledge Operations

The wire protocol's foundation (32-byte frame header, opcode + body, CRC32C, rkyv-serialized bodies) is defined in section 03. This section defines the opcodes that operate on the knowledge layer: schema, entities, statements, relations, queries, and admin.

The substrate's cognitive primitives (ENCODE, RECALL, PLAN, REASON, FORGET, LINK, UNLINK, TXN_*, SUBSCRIBE) are defined in section 03. Together with the opcodes here, they form the complete wire surface.

## File map

This section's structure mirrors [`../03_wire_protocol/`](../03_wire_protocol/) (which the knowledge layer inherits transport / handshake / frame-header from). File numbering is **sequential write-order** (no gaps for unwritten content); each backfill sitting appends the next numbers.

### Live files

| File | Purpose |
|---|---|
| [`00_purpose.md`](./00_purpose.md) | This file — opcode tables, families, error-code table, schema-optional mode. |
| [`01_entity_frames.md`](./01_entity_frames.md) | Body shapes for `0x0130–0x0138` entity ops. |
| [`02_subscribe_events.md`](./02_subscribe_events.md) | Knowledge event types riding substrate SUBSCRIBE. |
| [`03_errors.md`](./03_errors.md) | §28 error code mapping into substrate ERROR frame. |
| [`04_validation.md`](./04_validation.md) | Field-level validation rules per opcode. |

### Planned (Sittings B + C, tracked in `.claude/plans/phase-28-backfill.md`)

In write order, with sequential file numbers assigned as each lands:

- **Sitting B (next):** `schema_frames` (`0x0120–0x0126`), `statement_frames` (`0x0140–0x0146`), `relation_frames` (`0x0150–0x0156`), `schema_optional_mode`, `open_questions`, `references`, `README`.
- **Sitting C:** `design_choices`, `payload_encoding`, `query_frames` (`0x0160–0x0163` + streaming detail), `admin_frames` (`0x0170–0x0177`).

## Opcode namespace

All knowledge-layer opcodes live in the **`0x01xx` namespace** of the u16 wire opcode (spec [`../03_wire_protocol/03_frame_header.md`](../03_wire_protocol/03_frame_header.md) §3.3 + [`../03_wire_protocol/05_opcodes.md`](../03_wire_protocol/05_opcodes.md)). The substrate occupies `0x00xx`; the knowledge layer occupies `0x01xx`; other namespaces (`0x02xx`–`0xFFxx`) are reserved.

Within `0x01xx`, low-byte ranges are partitioned by operation family:

```
0x0100–0x010F   reserved
0x0110–0x011F   reserved future (substrate's cognitive primitives live in 0x00xx)
0x0120–0x012F   schema operations
0x0130–0x013F   entity operations
0x0140–0x014F   statement operations
0x0150–0x015F   relation operations
0x0160–0x016F   query operations (hybrid retrieval)
0x0170–0x017F   admin operations
0x0180–0x018F   reserved future
```

The low byte's high bit selects direction within the knowledge namespace, mirroring substrate convention. For example, `ENTITY_CREATE` is `0x0130` (request) and its response `ENTITY_CREATE_RESP` is `0x01B0`.

## Schema operations (0x0120–0x012F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0120 | SCHEMA_UPLOAD | schema document (text) | schema_version, validation_errors |
| 0x0121 | SCHEMA_GET | version_id (latest if 0) | schema document |
| 0x0122 | SCHEMA_LIST | (none) | list of versions with timestamps |
| 0x0123 | SCHEMA_VALIDATE | schema document | validation_errors (without commit) |
| 0x0124 | EXTRACTOR_LIST | (none) | active extractors |
| 0x0125 | EXTRACTOR_DISABLE | extractor_id | confirmation |
| 0x0126 | EXTRACTOR_ENABLE | extractor_id | confirmation |

## Entity operations (0x0130–0x013F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0130 | ENTITY_CREATE | type, canonical_name, attributes | EntityId |
| 0x0131 | ENTITY_GET | EntityId | Entity record |
| 0x0132 | ENTITY_UPDATE | EntityId, attribute_deltas | confirmation |
| 0x0133 | ENTITY_RENAME | EntityId, new_name, move_to_alias | confirmation |
| 0x0134 | ENTITY_MERGE | survivor, merged, confidence | merge audit_id |
| 0x0135 | ENTITY_UNMERGE | merged_entity | restored EntityId |
| 0x0136 | ENTITY_RESOLVE | candidate_name, context, hint | ResolutionOutcome |
| 0x0137 | ENTITY_LIST | filter (type, name_prefix, mention_count_min) | EntityIds |
| 0x0138 | ENTITY_TOMBSTONE | EntityId, reason | confirmation |

## Statement operations (0x0140–0x014F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0140 | STATEMENT_CREATE | kind, subject, predicate, object, evidence, confidence | StatementId |
| 0x0141 | STATEMENT_GET | StatementId | Statement record |
| 0x0142 | STATEMENT_SUPERSEDE | old_id, new_statement | new StatementId |
| 0x0143 | STATEMENT_TOMBSTONE | StatementId, reason | confirmation |
| 0x0144 | STATEMENT_RETRACT | StatementId | confirmation |
| 0x0145 | STATEMENT_HISTORY | StatementId or chain_root | full chain |
| 0x0146 | STATEMENT_LIST | filter (subject, predicate, kind, time, confidence) | StatementIds |

## Relation operations (0x0150–0x015F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0150 | RELATION_CREATE | type, from, to, properties, evidence | RelationId |
| 0x0151 | RELATION_GET | RelationId | Relation record |
| 0x0152 | RELATION_SUPERSEDE | old_id, new_relation | new RelationId |
| 0x0153 | RELATION_TOMBSTONE | RelationId, reason | confirmation |
| 0x0154 | RELATION_LIST_FROM | EntityId, type_filter, time_filter | RelationIds |
| 0x0155 | RELATION_LIST_TO | EntityId, type_filter, time_filter | RelationIds |
| 0x0156 | RELATION_TRAVERSE | start, types, depth, direction | path/subgraph |

## Query operations (0x0160–0x016F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0160 | QUERY | QueryRequest | QueryResult (streamed if large) |
| 0x0161 | QUERY_EXPLAIN | QueryRequest | QueryPlan (no execution) |
| 0x0162 | QUERY_TRACE | QueryRequest | QueryResult + per-retriever debug |
| 0x0163 | RECALL_HYBRID | text, filters, retriever_selection | RecallResult |

`QUERY` is the primary structured query opcode. `RECALL_HYBRID` is the simple-text fast path used by clients that just want hybrid text-only retrieval with no entity anchoring.

The simpler `RECALL` opcode (substrate, `0x0021`) is the substrate-level vector recall. When a schema is declared, the server routes `RECALL` through the hybrid retriever transparently — clients see the same response shape with additional metadata fields (`contributing_retrievers`, etc.).

## Admin operations (0x0170–0x017F)

| Opcode | Name | Body | Response |
|---|---|---|---|
| 0x0170 | ADMIN_REBUILD_INDEX | index_name, shard_id | job_id |
| 0x0171 | ADMIN_REINDEX_TANTIVY | shard_id | job_id |
| 0x0172 | ADMIN_LIST_PENDING_RESOLUTIONS | (none) | list of ambiguity audits |
| 0x0173 | ADMIN_RESOLVE_AMBIGUITY | audit_id, chosen_entity | confirmation |
| 0x0174 | ADMIN_GET_AUDIT | audit_id | AuditEntry |
| 0x0175 | ADMIN_LIST_STALE_STATEMENTS | filter | StatementIds |
| 0x0176 | ADMIN_BACKFILL | extractor_ids, memory_range | job_id |
| 0x0177 | ADMIN_JOB_STATUS | job_id | status, progress, ETA |

## Body encoding

All bodies use rkyv (zero-copy deserialization), the same encoding used for cognitive primitives. Variable-length fields use rkyv length-prefixed encoding. CRC32C on the body. Per-opcode body shapes are specified in files `03_–08_` of this section.

## Streaming responses

Large query results (`QUERY`, `ENTITY_LIST`, `STATEMENT_LIST`, admin job-progress) stream via the substrate's existing streaming model ([`../03_wire_protocol/09_streaming.md`](../03_wire_protocol/09_streaming.md)): a sequence of frames sharing the same `stream_id`, intermediate frames clear the EOS bit, the final frame sets EOS. There is no `STREAM_START` / `STREAM_ITEM` / `STREAM_END` envelope — knowledge opcodes reuse substrate streaming verbatim.

Clients cancel mid-stream by sending `CANCEL_STREAM` (`0x0050`) with the offending stream's id. See [`07_query_frames.md`](./07_query_frames.md) (TBD) for per-opcode streaming semantics.

## SUBSCRIBE event types

The SUBSCRIBE primitive (section 03) carries event types for the knowledge layer:

- `ENTITY_CREATED`, `ENTITY_UPDATED`, `ENTITY_MERGED`, `ENTITY_UNMERGED`, `ENTITY_RENAMED`, `ENTITY_TOMBSTONED`
- `STATEMENT_CREATED`, `STATEMENT_SUPERSEDED`, `STATEMENT_TOMBSTONED`
- `RELATION_CREATED`, `RELATION_SUPERSEDED`
- `EXTRACTION_COMPLETED` (with extractor_id, memory_id, output_count)
- `EXTRACTION_FAILED` (with extractor_id, memory_id, error)
- `SCHEMA_UPDATED` (with from_version, to_version)

Subscribers filter by event type, entity_id, predicate, etc. Full event-payload schemas and emission semantics live in [`02_subscribe_events.md`](./02_subscribe_events.md).

## Error codes

Beyond the substrate's error codes (section 03):

| Code | Meaning |
|---|---|
| 0x20 | SCHEMA_INVALID |
| 0x21 | SCHEMA_MIGRATION_REQUIRED |
| 0x30 | ENTITY_NOT_FOUND |
| 0x31 | ENTITY_TYPE_MISMATCH |
| 0x32 | ENTITY_AMBIGUOUS |
| 0x33 | ENTITY_MERGE_CONFLICT |
| 0x40 | STATEMENT_NOT_FOUND |
| 0x41 | STATEMENT_OBJECT_TYPE_MISMATCH |
| 0x42 | STATEMENT_CONTRADICTS_EXISTING |
| 0x50 | RELATION_CARDINALITY_VIOLATION |
| 0x60 | QUERY_TIMEOUT |
| 0x61 | QUERY_OVER_BUDGET |
| 0x70 | EXTRACTOR_DISABLED |
| 0x71 | EXTRACTOR_BUDGET_EXCEEDED |
| 0x72 | EXTRACTION_FAILED |

Error responses include human-readable detail in the body. Mapping into the substrate ERROR frame's `ErrorCodeWire` / `ErrorCategoryWire` enums and retry semantics are specified in [`03_errors.md`](./03_errors.md). Per-opcode validation rules and the field caps that produce these errors are in [`04_validation.md`](./04_validation.md).

## Schema-optional behavior

The server operates with or without a declared schema:

- **No schema declared**: knowledge-namespace opcodes `0x0120–0x0177` return `SCHEMA_NOT_DECLARED` errors except for `SCHEMA_UPLOAD` (`0x0120`) itself. Substrate primitives (the `0x00xx` namespace) work normally.
- **Schema declared**: all opcodes function. RECALL routes through hybrid retrieval.

This is a deployment choice, not a compatibility mode. A deployment that wants only the vector substrate simply doesn't declare a schema.
