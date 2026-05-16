//! Extractor-op response payloads. Spec §28/05 §6-§7.

use rkyv::{Archive, Deserialize, Serialize};

/// One row in [`ExtractorListResponseFrame`].
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
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
/// phase 23 may split into streaming if registry counts demand.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ExtractorListResponseFrame {
    pub items: Vec<ExtractorListItem>,
    pub total: u32,
    /// Always `true` in v1. Phase 23 may set `false` on intermediate
    /// frames if streaming lands.
    pub is_final: bool,
}

/// `EXTRACTOR_DISABLE_RESP` (`0x01A5`). Spec §28/05 §7.2.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ExtractorDisableResponse {
    pub previously_enabled: bool,
    pub disabled_at_unix_nanos: u64,
}

/// `EXTRACTOR_ENABLE_RESP` (`0x01A6`).
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ExtractorEnableResponse {
    pub previously_disabled: bool,
    pub enabled_at_unix_nanos: u64,
}
