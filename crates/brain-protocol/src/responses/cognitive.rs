//! Cognitive-op responses: ENCODE / RECALL / PLAN / REASON / FORGET frames.

use rkyv::{Archive, Deserialize, Serialize};

use super::types::{InferenceKind, PlanStatus, ReasonStatus, RetrieverNameWire, TransitionKind};
use crate::request::{EdgeKindWire, MemoryKindWire, WireContextId, WireMemoryId, WireUuid};

/// Spec §08 §1 `ENCODE_RESP`. Same shape used for §08 §2
/// `ENCODE_VECTOR_DIRECT_RESP`.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EncodeResponse {
    pub memory_id: WireMemoryId,
    pub was_deduplicated: bool,
    pub salience: f32,
    pub auto_edges_added: u32,
    // ── Provenance + chaining (added by the v1 subscribe-replay PR) ──
    /// WAL LSN the encode was recorded at. `0` for the in-memory
    /// test path / substrate-only deployments without a WAL sink.
    /// Production clients chain `encode → subscribe --start-lsn lsn+1`
    /// to follow downstream events from this point.
    pub lsn: u64,
    /// Agent the row was attributed to. Echoes the connection's
    /// AUTH-time agent so the client can verify routing.
    pub agent_id: WireUuid,
    /// Context the row was filed under. Echoes the request's
    /// `context_id`.
    pub context_id: WireContextId,
    /// Memory kind that was stored.
    pub kind: MemoryKindWire,
    /// Server unix-nanos at write time. Useful when client clock
    /// drifts vs the server.
    pub created_at_unix_nanos: u64,
    /// Outgoing edges that actually landed (the request may carry
    /// edges whose targets are missing — those are dropped silently;
    /// this count reflects the survivors).
    pub edges_out_count: u32,
    /// Embedding-model fingerprint stamped on the row. Lets the
    /// client detect when a model migration would change the vector.
    pub embedding_model_fp: [u8; 16],
}

/// Spec §08 §3 — one streaming RECALL frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RecallResponseFrame {
    pub results: Vec<MemoryResult>,
    pub is_final: bool,
    pub cumulative_count: u32,
    pub estimated_remaining: Option<u32>,
}

/// Spec §08 §3.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct MemoryResult {
    pub memory_id: WireMemoryId,
    pub text: String,
    pub similarity_score: f32,
    pub confidence: f32,
    pub salience: f32,
    pub kind: MemoryKindWire,
    pub context_id: WireContextId,
    pub created_at_unix_nanos: u64,
    pub last_accessed_at_unix_nanos: u64,
    pub vector_offset: u32,
    pub vector_dim: u16,
    pub edges: Option<Vec<EdgeView>>,
    /// Retrievers that surfaced this memory. Empty on substrate-only
    /// deployments and inside transactions; populated when the server
    /// routes RECALL through the hybrid engine (spec §28/08 §5).
    pub contributing_retrievers: Vec<RetrieverNameWire>,
    /// Post-RRF fused rank score. `0.0` on substrate-only deployments
    /// and inside transactions; positive when hybrid retrieval ran
    /// (spec §28/08 §5).
    pub fused_score: f32,
    // ── Memory provenance + decay signals (v1 expansion) ──
    /// Salience the row was first written with. Together with
    /// `salience` this shows how much decay has happened.
    pub salience_initial: f32,
    /// How many times this memory has been accessed (RECALL hits +
    /// explicit gets). Hotness signal — clients can sort by it for
    /// a recency-vs-popularity tradeoff.
    pub access_count: u32,
    /// WAL LSN this row was written at — derived from
    /// `MemoryMetadata.created_at_unix_nanos` + the shard's
    /// next_lsn watermark. `0` for substrate-only deployments that
    /// never wired a WAL sink. Lets the client say "subscribe from
    /// the moment this memory was written."
    pub lsn: u64,
    /// Status flags. ACTIVE = 0x1, HARD_FORGOTTEN = 0x2,
    /// CONSOLIDATED = 0x4, DEDUP_BACKREF = 0x8 (matches
    /// `brain_metadata::tables::memory::flags`).
    pub flags: u32,
    /// `Some(t)` when this row was produced by a consolidation
    /// worker (and is therefore a summary, not a raw memory).
    /// `None` for ordinary ENCODE-produced rows.
    pub consolidated_at_unix_nanos: Option<u64>,
    /// Denormalised outgoing-edge count (matches the source row's
    /// `edges_out_count`). Cheap connectivity signal even when the
    /// caller didn't ask for `--include-edges`.
    pub edges_out_count: u32,
    /// Denormalised incoming-edge count. "How linked-into is this?"
    pub edges_in_count: u32,
}

/// Spec §08 §3.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EdgeView {
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    pub weight: f32,
}

/// Spec §08 §4 — one streaming PLAN frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct PlanResponseFrame {
    pub steps: Vec<PlanStep>,
    pub is_final: bool,
    pub plan_status: Option<PlanStatus>,
}

/// Spec §08 §4.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct PlanStep {
    pub step_index: u32,
    pub memory_id: WireMemoryId,
    pub text: String,
    pub transition_kind: TransitionKind,
    pub confidence: f32,
    pub estimated_distance_to_goal: f32,
}

/// Spec §08 §5 — one streaming REASON frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ReasonResponseFrame {
    pub inferences: Vec<InferenceStep>,
    pub is_final: bool,
    pub reason_status: Option<ReasonStatus>,
}

/// Spec §08 §5.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct InferenceStep {
    pub step_index: u32,
    pub claim: String,
    pub supporting_memories: Vec<WireMemoryId>,
    pub contradicting_memories: Vec<WireMemoryId>,
    pub confidence: f32,
    pub inference_kind: InferenceKind,
}

/// Spec §08 §6.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ForgetResponse {
    pub memory_id: WireMemoryId,
    pub was_already_forgotten: bool,
    pub edges_removed: u32,
}
