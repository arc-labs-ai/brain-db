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
