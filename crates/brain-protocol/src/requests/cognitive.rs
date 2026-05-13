//! Cognitive-op requests: ENCODE / ENCODE_VECTOR_DIRECT / RECALL / PLAN /
//! REASON / FORGET.

use rkyv::{Archive, Deserialize, Serialize};

use super::types::{
    EdgeKindWire, ForgetMode, MemoryKindWire, ObservationInput, PlanState, PlanStrategy,
    RecallStrategy,
};
use crate::request::{WireContextId, WireMemoryId, WireUuid};

/// Spec §07/1.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EncodeRequest {
    pub text: String,
    pub context_id: WireContextId,
    pub kind: MemoryKindWire,
    pub salience_hint: f32,
    pub edges: Vec<EdgeRequest>,
    pub request_id: WireUuid,
    pub txn_id: Option<WireUuid>,
    pub deduplicate: bool,
}

/// Edge attached to an `ENCODE_REQ`.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EdgeRequest {
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    pub weight: f32,
}

/// Spec §07/2.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EncodeVectorDirectRequest {
    pub text: String,
    pub vector_offset: u32,
    pub vector_dim: u16,
    pub model_fingerprint: [u8; 16],
    pub context_id: WireContextId,
    pub kind: MemoryKindWire,
    pub salience_hint: f32,
    pub edges: Vec<EdgeRequest>,
    pub request_id: WireUuid,
    pub txn_id: Option<WireUuid>,
}

/// Spec §07/3.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RecallRequest {
    pub cue_text: String,
    pub cue_vector_offset: u32,
    pub cue_vector_dim: u16,
    pub top_k: u32,
    pub confidence_threshold: f32,
    pub context_filter: Option<Vec<WireContextId>>,
    pub age_bound_unix_nanos: Option<u64>,
    pub kind_filter: Option<Vec<MemoryKindWire>>,
    pub salience_floor: f32,
    pub strategy_hint: Option<RecallStrategy>,
    pub include_vectors: bool,
    pub include_edges: bool,
    pub request_id: Option<WireUuid>,
    /// Spec §09/08 §5: when set, RECALL reads against a snapshot
    /// that includes the txn's pending writes (read-your-writes).
    pub txn_id: Option<WireUuid>,
}

/// Spec §07/4.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct PlanRequest {
    pub start: PlanState,
    pub goal: PlanState,
    pub budget: PlanBudget,
    pub strategy_hint: Option<PlanStrategy>,
    pub context_filter: Option<Vec<WireContextId>>,
    pub request_id: Option<WireUuid>,
    pub txn_id: Option<WireUuid>,
}

/// Spec §07/4 — plan budget.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct PlanBudget {
    pub max_steps: u32,
    pub max_wall_time_ms: u32,
    pub max_branches_explored: u32,
}

/// Spec §07/5.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ReasonRequest {
    pub observation: ObservationInput,
    pub depth: u32,
    pub confidence_threshold: f32,
    pub context_filter: Option<Vec<WireContextId>>,
    pub max_inferences: u32,
    pub budget_wall_time_ms: u32,
    pub request_id: Option<WireUuid>,
    pub txn_id: Option<WireUuid>,
}

/// Spec §07/6.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ForgetRequest {
    pub memory_id: WireMemoryId,
    pub mode: ForgetMode,
    pub request_id: WireUuid,
    pub txn_id: Option<WireUuid>,
}
