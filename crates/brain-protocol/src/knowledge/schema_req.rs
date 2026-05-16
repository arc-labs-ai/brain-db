//! Schema-op request payloads. Spec §28/05.
//!
//! Aligns with the §21/05 truth (per-namespace versioning, no
//! migration); see §21/07 Q15 for the §28/05 reconciliation.

use rkyv::{Archive, Deserialize, Serialize};

use crate::request::WireUuid;

/// `SCHEMA_UPLOAD` (`0x0120`). Spec §28/05 §2.1.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SchemaUploadRequest {
    /// DSL source text per §21.
    pub schema_document: String,
    /// Parse + validate without persisting. Identical to
    /// `SCHEMA_VALIDATE` when `true`.
    pub dry_run: bool,
    /// Reserved for §28/05 forward-compat. Ignored in v1.
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
