//! Schema-op response payloads. Spec §28/05 reconciled against
//! §21/05.

use rkyv::{Archive, Deserialize, Serialize};

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
    /// Always `true` in v1 (no diff computed). Reserved for §28/05
    /// forward-compat.
    pub backward_compatible: bool,
    /// Reserved opaque blob for §28/05 `SchemaMigrationSummary`.
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
/// phase 23 may split into streaming.
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
