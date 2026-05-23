//! Schema-op request payloads.
//!
//! Per-namespace versioning. No migrations in v1; breaking schema
//! changes are made in place.

use rkyv::{Archive, Deserialize, Serialize};

use crate::request::WireUuid;

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
