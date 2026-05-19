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
    /// fixtures, substrate-only deployments without a WAL sink).
    /// Surfaced as `MemoryResult.lsn` so clients can chain
    /// `recall → subscribe --start-lsn lsn+1`.
    pub encoded_at_lsn: u64,
}

#[derive(Debug, Clone)]
pub struct EncodeResult {
    pub memory_id: MemoryId,
    pub edge_results: Vec<EdgeOutcome>,
    /// `true` when the writer replayed a cached idempotency entry;
    /// `false` for a fresh write. Spec §08/04 §4. Transparent —
    /// the wire response does not carry this.
    pub replayed: bool,
    /// `true` when the caller asked for dedup AND the fingerprint
    /// table hit (spec §07/07 §6). The returned `memory_id` is
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

/// Outcome of `execute_path`. Spec §09/04 §3 — multiple paths are
/// computable, but the v1 wire frame carries only the top-1; this
/// type preserves the full result for Phase 9's streaming chunker.
#[derive(Debug, Clone)]
pub struct PathResult {
    pub paths: Vec<Path>,
    pub status: PlanStatus,
}

/// One node-and-edge chain from a start memory to a goal memory.
/// `edges[i]` is the edge that connects `nodes[i]` → `nodes[i + 1]`;
/// `edge_weights[i]` is its weight (LINK default 1.0; arbitrary if
/// the link was created with a different weight). Spec §09/04 §10
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

/// Outcome of `execute_reason`. Spec §09/05 §3 — supporting +
/// contradicting evidence with an aggregate confidence.
#[derive(Debug, Clone)]
pub struct ReasonResult {
    pub base_memories: Vec<MemoryId>,
    pub supporting: Vec<EvidenceItem>,
    pub contradicting: Vec<EvidenceItem>,
    /// `(sum_s - sum_c) / (sum_s + sum_c)`; in `[-1, 1]`; `0` when the
    /// denominator is zero (spec §09/05 §6).
    pub confidence: f32,
    pub status: ReasonStatus,
}

/// One piece of evidence the executor found. Spec §09/05 §3.
#[derive(Debug, Clone)]
pub struct EvidenceItem {
    pub memory_id: MemoryId,
    /// `base_similarity × decay(distance) × ∏ edge.weight`; in
    /// `[0, 1]`. Spec §09/05 §17.
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
