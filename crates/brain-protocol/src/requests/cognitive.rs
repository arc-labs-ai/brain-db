//! Cognitive-op requests: ENCODE / ENCODE_VECTOR_DIRECT / RECALL / PLAN /
//! REASON / FORGET.

use rkyv::{Archive, Deserialize, Serialize};

use super::types::{
    EdgeKindWire, ForgetMode, MemoryKindWire, ObservationInput, PlanState, PlanStrategy,
};
use crate::request::{WireContextId, WireMemoryId, WireUuid};

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

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RecallRequest {
    pub cue_text: String,
    pub top_k: u32,
    pub confidence_threshold: f32,
    pub context_filter: Option<Vec<WireContextId>>,
    pub age_bound_unix_nanos: Option<u64>,
    pub kind_filter: Option<Vec<MemoryKindWire>>,
    pub salience_floor: f32,
    pub include_edges: bool,
    /// When set, each `MemoryResult` carries a populated
    /// `graph: GraphEnrichment` field listing entities mentioned by
    /// the memory, statements sourced from it, and typed relations
    /// incident to those entities. Server-side typed-graph queries;
    /// if the memory wasn't extracted (no schema declared, no
    /// extractors registered, or a mention-less memory), the field
    /// is `None` even when this flag is set.
    pub include_graph: bool,
    /// When set, each `MemoryResult` carries the memory's stored UTF-8
    /// text. Costs one batched read against the per-shard `texts`
    /// table. When unset, `MemoryResult.text` is the empty string.
    pub include_text: bool,
    pub request_id: Option<WireUuid>,
    /// When set, RECALL reads against a snapshot that includes the
    /// txn's pending writes (read-your-writes).
    pub txn_id: Option<WireUuid>,
    /// Opt-in cross-encoder rerank over the RRF-fused candidates.
    /// Defaults to `false` so existing clients see no behaviour
    /// change. When set, the server runs `bge-reranker-base` over
    /// the top fused candidates and re-sorts; if the model isn't
    /// loaded the request still succeeds with RRF-only ordering.
    pub rerank: bool,
}

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

/// — plan budget.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct PlanBudget {
    pub max_steps: u32,
    pub max_wall_time_ms: u32,
    pub max_branches_explored: u32,
}

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

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ForgetRequest {
    pub memory_id: WireMemoryId,
    pub mode: ForgetMode,
    pub request_id: WireUuid,
    pub txn_id: Option<WireUuid>,
}
