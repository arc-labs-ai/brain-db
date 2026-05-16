//! Extractor-op request payloads. Spec §28/05 §6-§7, phase 20.8.

use rkyv::{Archive, Deserialize, Serialize};

use crate::request::WireUuid;

/// `EXTRACTOR_LIST` (`0x0124`). Spec §28/05 §6.1.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ExtractorListRequest {
    pub include_disabled: bool,
}

/// `EXTRACTOR_DISABLE` (`0x0125`). Spec §28/05 §7.1.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ExtractorDisableRequest {
    pub extractor_id: u32,
    /// Free-form reason recorded in the audit; ≤ 4 KiB.
    pub reason: String,
    pub request_id: WireUuid,
}

/// `EXTRACTOR_ENABLE` (`0x0126`). Spec §28/05 §7.1.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ExtractorEnableRequest {
    pub extractor_id: u32,
    pub request_id: WireUuid,
}
