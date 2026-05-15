# 28.05 Schema Wire Frames

Request/response body schemas for every opcode in the `0x0120–0x012F` schema range. Schema operations let clients declare entity types, predicates, and relation types via the schema DSL ([§21](../21_schema_dsl/)); they also govern extractor enablement ([§22](../22_extractors/), [§30](../30_extractor_governance/)).

Cross-references:
- [`../21_schema_dsl/`](../21_schema_dsl/) — grammar, semantics, migration model.
- [`../22_extractors/00_purpose.md`](../22_extractors/00_purpose.md) — the registry that `EXTRACTOR_*` ops manipulate.
- [`./03_errors.md`](./03_errors.md), [`./04_validation.md`](./04_validation.md) — error mapping and field caps.

## 1. Opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0120` | `SCHEMA_UPLOAD` | §2 | spec-only — code lands phase 19 |
| `0x0121` | `SCHEMA_GET` | §3 | spec-only — phase 19 |
| `0x0122` | `SCHEMA_LIST` | §4 | spec-only — phase 19 |
| `0x0123` | `SCHEMA_VALIDATE` | §5 | spec-only — phase 19 |
| `0x0124` | `EXTRACTOR_LIST` | §6 | spec-only — phase 20 |
| `0x0125` | `EXTRACTOR_DISABLE` | §7 | spec-only — phase 20 |
| `0x0126` | `EXTRACTOR_ENABLE` | §8 | spec-only — phase 20 |

Responses live at `0x01A0–0x01A6` (low byte with high bit set).

All structs derive `Archive + Serialize + Deserialize + check_bytes` per [`./01_entity_frames.md`](./01_entity_frames.md) §1.1.

## 2. SCHEMA_UPLOAD (0x0120)

### 2.1 Request — `SchemaUploadRequest`

```rust
pub struct SchemaUploadRequest {
    pub schema_document: String,       // DSL source text per §21
    pub allow_breaking: bool,          // false = reject if migration would be required
    pub dry_run: bool,                 // identical to SCHEMA_VALIDATE when true
    pub request_id: WireUuid,
}
```

Semantics:

1. Parse `schema_document` per the §21 grammar. Syntax errors → `SCHEMA_INVALID` with line/column in `ErrorDetails`.
2. Diff against current schema. If breaking changes are present and `allow_breaking = false`, return `SCHEMA_MIGRATION_REQUIRED`.
3. If `dry_run`, return validation result without persisting.
4. Otherwise commit the new schema version inside a redb transaction: writes to the `schemas` table, allocates a fresh `schema_version`, increments the registry, fires `SCHEMA_UPDATED` event (see [`./02_subscribe_events.md`](./02_subscribe_events.md)).
5. Existing entities / statements / relations remain valid against the *old* schema version until a migration pass re-validates them. Migration policy is in [`../21_schema_dsl/`](../21_schema_dsl/).

### 2.2 Response — `SchemaUploadResponse`

```rust
pub struct SchemaUploadResponse {
    pub schema_version: u32,           // 0 if dry_run rejected the upload
    pub validation_errors: Vec<SchemaValidationError>,
    pub backward_compatible: bool,
    pub migration_summary: Option<SchemaMigrationSummary>,
}

pub struct SchemaValidationError {
    pub line: u32,
    pub column: u32,
    pub message: String,
    pub severity: u8,                  // 0=info, 1=warning, 2=error
}

pub struct SchemaMigrationSummary {
    pub entity_types_added: Vec<String>,
    pub entity_types_removed: Vec<String>,
    pub predicates_added: Vec<String>,
    pub predicates_removed: Vec<String>,
    pub relation_types_added: Vec<String>,
    pub relation_types_removed: Vec<String>,
    pub estimated_rows_to_revalidate: u64,
}
```

### 2.3 Errors

- `SCHEMA_INVALID` — parse or validation failure (severity ≥ 2 in any error).
- `SCHEMA_MIGRATION_REQUIRED` — breaking change with `allow_breaking = false`.
- `INVALID_ARGUMENT` — `schema_document` empty or > 1 MiB (per [`./04_validation.md`](./04_validation.md) §3.1).

## 3. SCHEMA_GET (0x0121)

### 3.1 Request — `SchemaGetRequest`

```rust
pub struct SchemaGetRequest {
    pub version_id: u32,               // 0 = latest
}
```

### 3.2 Response — `SchemaGetResponse`

```rust
pub struct SchemaGetResponse {
    pub schema_version: u32,
    pub schema_document: String,       // canonicalized DSL form
    pub created_at_unix_nanos: u64,
    pub uploaded_by_agent_id: WireUuid,
}
```

`schema_document` is the **canonicalized** form — comments removed, whitespace normalized — not the original upload text. Clients that want the original can read the audit table via `ADMIN_GET_AUDIT` ([`./14_admin_frames.md`](./14_admin_frames.md)).

### 3.3 Errors

- `SCHEMA_INVALID` — `version_id != 0` and not in registry.

## 4. SCHEMA_LIST (0x0122)

### 4.1 Request — `SchemaListRequest`

```rust
pub struct SchemaListRequest {
    pub limit: u32,                    // 1..=100
    pub cursor: Vec<u8>,               // opaque
}
```

### 4.2 Response — streaming, per-item `SchemaListItem`

```rust
pub struct SchemaListItem {
    pub schema_version: u32,
    pub created_at_unix_nanos: u64,
    pub backward_compatible_with_previous: bool,
    pub change_summary: String,        // human-readable, ≤ 4 KiB
}

pub struct SchemaListResponseTail {
    pub next_cursor: Vec<u8>,
    pub total_returned: u32,
}
```

Stream contract: same as `ENTITY_LIST` ([§01](./01_entity_frames.md) §10.2) — substrate streaming with EOS on the tail frame.

## 5. SCHEMA_VALIDATE (0x0123)

### 5.1 Request — `SchemaValidateRequest`

```rust
pub struct SchemaValidateRequest {
    pub schema_document: String,
}
```

### 5.2 Response — `SchemaValidateResponse`

Same shape as `SchemaUploadResponse` (§2.2), but `schema_version` is always 0 (no commit).

### 5.3 Errors

- `INVALID_ARGUMENT` — empty or oversized document.

## 6. EXTRACTOR_LIST (0x0124)

### 6.1 Request — `ExtractorListRequest`

```rust
pub struct ExtractorListRequest {
    pub include_disabled: bool,
}
```

### 6.2 Response — streaming, per-item `ExtractorListItem`

```rust
pub struct ExtractorListItem {
    pub extractor_id: u32,
    pub name: String,                  // e.g. "pattern:role-assignment"
    pub tier: u8,                      // 1=pattern, 2=classifier, 3=llm
    pub enabled: bool,
    pub schema_version: u32,           // version this extractor binds to
    pub last_run_unix_nanos: u64,
    pub statements_produced_lifetime: u64,
    pub failures_lifetime: u64,
}

pub struct ExtractorListResponseTail {
    pub total_returned: u32,
}
```

Cross-ref: [`../22_extractors/00_purpose.md`](../22_extractors/00_purpose.md) defines the tier model.

## 7. EXTRACTOR_DISABLE (0x0125) / EXTRACTOR_ENABLE (0x0126)

### 7.1 Requests

```rust
pub struct ExtractorDisableRequest {
    pub extractor_id: u32,
    pub reason: String,                // ≤ 4 KiB
    pub request_id: WireUuid,
}

pub struct ExtractorEnableRequest {
    pub extractor_id: u32,
    pub request_id: WireUuid,
}
```

### 7.2 Responses

```rust
pub struct ExtractorDisableResponse {
    pub previously_enabled: bool,
    pub disabled_at_unix_nanos: u64,
}

pub struct ExtractorEnableResponse {
    pub previously_disabled: bool,
    pub enabled_at_unix_nanos: u64,
}
```

### 7.3 Errors

- `INVALID_ARGUMENT` — `extractor_id` not registered.
- `EXTRACTOR_DISABLED` — `EXTRACTOR_DISABLE` on an already-disabled extractor returns success (idempotent); `EXTRACTOR_ENABLE` on a non-existent id returns this code.

Disabling an extractor takes effect on the *next* ENCODE; in-flight extractions complete. The server emits no `EXTRACTION_FAILED` event for in-flight cancellations — disabling is non-disruptive.

## 8. Authorization

All schema-namespace opcodes (`0x0120–0x0123`) and extractor-governance opcodes (`0x0125–0x0126`) require **admin** permissions in the agent's `AgentPermissions` ([substrate §06 handshake](../03_wire_protocol/06_handshake.md)). `SCHEMA_GET`, `SCHEMA_LIST`, `EXTRACTOR_LIST` are readable by any authenticated agent.

Unauthorized requests return substrate `ErrorCategory::Authorization` with code `AdminPermissionRequired`.

## 9. Cross-shard semantics

Schema is a **cluster-wide** concept; `SCHEMA_UPLOAD` on any shard takes effect across all shards. The implementation routes the upload through a coordinated commit (multi-shard 2PC or single-shard authority — to be decided by phase 19; tracked in [`./09_open_questions.md`](./09_open_questions.md)).

`SCHEMA_LIST` / `SCHEMA_GET` are local reads — every shard holds an identical registry copy. Inconsistencies (a shard with a stale registry) are recovery bugs, not client-facing concerns.
