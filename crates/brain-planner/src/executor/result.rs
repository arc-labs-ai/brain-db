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
    /// equals the dot product / cosine similarity.
    pub score: f32,
    pub kind: MemoryKind,
    pub context_id: ContextId,
    pub salience: f32,
    pub created_at_unix_nanos: u64,
    /// `None` until a wire-level `include_text` flag lands and the
    /// planner builds a `TextFetchStep`.
    pub text: Option<String>,
    // ── Provenance + decay signals (v1 expansion) ──
    /// Salience the row was first written with.
    pub salience_initial: f32,
    /// RECALL hit + explicit-get accumulator.
    pub access_count: u32,
    /// MemoryMetadata flags (ACTIVE / DEDUP_BACKREF / etc.).
    pub flags: u32,
    /// `Some(t)` for consolidation-worker-produced rows.
    pub consolidated_at_unix_nanos: Option<u64>,
    /// Denormalised outgoing edge count from the source row.
    pub edges_out_count: u32,
    /// Denormalised incoming edge count.
    pub edges_in_count: u32,
    /// Last-access timestamp (separate from `created_at`).
    pub last_accessed_at_unix_nanos: u64,
    /// WAL LSN this memory was encoded at — copied from
    /// `MemoryMetadata.encoded_at_lsn`. `0` when unknown (test
    /// fixtures, no-schema deployments without a WAL sink).
    /// Surfaced as `MemoryResult.lsn` so clients can chain
    /// `recall → subscribe --start-lsn lsn+1`.
    pub encoded_at_lsn: u64,
}

#[derive(Debug, Clone)]
pub struct EncodeResult {
    pub memory_id: MemoryId,
    pub edge_results: Vec<EdgeOutcome>,
    /// `true` when the writer replayed a cached idempotency entry;
    /// `false` for a fresh write. Transparent —
    /// the wire response does not carry this.
    pub replayed: bool,
    /// `true` when the caller asked for dedup AND the fingerprint
    /// table hit. The returned `memory_id` is
    /// the pre-existing Active memory's; no new slot was
    /// allocated. Surfaced to the wire as
    /// `EncodeResponse.was_deduplicated`.
    pub was_deduplicated: bool,
    /// WAL LSN this encode was recorded at (production); `None`
    /// for the in-memory test path. Surfaced as
    /// `EncodeResponse.lsn` so the client can chain subscribe.
    pub lsn: Option<u64>,
    /// Server unix-nanos timestamp on the memory row.
    pub created_at_unix_nanos: u64,
    /// Edges actually inserted (Inserted-outcome count).
    pub edges_out_count: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct ForgetResult {
    pub memory_id: MemoryId,
    pub outcome: ForgetOutcome,
    pub replayed: bool,
}

/// Outcome of `execute_path` — multiple paths are
/// computable, but the v1 wire frame carries only the top-1; this
/// type preserves the full result for Phase 9's streaming chunker.
#[derive(Debug, Clone)]
pub struct PathResult {
    pub paths: Vec<Path>,
    pub status: PlanStatus,
}

/// One scored start-to-goal path, ready to be encoded as a single
/// PLAN stream frame. The executor emits these in score-descending
/// order — clients see the best path first and can stop polling once
/// they have what they need.
#[derive(Debug, Clone)]
pub struct PathFrame {
    /// Position in the emission order; first emitted path is 0. Mid-
    /// stream paths share this with the `step_index` field on the
    /// wire frame.
    pub path_index: u32,
    pub path: Path,
}

/// Closing summary emitted after every `PathFrame`. Carries the
/// reason traversal stopped (goal reached, budget exhausted, no path
/// found, timeout) and the total count of paths the stream produced.
/// Maps onto the final PLAN wire frame (the one with `is_final = true`).
#[derive(Debug, Clone, Copy)]
pub struct PathStreamTerminal {
    pub status: PlanStatus,
    pub paths_emitted: u32,
}

/// Output of `execute_path_stream` — the per-path frames followed by
/// the terminal summary. The handler walks `paths` to emit mid-stream
/// frames and then writes a single terminal frame carrying `terminal`.
#[derive(Debug, Clone)]
pub struct PathStream {
    pub paths: Vec<PathFrame>,
    pub terminal: PathStreamTerminal,
}

/// One node-and-edge chain from a start memory to a goal memory.
/// `edges[i]` is the edge that connects `nodes[i]` → `nodes[i + 1]`;
/// `edge_weights[i]` is its weight (LINK default 1.0; arbitrary if
/// the link was created with a different weight)
/// uses these in the path score.
#[derive(Debug, Clone)]
pub struct Path {
    pub nodes: Vec<MemoryId>,
    pub edges: Vec<EdgeKind>,
    pub edge_weights: Vec<f32>,
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

/// Outcome of `execute_reason` — supporting +
/// contradicting evidence with an aggregate confidence.
#[derive(Debug, Clone)]
pub struct ReasonResult {
    pub base_memories: Vec<MemoryId>,
    pub supporting: Vec<EvidenceItem>,
    pub contradicting: Vec<EvidenceItem>,
    /// `(sum_s - sum_c) / (sum_s + sum_c)`; in `[-1, 1]`; `0` when the
    /// denominator is zero.
    pub confidence: f32,
    pub status: ReasonStatus,
}

/// One piece of evidence the executor found.
#[derive(Debug, Clone)]
pub struct EvidenceItem {
    pub memory_id: MemoryId,
    /// `base_similarity × decay(distance) × ∏ edge.weight`; in
    /// `[0, 1]`.
    pub score: f32,
    /// Edges traversed from the base set to this item; empty for
    /// direct-similarity (distance = 0) evidence.
    pub edge_path: Vec<EdgeKind>,
    /// Edge weights matching `edge_path[i]` index-by-index.
    pub edge_weights: Vec<f32>,
    /// Hops from the base set; `0` for direct-similarity items.
    pub distance: usize,
}

/// Why `execute_reason` returned. Mirrors the wire enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonStatus {
    Complete,
    BudgetExhausted,
    DepthLimitReached,
    Cancelled,
}

/// One inference step emitted as REASON streams. In v1 the executor
/// always produces exactly one step (the aggregate of all supporting
/// and contradicting evidence), so the stream is length-1. The shape
/// is kept multi-frame-ready: future iterations that walk supporting
/// and contradicting passes independently can emit one step per pass
/// without changing the framing.
#[derive(Debug, Clone)]
pub struct InferenceStep {
    /// Position in the emission order; first emitted step is 0.
    pub step_index: u32,
    pub base_memories: Vec<MemoryId>,
    pub supporting: Vec<EvidenceItem>,
    pub contradicting: Vec<EvidenceItem>,
    /// `(sum_s - sum_c) / (sum_s + sum_c)`; in `[-1, 1]`; `0` when
    /// the denominator is zero.
    pub confidence: f32,
}

/// Closing summary emitted after every `InferenceStep`. Maps onto the
/// final REASON wire frame.
#[derive(Debug, Clone, Copy)]
pub struct InferenceStreamTerminal {
    pub status: ReasonStatus,
    /// Aggregate confidence across the whole stream. Equals the lone
    /// step's confidence in v1; future multi-step iterations will
    /// average / combine here.
    pub confidence: f32,
    pub steps_emitted: u32,
}

/// Output of `execute_reason_stream` — the inference-step frames
/// followed by the terminal summary. The handler walks `steps` to
/// emit mid-stream frames and writes a single terminal frame
/// carrying `terminal`.
#[derive(Debug, Clone)]
pub struct InferenceStream {
    pub steps: Vec<InferenceStep>,
    pub terminal: InferenceStreamTerminal,
}
