//! Rust-side result types returned by `execute_*`. Phase 9's server
//! wraps these into the wire `ResponseBody` variants; for Phase 6
//! they're the integration-test assertion targets.

use brain_core::{ContextId, MemoryId, MemoryKind};

use super::writer::{EdgeOutcome, ForgetOutcome};

#[derive(Debug, Clone)]
pub struct RecallResult {
    pub hits: Vec<RecallHit>,
}

#[derive(Debug, Clone)]
pub struct RecallHit {
    pub memory_id: MemoryId,
    /// Similarity score (higher = better). For unit-norm vectors this
    /// equals the dot product / cosine similarity (spec §06/04).
    pub score: f32,
    pub kind: MemoryKind,
    pub context_id: ContextId,
    pub salience: f32,
    pub created_at_unix_nanos: u64,
    /// `None` until a wire-level `include_text` flag lands and the
    /// planner builds a `TextFetchStep`.
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EncodeResult {
    pub memory_id: MemoryId,
    pub edge_results: Vec<EdgeOutcome>,
    /// `true` when the writer replayed a cached idempotency entry;
    /// `false` for a fresh write. Spec §08/04 §4.
    pub replayed: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ForgetResult {
    pub memory_id: MemoryId,
    pub outcome: ForgetOutcome,
    pub replayed: bool,
}
