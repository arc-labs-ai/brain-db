//! Cognitive-op requests: ENCODE / ENCODE_VECTOR_DIRECT / RECALL / PLAN /
//! REASON / FORGET.

use rkyv::{Archive, Deserialize, Serialize};

use crate::shared::primitives::{EdgeKindWire, ForgetMode, MemoryKindWire, ObservationInput, PlanState, PlanStrategy};
use crate::envelope::request::{WireContextId, WireMemoryId, WireUuid};

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

// ============================================================
// Response payloads (cognitive)
// ============================================================


use crate::shared::enums::{InferenceKind, PlanStatus, ReasonStatus, RetrieverNameWire, StageKind, TransitionKind};

/// `ENCODE_RESP`.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EncodeResponse {
    pub memory_id: WireMemoryId,
    pub was_deduplicated: bool,
    pub salience: f32,
    pub auto_edges_added: u32,
    // ── Provenance + chaining (added by the v1 subscribe-replay PR) ──
    /// WAL LSN the encode was recorded at. `0` for the in-memory
    /// test path / no-schema deployments without a WAL sink.
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
    /// Background stages this write queued. Each entry will emit a
    /// `SubscriptionEvent` with `event_type == StageCompleted` once
    /// the corresponding worker commits its derived phases. Empty
    /// when the write triggered no background work (e.g. schemaless
    /// deployment with workers disabled, or a dedup hit).
    pub pending_stages: Vec<StageKind>,
    /// Whether a user schema is currently declared on the shard the
    /// write landed on. Lets the client distinguish two structurally
    /// identical "0 statements, 0 relations" extractor outcomes:
    /// (a) no schema declares matching predicates — the renderer can
    /// say so up front, and (b) schema IS declared but the extractor
    /// couldn't find a sentence with a matching predicate. The
    /// distinction matters because (a) is a deployment-time
    /// configuration story and (b) is a per-memory content story.
    pub has_active_schema: bool,
    /// Whether at least one LLM-tier extractor is registered and
    /// enabled on the shard the write landed on. When false (no
    /// `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` configured at boot, or
    /// every LLM extractor disabled via `EXTRACTOR_DISABLE`),
    /// statements/relations are unreachable regardless of input
    /// content — the renderer surfaces a config hint instead of the
    /// generic "extractor produced no statements" suffix. Independent
    /// of `has_active_schema` because the two failure modes need
    /// different operator remediation.
    pub has_llm_extractor: bool,
}

/// — one streaming RECALL frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RecallResponseFrame {
    pub results: Vec<MemoryResult>,
    pub is_final: bool,
    pub cumulative_count: u32,
    pub estimated_remaining: Option<u32>,
}

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
    pub edges: Option<Vec<EdgeView>>,
    /// Retrievers that surfaced this memory. Empty when no schema is
    /// declared and inside transactions; populated when the server
    /// routes RECALL through the hybrid engine.
    pub contributing_retrievers: Vec<RetrieverNameWire>,
    /// Post-RRF fused rank score. `0.0` on no-schema deployments
    /// and inside transactions; positive when hybrid retrieval ran
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
    /// next_lsn watermark. `0` for no-schema deployments that
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
    /// Per-hit graph enrichment populated when the request carries
    /// `include_graph = true` AND the memory was processed by the
    /// typed-graph extractors (mentions edges exist). `None` on
    /// schemaless deployments and when `include_graph` is unset.
    pub graph: Option<GraphEnrichment>,
}

/// Per-hit typed-graph side-channel surfaced when the client
/// passes `include_graph = true`. Empty vectors are valid (the memory
/// went through extractors but produced no entities/statements/
/// relations) — the renderer omits empty sections.
///
/// All three lists are capped server-side so the response stays
/// bounded for memories that mention dozens of entities. Caps:
///   * entities — 16 (all mentioned entities, scored by mention
///     recency; oldest dropped first if over cap)
///   * statements — 5 (top by `confidence` desc, restricted to
///     `is_current = 1`)
///   * relations — 5 (top by `created_at_unix_nanos` desc, both
///     incoming and outgoing typed edges incident to mentioned
///     entities)
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct GraphEnrichment {
    pub entities: Vec<EnrichedEntity>,
    pub statements: Vec<EnrichedStatement>,
    pub relations: Vec<EnrichedRelation>,
}

/// Wire form of one entity mentioned by the recalled memory. Carries
/// the canonical name + type label so the renderer doesn't need to
/// follow back-references to the entity table.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EnrichedEntity {
    pub id: [u8; 16],
    pub name: String,
    /// Human-readable `"namespace:typename"` (or bare `"typename"` for
    /// the default namespace). The renderer prints this inline beside
    /// the entity name.
    pub type_qname: String,
}

/// Wire form of one statement sourced by the recalled memory.
/// `predicate` and `object_label` are pre-rendered server-side so the
/// renderer doesn't have to chase predicate-id / object-blob lookups.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EnrichedStatement {
    pub id: [u8; 16],
    pub subject_name: String,
    pub predicate: String,
    /// Stringified object — entity canonical name for entity objects,
    /// formatted scalar for literal objects.
    pub object_label: String,
    pub confidence: f32,
}

/// Wire form of one typed relation incident to an entity mentioned by
/// the recalled memory. `predicate` is the human-readable name of the
/// relation type (e.g. `"works_at"`, `"lives_in"`).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EnrichedRelation {
    pub from_name: String,
    pub predicate: String,
    pub to_name: String,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EdgeView {
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    pub weight: f32,
}

/// — one streaming PLAN frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct PlanResponseFrame {
    pub steps: Vec<PlanStep>,
    pub is_final: bool,
    pub plan_status: Option<PlanStatus>,
}

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

/// — one streaming REASON frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ReasonResponseFrame {
    pub inferences: Vec<InferenceStep>,
    pub is_final: bool,
    pub reason_status: Option<ReasonStatus>,
}

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

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ForgetResponse {
    pub memory_id: WireMemoryId,
    pub was_already_forgotten: bool,
    pub edges_removed: u32,
}

// ============================================================
// Request payloads (link)
// ============================================================

/// — `LINK_REQ` body. Creates an edge between two memories.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct LinkRequest {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    /// `[0, 1]` for most kinds; `[-1, 1]` for `Contradicts`.
    pub weight: f32,
    pub request_id: WireUuid,
    pub txn_id: Option<WireUuid>,
}

/// — `UNLINK_REQ` body. Removes an edge identified by the
/// `(source, kind, target)` triple.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnlinkRequest {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    pub request_id: WireUuid,
    pub txn_id: Option<WireUuid>,
}

// ============================================================
// Response payloads (link)
// ============================================================



/// — `LINK_RESP` body.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct LinkResponse {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    pub weight: f32,
    pub created_at_unix_nanos: u64,
    /// `true` if this edge already existed (LINK is overwriting weight),
    /// `false` if newly created.
    pub already_existed: bool,
}

/// — `UNLINK_RESP` body.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnlinkResponse {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    /// `true` if the edge existed and was removed; `false` if it
    /// didn't exist (UNLINK is idempotent — non-existent = no-op).
    pub removed: bool,
}
