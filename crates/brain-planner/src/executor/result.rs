//! Rust-side result types returned by `execute_*`. Phase 9's server
//! wraps these into the wire `ResponseBody` variants; for Phase 6
//! they're the integration-test assertion targets.

use brain_core::{ContextId, MemoryId, MemoryKind};

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
