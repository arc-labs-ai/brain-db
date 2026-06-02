//! Cognitive-op requests: ENCODE / ENCODE_VECTOR_DIRECT / RECALL / PLAN /
//! REASON / FORGET.

use crate::envelope::request::{WireContextId, WireMemoryId, WireUuid};
use crate::shared::primitives::{
    EdgeKindWire, ForgetMode, MemoryKindWire, ObservationInput, PlanState, PlanStrategy,
};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeRequest {
    pub text: String,
    pub context_id: WireContextId,
    pub kind: MemoryKindWire,
    pub salience_hint: f32,
    pub edges: Vec<EdgeRequest>,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub txn_id: Option<WireUuid>,
    pub deduplicate: bool,
}

/// Power-user encode: client supplies the embedding vector itself and
/// the fingerprint of the model that produced it. Used by deployments
/// running their own (often domain-specific or multi-modal) embedder
/// outside Brain. The server skips its own embed step entirely, but
/// still runs every downstream validation, dedup, slot reservation,
/// edge wiring, and write submission.
///
/// The vector must be L2-normalised within `+/- 1e-3` (cosine
/// similarity assumes unit norm) and the fingerprint must match the
/// shard's currently-loaded model. Both checks fail with
/// `InvalidArgument` carrying a precise human-readable reason — power
/// users get to debug their embed pipeline.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeVectorDirectRequest {
    /// Text that produced the vector. Stored verbatim (for tantivy +
    /// future re-embedding). May be empty when the upstream embedder
    /// is multi-modal — but the server still requires a stable
    /// identifier; pass an empty string and the content-hash dedup
    /// path becomes a vector-bytes hash instead.
    pub text: String,
    /// The L2-normalised embedding (384 floats for the BGE-small v1
    /// fingerprint Brain ships with). Server validates length matches
    /// `brain_embed::VECTOR_DIM` and the norm is within tolerance.
    ///
    /// Excluded from the CBOR map: the floats ride the trailing raw
    /// little-endian `f32` section of the payload at full precision (CBOR
    /// would tag each float and round to half-precision). The encode arm
    /// appends them; the decode arm repopulates this field from the
    /// trailing bytes. `serde(default)` lets the CBOR-only decode
    /// reconstruct the struct with an empty vector before it is filled.
    #[serde(skip, default)]
    pub vector: Vec<f32>,
    /// Fingerprint of the model that produced `vector`. Must match
    /// the shard's loaded model fingerprint — a mismatch fails the
    /// write because the resulting memory would be unsearchable
    /// against future text-cued recalls.
    #[serde(with = "serde_bytes")]
    pub model_fingerprint: [u8; 16],
    pub context_id: WireContextId,
    pub kind: MemoryKindWire,
    pub salience_hint: f32,
    pub edges: Vec<EdgeRequest>,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub txn_id: Option<WireUuid>,
    pub deduplicate: bool,
}

/// Edge attached to an `ENCODE_REQ`.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EdgeRequest {
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    pub weight: f32,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub request_id: Option<WireUuid>,
    /// When set, RECALL reads against a snapshot that includes the
    /// txn's pending writes (read-your-writes).
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub txn_id: Option<WireUuid>,
    /// Explicit agent-id scope for the recall. Controls cross-agent
    /// isolation together with `include_other_agents`:
    ///   * empty + `include_other_agents == false` (the default) —
    ///     the server fills in the calling connection's agent, so
    ///     recall is isolated to the caller's own memories.
    ///   * non-empty — recall is scoped to exactly this set of agents,
    ///     regardless of who the caller is.
    ///   * any value + `include_other_agents == true` — see that flag;
    ///     no implicit caller filter is applied.
    #[serde(with = "crate::codec::cbor::vec_byte_array16")]
    pub agent_filter: Vec<WireUuid>,
    /// When true, the server does NOT inject the implicit
    /// caller-agent filter, yielding the across-agents view. Combined
    /// with an empty `agent_filter` this returns hits from every
    /// agent; combined with a non-empty `agent_filter` it still scopes
    /// to that explicit set. Defaults to false (caller-isolated).
    pub include_other_agents: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanRequest {
    pub start: PlanState,
    pub goal: PlanState,
    pub budget: PlanBudget,
    pub strategy_hint: Option<PlanStrategy>,
    pub context_filter: Option<Vec<WireContextId>>,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub request_id: Option<WireUuid>,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub txn_id: Option<WireUuid>,
}

/// — plan budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanBudget {
    pub max_steps: u32,
    pub max_wall_time_ms: u32,
    pub max_branches_explored: u32,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReasonRequest {
    pub observation: ObservationInput,
    pub depth: u32,
    pub confidence_threshold: f32,
    pub context_filter: Option<Vec<WireContextId>>,
    pub max_inferences: u32,
    pub budget_wall_time_ms: u32,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub request_id: Option<WireUuid>,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub txn_id: Option<WireUuid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForgetRequest {
    pub memory_id: WireMemoryId,
    pub mode: ForgetMode,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub txn_id: Option<WireUuid>,
}

// ============================================================
// Response payloads (cognitive)
// ============================================================

use crate::shared::enums::{
    InferenceKind, PlanStatus, ReasonStatus, RetrieverNameWire, StageKind, TransitionKind,
};

/// `ENCODE_RESP`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    #[serde(with = "serde_bytes")]
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
    #[serde(with = "serde_bytes")]
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
}

/// — one streaming RECALL frame.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecallResponseFrame {
    pub results: Vec<MemoryResult>,
    pub is_final: bool,
    pub cumulative_count: u32,
    pub estimated_remaining: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryResult {
    pub memory_id: WireMemoryId,
    pub text: String,
    pub similarity_score: f32,
    pub confidence: f32,
    pub salience: f32,
    pub kind: MemoryKindWire,
    /// Agent that owns this memory row. Lets the client see provenance
    /// and, on a cross-agent recall (`include_other_agents == true` or
    /// an explicit `agent_filter`), tell whose memory each hit is.
    #[serde(with = "serde_bytes")]
    pub agent_id: WireUuid,
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
    /// Cross-encoder relevance score, present iff the rerank stage
    /// actually scored this hit (cross-encoder loaded on the shard
    /// AND the hit fell inside the rerank window with fetchable
    /// text). `None` means the result is RRF-only ordered. When
    /// `Some`, the result list was re-sorted by this score, not by
    /// `fused_score`.
    pub rerank_score: Option<f32>,
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
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GraphEnrichment {
    pub entities: Vec<EnrichedEntity>,
    pub statements: Vec<EnrichedStatement>,
    pub relations: Vec<EnrichedRelation>,
}

/// Wire form of one entity mentioned by the recalled memory. Carries
/// the canonical name + type label so the renderer doesn't need to
/// follow back-references to the entity table.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnrichedEntity {
    #[serde(with = "serde_bytes")]
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
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnrichedStatement {
    #[serde(with = "serde_bytes")]
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
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnrichedRelation {
    pub from_name: String,
    pub predicate: String,
    pub to_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EdgeView {
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    pub weight: f32,
}

/// — one streaming PLAN frame.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanResponseFrame {
    pub steps: Vec<PlanStep>,
    pub is_final: bool,
    pub plan_status: Option<PlanStatus>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanStep {
    pub step_index: u32,
    pub memory_id: WireMemoryId,
    pub text: String,
    pub transition_kind: TransitionKind,
    pub confidence: f32,
    pub estimated_distance_to_goal: f32,
}

/// — one streaming REASON frame.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReasonResponseFrame {
    pub inferences: Vec<InferenceStep>,
    pub is_final: bool,
    pub reason_status: Option<ReasonStatus>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InferenceStep {
    pub step_index: u32,
    pub claim: String,
    pub supporting_memories: Vec<WireMemoryId>,
    pub contradicting_memories: Vec<WireMemoryId>,
    pub confidence: f32,
    pub inference_kind: InferenceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForgetResponse {
    pub memory_id: WireMemoryId,
    pub was_already_forgotten: bool,
    pub edges_removed: u32,
}

// ============================================================
// Request payloads (link)
// ============================================================

/// — `LINK_REQ` body. Creates an edge between two memories.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LinkRequest {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    /// `[0, 1]` for most kinds; `[-1, 1]` for `Contradicts`.
    pub weight: f32,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub txn_id: Option<WireUuid>,
}

/// — `UNLINK_REQ` body. Removes an edge identified by the
/// `(source, kind, target)` triple.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UnlinkRequest {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub txn_id: Option<WireUuid>,
}

// ============================================================
// Response payloads (link)
// ============================================================

/// — `LINK_RESP` body.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnlinkResponse {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    /// `true` if the edge existed and was removed; `false` if it
    /// didn't exist (UNLINK is idempotent — non-existent = no-op).
    pub removed: bool,
}

#[cfg(test)]
mod serde_smoke {
    use super::*;

    // Proves the additive serde derives actually function: a JSON
    // round-trip of one representative payload. The codec path is
    // covered by the round-trip tests in request.rs / response.rs.
    #[test]
    fn encode_request_serde_json_round_trips() {
        let req = EncodeRequest {
            text: "hello world".to_string(),
            context_id: 1,
            kind: MemoryKindWire::Episodic,
            salience_hint: 0.25,
            edges: vec![EdgeRequest {
                target: 0,
                kind: EdgeKindWire::Caused,
                weight: 0.9,
            }],
            request_id: [7u8; 16],
            txn_id: Some([8u8; 16]),
            deduplicate: true,
        };
        let j = serde_json::to_vec(&req).expect("serde_json encode");
        let back: EncodeRequest = serde_json::from_slice(&j).expect("serde_json decode");
        assert_eq!(req, back);
    }

    // The raw embedding for ENCODE_VECTOR_DIRECT rides the trailing raw
    // section of the frame payload, not the CBOR map. This proves three
    // things at once:
    //   1. the floats survive an encode/decode round-trip bit-for-bit,
    //   2. the CBOR section is *independent* of the vector contents
    //      (changing the vector does not change the CBOR prefix length —
    //      the floats are not encoded inside the map), and
    //   3. the raw f32 bytes appear exactly once on the wire (the CBOR
    //      prefix never contains them), so the total payload length is
    //      cbor_prefix_len + 4*dim with the prefix invariant to dim.
    #[test]
    fn encode_vector_direct_carries_vector_in_trailing_section() {
        use crate::codec::opcode::Opcode;
        use crate::envelope::request::RequestBody;

        let vector: Vec<f32> = vec![1.0, -2.5, 3.25, 0.0, 42.0, -0.125];
        let req = EncodeVectorDirectRequest {
            text: "precomputed".into(),
            vector: vector.clone(),
            model_fingerprint: [0xAB; 16],
            context_id: 9,
            kind: MemoryKindWire::Semantic,
            salience_hint: 0.5,
            edges: vec![],
            request_id: [3u8; 16],
            txn_id: None,
            deduplicate: false,
        };

        // Same struct, different vector. Because `vector` is skipped from
        // CBOR, the two CBOR prefixes must be byte-identical — this is
        // what proves the floats are not double-encoded into the map.
        let mut req_other = req.clone();
        req_other.vector = vec![9.0; 32];
        let cbor_a = crate::codec::cbor::to_cbor_bytes(&req);
        let cbor_b = crate::codec::cbor::to_cbor_bytes(&req_other);
        assert_eq!(
            cbor_a, cbor_b,
            "CBOR prefix must be invariant to vector contents (single-encode)"
        );

        // The raw little-endian f32 trailer bytes must NOT appear inside
        // the CBOR section.
        let raw = crate::codec::cbor::f32_slice_to_le_bytes(&vector);
        assert!(
            !cbor_a.windows(raw.len()).any(|w| w == raw.as_slice()),
            "CBOR section must not contain the raw f32 vector bytes"
        );

        let body = RequestBody::EncodeVectorDirect(req);
        let bytes = body.encode();

        assert_eq!(
            bytes.len(),
            cbor_a.len() + 4 * vector.len(),
            "payload == CBOR prefix + 4 bytes per float (single trailer)"
        );

        let decoded = RequestBody::decode(Opcode::EncodeVectorDirectReq, &bytes)
            .expect("decode EncodeVectorDirect");
        match decoded {
            RequestBody::EncodeVectorDirect(r) => assert_eq!(r.vector, vector),
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
