//! Extractor introspection request/response payloads.
//!
//! Extraction is always-on; there is no runtime enable/disable. The
//! only extractor wire op is the read-only `EXTRACTOR_LIST`.

/// `EXTRACTOR_LIST` (`0x0124`). Every extractor registered for the effective
/// namespace is returned; there is nothing else to ask for.
///
/// No longer `Copy`: the `act_as` selector owns two `String`s. The struct
/// still encodes to an empty CBOR map when the selector is absent, so the
/// on-wire shape for a self-hosted caller is unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractorListRequest {
    /// Effective identity this listing runs as, on behalf of the
    /// authenticated connection principal. `None` (the common case, and
    /// omitted on the wire) means the op runs as the connection's own
    /// key-bound identity.
    /// The listing is filtered to the effective namespace, so a shared-pool
    /// gateway sees what the tenant declared rather than its own.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<crate::ActAs>,
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
