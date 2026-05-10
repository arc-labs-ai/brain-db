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

    #[error("internal error: {0}")]
    Internal(String),
}

/// Workspace result alias.
pub type Result<T> = std::result::Result<T, Error>;
