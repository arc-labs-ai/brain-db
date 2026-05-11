//! Workspace-wide error type and result alias.
//!
//! The variant set mirrors the wire-protocol error codes in
//! `spec/03_wire_protocol/10_errors.md`. Keep them aligned.

use thiserror::Error;

/// The unified error type for the Brain workspace.
///
/// Crates further down the stack (storage, metadata, etc.) may define their
/// own internal errors and convert into this for propagation across crate
/// boundaries.
#[derive(Debug, Error)]
pub enum Error {
    #[error("not found")]
    NotFound,

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("permission denied")]
    PermissionDenied,

    #[error("conflict (idempotency or version mismatch)")]
    Conflict,

    #[error("the substrate is overloaded; retry with backoff")]
    Overloaded,

    #[error("the addressed shard is unavailable")]
    ShardUnavailable,

    #[error("the underlying storage layer reported: {0}")]
    Storage(String),

    #[error("data integrity check failed: {0}")]
    Corruption(String),

    #[error("operation timed out")]
    Timeout,

    /// The arena's slot space is exhausted (no free slots and growth refused
    /// or impossible). Spec `§03/10` error class `ResourceExhausted`.
    #[error("the shard is out of slots")]
    OutOfSlots,

    /// Memory allocation failed (e.g., an arena resize couldn't fallocate, or
    /// an in-memory structure exceeded its budget). Spec `§03/10`
    /// `ResourceExhausted`.
    #[error("out of memory: {0}")]
    OutOfMemory(String),

    /// The caller's request was throttled. Spec `§03/10` `ResourceExhausted`.
    /// The error message includes operator-facing hints (e.g., retry-after
    /// duration); SDKs map it to the language-native rate-limit type.
    #[error("rate limited: {0}")]
    RateLimited(String),

    #[error("internal error: {0}")]
    Internal(String),
}

/// Workspace result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_exhausted_variants_display_with_context() {
        assert_eq!(Error::OutOfSlots.to_string(), "the shard is out of slots");
        assert_eq!(
            Error::OutOfMemory("arena grow failed".into()).to_string(),
            "out of memory: arena grow failed"
        );
        assert_eq!(
            Error::RateLimited("retry after 30s".into()).to_string(),
            "rate limited: retry after 30s"
        );
    }

    #[test]
    fn resource_exhausted_variants_are_distinct() {
        // Pin that the three new variants don't accidentally collapse into
        // a single shape — they map to different spec error codes.
        let a: Error = Error::OutOfSlots;
        let b: Error = Error::OutOfMemory(String::new());
        let c: Error = Error::RateLimited(String::new());
        assert!(matches!(a, Error::OutOfSlots));
        assert!(matches!(b, Error::OutOfMemory(_)));
        assert!(matches!(c, Error::RateLimited(_)));
    }
}
