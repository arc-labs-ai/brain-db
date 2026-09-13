//! Extractor introspection request/response payloads.
//!
//! Extraction is always-on; there is no runtime enable/disable. The
//! only extractor wire op is the read-only `EXTRACTOR_LIST`.

/// `EXTRACTOR_LIST` (`0x0124`). Takes no arguments — every registered
/// extractor is returned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractorListRequest {}

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
