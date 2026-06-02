//! Extractor-op request payloads.

use crate::envelope::request::WireUuid;

/// `EXTRACTOR_LIST` (`0x0124`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractorListRequest {
    pub include_disabled: bool,
}

/// `EXTRACTOR_DISABLE` (`0x0125`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractorDisableRequest {
    pub extractor_id: u32,
    /// Free-form reason recorded in the audit; ≤ 4 KiB.
    pub reason: String,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `EXTRACTOR_ENABLE` (`0x0126`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractorEnableRequest {
    pub extractor_id: u32,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

// ============================================================
// Response payloads
// ============================================================

/// One row in [`ExtractorListResponseFrame`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractorListItem {
    pub extractor_id: u32,
    pub namespace: String,
    pub name: String,
    /// `0`=pattern, `1`=classifier, `2`=llm.
    pub kind: u8,
    pub enabled: bool,
    pub schema_version: u32,
    pub created_at_unix_nanos: u64,
}

/// `EXTRACTOR_LIST_RESP` (`0x01A4`). Single-frame snapshot in v1;
/// a later cut may split into streaming if registry counts demand.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractorListResponseFrame {
    pub items: Vec<ExtractorListItem>,
    pub total: u32,
    /// Always `true` in v1. A later streaming cut may set `false` on
    /// intermediate frames.
    pub is_final: bool,
}

/// `EXTRACTOR_DISABLE_RESP` (`0x01A5`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractorDisableResponse {
    pub previously_enabled: bool,
    pub disabled_at_unix_nanos: u64,
}

/// `EXTRACTOR_ENABLE_RESP` (`0x01A6`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractorEnableResponse {
    pub previously_disabled: bool,
    pub enabled_at_unix_nanos: u64,
}
