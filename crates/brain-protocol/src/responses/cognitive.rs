//! Cognitive-op responses: ENCODE / RECALL / PLAN / REASON / FORGET frames.

use rkyv::{Archive, Deserialize, Serialize};

use super::types::{InferenceKind, PlanStatus, ReasonStatus, TransitionKind};
use crate::request::{EdgeKindWire, MemoryKindWire, WireContextId, WireMemoryId};

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
