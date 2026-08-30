//! Schema-op request payloads.
//!
//! Per-namespace versioning. No migrations in v1; breaking schema
//! changes are made in place.

use crate::envelope::request::WireUuid;

/// `SCHEMA_UPLOAD` (`0x0120`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaUploadRequest {
    /// Schema DSL source text.
    pub schema_document: String,
    /// Parse + validate without persisting. Identical to
    /// `SCHEMA_VALIDATE` when `true`.
    pub dry_run: bool,
    /// Reserved for forward-compat with future migration support.
    /// Ignored in v1.
    pub allow_breaking: bool,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `SCHEMA_GET` (`0x0121`). `version == 0` → active version.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaGetRequest {
    pub namespace: String,
    pub version: u32,
}

/// `SCHEMA_LIST` (`0x0122`). `limit == 0` → unlimited (v1 caps
/// to schema_list output size).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaListRequest {
    pub namespace: String,
    pub limit: u32,
    pub cursor: Vec<u8>,
}

/// `SCHEMA_VALIDATE` (`0x0123`). Dry-run; never touches storage.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaValidateRequest {
    pub schema_document: String,
}

/// `SCHEMA_REPLACE` (`0x0127`). Destructive counterpart to
/// `SCHEMA_UPLOAD`'s associative merge: drops every schema-declared
/// row in the namespace (predicates, relation_types, extractors) and
/// re-runs the apply path against the supplied DSL. Existing
/// statements / relations / entities whose predicate or relation_type
/// disappears stay as orphans — readable as plain memories, no longer
/// enriched from the typed-graph tables.
///
/// `force_drop_existing` MUST be `true`; the handler rejects a
/// `false` value with `InvalidRequest`. The explicit flag is a
/// confirmation step for an irreversible operation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaReplaceRequest {
    /// Schema DSL source text. Must declare the same namespace as
    /// the wire `namespace` field, or the handler rejects with
    /// `InvalidRequest`.
    pub schema_document: String,
    /// Confirmation flag — MUST be `true`. Reserved name to keep the
    /// client ergonomics symmetric with the destructive intent.
    pub force_drop_existing: bool,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// Target kind for a `SCHEMA_DROP` request.
///
/// Entity types are deliberately absent: they are global in the v1
/// storage model (no namespace key), so dropping one would race rows in
/// other namespaces that reference the same shared type — the same
/// reason `SCHEMA_REPLACE` never drops them. The handler rejects any
/// other discriminant with `InvalidRequest`.
pub mod schema_drop_target {
    /// Drop a declared predicate.
    pub const PREDICATE: u8 = 0;
    /// Drop a declared relation_type.
    pub const RELATION_TYPE: u8 = 1;
}

/// `SCHEMA_DROP` (`0x0125`). The surgical counterpart to the
/// namespace-wide `SCHEMA_REPLACE`: removes (narrows) a single declared
/// predicate or relation_type from the active schema set, then bumps the
/// namespace to a new schema version whose document no longer declares
/// the dropped type. Admin-only, tenant-bound like `SCHEMA_REPLACE`.
///
/// Safety posture (mirrors `SCHEMA_REPLACE`'s explicit-confirmation
/// contract): dropping a type that still has live (non-tombstoned) rows
/// requires `force: true`. With live rows present and `force` unset the
/// handler rejects with `Conflict` and mutates nothing. A type with no
/// live rows drops without `force`. Existing rows on a dropped type stay
/// as orphans — readable as plain memories, no longer enriched from the
/// typed-graph tables — exactly as under `SCHEMA_REPLACE`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaDropRequest {
    /// Namespace the target lives in. MUST equal the caller's own
    /// namespace, or the handler rejects with `Unauthorized`.
    pub namespace: String,
    /// One of [`schema_drop_target`]. Any other value → `InvalidRequest`.
    pub target_kind: u8,
    /// Local name of the predicate / relation_type to drop (the qname is
    /// `{namespace}:{target_name}`).
    pub target_name: String,
    /// Confirmation flag required only when the target still has live
    /// rows. `false` with live rows present → `Conflict`, no mutation.
    pub force: bool,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

// ============================================================
// Response payloads
// ============================================================

/// `SCHEMA_UPLOAD_RESP` (`0x01A0`).
///
/// `schema_version == 0` indicates the upload was rejected
/// (validation failure or dry_run). `validation_errors` carries
/// the structured error list when present.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaUploadResponse {
    pub namespace: String,
    pub schema_version: u32,
    pub validation_errors: Vec<SchemaValidationErrorWire>,
    /// Always `true` in v1 (no diff computed). Reserved for a
    /// future migration-aware schema cut.
    pub backward_compatible: bool,
    /// Reserved opaque blob for a future `SchemaMigrationSummary`.
    /// Empty in v1.
    pub migration_summary_blob: Vec<u8>,
}

/// `SCHEMA_GET_RESP` (`0x01A1`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaGetResponse {
    pub namespace: String,
    pub schema_version: u32,
    /// Verbatim DSL text if uploaded as such; empty string for
    /// programmatic uploads.
    pub schema_document: String,
    /// `serde_json::to_vec(&Schema)` of the parsed AST.
    pub source_blob: Vec<u8>,
    pub uploaded_at_unix_nanos: u64,
    pub validator_version: u32,
}

/// `SCHEMA_LIST_RESP` (`0x01A2`). Single-frame snapshot in v1;
/// a later cut may split into streaming.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaListResponseFrame {
    pub namespace: String,
    /// Newest first.
    pub items: Vec<SchemaListItemWire>,
    pub total: u32,
    pub next_cursor: Vec<u8>,
    pub is_final: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaListItemWire {
    pub schema_version: u32,
    pub uploaded_at_unix_nanos: u64,
    pub validator_version: u32,
    pub has_source_text: bool,
}

/// `SCHEMA_VALIDATE_RESP` (`0x01A3`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaValidateResponse {
    /// Namespace parsed from the document; `""` if parse failed
    /// before reaching `namespace`.
    pub namespace: String,
    /// `current_active + 1` if validation passed; `0` otherwise.
    pub would_be_version: u32,
    pub validation_errors: Vec<SchemaValidationErrorWire>,
}

/// `SCHEMA_REPLACE_RESP` (`0x01A7`). Carries the count of declared
/// rows dropped before the new schema landed. `version` is the new
/// active version (always > the pre-replace version).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaReplaceResponse {
    pub namespace: String,
    pub schema_version: u32,
    pub dropped_count: u32,
    pub validation_errors: Vec<SchemaValidationErrorWire>,
}

/// `SCHEMA_DROP_RESP` (`0x01A5`). `schema_version` is the new active
/// version after the narrow, or `0` when nothing was dropped (the target
/// was not a declared type) or the drop was rejected. `dropped` is
/// `true` only when a declared row was actually removed. `live_rows` is
/// the count of live rows found referencing the target — non-zero and
/// `dropped == false` means the drop was refused for lack of `force`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaDropResponse {
    pub namespace: String,
    pub schema_version: u32,
    /// Echo of the requested target kind.
    pub target_kind: u8,
    /// Echo of the requested target local name.
    pub target_name: String,
    /// `true` when a declared row was removed and the version bumped.
    pub dropped: bool,
    /// Live (non-tombstoned) rows found referencing the target.
    pub live_rows: u32,
    pub validation_errors: Vec<SchemaValidationErrorWire>,
}

/// One structured parse-or-validate error. `code` is the variant
/// name from `ParseError` / `ValidationErrorCode`. `line` / `col`
/// are 1-based; `0` if no source position is known.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaValidationErrorWire {
    pub code: String,
    pub message: String,
    pub line: u32,
    pub column: u32,
    pub length: u32,
    /// `0` info / `1` warning / `2` error. Always `2` in v1.
    pub severity: u8,
}
