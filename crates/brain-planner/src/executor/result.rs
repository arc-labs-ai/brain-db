//! Rust-side result types returned by `execute_*`. The server wraps
//! these into the wire `ResponseBody` variants; for now they're the
//! integration-test assertion targets.

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
/// type preserves the full result for the streaming chunker.
#[derive(Debug, Clone)]
pub struct PathResult {
    pub paths: Vec<Path>,
    pub status: PlanStatus,
    /// Full per-stage trace of the bidirectional BFS. `Some` only when
    /// `execute_path`/`execute_path_stream` was called with
    /// `trace = true`; `None` on the default fast path, where none of
    /// this is collected or allocated. See [`PlanExecutionMetadata`].
    pub trace: Option<PlanExecutionMetadata>,
}

/// Opt-in full-detail trace of a PLAN bidirectional-BFS execution —
/// the direct analogue of RECALL's `QueryMetadata` full-detail fields
/// and REASON's considered-edge trace. Populated only when the
/// executor is called with `trace = true`; every field stays empty
/// on the default path so the fast path allocates nothing extra.
///
/// This is the internal (non-wire) shape. A later phase bridges these
/// fields into `brain-protocol`'s wire `PlanTrace` type.
#[derive(Debug, Clone, Default)]
pub struct PlanExecutionMetadata {
    /// Every node touched during the bidirectional BFS, in both the
    /// forward and backward visited maps — the full detail behind the
    /// scalar `nodes_explored` count the non-traced path only uses
    /// internally for budget accounting.
    pub explored: Vec<PlanTraceNode>,
    /// Every meeting point the BFS found (a node visited from both
    /// directions), including ones beyond `traversal.max_paths` that
    /// today are silently dropped from the result set. Each entry
    /// flags whether it made it into the final (capped) path set.
    pub meeting_points: Vec<PlanTraceMeetingPoint>,
}

/// One node from a BFS visited map (`fwd` or `bwd`), captured for the
/// full-detail trace.
#[derive(Debug, Clone, Copy)]
pub struct PlanTraceNode {
    pub memory_id: MemoryId,
    /// Which side of the bidirectional search touched this node.
    pub direction: PlanTraceDirection,
    /// Hops from this node's seed (the start set for `Forward`, the
    /// goal set for `Backward`); `0` for a seed node itself.
    pub depth: usize,
    /// The edge into this node from its BFS parent; `None` for a seed
    /// node (no parent).
    pub parent_edge: Option<EdgeKind>,
    /// The BFS parent node itself (the node this one was reached
    /// from); `None` for a seed node. Distinct from `parent_edge`,
    /// which carries the edge *kind* — this carries the parent's
    /// identity so a caller can reconstruct the visited-map tree, not
    /// just the per-hop edge kind. Populated from the same `Crumb`
    /// that already tracks it internally.
    pub parent_id: Option<MemoryId>,
    /// Cosine alignment to the goal centroid, as computed by
    /// `order_by_goal_proximity` when the forward frontier was sorted
    /// toward the goal. `None` when this node wasn't scored — always
    /// true for `Backward` nodes (the heuristic only orders forward
    /// expansion) and for `Forward` nodes reached while no goal
    /// centroid was available.
    pub alignment_score: Option<f32>,
}

/// Which side of the bidirectional BFS a [`PlanTraceNode`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanTraceDirection {
    /// Expanded outward from the start set.
    Forward,
    /// Expanded outward from the goal set.
    Backward,
}

/// One meeting point the BFS found, flagged with whether it survived
/// the `traversal.max_paths` cap into the final result set.
#[derive(Debug, Clone, Copy)]
pub struct PlanTraceMeetingPoint {
    pub memory_id: MemoryId,
    /// `true` when this meeting point is one of the first
    /// `traversal.max_paths` found (i.e. its reconstructed path is in
    /// `PathResult.paths`); `false` when the cap dropped it.
    pub included_in_result: bool,
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
#[derive(Debug, Clone)]
pub struct PathStreamTerminal {
    pub status: PlanStatus,
    pub paths_emitted: u32,
    /// Carried through from [`PathResult::trace`]. `Some` only when
    /// the stream was driven with `trace = true`.
    pub trace: Option<PlanExecutionMetadata>,
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
    /// Full per-stage trace, populated only when `execute_reason` /
    /// `execute_reason_stream` was called with `trace = true`. `None` on
    /// the default fast path — no extra capture, no extra allocation.
    /// See [`ReasonTrace`].
    pub trace: Option<ReasonTrace>,
    /// Which inference pattern drove this result. `AnalogicalInference`
    /// when the VSA structural-fit nudge (see `executor::analogical`)
    /// moved the aggregate confidence materially away from its
    /// evidence-only baseline; `EvidenceAccumulation` otherwise — the
    /// v1 default and today's only other reachable value.
    /// `CausalExplanation` is reserved for a future causal-chain
    /// executor path that doesn't exist yet.
    pub inference_kind: InferenceKind,
}

/// Which inference pattern produced a `ReasonResult` / `InferenceStep`.
///
/// Internal mirror of the wire `InferenceKind`
/// (`brain_protocol::shared::enums::InferenceKind`) — kept as its own
/// type, same as [`ReasonStatus`] below, so brain-planner's executor
/// internals don't couple to the wire crate's enum shape; brain-ops
/// maps this 1:1 onto the wire enum when building the response frame.
/// No `Other(String)` variant: the executor never produces an
/// arbitrary-string kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceKind {
    /// Reserved for a future causal-chain executor path — not
    /// produced by `execute_reason` today.
    CausalExplanation,
    /// The v1 evidence-traversal walk (`walk_outward` over
    /// Supports/Contradicts edges), unmodified or only mildly nudged
    /// by the analogical-fit term.
    EvidenceAccumulation,
    /// The evidence-traversal walk's ranking was materially reshaped
    /// by the VSA structural-fit nudge (see `executor::analogical`).
    AnalogicalInference,
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

// ---------------------------------------------------------------------------
// Full per-stage REASON trace (opt-in, `trace = true`).
//
// Internal (non-wire) shape mirroring `QueryMetadata`'s full-detail trace
// fields on the RECALL side (`brain_planner::retrieval::executor::logic`).
// A later phase bridges these into `brain-protocol`'s wire `ReasonTrace`
// struct; the field names here stay close to that wire shape without
// being bound to it. Every field is empty/default when the executor runs
// with `trace = false` — see the `trace` gate at each capture site in
// `executor::reason`.
// ---------------------------------------------------------------------------

/// Full per-stage REASON trace, attached to [`ReasonResult`] and
/// [`InferenceStreamTerminal`] only when the caller requested
/// `trace = true`.
#[derive(Debug, Clone, Default)]
pub struct ReasonTrace {
    /// The full `resolve_base` candidate set (HNSW hits for `ByText`, the
    /// single resolved seed for `ByMemoryId`), not just the ids that made
    /// it into `base_memories`.
    pub base: ReasonTraceBase,
    /// Everything the supports-edge-kind `walk_outward` pass touched,
    /// before/at each of its three prune points (edge-kind, tombstone,
    /// already-visited).
    pub supports_walk: ReasonTraceWalk,
    /// Same as `supports_walk`, for the contradicts-edge-kind pass.
    pub contradicts_walk: ReasonTraceWalk,
    /// What `filter_and_trim` dropped from the supporting-evidence list
    /// (confidence floor, then the `max_supporting` trim cap).
    pub supports_trim: ReasonTraceTrim,
    /// Same as `supports_trim`, for the contradicting-evidence list.
    pub contradicts_trim: ReasonTraceTrim,
    /// Un-collapsed `topic_alignment_factor` components for every
    /// surviving evidence item (supporting, contradicting, and the
    /// direct-similarity base items), keyed by `memory_id`.
    pub scoring: Vec<ReasonTraceScoreBreakdown>,
    /// Whether `build_base_centroid` produced a centroid, and if not,
    /// which short-circuit path caused the `None`.
    pub centroid: ReasonTraceCentroid,
}

/// The full `resolve_base` candidate set.
#[derive(Debug, Clone, Default)]
pub struct ReasonTraceBase {
    pub candidates: Vec<ReasonTraceCandidate>,
}

/// One `resolve_base` candidate: the memory id, its stored text (when
/// fetchable), and its base score (`1.0` for a `ByMemoryId` seed, cosine
/// similarity for a `ByText` ANN hit).
#[derive(Debug, Clone)]
pub struct ReasonTraceCandidate {
    pub memory_id: MemoryId,
    pub text: Option<String>,
    pub score: f32,
}

/// Everything one `walk_outward` pass touched: the full considered
/// out-edge set before any pruning, plus what each of the three prune
/// steps dropped.
#[derive(Debug, Clone, Default)]
pub struct ReasonTraceWalk {
    /// Every out-edge `list_memory_edges_from` returned for any visited
    /// node in this pass, before the edge-kind / tombstone /
    /// already-visited filters run. The direct analogue of
    /// `RecallTraceRetriever.candidates` on the RECALL side.
    pub considered: Vec<ReasonTraceEdgeCandidate>,
    /// Edges whose kind isn't in this pass's `edge_kinds` set.
    pub dropped_by_edge_kind: Vec<ReasonTraceEdgeCandidate>,
    /// Edges whose target is a tombstoned memory (committed, or
    /// tombstoned in-txn).
    pub dropped_by_tombstone: Vec<ReasonTraceEdgeCandidate>,
    /// Edges whose target the BFS had already visited.
    pub dropped_by_visited: Vec<ReasonTraceEdgeCandidate>,
}

/// One edge `walk_outward` considered (or dropped), before it becomes an
/// `EvidenceItem`.
#[derive(Debug, Clone, Copy)]
pub struct ReasonTraceEdgeCandidate {
    /// The edge's target — the candidate memory.
    pub memory_id: MemoryId,
    pub edge_kind: EdgeKind,
    /// Hops from the base set this candidate would sit at if kept.
    pub depth: usize,
    /// The node the edge was walked from.
    pub from_memory_id: MemoryId,
    /// The edge's stored weight (`EdgeData.weight`).
    pub raw_score: f32,
}

/// What `filter_and_trim` dropped for one traversal (supporting or
/// contradicting evidence): items below the confidence floor, and items
/// cut by the `max_supporting` / `max_contradicting` trim cap. Each pair
/// is `(memory_id, score)`.
#[derive(Debug, Clone, Default)]
pub struct ReasonTraceTrim {
    pub dropped_by_confidence: Vec<(MemoryId, f32)>,
    pub dropped_by_trim_cap: Vec<(MemoryId, f32)>,
}

/// Un-collapsed `topic_alignment_factor` components for one surviving
/// evidence item — `final_score = base_similarity * decay * weight_product
/// * alignment`.
#[derive(Debug, Clone, Copy)]
pub struct ReasonTraceScoreBreakdown {
    pub memory_id: MemoryId,
    pub base_similarity: f32,
    pub decay: f32,
    pub weight_product: f32,
    pub alignment: f32,
    /// VSA structural-fit multiplier (see `executor::analogical`),
    /// bounded to `[ANALOGICAL_FIT_MIN, ANALOGICAL_FIT_MAX]`. Defaults
    /// to `1.0` (neutral) for every item until the analogical-fit pass
    /// runs; items dropped by the confidence floor before that pass
    /// keep the default, since the nudge only ever applies to
    /// survivors.
    pub analogical_fit: f32,
    /// `base_similarity * decay * weight_product * alignment *
    /// analogical_fit`.
    pub final_score: f32,
}

/// Whether `build_base_centroid` produced a centroid. `skipped_reason` is
/// `Some` with a short machine-readable tag (`"singleton_base"`,
/// `"by_text_observation"`, `"missing_text_rows"`, `"embed_failed"`,
/// `"bundle_failed"`, or a `read_txn_failed:`/`table_open_failed:`
/// message) whenever `computed = false`.
#[derive(Debug, Clone, Default)]
pub struct ReasonTraceCentroid {
    pub computed: bool,
    pub skipped_reason: Option<String>,
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
    /// Copied from [`ReasonResult::inference_kind`].
    pub inference_kind: InferenceKind,
}

/// Closing summary emitted after every `InferenceStep`. Maps onto the
/// final REASON wire frame.
#[derive(Debug, Clone)]
pub struct InferenceStreamTerminal {
    pub status: ReasonStatus,
    /// Aggregate confidence across the whole stream. Equals the lone
    /// step's confidence in v1; future multi-step iterations will
    /// average / combine here.
    pub confidence: f32,
    pub steps_emitted: u32,
    /// Full per-stage trace, populated only when `execute_reason_stream`
    /// was called with `trace = true`. `None` on the default fast path.
    /// This is the field that maps onto the final (`is_final: true`)
    /// REASON wire frame's trace payload.
    pub trace: Option<ReasonTrace>,
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
