//! Workspace-wide error type and result alias.
//!
//! The variant set mirrors the wire-protocol error codes. Keep them
//! aligned.

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
    /// or impossible). A `ResourceExhausted` error class.
    #[error("the shard is out of slots")]
    OutOfSlots,

    /// Memory allocation failed (e.g., an arena resize couldn't fallocate, or
    /// an in-memory structure exceeded its budget). A `ResourceExhausted`
    /// error class.
    #[error("out of memory: {0}")]
    OutOfMemory(String),

    /// The caller's request was throttled. A `ResourceExhausted` error class.
    /// The error message includes operator-facing hints (e.g., retry-after
    /// duration); clients map it to the language-native rate-limit type.
    #[error("rate limited: {0}")]
    RateLimited(String),

    #[error("internal error: {0}")]
    Internal(String),
}

/// Workspace result alias.
pub type Result<T> = std::result::Result<T, Error>;
