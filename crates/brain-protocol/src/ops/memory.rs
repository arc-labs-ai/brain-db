//! Cognitive-op requests: ENCODE / ENCODE_VECTOR_DIRECT / RECALL / PLAN /
//! REASON / FORGET.

use crate::envelope::request::{WireContextId, WireMemoryId, WireUuid};
use crate::shared::primitives::{
    EdgeKindWire, ForgetMode, MemoryKindWire, ObservationInput, PlanState, PlanStrategy,
};

/// Per-request effective-identity selector carried on data-plane op
/// requests. When present, the op runs as this `(namespace, agent_id)`
/// on behalf of the authenticated connection principal; when the field
/// is absent the op runs as the connection's own key-bound identity.
///
/// Honored only when the connection principal holds `can_act_as` and
/// `namespace` lies within its granted `may_act` allowlist — otherwise
/// the op is rejected with `ActAsDenied`. This is the wire form only;
/// the trust model (connection-principal-vs-effective-identity, the six
/// invariants) is enforced server-side, not by this codec.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActAs {
    /// Effective namespace. Must be within the principal's `may_act`
    /// allowlist; anything outside it is rejected.
    pub namespace: String,
    /// 16-byte effective agent id.
    #[serde(with = "serde_bytes")]
    pub agent_id: WireUuid,
}

/// `ENCODE_REQ` body. Expresses client *intent* only: the text to
/// remember, where it belongs, and when its content happened. Brain's
/// write router decides everything mechanical — the memory kind,
/// salience, whether the write deduplicates against an existing row, and
/// which edges get wired — server-side. Clients do not (and cannot)
/// dictate that machinery; they say what to remember, not how to file it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeRequest {
    pub text: String,
    pub context_id: WireContextId,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub txn_id: Option<WireUuid>,
    /// Client-supplied event time — when the memory's content actually
    /// happened, distinct from the server's write time (`created_at`).
    /// `None` when the client doesn't know it. Lets time-aware clients
    /// store the real timeline instead of cramming dates into the text.
    pub occurred_at_unix_nanos: Option<u64>,
    /// Effective identity this encode runs as, on behalf of the
    /// authenticated connection principal. `None` (the common case, and
    /// omitted on the wire) means the op runs as the connection's own
    /// key-bound identity.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
    /// How long the write blocks before replying. This is the single
    /// completion knob for writes (reads use `trace` instead — see the API
    /// convention on [`WaitMode`]).
    ///
    /// - [`WaitMode::Ack`] (the default, omitted on the wire): return as soon
    ///   as the WAL record is durable — the synchronous ack. The async
    ///   derivation stages (auto-edge / temporal-edge / extractor / HyPE) run
    ///   in the background and flow through SUBSCRIBE; the response carries
    ///   `lsn` + `pending_stages` to follow them, and `trace` is `None`.
    /// - [`WaitMode::Derived`]: block until the async derivation completes,
    ///   then return a populated `trace: EncodeTrace` — the full per-stage
    ///   timeline plus every artifact the write produced (vector, record,
    ///   keyword terms, HyPE questions, entities / statements / relations /
    ///   graph). Bounded by the shard's trace drain window so a stalled worker
    ///   can't hang the call; stragglers are marked `Timeout` and still land
    ///   in `MEMORY_INSPECT` later.
    #[serde(default, skip_serializing_if = "WaitMode::is_ack")]
    pub wait: WaitMode,
    /// Opt out of content dedup and force a distinct memory. Default `false`:
    /// Brain dedupes text ENCODE on (agent_id, context_id, BLAKE3(text)) — a
    /// repeat of byte-identical text returns the existing MemoryId
    /// (was_deduplicated = true) and writes nothing new. Set `true` when the
    /// same text is a genuinely distinct observation that must coexist (e.g. the
    /// same fact re-stated at a different occurred_at). Omitted on the wire in
    /// the default (dedup-on) case.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_duplicates: bool,
}

/// `skip_serializing_if` predicate — omit `false` from the CBOR map so the
/// default (dedup-on) encode stays byte-minimal and wire-compatible.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Write-completion mode — how long a write op blocks before it returns.
///
/// **API convention:** writes take `wait` (this enum); reads take `trace:
/// bool`. They are never both on one op. On a write, waiting and the trace
/// payload are one decision — the only reason to wait for async derivation is
/// to observe it, and the only way to observe it is to wait — so a single
/// `wait` knob controls both (its payload follows its timing). On a read there
/// is no async to wait for, so `trace` is a pure observability toggle with no
/// timing effect.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde_repr::Serialize_repr,
    serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum WaitMode {
    /// Return after the durable sync ack; async derivation runs in the
    /// background. The fast default.
    #[default]
    Ack = 0,
    /// Block until async derivation completes, then return the full trace.
    Derived = 1,
}

impl WaitMode {
    /// `true` for the default [`WaitMode::Ack`] — used to omit the field from
    /// the wire map in the common case.
    #[must_use]
    pub fn is_ack(&self) -> bool {
        matches!(self, WaitMode::Ack)
    }
}

/// Admin / bulk-import encode path — NOT a primary client verb. Brain
/// owns the embedding model; ordinary clients send text via `ENCODE` and
/// let the server embed it. This op exists for bulk or administrative
/// import from deployments running their own (often domain-specific or
/// multi-modal) embedder outside Brain: the caller supplies the embedding
/// vector itself plus the fingerprint of the model that produced it. The
/// server skips its own embed step entirely, but still runs every
/// downstream validation, dedup, slot reservation, edge wiring, and write
/// submission.
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
    /// Optional explicit subject hint for the precise grounded overlay, by
    /// name. Resolved server-side (cross-type). When empty, the server mines
    /// subjects from the cue itself. There is no read "mode": recall always
    /// runs the episodic associative path and overlays a precise grounded
    /// value when one is named exactly — this field just adds an explicit
    /// subject candidate to that overlay.
    pub subject_name: String,
    /// Safety cap on returned items (set members / episodic hits). NOT a
    /// ranking knob — the answer's shape comes from the data + kind, never
    /// from a caller count. `0` ⇒ server default.
    pub max_results: u32,
    pub confidence_threshold: f32,
    pub context_filter: Option<Vec<WireContextId>>,
    pub age_bound_unix_nanos: Option<u64>,
    /// Bi-temporal time-travel anchor. When `Some(t)`, the read is
    /// resolved against the state the substrate believed at record-time
    /// `t` (statement results are filtered to that as-of view) and `t`
    /// also becomes the reference point for recency ranking. `None` is
    /// the current-state default. Distinct from `age_bound_unix_nanos`,
    /// which is a lower-bound *event-time* cutoff.
    pub as_of_record_time_unix_nanos: Option<u64>,
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
    /// Opt-in per-stage observability. When `true`, the FINAL response
    /// frame carries a populated `trace: RecallTrace` describing each
    /// retriever lane's outcome/latency/count, the filter-chain survivor
    /// counts, the rerank outcome, and the total wall-time — the same data
    /// the read pipeline already computes internally. When `false` (the
    /// default) the pipeline discards that data as before, so the flag is
    /// zero-cost on the hot path. Distinct from the debug-only
    /// `QUERY_TRACE` op, which returns rendered text rather than structured
    /// data on the read itself.
    #[serde(default)]
    pub trace: bool,
    /// Effective identity this recall runs as, on behalf of the
    /// authenticated connection principal. `None` (the common case, and
    /// omitted on the wire) means the op runs as the connection's own
    /// key-bound identity.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
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
    /// Effective identity this plan runs as, on behalf of the
    /// authenticated connection principal. `None` (the common case, and
    /// omitted on the wire) means the op runs as the connection's own
    /// key-bound identity.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
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
    /// Effective identity this reason runs as, on behalf of the
    /// authenticated connection principal. `None` (the common case, and
    /// omitted on the wire) means the op runs as the connection's own
    /// key-bound identity.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForgetRequest {
    pub memory_id: WireMemoryId,
    pub mode: ForgetMode,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub txn_id: Option<WireUuid>,
    /// Effective identity this forget runs as, on behalf of the
    /// authenticated connection principal. `None` (the common case, and
    /// omitted on the wire) means the op runs as the connection's own
    /// key-bound identity.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

// ============================================================
// MEMORY_LIST — paginated enumeration read
// ============================================================

/// Sort axis for `MEMORY_LIST`. `Created` is the only axis backed by a
/// tenant-scoped index today; the others are declared here so the wire
/// shape is stable while their server-side index support lands.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum MemoryListSortWire {
    Created = 0,
    Salience = 1,
    Occurred = 2,
    LastAccessed = 3,
}

/// Sort direction for `MEMORY_LIST`.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum MemoryListDirWire {
    Asc = 0,
    Desc = 1,
}

/// Which time field a `from`/`to` range filters on. `Created` is the
/// write time (indexed); `Occurred` is the client-supplied event time
/// (not indexed for memories yet).
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum MemoryListTimeAxisWire {
    Created = 0,
    Occurred = 1,
}

/// `MEMORY_LIST` (0x0027) — a pure paginated enumeration of the caller's
/// `(namespace, agent)` memories. This is not RECALL: there is no query,
/// no ranking, no relevance suppression. It walks the tenant timeline in
/// a stable order and returns a page plus an opaque keyset cursor.
///
/// The cursor is opaque and signed: it encodes the sort, direction, the
/// last key seen, and a signature over the active filters. Echoing a
/// cursor back after changing any filter or the sort is rejected
/// (`stale_cursor`), because the resumed page would otherwise belong to a
/// different result set.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryListRequest {
    pub sort: MemoryListSortWire,
    pub dir: MemoryListDirWire,
    /// Page size, validated server-side to `1..=100`.
    pub limit: u32,
    /// Empty on the first page; otherwise the opaque `next_cursor` from a
    /// previous response.
    pub cursor: Vec<u8>,
    /// Empty = all kinds; otherwise only these memory kinds are returned.
    pub kinds: Vec<MemoryKindWire>,
    /// When false (the default), tombstoned memories are excluded; when
    /// true, both active and tombstoned rows are enumerated.
    pub include_tombstoned: bool,
    /// Which time field the `from`/`to` bounds apply to.
    pub time_axis: MemoryListTimeAxisWire,
    /// Inclusive lower time bound in unix-nanos; `0` = no lower bound.
    pub from_unix_nanos: u64,
    /// Inclusive upper time bound in unix-nanos; `0` = no upper bound.
    pub to_unix_nanos: u64,
    /// Inclusive salience floor in `[0, 1]`.
    pub salience_min: f32,
    /// Inclusive salience ceiling in `[0, 1]`.
    pub salience_max: f32,
    /// Substring/token filter over memory text; empty = no filter.
    pub text_contains: String,
    /// Effective identity this list runs as, on behalf of the
    /// authenticated connection principal. `None` (the common case, and
    /// omitted on the wire) means the op runs as the connection's own
    /// key-bound identity. The list is scoped to the effective
    /// `(namespace, agent)`, so it enumerates only that tenant's memories.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

/// One memory in a `MEMORY_LIST` response batch. Carries the
/// enumeration-relevant fields plus relationship-handle counts so a UI
/// row can show link counts without a second call.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryListItem {
    #[serde(with = "serde_bytes")]
    pub memory_id: [u8; 16],
    pub text: String,
    /// Raw memory-kind byte (0 = Episodic, 1 = Semantic, 2 = Consolidated).
    pub kind: u8,
    /// Lifecycle state byte (0 = active, 1 = tombstoned).
    pub state: u8,
    pub created_at_unix_nanos: u64,
    /// Client-supplied event time; `0` when the memory has none.
    pub occurred_at_unix_nanos: u64,
    pub last_accessed_at_unix_nanos: u64,
    /// Point-in-time salience — it decays, so callers must treat it as a
    /// snapshot, not a stored constant.
    pub salience: f32,
    pub access_count: u32,
    #[serde(with = "serde_bytes")]
    pub source_request_id: [u8; 16],
    pub statement_count: u32,
    pub entity_count: u32,
    pub relation_count: u32,
}

/// Response body for `MEMORY_LIST` (`0x00A7`). One frame carries a page
/// of items; a single frame with `is_final = true` is the whole page.
/// Empty `next_cursor` means the enumeration is exhausted; a non-empty
/// `next_cursor` is the opaque token to resume from.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryListResponseFrame {
    pub items: Vec<MemoryListItem>,
    /// Empty when exhausted; otherwise the keyset token to resume with.
    pub next_cursor: Vec<u8>,
    /// Cumulative count of items emitted across this stream so far.
    pub cumulative_count: u32,
    pub is_final: bool,
}

impl MemoryListResponseFrame {
    /// True for the final tail frame. Mirrors the body-side `is_final`
    /// signal used by the other streaming list responses.
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.is_final
    }
}

/// `MEMORY_INSPECT_REQ` — fetch the durable write-artifact bundle for one
/// memory. Single-shot (not paginated): the reply carries the whole bundle.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryInspectRequest {
    #[serde(with = "serde_bytes")]
    pub memory_id: [u8; 16],
    /// Effective identity for the read, on behalf of the connection principal.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

/// `MEMORY_INSPECT_RESP` — the durable per-memory artifact bundle: what each
/// write stage produced (embedding vector, redb record, analyzed keyword terms,
/// write-time HyPE questions, the extracted knowledge graph), plus the memory
/// text. Reuses [`EncodeStageArtifact`] as the bundle shape so the live ENCODE
/// trace and the stored inspection view are one type. `found = false` (with an
/// empty `artifact`) when no memory / no bundle exists for the id.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryInspectResponse {
    pub found: bool,
    #[serde(with = "serde_bytes")]
    pub memory_id: [u8; 16],
    pub text: String,
    pub artifact: EncodeStageArtifact,
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
    /// Full synchronous write-analysis trace. Populated only when the
    /// request set `trace = true`; `None` otherwise (and omitted from the
    /// wire map so `trace = false` encodes pay nothing). The async
    /// derivation stages are ALSO available on SUBSCRIBE keyed by `lsn` +
    /// `pending_stages` — this field is the synchronous alternative for a
    /// caller (e.g. a playground) that wants the whole timeline back in
    /// one response without opening a subscribe stream.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub trace: Option<EncodeTrace>,
}

/// Full synchronous write-analysis trace for one ENCODE, surfaced on the
/// response when the request opted in with `trace = true`. Two halves:
/// `stages` is the per-phase timeline (the synchronous validate / embed /
/// reserve / persist phases plus the async auto-edge / temporal-edge /
/// extractor phases the handler synchronously waited to drain), and
/// `artifacts` is what the write actually produced (entities, statements,
/// relations, the indexes it landed in, and the dedup verdict). This is
/// the ENCODE analog of `RecallTrace`: it lets a caller render "here is
/// exactly what your write produced" without a second round-trip.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeTrace {
    /// The write timeline, in execution order: the synchronous phases
    /// first, then each async derivation stage as it completed (or a
    /// `Timeout` entry for a stage that didn't finish within the wait
    /// window).
    pub stages: Vec<EncodeTraceStage>,
    /// What the write produced — resolved from the typed-graph rows and
    /// index state after the async stages drained.
    pub artifacts: EncodeTraceArtifacts,
    /// End-to-end wall-time of the whole synchronous path (validation
    /// through the async-stage drain), in microseconds.
    pub total_latency_us: u64,
}

/// One phase in an `EncodeTrace` timeline.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeTraceStage {
    /// Phase name — one of the synchronous phases (`validate`, `embed`,
    /// `reserve`, `persist`) or an async stage (`auto_edge`,
    /// `temporal_edge`, `extractor`).
    pub name: String,
    /// Terminal status of the phase.
    pub status: EncodeTraceStageStatus,
    /// Phase wall-time in microseconds. For an async stage this is the
    /// time from write-durable to the stage's `StageCompleted` event; `0`
    /// for a stage that timed out.
    pub latency_us: u64,
    /// Human-readable detail: for the sync phases a short summary (e.g.
    /// the embedding dimension), for an async stage the produced counts /
    /// audit status, and for a `Timeout`/`Failed` stage the reason.
    pub detail: String,
    /// The concrete data this stage produced — the embedding vector (`embed`),
    /// the redb metadata row (`persist`), the write-time HyPE questions, the
    /// analyzed keyword terms (text-index stages), or the extracted knowledge
    /// graph (`extractor`). Present only when the caller set `wait = Derived`
    /// and the stage produced inspectable output; `None` otherwise (and absent
    /// from the wire map). Lets a caller show *what was built* at each step, not
    /// just its latency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<EncodeStageArtifact>,
}

/// The concrete output one ENCODE stage produced behind the scenes, for
/// `wait = Derived` callers that want to inspect *what each step built* — not
/// just its latency. This is a per-stage output bag: every field is optional and
/// each stage populates only the subset it generated (`embed` → `vector`,
/// `persist` → `record`, the HyPE step → `hype_questions`, a text-index stage →
/// `keyword_fields`, the extractor → `graph`, an edge stage → `graph.edges`).
/// Empty/absent fields are omitted from the wire map. Reused by
/// [`MemoryInspectResponse`] so the live ENCODE trace and the stored inspection
/// view are one type.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeStageArtifact {
    /// The embedding vector the `embed` stage produced (full width, e.g. 384
    /// `f32`s).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vector: Vec<f32>,
    /// The metadata row the `persist` stage committed to redb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<EncodeStageRecord>,
    /// Hypothetical questions the write-time HyPE step generated for this memory
    /// (the alternate phrasings it also embeds so recall can match a question).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hype_questions: Vec<String>,
    /// The analyzed keyword terms a text-index stage derived, per index field —
    /// the actual tokens tantivy will match on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keyword_fields: Vec<EncodeStageKeywordField>,
    /// The knowledge-graph fragment this stage produced — the extractor stage
    /// carries the full nodes + edges it derived; an edge stage
    /// (`auto_edge` / `temporal_edge`) carries just the edges it added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<EncodeStageGraph>,
}

/// The redb metadata row a `persist` stage wrote — the durable record fields.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeStageRecord {
    #[serde(with = "serde_bytes")]
    pub memory_id: [u8; 16],
    /// Memory kind discriminant (as stored).
    pub kind: u8,
    /// Salience the write assigned.
    pub salience: f32,
    /// Record (ingest) time, unix nanoseconds.
    pub created_at_unix_nanos: u64,
    /// Event time, when the caller supplied `occurred_at`; `0` otherwise.
    pub occurred_at_unix_nanos: u64,
    /// Stored embedding dimension.
    pub vector_dim: u32,
    /// Byte length of the stored memory text.
    pub text_len: u32,
    /// WAL log-sequence number the write landed at.
    pub lsn: u64,
}

/// One text-index field and the analyzed terms the write produced for it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeStageKeywordField {
    /// Index field name — e.g. `memory_text`, `statement_text`.
    pub field: String,
    /// The analyzed tokens (post-tokenizer) the field will match on.
    pub terms: Vec<String>,
}

/// A node in the knowledge graph an ENCODE produced — an entity, the memory
/// itself, or a literal object value.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeGraphNode {
    /// Stable node id: the entity id, the memory id, or a synthetic id for a
    /// literal value node.
    #[serde(with = "serde_bytes")]
    pub id: [u8; 16],
    /// Display name / value.
    pub name: String,
    /// `"entity"`, `"memory"`, or `"literal"`.
    pub kind: String,
    /// For entity nodes, the `"namespace:typename"` (empty otherwise).
    pub type_qname: String,
}

/// A directed edge in the knowledge graph an ENCODE produced.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeGraphEdge {
    /// Source node id (subject).
    #[serde(with = "serde_bytes")]
    pub source: [u8; 16],
    /// Target node id (object / relation target).
    #[serde(with = "serde_bytes")]
    pub target: [u8; 16],
    /// Predicate qname.
    pub predicate: String,
    /// `"statement"` or `"relation"`.
    pub kind: String,
    /// Extraction confidence, when applicable.
    pub confidence: f32,
}

/// The knowledge graph an ENCODE produced — nodes (entities, the memory,
/// literal values) and the directed edges (statements, relations) between them.
/// A renderable view of what the write added to the typed graph.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeStageGraph {
    pub nodes: Vec<EncodeGraphNode>,
    pub edges: Vec<EncodeGraphEdge>,
}

/// Terminal status of an `EncodeTrace` phase.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum EncodeTraceStageStatus {
    /// The phase ran and completed (produced output or ran cleanly).
    Ok = 0,
    /// The phase was not applicable / not provisioned (e.g. an index the
    /// write didn't touch, or a stage that was never queued).
    Skipped = 1,
    /// The phase errored. `detail` carries the reason.
    Failed = 2,
    /// An async stage that was queued but did not complete within the
    /// handler's bounded wait window. Its `StageCompleted` event will
    /// still arrive on SUBSCRIBE later.
    Timeout = 3,
}

/// What an ENCODE produced, resolved after the async stages drained. Empty
/// vectors are valid (the write went through extraction but produced no
/// typed-graph rows).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeTraceArtifacts {
    /// Entities the extraction pipeline identified / resolved for this
    /// memory.
    pub entities: Vec<EncodeTraceEntity>,
    /// Statements the write generated.
    pub statements: Vec<EncodeTraceStatement>,
    /// Typed relations incident to the entities this memory mentions.
    pub relations: Vec<EncodeTraceRelation>,
    /// Which indexes the memory (and its derived rows) landed in.
    pub indexes: Vec<EncodeTraceIndex>,
    /// The dedup verdict for this write.
    pub dedup: EncodeTraceDedup,
}

/// One entity artifact in an `EncodeTrace`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeTraceEntity {
    #[serde(with = "serde_bytes")]
    pub id: [u8; 16],
    pub name: String,
    /// Human-readable `"namespace:typename"` (or bare `"typename"` for the
    /// default namespace).
    pub type_qname: String,
}

/// One statement artifact in an `EncodeTrace`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeTraceStatement {
    #[serde(with = "serde_bytes")]
    pub id: [u8; 16],
    pub subject_name: String,
    pub predicate: String,
    /// Stringified object — entity canonical name for entity objects,
    /// formatted scalar for literal objects.
    pub object_name: String,
    pub confidence: f32,
}

/// One relation artifact in an `EncodeTrace`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeTraceRelation {
    pub source_name: String,
    pub predicate: String,
    pub target_name: String,
}

/// One index the write landed in.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EncodeTraceIndex {
    /// Index name — e.g. `memory_hnsw`, `memory_text`, `statement_text`.
    pub name: String,
    /// Whether the memory was inserted (`Ok`) or the index was not
    /// applicable to this write (`Skipped`).
    pub status: EncodeTraceStageStatus,
}

/// The dedup verdict carried on an `EncodeTrace`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EncodeTraceDedup {
    /// `true` when the write collapsed onto an existing memory instead of
    /// creating a new row.
    pub was_deduplicated: bool,
    /// The memory the write deduplicated against, when
    /// `was_deduplicated`; `None` for a fresh write.
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub matched_memory_id: Option<[u8; 16]>,
}

/// The shape of a RECALL answer — pure cardinality, decided by the server's
/// router from the stored data. A memory database answers with memories:
/// one, several, or none. There is NO retrieval-mechanism vocabulary here
/// (no "episodic", no "grounded") — how the router found the memories is an
/// internal concern the caller never sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AnswerKindWire {
    /// One memory is the answer. `memories` has exactly one entry.
    Single,
    /// Several memories together are the answer. `memories` has 2+ entries.
    Many,
    /// Not available — the substrate has no memory that answers the cue.
    /// `memories` is empty. Absence is explicit, never a fabricated guess.
    None,
}

/// — one streaming RECALL frame. The answer is always memories: the router
/// returns the one memory, the array of memories, or none — `answer_kind`
/// carries which. The stored value lives on each memory (text + optional
/// graph enrichment); there is no separate value channel.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecallResponseFrame {
    /// The answer cardinality the router decided.
    pub answer_kind: AnswerKindWire,
    /// The answering memories: 1 for `Single`, 2+ for `Many`, 0 for `None`.
    pub memories: Vec<MemoryResult>,
    pub is_final: bool,
    pub cumulative_count: u32,
    pub estimated_remaining: Option<u32>,
    /// Per-stage read-pipeline trace. Populated only on the FINAL frame and
    /// only when the request set `trace = true`; `None` otherwise (and
    /// omitted from the wire map so `trace = false` recalls pay nothing).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub trace: Option<RecallTrace>,
}

/// Per-stage observability for one RECALL, surfaced on the final frame when
/// the request opted in with `trace = true`. Mirrors the read pipeline's
/// internal `QueryMetadata` as structured data: one entry per retriever lane,
/// the filter-chain survivor counts, the rerank outcome, and the total
/// wall-time. Callers use it to see which lane was slow, which filter step
/// narrowed the pool most, and whether the cross-encoder re-sorted the list.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecallTrace {
    /// One entry per retriever lane the plan invoked, in plan order.
    pub retrievers: Vec<RecallTraceRetriever>,
    /// Survivor counts after each filter-chain step.
    pub filter_chain: RecallTraceFilterChain,
    /// The rerank stage's outcome. `None` when the cross-encoder isn't
    /// loaded on this shard (operator opted out of rerank), so the result
    /// is RRF-only ordered.
    pub rerank: Option<RecallTraceRerank>,
    /// End-to-end wall-time of the retrieval execution, in milliseconds.
    pub total_latency_ms: f64,
}

/// What one retriever lane did during a traced RECALL.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecallTraceRetriever {
    /// Which lane this is (semantic / lexical / graph).
    pub name: RetrieverNameWire,
    /// Terminal status of the lane.
    pub status: RecallTraceRetrieverStatus,
    /// Human-readable detail for the non-terminal statuses: the skip reason
    /// for `Skipped`, the error message for `Failure`. Empty for `Success`
    /// and `Timeout`.
    pub status_detail: String,
    /// Lane wall-time in milliseconds. `0.0` when the lane was skipped.
    pub latency_ms: f64,
    /// Raw candidate count this lane contributed before fusion.
    pub candidate_count: u32,
}

/// Terminal status of a retriever lane in a RECALL trace.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum RecallTraceRetrieverStatus {
    /// The lane ran and contributed its candidates.
    Success = 0,
    /// The lane was skipped because the request lacked its required signal
    /// (e.g. graph with no resolved anchor). `status_detail` carries why.
    Skipped = 1,
    /// The lane exceeded its per-lane timeout. Its items were still fused.
    Timeout = 2,
    /// The lane returned an error and was dropped from fusion.
    /// `status_detail` carries the message.
    Failure = 3,
}

/// Filter-chain survivor counts after each step, mirroring the pipeline's
/// internal `FilterChainStats`. Each field is the number of candidates that
/// survived that step; `before` is the pre-filter fused count.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecallTraceFilterChain {
    pub before: u32,
    pub after_type: u32,
    pub after_temporal: u32,
    pub after_confidence: u32,
    pub after_tombstone: u32,
    pub after_supersession: u32,
    pub after_as_of: u32,
    pub after_limit: u32,
}

/// Outcome of the cross-encoder rerank stage in a RECALL trace.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecallTraceRerank {
    /// `true` when the cross-encoder ran and re-sorted the fused list;
    /// `false` when it was loaded but had no candidates with fetchable text
    /// (RRF order returned unchanged).
    pub applied: bool,
    /// Number of candidates the cross-encoder scored. `0` when not applied.
    pub candidates: u32,
    /// Rerank wall-time in milliseconds. `0.0` when not applied.
    pub latency_ms: f64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryResult {
    pub memory_id: WireMemoryId,
    pub text: String,
    pub similarity_score: f32,
    pub confidence: f32,
    pub salience: f32,
    pub kind: MemoryKindWire,
    /// Agent that owns this memory row — always the calling key's own
    /// agent, since recall is isolated to the caller. Echoed for
    /// provenance / routing verification.
    #[serde(with = "serde_bytes")]
    pub agent_id: WireUuid,
    pub context_id: WireContextId,
    pub created_at_unix_nanos: u64,
    pub last_accessed_at_unix_nanos: u64,
    pub edges: Option<Vec<EdgeView>>,
    /// Retrievers that surfaced this memory. Empty when no schema is
    /// declared and inside transactions; populated when the server
    /// routes RECALL through the retrieval engine.
    pub contributing_retrievers: Vec<RetrieverNameWire>,
    /// Post-RRF fused rank score. `0.0` on no-schema deployments
    /// and inside transactions; positive when retrieval ran
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
    /// Client-supplied event time (when the memory's content happened),
    /// echoed from `EncodeRequest.occurred_at_unix_nanos`. `None` when the
    /// client didn't supply one. Distinct from `created_at` (write time).
    pub occurred_at_unix_nanos: Option<u64>,
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
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// Effective identity this link runs as, on behalf of the
    /// authenticated connection principal. `None` (the common case, and
    /// omitted on the wire) means the op runs as the connection's own
    /// key-bound identity.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

/// — `UNLINK_REQ` body. Removes an edge identified by the
/// `(source, kind, target)` triple.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UnlinkRequest {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(with = "crate::codec::cbor::opt_byte_array16")]
    pub txn_id: Option<WireUuid>,
    /// Effective identity this unlink runs as, on behalf of the
    /// authenticated connection principal. `None` (the common case, and
    /// omitted on the wire) means the op runs as the connection's own
    /// key-bound identity.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
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
mod memory_list_tests {
    use super::*;
    use crate::codec::opcode::Opcode;
    use crate::envelope::request::RequestBody;
    use crate::envelope::response::ResponseBody;

    fn sample_request() -> MemoryListRequest {
        MemoryListRequest {
            sort: MemoryListSortWire::Created,
            dir: MemoryListDirWire::Desc,
            limit: 50,
            cursor: vec![1, 2, 3, 4],
            kinds: vec![MemoryKindWire::Episodic, MemoryKindWire::Semantic],
            include_tombstoned: true,
            time_axis: MemoryListTimeAxisWire::Created,
            from_unix_nanos: 1_700_000_000_000_000_000,
            to_unix_nanos: 1_710_000_000_000_000_000,
            salience_min: 0.1,
            salience_max: 0.9,
            text_contains: String::new(),
            act_as: None,
        }
    }

    #[test]
    fn memory_list_request_round_trips() {
        let body = RequestBody::MemoryList(sample_request());
        let bytes = body.encode();
        let decoded = RequestBody::decode(Opcode::MemoryListReq, &bytes).expect("decode");
        assert_eq!(decoded, body);
    }

    #[test]
    fn memory_list_request_round_trips_with_act_as() {
        let mut req = sample_request();
        req.act_as = Some(ActAs {
            namespace: "acme".into(),
            agent_id: [7u8; 16],
        });
        req.cursor = Vec::new();
        let body = RequestBody::MemoryList(req);
        let bytes = body.encode();
        let decoded = RequestBody::decode(Opcode::MemoryListReq, &bytes).expect("decode");
        assert_eq!(decoded, body);
    }

    #[test]
    fn memory_list_response_round_trips() {
        let frame = MemoryListResponseFrame {
            items: vec![MemoryListItem {
                memory_id: [0x11; 16],
                text: "the sky is blue".into(),
                kind: 0,
                state: 0,
                created_at_unix_nanos: 1_700_000_000_000_000_000,
                occurred_at_unix_nanos: 0,
                last_accessed_at_unix_nanos: 1_700_000_001_000_000_000,
                salience: 0.5,
                access_count: 3,
                source_request_id: [0x22; 16],
                statement_count: 0,
                entity_count: 0,
                relation_count: 0,
            }],
            next_cursor: vec![9, 8, 7],
            cumulative_count: 1,
            is_final: true,
        };
        let body = ResponseBody::MemoryList(frame);
        let bytes = body.encode();
        let decoded = ResponseBody::decode(Opcode::MemoryListResp, &bytes).expect("decode");
        assert_eq!(decoded, body);
        assert_eq!(body.is_final(), Some(true));
    }

    #[test]
    fn empty_page_round_trips() {
        let frame = MemoryListResponseFrame {
            items: Vec::new(),
            next_cursor: Vec::new(),
            cumulative_count: 0,
            is_final: true,
        };
        let body = ResponseBody::MemoryList(frame);
        let bytes = body.encode();
        let decoded = ResponseBody::decode(Opcode::MemoryListResp, &bytes).expect("decode");
        assert_eq!(decoded, body);
    }
}

#[cfg(test)]
mod serde_smoke {
    use super::*;

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

    // The opt-in `trace` flag round-trips on the request, and the populated
    // `EncodeTrace` (sync + async stages, artifacts) round-trips on the
    // response. A `trace = false` request / `trace: None` response omit the
    // field from the wire map (proven by the existing `resp_encode`
    // conformance fixture being byte-identical), so this only pins the
    // opted-in shape.
    #[test]
    fn encode_trace_round_trips_request_and_response() {
        use crate::codec::opcode::Opcode;
        use crate::envelope::request::RequestBody;
        use crate::envelope::response::ResponseBody;

        let req = EncodeRequest {
            text: "trace me".into(),
            context_id: 3,
            request_id: [7u8; 16],
            txn_id: None,
            occurred_at_unix_nanos: None,
            act_as: None,
            wait: WaitMode::Derived,
            allow_duplicates: false,
        };
        let body = RequestBody::Encode(req.clone());
        let bytes = body.encode();
        let decoded = RequestBody::decode(Opcode::EncodeReq, &bytes).expect("decode");
        assert_eq!(decoded, body);
        match decoded {
            RequestBody::Encode(r) => assert_eq!(r.wait, WaitMode::Derived),
            other => panic!("wrong variant: {other:?}"),
        }

        let trace = EncodeTrace {
            stages: vec![
                EncodeTraceStage {
                    name: "embed".into(),
                    status: EncodeTraceStageStatus::Ok,
                    latency_us: 1200,
                    detail: "dim=384".into(),
                    artifact: None,
                },
                EncodeTraceStage {
                    name: "persist".into(),
                    status: EncodeTraceStageStatus::Ok,
                    latency_us: 800,
                    detail: "lsn=9".into(),
                    artifact: None,
                },
                EncodeTraceStage {
                    name: "extractor".into(),
                    status: EncodeTraceStageStatus::Timeout,
                    latency_us: 0,
                    detail: "stage did not complete within the trace wait window".into(),
                    artifact: None,
                },
            ],
            artifacts: EncodeTraceArtifacts {
                entities: vec![EncodeTraceEntity {
                    id: [1u8; 16],
                    name: "brain".into(),
                    type_qname: "org:project".into(),
                }],
                statements: vec![EncodeTraceStatement {
                    id: [2u8; 16],
                    subject_name: "niraj".into(),
                    predicate: "org:works_on".into(),
                    object_name: "brain".into(),
                    confidence: 0.9,
                }],
                relations: vec![EncodeTraceRelation {
                    source_name: "niraj".into(),
                    predicate: "org:member_of".into(),
                    target_name: "arc-labs".into(),
                }],
                indexes: vec![EncodeTraceIndex {
                    name: "memory_hnsw".into(),
                    status: EncodeTraceStageStatus::Ok,
                }],
                dedup: EncodeTraceDedup {
                    was_deduplicated: false,
                    matched_memory_id: None,
                },
            },
            total_latency_us: 44_000,
        };
        let resp = EncodeResponse {
            memory_id: 9,
            was_deduplicated: false,
            salience: 0.5,
            auto_edges_added: 0,
            lsn: 9,
            agent_id: [0u8; 16],
            context_id: 3,
            kind: MemoryKindWire::Episodic,
            created_at_unix_nanos: 1,
            edges_out_count: 0,
            embedding_model_fp: [0u8; 16],
            pending_stages: vec![StageKind::Extractor],
            has_active_schema: true,
            trace: Some(trace),
        };
        let body = ResponseBody::Encode(resp);
        let bytes = body.encode();
        let decoded = ResponseBody::decode(Opcode::EncodeResp, &bytes).expect("decode");
        assert_eq!(decoded, body);
    }
}
