# 19.6 — Wire opcodes 0x0120-0x0123 + handlers

Wires the SCHEMA opcodes over the framed Brain wire and provides
handlers backed by the 19.3–19.5 parse/validate/store path.

## §28/05 spec divergences (resolved against §21)

§28/05 was authored before the no-migration directive landed in
§21. The implementation lands the wire surface that matches the
**§21 truth**:

- `allow_breaking` / `SchemaMigrationSummary` / `backward_compatible`
  fields in §28/05's `SchemaUploadResponse` carry **no behaviour**
  in v1 (no migrations). We keep the wire fields populated with
  conservative defaults (`backward_compatible = true`, summary
  empty / migration_summary `None`) so a §28/05-aware client can
  still read responses. New fields the §21 model needs
  (`namespace`) are added.
- §28/05's `SchemaGetRequest.version_id = 0` means "latest"; we
  extend it with a `namespace` field — `version_id == 0` returns
  the active version for that namespace.
- §28/05's `SchemaListRequest.cursor` is honoured as opaque (passed
  through but unused — list is bounded by schema count, not by
  cursor pagination in v1).

Documented as a §21/07 open question (Q15 added in this sub-task).

## Files written / modified

| Path | Purpose |
|---|---|
| `crates/brain-protocol/src/knowledge/schema_req.rs` | Request types: `SchemaUploadRequest`, `SchemaGetRequest`, `SchemaListRequest`, `SchemaValidateRequest`. |
| `crates/brain-protocol/src/knowledge/schema_resp.rs` | Response types: `SchemaUploadResponse`, `SchemaGetResponse`, `SchemaListResponseFrame` (single-frame snapshot in v1), `SchemaValidateResponse`, `SchemaValidationErrorWire`, `SchemaListItemWire`. |
| `crates/brain-protocol/src/knowledge/mod.rs` | Re-exports. |
| `crates/brain-protocol/src/knowledge/events.rs` | Add `namespace: String` to `SchemaUpdatedEvent`. |
| `crates/brain-protocol/src/opcode.rs` | Add `SchemaUploadReq=0x0120` / `Resp=0x01A0`, `SchemaGetReq=0x0121` / `Resp=0x01A1`, `SchemaListReq=0x0122` / `Resp=0x01A2`, `SchemaValidateReq=0x0123` / `Resp=0x01A3`. |
| `crates/brain-protocol/src/request.rs` | Add `SchemaUpload/Get/List/Validate` variants; encode/decode wiring. |
| `crates/brain-protocol/src/response.rs` | Add response variants; encode/decode/`is_final` wiring. |
| `crates/brain-ops/src/ops/knowledge_schema.rs` | Handlers. |
| `crates/brain-ops/src/ops/mod.rs` | Module registration. |
| `crates/brain-ops/src/dispatch.rs` | Dispatch arms. |
| `crates/brain-ops/Cargo.toml` | Schema_store calls into brain-metadata which is already a dep. brain-protocol::schema already accessible. No new deps. |
| `spec/21_schema_dsl/07_open_questions.md` | Append Q15 — §28/05 vs §21/05 divergences resolved in 19.6. |

## Wire types — exact shape

### `SchemaUploadRequest`

```rust
pub struct SchemaUploadRequest {
    pub schema_document: String,   // DSL source text per §21.
    pub dry_run: bool,             // identical to SCHEMA_VALIDATE.
    pub allow_breaking: bool,      // ignored in v1; preserved for §28/05.
    pub request_id: WireUuid,
}
```

### `SchemaUploadResponse`

```rust
pub struct SchemaUploadResponse {
    pub namespace: String,
    pub schema_version: u32,             // 0 if dry_run or validation failed.
    pub validation_errors: Vec<SchemaValidationErrorWire>,
    pub backward_compatible: bool,       // always true in v1 (no diff).
    /// `None` in v1 — placeholder for §28/05 forward-compat.
    pub migration_summary_blob: Vec<u8>,
}

pub struct SchemaValidationErrorWire {
    pub code: String,                    // e.g. "UnresolvedTypeRef"
    pub message: String,
    pub line: u32,                       // 0 if absent
    pub column: u32,
    pub length: u32,
    pub severity: u8,                    // always 2 (error) in v1
}
```

Mapping rules:

- ParseError → one `SchemaValidationErrorWire` with `code` derived
  from the variant name (`"Syntax"`, `"InvalidJson"`, …), line/col
  taken verbatim, `severity = 2`.
- ValidationError → one wire entry per error, `code` = the
  `ValidationErrorCode` debug name.

### `SchemaGetRequest` / `SchemaGetResponse`

```rust
pub struct SchemaGetRequest {
    pub namespace: String,
    pub version: u32,                    // 0 = active
}

pub struct SchemaGetResponse {
    pub namespace: String,
    pub schema_version: u32,
    pub schema_document: String,         // source_text if available, else "".
    pub source_blob: Vec<u8>,            // serde_json AST blob.
    pub uploaded_at_unix_nanos: u64,
    pub validator_version: u32,
}
```

`schema_document` falls back to `""` for programmatic uploads
without source text — clients can decode `source_blob` if they
need the AST.

### `SchemaListRequest` / `SchemaListResponseFrame`

```rust
pub struct SchemaListRequest {
    pub namespace: String,
    pub limit: u32,                     // 0 = unlimited
    pub cursor: Vec<u8>,                // ignored in v1
}

pub struct SchemaListResponseFrame {
    pub namespace: String,
    pub items: Vec<SchemaListItemWire>, // newest first
    pub total: u32,
    pub next_cursor: Vec<u8>,           // empty in v1
    pub is_final: bool,                 // always true in v1
}

pub struct SchemaListItemWire {
    pub schema_version: u32,
    pub uploaded_at_unix_nanos: u64,
    pub validator_version: u32,
    pub has_source_text: bool,
}
```

Single-frame snapshot pattern matches RelationListFromResponseFrame
(phase 18.7). Phase 23 may split into streaming.

### `SchemaValidateRequest` / `SchemaValidateResponse`

```rust
pub struct SchemaValidateRequest {
    pub schema_document: String,
}

pub struct SchemaValidateResponse {
    pub namespace: String,              // "" if parse failed before namespace
    pub would_be_version: u32,          // current_active + 1 (0 if invalid)
    pub validation_errors: Vec<SchemaValidationErrorWire>,
}
```

## Opcode assignments

```
0x0120  SCHEMA_UPLOAD_REQ        0x01A0  SCHEMA_UPLOAD_RESP
0x0121  SCHEMA_GET_REQ           0x01A1  SCHEMA_GET_RESP
0x0122  SCHEMA_LIST_REQ          0x01A2  SCHEMA_LIST_RESP
0x0123  SCHEMA_VALIDATE_REQ      0x01A3  SCHEMA_VALIDATE_RESP
```

The response opcodes sit in 0x01A0-0x01A3, ahead of the
entity-response range 0x01B0+ — matches the spec §28/05 §1 layout
(opcodes 0x01A0–0x01A6 reserved for schema + extractor responses).

## Handlers (`knowledge_schema.rs`)

Single module with four async handlers. Each:

1. Validates wire-layer input (1 MiB cap on `schema_document` per
   §28/05 §2.3 / `errors.md`).
2. Calls `parse_schema` (brain-protocol). On parse failure ⇒ build
   one wire error, return success-shape response with empty body
   fields and `schema_version = 0`.
3. Calls `validate` (brain-protocol). On error list ⇒ map each to
   wire error.
4. **UPLOAD only:** acquires `MetadataDb` lock, opens wtxn, calls
   `schema_upload`, commits, emits `SchemaUpdated` event.
5. **GET / LIST:** opens rtxn, calls schema_store reads.
6. **VALIDATE:** never touches storage, except an rtxn lookup of
   `schema_active(namespace)` to compute `would_be_version`.

Reserved `brain:` namespace upload rejection happens at the
validator level — wire handler doesn't second-guess.

Error mapping:
- `OpError::InvalidRequest` for empty / oversized `schema_document`.
- Parse + validate errors are NOT `OpError` — they're returned in
  the response body's `validation_errors` field with
  `schema_version = 0`. This matches §28/05 §2.2 semantics
  (validation failure is a structured response, not an error
  frame).
- `OpError::Internal` for storage / commit errors.
- `OpError::NotFound { what: "schema", detail }` for
  `SCHEMA_GET(namespace, version)` where the row doesn't exist.

## Dispatch wiring

Add four new `match` arms in `crates/brain-ops/src/dispatch.rs`,
each calling into `knowledge_schema::handle_*`.

## Tests

### Unit tests (`knowledge_schema.rs`)

- Parse failure → `validation_errors` populated, `schema_version = 0`.
- Validation failure (e.g. `namespace brain`) → ditto.
- Successful upload → version 1, then 2.
- `SchemaGet` of missing row → `NotFound`.
- `SchemaValidate` returns `would_be_version = current + 1`.
- `SchemaList` returns newest-first.
- Empty `schema_document` → `OpError::InvalidRequest`.
- Oversized `schema_document` (>1 MiB) → `OpError::InvalidRequest`.

### Integration tests deferred to 19.10a

The wire-level integration suite (start_server, encode requests,
decode responses) lands in 19.10a together with end-to-end
lifecycle tests.

## Cap constants

```rust
pub const MAX_SCHEMA_DOCUMENT_BYTES: usize = 1024 * 1024; // 1 MiB
```

In `knowledge_schema.rs`. Referenced by all handlers.

## Out of scope

- §28/05 admin-only authorization (`AdminPermissionRequired`). 19.6
  ships open access — phase 21 admin work adds auth.
- Cross-shard semantics (§28/05 §9). Phase 19 ships single-shard
  authority; cluster coordination is post-v1.
- EXTRACTOR_LIST / _DISABLE / _ENABLE (0x0124–0x0126) — phase 20.
- Stream-paginated SCHEMA_LIST — phase 23.
- Auditing the agent_id that uploaded each version
  (`uploaded_by_agent_id` in §28/05 §3.2). Phase 22 admin.

## Single commit

`feat(protocol,ops): 19.6 — schema wire ops + handlers`

## Verification

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo test -p brain-protocol --lib knowledge::schema
cargo test -p brain-ops --lib knowledge_schema   # native
just docker cargo test -p brain-ops --lib knowledge_schema
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
