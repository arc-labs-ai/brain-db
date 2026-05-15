# 28.10 Knowledge-Layer Errors on the Wire

Errors raised by knowledge-layer opcodes ride the substrate's existing `ERROR` frame (`opcode = 0x00FF`, payload = `ErrorResponse`, defined in [`../03_wire_protocol/10_errors.md`](../03_wire_protocol/10_errors.md)). This file specifies the **knowledge-specific error codes** and how they map into the substrate's `ErrorCode` / `ErrorCategory` taxonomy.

Cross-references:
- [`../03_wire_protocol/10_errors.md`](../03_wire_protocol/10_errors.md) — substrate error taxonomy.
- [`./01_entity_frames.md`](./01_entity_frames.md), [`./05_statement_frames.md`](./05_statement_frames.md), etc. — per-opcode error lists.

## 1. Knowledge error code namespace

§28's top-level table lists 14 knowledge-specific error codes:

| Code | Name | Family |
|---|---|---|
| `0x20` | `SCHEMA_INVALID` | Schema |
| `0x21` | `SCHEMA_MIGRATION_REQUIRED` | Schema |
| `0x30` | `ENTITY_NOT_FOUND` | Entity |
| `0x31` | `ENTITY_TYPE_MISMATCH` | Entity |
| `0x32` | `ENTITY_AMBIGUOUS` | Entity |
| `0x33` | `ENTITY_MERGE_CONFLICT` | Entity |
| `0x40` | `STATEMENT_NOT_FOUND` | Statement |
| `0x41` | `STATEMENT_OBJECT_TYPE_MISMATCH` | Statement |
| `0x42` | `STATEMENT_CONTRADICTS_EXISTING` | Statement |
| `0x50` | `RELATION_CARDINALITY_VIOLATION` | Relation |
| `0x60` | `QUERY_TIMEOUT` | Query |
| `0x61` | `QUERY_OVER_BUDGET` | Query |
| `0x70` | `EXTRACTOR_DISABLED` | Extractor |
| `0x71` | `EXTRACTOR_BUDGET_EXCEEDED` | Extractor |
| `0x72` | `EXTRACTION_FAILED` | Extractor |

These codes are **carried in the ERROR frame body** ([§03/10 §3](../03_wire_protocol/10_errors.md)) — they are not opcodes. The numeric values are independent of the opcode namespace.

## 2. Carrying knowledge codes in the substrate ERROR frame

The substrate's `ErrorResponse` body is currently typed as:

```rust
pub struct ErrorResponse {
    pub code: ErrorCodeWire,      // enum of substrate codes (§03/10 §3)
    pub category: ErrorCategoryWire,
    pub message: String,
    pub details: Option<ErrorDetails>,
    pub retry_after_ms: Option<u32>,
}
```

Two strategies for surfacing knowledge codes:

### 2.1 Strategy A — extend `ErrorCodeWire`

Add knowledge codes as new variants of `ErrorCodeWire`. This is the cleanest long-term shape but requires a coordinated extension of [`../03_wire_protocol/10_errors.md`](../03_wire_protocol/10_errors.md). **Chosen for v1.0.**

Each knowledge code maps to one substrate category for client retry behavior:

| §28 code | New `ErrorCodeWire` variant | Substrate category |
|---|---|---|
| `SCHEMA_INVALID` | `SchemaInvalid` | Validation |
| `SCHEMA_MIGRATION_REQUIRED` | `SchemaMigrationRequired` | Conflict |
| `ENTITY_NOT_FOUND` | `EntityNotFound` | NotFound |
| `ENTITY_TYPE_MISMATCH` | `EntityTypeMismatch` | Validation |
| `ENTITY_AMBIGUOUS` | `EntityAmbiguous` | Conflict |
| `ENTITY_MERGE_CONFLICT` | `EntityMergeConflict` | Conflict |
| `STATEMENT_NOT_FOUND` | `StatementNotFound` | NotFound |
| `STATEMENT_OBJECT_TYPE_MISMATCH` | `StatementObjectTypeMismatch` | Validation |
| `STATEMENT_CONTRADICTS_EXISTING` | `StatementContradictsExisting` | Conflict |
| `RELATION_CARDINALITY_VIOLATION` | `RelationCardinalityViolation` | Validation |
| `QUERY_TIMEOUT` | `QueryTimeout` | Unavailable |
| `QUERY_OVER_BUDGET` | `QueryOverBudget` | ResourceExhausted |
| `EXTRACTOR_DISABLED` | `ExtractorDisabled` | Conflict |
| `EXTRACTOR_BUDGET_EXCEEDED` | `ExtractorBudgetExceeded` | ResourceExhausted |
| `EXTRACTION_FAILED` | `ExtractionFailed` | Internal |

### 2.2 Strategy B — transitional fallback (phase 16.6c interim)

Until §03/10 is extended (a coordinated spec edit, separate from this backfill), the phase-16.6c handler maps `EntityOpError` onto the **closest existing substrate code**:

| `EntityOpError` | Interim substrate code | Final §28 code |
|---|---|---|
| `NotFound(_)` | `MemoryNotFound` → category `NotFound` | `ENTITY_NOT_FOUND` (Strategy A) |
| `UnknownEntityType(_)` | `InvalidArgument` → category `Validation` | `ENTITY_TYPE_MISMATCH` |
| `DuplicateCanonicalName{..}` | `IdempotencyConflict` → category `Conflict` | `ENTITY_AMBIGUOUS` |
| `Storage(_)` / `Table(_)` | `StorageError` → category `Internal` | (no §28 equivalent) |
| `TrigramOp(_)` | `IndexError` → category `Internal` | (no §28 equivalent) |

The mapping in `crates/brain-ops/src/ops/knowledge_entity.rs::map_entity_op_error` follows this fallback. **TODO:** swap to Strategy A once §03/10 is extended — tracked in [`./09_open_questions.md`](./09_open_questions.md).

## 3. Retry semantics

Per substrate [§03/10 §6](../03_wire_protocol/10_errors.md), categories drive client retry behavior:

| Category | Default SDK retry? |
|---|---|
| Validation, NotFound, Conflict | No |
| Authorization, Authentication | No |
| ResourceExhausted, Internal, Unavailable | Yes (exponential backoff) |

Mapping consequences for knowledge codes (Strategy A column):

- `QUERY_TIMEOUT` (Unavailable) and `EXTRACTOR_BUDGET_EXCEEDED` (ResourceExhausted) are retryable — clients should back off and retry, possibly with reduced top_k / depth.
- `ENTITY_AMBIGUOUS` and `ENTITY_MERGE_CONFLICT` (Conflict) are **not** retryable on their own; resolution is a human / admin action via `ADMIN_RESOLVE_AMBIGUITY`.
- `EXTRACTION_FAILED` (Internal) is retryable; the substrate's extractor cache (§22) keeps the same body / model fingerprint so a retry usually hits cached results.

## 4. ErrorDetails for knowledge errors

The optional `ErrorDetails` carries structured context. Knowledge handlers populate:

| Code | `field` | `expected` | `actual` |
|---|---|---|---|
| `ENTITY_TYPE_MISMATCH` | `"entity_type_id"` | list of valid ids | the supplied id |
| `ENTITY_AMBIGUOUS` | `"canonical_name"` | (empty) | newline-joined existing EntityIds |
| `ENTITY_MERGE_CONFLICT` | `"merge"` | reason (e.g. "grace period expired", "already merged") | (empty) |
| `STATEMENT_OBJECT_TYPE_MISMATCH` | predicate name | expected object type | actual encountered |
| `RELATION_CARDINALITY_VIOLATION` | `"cardinality"` | declared rule (e.g. "one_to_many") | (empty) |
| `QUERY_TIMEOUT` | `"timeout_ms"` | wall budget | elapsed |
| `EXTRACTOR_BUDGET_EXCEEDED` | extractor name | tier budget | usage |

Free-form `message` accompanies every error and is intended for log lines, not programmatic dispatch.

## 5. Schema-not-declared mode

If no schema has been declared on the deployment ([`./00_purpose.md`](./00_purpose.md) §"Schema-optional behavior"), knowledge-namespace opcodes other than `SCHEMA_UPLOAD` (0x0120) return:

```
code:    SchemaNotDeclared    (new substrate variant under Strategy A)
category: Conflict
message: "operation requires a schema; call SCHEMA_UPLOAD first"
```

The substrate's cognitive primitives (the `0x00xx` namespace) are unaffected and continue to work normally.

## 6. EOS and stream_id

ERROR frames in response to a knowledge request use the **same `stream_id`** as the offending request and **set the EOS flag**. They terminate the request's stream. Per substrate [§03/05 §5.1](../03_wire_protocol/05_opcodes.md) — same convention as the substrate.

For streaming opcodes (`ENTITY_LIST`, `QUERY` and variants): the first ERROR frame on a stream ends the stream. Partial results already emitted are not rolled back; the client treats whatever it has as the prefix of an incomplete response.

## 7. Audit trail

Errors that originate from **state-mutating** opcodes (CREATE / UPDATE / RENAME / MERGE / TOMBSTONE / SCHEMA_UPLOAD / extractor governance ops) are written to the `entity_resolution_audit` / `schema_audit` tables (see [`../18_entities/02_storage.md`](../18_entities/02_storage.md) §"Audit") regardless of whether the operation succeeded. The audit row's outcome is the relevant §28 code.

Read-only errors (`ENTITY_GET` returning `ENTITY_NOT_FOUND`, etc.) are not audited.

## 8. Open questions

See [`./09_open_questions.md`](./09_open_questions.md). Notably:

- Strategy A vs B timeline — when does substrate §03/10 get extended?
- Should the `details` field be machine-typed (sum type) vs free-form `String`? Currently free-form for ergonomics; the structured columns above are conventions.
- Cross-shard error aggregation for `QUERY` and `ENTITY_LIST` — what shape when only some shards error?
