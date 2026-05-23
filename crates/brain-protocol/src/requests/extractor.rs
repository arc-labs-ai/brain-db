//! Extractor-op request payloads.

use rkyv::{Archive, Deserialize, Serialize};

use crate::request::WireUuid;

/// `EXTRACTOR_LIST` (`0x0124`).
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ExtractorListRequest {
    pub include_disabled: bool,
}

/// `EXTRACTOR_DISABLE` (`0x0125`).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ExtractorDisableRequest {
    pub extractor_id: u32,
    /// Free-form reason recorded in the audit; ≤ 4 KiB.
    pub reason: String,
    pub request_id: WireUuid,
}

/// `EXTRACTOR_ENABLE` (`0x0126`).
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ExtractorEnableRequest {
    pub extractor_id: u32,
    pub request_id: WireUuid,
}
