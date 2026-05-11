//! Rust-side result types returned by `execute_*`. Phase 9's server
//! wraps these into the wire `ResponseBody` variants; for Phase 6
//! they're the integration-test assertion targets.

use brain_core::{ContextId, EdgeKind, MemoryId, MemoryKind};

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

/// Outcome of `execute_path`. Spec §09/04 §3 — multiple paths are
/// computable, but the v1 wire frame carries only the top-1; this
/// type preserves the full result for Phase 9's streaming chunker.
#[derive(Debug, Clone)]
pub struct PathResult {
    pub paths: Vec<Path>,
    pub status: PlanStatus,
}

/// One node-and-edge chain from a start memory to a goal memory.
/// `edges[i]` is the edge that connects `nodes[i]` → `nodes[i + 1]`.
#[derive(Debug, Clone)]
pub struct Path {
    pub nodes: Vec<MemoryId>,
    pub edges: Vec<EdgeKind>,
    pub score: f32,
    pub node_salience: Vec<f32>,
    pub node_text: Vec<String>,
}

/// Why `execute_path` returned. Mirrors the wire `PlanStatus` enum so
/// the brain-ops handler can pass it through unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    GoalReached,
    BudgetExhausted,
    NoPathFound,
    Timeout,
}
