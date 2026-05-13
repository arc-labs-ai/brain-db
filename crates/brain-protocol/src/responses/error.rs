//! ERROR response frame (spec §08 §25).

use rkyv::{Archive, Deserialize, Serialize};

use super::types::{ErrorCategoryWire, ErrorCodeWire};

/// Spec §08 §25 — error frame body.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ErrorResponse {
    pub code: ErrorCodeWire,
    pub category: ErrorCategoryWire,
    pub message: String,
    pub details: Option<ErrorDetails>,
    pub retry_after_ms: Option<u32>,
}

/// Spec §08 §25.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ErrorDetails {
    pub field: Option<String>,
    pub expected: Option<String>,
    pub actual: Option<String>,
}
