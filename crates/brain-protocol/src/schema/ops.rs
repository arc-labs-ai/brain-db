//! Schema-op request payloads.
//!
//! Per-namespace versioning. No migrations in v1; breaking schema
//! changes are made in place.

use rkyv::{Archive, Deserialize, Serialize};

use crate::envelope::request::WireUuid;

/// `SCHEMA_UPLOAD` (`0x0120`).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SchemaUploadRequest {
    /// Schema DSL source text.
    pub schema_document: String,
    /// Parse + validate without persisting. Identical to
    /// `SCHEMA_VALIDATE` when `true`.
    pub dry_run: bool,
    /// Reserved for forward-compat with future migration support.
    /// Ignored in v1.
    pub allow_breaking: bool,
    pub request_id: WireUuid,
}

/// `SCHEMA_GET` (`0x0121`). `version == 0` → active version.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SchemaGetRequest {
    pub namespace: String,
    pub version: u32,
}

/// `SCHEMA_LIST` (`0x0122`). `limit == 0` → unlimited (v1 caps
/// to schema_list output size).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SchemaListRequest {
    pub namespace: String,
    pub limit: u32,
    pub cursor: Vec<u8>,
}

/// `SCHEMA_VALIDATE` (`0x0123`). Dry-run; never touches storage.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SchemaValidateRequest {
    pub schema_document: String,
}

// ============================================================
// Response payloads
// ============================================================


/// `SCHEMA_UPLOAD_RESP` (`0x01A0`).
///
/// `schema_version == 0` indicates the upload was rejected
/// (validation failure or dry_run). `validation_errors` carries
/// the structured error list when present.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
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
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
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
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SchemaListResponseFrame {
    pub namespace: String,
    /// Newest first.
    pub items: Vec<SchemaListItemWire>,
    pub total: u32,
    pub next_cursor: Vec<u8>,
    pub is_final: bool,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SchemaListItemWire {
    pub schema_version: u32,
    pub uploaded_at_unix_nanos: u64,
    pub validator_version: u32,
    pub has_source_text: bool,
}

/// `SCHEMA_VALIDATE_RESP` (`0x01A3`).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SchemaValidateResponse {
    /// Namespace parsed from the document; `""` if parse failed
    /// before reaching `namespace`.
    pub namespace: String,
    /// `current_active + 1` if validation passed; `0` otherwise.
    pub would_be_version: u32,
    pub validation_errors: Vec<SchemaValidationErrorWire>,
}

/// One structured parse-or-validate error. `code` is the variant
/// name from `ParseError` / `ValidationErrorCode`. `line` / `col`
/// are 1-based; `0` if no source position is known.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SchemaValidationErrorWire {
    pub code: String,
    pub message: String,
    pub line: u32,
    pub column: u32,
    pub length: u32,
    /// `0` info / `1` warning / `2` error. Always `2` in v1.
    pub severity: u8,
}
