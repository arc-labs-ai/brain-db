//! Runtime errors raised by the executor. Distinct from `PlanError`,
//! which is the plan-time failure mode.
//!
//! Each variant maps to a wire-protocol error code at the server
//! boundary (Phase 9). For Phase 6 they're the Rust-side return shape
//! of `execute_recall` (and future `execute_encode`, etc.).

use brain_embed::EmbedError;
use thiserror::Error;

use super::writer::WriterError;

#[derive(Debug, Error)]
pub enum ExecError {
    /// The embedder failed (model load, forward pass, tokenisation,
    /// numeric failure). Wraps `brain-embed`'s typed error.
    #[error("embedding failed: {0}")]
    EmbedFailed(#[from] EmbedError),

    /// ANN search returned an error (or panicked behind a result;
    /// `SharedHnsw::search_active` itself is infallible, so this is
    /// mostly future-proofing).
    #[error("ANN search failed: {0}")]
    IndexSearchFailed(String),

    /// Reading from the metadata DB failed (txn open, table open, key
    /// lookup). Keep the message as a string — `redb`'s error tree is
    /// large and we don't yet need to discriminate at the executor.
    #[error("metadata read failed: {0}")]
    MetadataReadFailed(String),

    /// HNSW returned a `MemoryId` not present in the metadata store —
    /// indicates a desync between the two. Surfaced loudly so it
    /// shows up in tests instead of silently swallowing the candidate.
    #[error("metadata missing for HNSW hit: {memory_id:?}")]
    MemoryNotFound { memory_id: brain_core::MemoryId },

    /// Request shape we accepted at plan-time but can't actually
    /// execute yet. Different from `PlanError::Unsupported`, which
    /// fires before the executor is reached.
    #[error("unsupported at execute-time: {0}")]
    Unsupported(&'static str),

    /// Catch-all for internal bookkeeping errors that don't have a
    /// dedicated variant yet.
    #[error("internal executor error: {0}")]
    Internal(String),

    /// Writer rejected or failed (overloaded queue, internal error).
    /// Spec §08/08 §14's backpressure surfaces here.
    #[error("writer rejected: {0}")]
    WriterFailed(#[from] WriterError),
}
