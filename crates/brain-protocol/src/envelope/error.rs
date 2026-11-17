//! ERROR response frame.

/// ERROR frames keep classification values open so an unfamiliar value costs
/// one field while the peer still learns the message, details, and retry hint.
/// `ErrorCodeWire` and `ErrorCategoryWire` remain the construction and
/// classification vocabulary; use their `TryFrom` implementations to match a
/// received integer back onto the vocabulary known by this build.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ErrorResponse {
    pub code: u16,
    pub category: u8,
    pub message: String,
    pub details: Option<ErrorDetails>,
    pub retry_after_ms: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ErrorDetails {
    pub field: Option<String>,
    pub expected: Option<String>,
    pub actual: Option<String>,
}
