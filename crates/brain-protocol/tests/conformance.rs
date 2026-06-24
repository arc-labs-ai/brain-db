//! Wire-protocol conformance corpus.
//!
//! This is the reference oracle for third-party client authors and the
//! acceptance gate for the wire format. It pins the on-the-wire byte layout
//! of a representative payload per opcode family to committed golden files.
//!
//! How it works:
//!
//! - Each case constructs a fixed `RequestBody`, `ResponseBody`, or `Frame`
//!   value (no clock, no randomness — fixed byte patterns only), encodes it,
//!   and compares the bytes against a committed golden file.
//! - Run with `BRAIN_CONFORMANCE_BLESS=1` to (re)generate the corpus. Without
//!   the env var (the CI default) a missing or mismatched fixture FAILS.
//! - For every case we also decode the golden bytes and assert they round-trip
//!   back to the original value.
//!
//! Fixture layout under `tests/conformance/corpus/`:
//!
//! - `<name>.bin`  — the exact wire bytes (the contract).
//! - `<name>.json` — a `serde_json` mirror of the payload struct so an
//!   implementer can read the expected field-map without a CBOR decoder.
//! - `index.json`  — manifest: every case's name, opcode (hex), kind, length.
//!
//! `RequestBody` / `ResponseBody` do not implement `Serialize` (they dispatch
//! to CBOR via `encode()`), so the JSON mirror is produced from the inner
//! payload struct, which does derive `Serialize`.
//!
//! Determinism: ciborium serializes a fixed value reproducibly. Each case is
//! built and encoded twice and the bytes are compared, so any nondeterministic
//! field ordering surfaces as a failure rather than being papered over.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use brain_protocol::connection::handshake::{
    AgentPermissions, AuthCredentials, AuthMethod, AuthOkPayload, AuthPayload, HelloCapabilities,
    HelloPayload, ServerFeatures, WelcomePayload,
};
use brain_protocol::envelope::error::{ErrorDetails, ErrorResponse};
use brain_protocol::envelope::response::{ErrorCategoryWire, ErrorCodeWire};
use brain_protocol::error::{ErrorCategory, ErrorCode};
use brain_protocol::ops::capabilities::{Capabilities, GetCapabilitiesResponse};
use brain_protocol::{
    AnswerKindWire, EdgeKindWire, EncodeRequest, EncodeResponse, EncodeVectorDirectRequest,
    EntityCreateRequest, EntityCreateResponse, EntityGetResponse, EntityListItem,
    EntityListResponseFrame, EntityResolveResponse, EntityView, EventType, EvidenceRefWire,
    ForgetMode, ForgetRequest, ForgetResponse, Frame, InferenceKind, InferenceStep, LinkResponse,
    MaterializeProceduralRequest, MaterializeProceduralResponse, MemoryKindWire, Opcode,
    PlanResponseFrame, PlanStatus, PlanStep, PongResponse, QueryRequest, QueryResponse,
    ReasonResponseFrame, ReasonStatus, RecallRequest, RecallResponseFrame,
    RelationCreateRequest, RelationCreateResponse, RelationListFromResponseFrame, RelationView,
    RequestBody, ResolutionOutcomeWire, ResponseBody, RetrieverSelectionWire, SchemaUploadRequest,
    SchemaUploadResponse, ServerPingResponse, StageKind, StatementCreateRequest,
    StatementCreateResponse, StatementGetResponse, StatementKindWire, StatementListResponseFrame,
    StatementObjectWire, StatementValueWire, StatementView, SubscriptionEvent, TransitionKind,
    TxnAbortResponse, TxnBeginResponse, TxnCommitResponse,
};

// Fixed byte patterns. No clock, no randomness — fixtures are reproducible.
const RID: [u8; 16] = [0x11; 16];
const AGENT: [u8; 16] = [0x22; 16];
const FP: [u8; 16] = [0x33; 16];
const EID: [u8; 16] = [0x44; 16];
const SID: [u8; 16] = [0x55; 16];

/// Equivalent of a packed `MemoryId` with fixed shard / slot / version.
fn mid() -> u128 {
    ((7u128) << 72) | ((42u128) << 56) | 0x12_3456_u128
}

// ---------------------------------------------------------------------------
// Case model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Request,
    Response,
    Frame,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Request => "request",
            Kind::Response => "response",
            Kind::Frame => "frame",
        }
    }
}

/// Decodes the golden bytes and returns `Err(reason)` on any mismatch.
type RoundTripFn = Box<dyn Fn(&[u8]) -> Result<(), String>>;
/// Rebuilds and re-encodes the case value from scratch (determinism check).
type ReencodeFn = Box<dyn Fn() -> Vec<u8>>;

struct Case {
    name: &'static str,
    opcode: Opcode,
    kind: Kind,
    bytes: Vec<u8>,
    json: String,
    /// Decode the golden bytes and assert equality with the source value.
    roundtrip: RoundTripFn,
    /// Rebuild and re-encode from scratch; used by the determinism self-check.
    reencode: ReencodeFn,
}

/// Build a request case. `json` is a serialized mirror of the inner payload
/// struct (since `RequestBody` itself is not `Serialize`).
fn req_case<P: Serialize>(name: &'static str, body: RequestBody, payload_mirror: &P) -> Case {
    let opcode = body.opcode();
    let bytes = body.encode();
    let json = json_of(payload_mirror);
    let expected = body.clone();
    let reenc = body.clone();
    Case {
        name,
        opcode,
        kind: Kind::Request,
        bytes,
        json,
        roundtrip: Box::new(move |golden| {
            let got =
                RequestBody::decode(opcode, golden).map_err(|e| format!("decode failed: {e}"))?;
            if got == expected {
                Ok(())
            } else {
                Err(format!("round-trip mismatch: {got:?} != {expected:?}"))
            }
        }),
        reencode: Box::new(move || reenc.encode()),
    }
}

/// Build a response case. `json` is a serialized mirror of the inner payload.
fn resp_case<P: Serialize>(name: &'static str, body: ResponseBody, payload_mirror: &P) -> Case {
    let opcode = body.opcode();
    let bytes = body.encode();
    let json = json_of(payload_mirror);
    let expected = body.clone();
    let reenc = body.clone();
    Case {
        name,
        opcode,
        kind: Kind::Response,
        bytes,
        json,
        roundtrip: Box::new(move |golden| {
            let got =
                ResponseBody::decode(opcode, golden).map_err(|e| format!("decode failed: {e}"))?;
            if got == expected {
                Ok(())
            } else {
                Err(format!("round-trip mismatch: {got:?} != {expected:?}"))
            }
        }),
        reencode: Box::new(move || reenc.encode()),
    }
}

/// Full-frame case. `payload` is the already-encoded body bytes (for
/// `EncodeVectorDirect` this already includes the trailing f32 section, since
/// `RequestBody::encode` appends it). Round-trips via `Frame::decode`.
fn frame_case(
    name: &'static str,
    opcode: Opcode,
    flags: u8,
    stream_id: u32,
    payload: Vec<u8>,
) -> Case {
    let frame = Frame::new(opcode.as_u16(), flags, stream_id, payload.clone());
    let bytes = frame.encode();
    let json = json_of(&FrameMirror {
        opcode_hex: format!("0x{:04X}", opcode.as_u16()),
        flags,
        stream_id,
        payload_len: payload.len(),
        payload_hex: hex(&payload),
    });
    let expected = frame.clone();
    let reenc_opcode = opcode.as_u16();
    let reenc_payload = payload.clone();
    Case {
        name,
        opcode,
        kind: Kind::Frame,
        bytes,
        json,
        roundtrip: Box::new(move |golden| {
            let (got, rest) = Frame::decode(golden).map_err(|e| format!("decode failed: {e}"))?;
            if !rest.is_empty() {
                return Err(format!("decoder left {} trailing bytes", rest.len()));
            }
            if got == expected {
                Ok(())
            } else {
                Err(format!("round-trip mismatch: {got:?} != {expected:?}"))
            }
        }),
        reencode: Box::new(move || {
            Frame::new(reenc_opcode, flags, stream_id, reenc_payload.clone()).encode()
        }),
    }
}

#[derive(Serialize)]
struct FrameMirror {
    opcode_hex: String,
    flags: u8,
    stream_id: u32,
    payload_len: usize,
    payload_hex: String,
}

fn json_of<T: Serialize>(v: &T) -> String {
    serde_json::to_string_pretty(v).expect("value must serialize to JSON")
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// Sample value builders (fixed; mirror the in-crate unit tests).
// ---------------------------------------------------------------------------

fn sample_hello() -> HelloPayload {
    HelloPayload {
        client_id: "brain-conformance/1".into(),
        supported_versions: vec![brain_protocol::VERSION],
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        client_session_token: None,
    }
}

fn sample_welcome() -> WelcomePayload {
    WelcomePayload {
        server_id: "brain-server/conformance".into(),
        chosen_version: 1,
        session_id: SID,
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        server_features: ServerFeatures {
            max_payload_size: 16 * 1024 * 1024 - 1,
            max_concurrent_streams: 1024,
            idle_timeout_seconds: 300,
            auth_methods: vec![AuthMethod::Token, AuthMethod::None],
        },
    }
}

fn sample_encode() -> EncodeRequest {
    EncodeRequest {
        text: "the sky is blue".into(),
        context_id: 1,
        request_id: RID,
        txn_id: None,
        occurred_at_unix_nanos: Some(1_700_000_000_000_000_000),
    }
}

fn sample_encode_vector_direct() -> EncodeVectorDirectRequest {
    EncodeVectorDirectRequest {
        text: "precomputed".into(),
        vector: vec![1.0, 0.5, -0.25, 0.125],
        model_fingerprint: FP,
        context_id: 1,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.25,
        edges: Vec::new(),
        request_id: RID,
        txn_id: None,
        deduplicate: false,
    }
}

fn sample_encode_response() -> EncodeResponse {
    EncodeResponse {
        memory_id: mid(),
        was_deduplicated: false,
        salience: 0.5,
        auto_edges_added: 1,
        lsn: 42,
        agent_id: AGENT,
        context_id: 1,
        kind: MemoryKindWire::Episodic,
        created_at_unix_nanos: 1_700_000_000_000_000_000,
        edges_out_count: 1,
        embedding_model_fp: FP,
        pending_stages: vec![StageKind::AutoEdge],
        has_active_schema: true,
    }
}

fn sample_statement_create() -> StatementCreateRequest {
    StatementCreateRequest {
        kind: StatementKindWire::Fact,
        subject: EID,
        predicate: "org:works_on".into(),
        object: StatementObjectWire::Value(StatementValueWire::Text("brain".into())),
        confidence: 0.9,
        evidence: EvidenceRefWire::Inline(vec![mid().to_be_bytes()]),
        extractor_id: 0,
        valid_from_unix_nanos: 1_700_000_000_000_000_000,
        valid_to_unix_nanos: 0,
        event_at_unix_nanos: 0,
        schema_version: 1,
        request_id: RID,
    }
}

fn sample_relation_create() -> RelationCreateRequest {
    RelationCreateRequest {
        relation_type: "org:mentors".into(),
        from_entity: EID,
        to_entity: AGENT,
        properties_blob: Vec::new(),
        evidence: EvidenceRefWire::Inline(vec![mid().to_be_bytes()]),
        extractor_id: 0,
        confidence: 0.9,
        valid_from_unix_nanos: 1_700_000_000_000_000_000,
        valid_to_unix_nanos: 0,
        request_id: RID,
    }
}

fn sample_query() -> QueryRequest {
    QueryRequest {
        text: "who works on brain".into(),
        entity_anchor: Some(EID),
        kind_filter: vec![0],
        predicate_filter: vec!["org:works_on".into()],
        time_filter: None,
        as_of_record_time_unix_nanos: Some(1_710_000_000_000_000_000),
        confidence_min: Some(0.25),
        include_tombstoned: false,
        include_superseded: false,
        limit: 25,
        retrievers: RetrieverSelectionWire::Auto,
        fusion_config: None,
        request_id: RID,
    }
}

fn sample_entity_view() -> EntityView {
    EntityView {
        entity_id: EID,
        entity_type_id: 1,
        canonical_name: "Ada".into(),
        normalized_name: "ada".into(),
        aliases: vec!["Ada L.".into()],
        attributes_blob: b"role=engineer".to_vec(),
        mention_count: 3,
        created_at_unix_nanos: 1_700_000_000_000_000_000,
        updated_at_unix_nanos: 1_700_000_001_000_000_000,
        merged_into: [0u8; 16],
        embedding_version: 1,
        flags: 0,
    }
}

fn sample_entity_get() -> EntityGetResponse {
    EntityGetResponse {
        entity: sample_entity_view(),
    }
}

fn sample_entity_list() -> EntityListResponseFrame {
    EntityListResponseFrame {
        items: vec![EntityListItem {
            entity: sample_entity_view(),
        }],
        next_cursor: Vec::new(),
        cumulative_count: 1,
        is_final: true,
    }
}

fn sample_entity_resolve() -> EntityResolveResponse {
    EntityResolveResponse {
        outcome: ResolutionOutcomeWire::Resolved,
        tier: 2,
        confidence: 0.95,
        resolved_entity: EID,
        candidate_ids: Vec::new(),
        audit_id: [0u8; 16],
    }
}

fn sample_statement_view() -> StatementView {
    StatementView {
        statement_id: RID,
        kind: StatementKindWire::Fact,
        subject: EID,
        subject_pending_audit_id: [0u8; 16],
        predicate: "org:works_on".into(),
        object: StatementObjectWire::Value(StatementValueWire::Text("brain".into())),
        confidence: 0.9,
        evidence: EvidenceRefWire::Inline(vec![mid().to_be_bytes()]),
        extractor_id: 0,
        extracted_at_unix_nanos: 1_700_000_000_000_000_000,
        schema_version: 1,
        valid_from_unix_nanos: 1_700_000_000_000_000_000,
        valid_to_unix_nanos: 0,
        event_at_unix_nanos: 0,
        version: 1,
        superseded_by: [0u8; 16],
        supersedes: [0u8; 16],
        chain_root: RID,
        tombstoned: false,
        tombstoned_at_unix_nanos: 0,
        tombstone_reason: 0,
        flags: 0,
        is_stateful: false,
    }
}

fn sample_statement_get() -> StatementGetResponse {
    StatementGetResponse {
        statement: sample_statement_view(),
        returned_via_supersession: false,
    }
}

fn sample_statement_list() -> StatementListResponseFrame {
    StatementListResponseFrame {
        items: vec![sample_statement_view()],
        next_cursor: Vec::new(),
        cumulative_count: 1,
        is_final: true,
    }
}

fn sample_relation_view() -> RelationView {
    RelationView {
        relation_id: RID,
        chain_root: RID,
        relation_type: "org:mentors".into(),
        from_entity: EID,
        to_entity: AGENT,
        properties_blob: Vec::new(),
        evidence: EvidenceRefWire::Inline(vec![mid().to_be_bytes()]),
        extractor_id: 0,
        extracted_at_unix_nanos: 1_700_000_000_000_000_000,
        confidence: 0.9,
        valid_from_unix_nanos: 1_700_000_000_000_000_000,
        valid_to_unix_nanos: 0,
        version: 1,
        superseded_by: [0u8; 16],
        supersedes: [0u8; 16],
        tombstoned: false,
        tombstoned_at_unix_nanos: 0,
        flags: 0,
    }
}

fn sample_relation_list() -> RelationListFromResponseFrame {
    RelationListFromResponseFrame {
        items: vec![sample_relation_view()],
        next_cursor: Vec::new(),
        cumulative_count: 1,
        is_final: true,
    }
}

fn sample_plan() -> PlanResponseFrame {
    PlanResponseFrame {
        steps: vec![PlanStep {
            step_index: 0,
            memory_id: mid(),
            text: "first step".into(),
            transition_kind: TransitionKind::Causal,
            confidence: 0.8,
            estimated_distance_to_goal: 0.5,
        }],
        is_final: true,
        plan_status: Some(PlanStatus::GoalReached),
    }
}

fn sample_reason() -> ReasonResponseFrame {
    ReasonResponseFrame {
        inferences: vec![InferenceStep {
            step_index: 0,
            claim: "the sky is blue".into(),
            supporting_memories: vec![mid()],
            contradicting_memories: Vec::new(),
            confidence: 0.85,
            inference_kind: InferenceKind::EvidenceAccumulation,
        }],
        is_final: true,
        reason_status: Some(ReasonStatus::Complete),
    }
}

fn sample_link() -> LinkResponse {
    LinkResponse {
        source: mid(),
        target: mid(),
        kind: EdgeKindWire::Caused,
        weight: 0.9,
        created_at_unix_nanos: 1_700_000_000_000_000_000,
        already_existed: false,
    }
}

fn sample_get_capabilities() -> GetCapabilitiesResponse {
    GetCapabilitiesResponse {
        capabilities: Capabilities {
            rerank: true,
            llm_extractor: false,
            classifier_extractor: true,
            pattern_extractor: true,
            schema_namespaces: vec!["org".into()],
            vector_dim: 384,
        },
    }
}

fn sample_subscribe_event() -> SubscriptionEvent {
    SubscriptionEvent {
        event_type: EventType::Encoded,
        memory_id: mid(),
        context_id: 1,
        text: "the sky is blue".into(),
        kind: MemoryKindWire::Episodic,
        salience: 0.5,
        timestamp_unix_nanos: 1_700_000_000_000_000_000,
        lsn: 42,
        graph_payload: None,
        edge_payload: None,
        stage_kind: None,
        stage_outcome: None,
        stage_payload: None,
    }
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

fn corpus() -> Vec<Case> {
    let mut cases = Vec::new();

    // ---- Handshake requests ----
    cases.push(req_case(
        "req_hello",
        RequestBody::Hello(sample_hello()),
        &sample_hello(),
    ));
    let auth = AuthPayload {
        method: AuthMethod::Token,
        agent_id: AGENT,
        credentials: AuthCredentials::Token(b"opaque-token".to_vec()),
    };
    cases.push(req_case("req_auth", RequestBody::Auth(auth.clone()), &auth));

    // ---- Memory substrate requests ----
    cases.push(req_case(
        "req_encode",
        RequestBody::Encode(sample_encode()),
        &sample_encode(),
    ));
    // EncodeVectorDirect's JSON mirror carries the vector field; its wire
    // payload is CBOR (without vector) + a trailing LE-f32 section appended by
    // RequestBody::encode. The mirror documents the full logical value.
    cases.push(req_case(
        "req_encode_vector_direct",
        RequestBody::EncodeVectorDirect(sample_encode_vector_direct()),
        &sample_encode_vector_direct(),
    ));
    let recall = RecallRequest {
        cue_text: "what color is the sky".into(),
        subject_name: "sky".into(),
        max_results: 10,
        confidence_threshold: 0.3,
        context_filter: Some(vec![1]),
        age_bound_unix_nanos: None,
        as_of_record_time_unix_nanos: Some(1_710_000_000_000_000_000),
        kind_filter: Some(vec![MemoryKindWire::Episodic]),
        salience_floor: 0.1,
        include_edges: true,
        include_graph: false,
        include_text: true,
        request_id: Some(RID),
        txn_id: None,
        agent_filter: Vec::new(),
        include_other_agents: false,
    };
    cases.push(req_case(
        "req_recall",
        RequestBody::Recall(recall.clone()),
        &recall,
    ));
    let forget = ForgetRequest {
        memory_id: mid(),
        mode: ForgetMode::Soft,
        request_id: RID,
        txn_id: None,
    };
    cases.push(req_case("req_forget", RequestBody::Forget(forget), &forget));

    // ---- Typed-graph requests ----
    let entity_create = EntityCreateRequest {
        entity_type_id: 1,
        canonical_name: "Ada".into(),
        aliases: vec!["Ada L.".into()],
        attributes_blob: b"role=engineer".to_vec(),
        request_id: RID,
    };
    cases.push(req_case(
        "req_entity_create",
        RequestBody::EntityCreate(entity_create.clone()),
        &entity_create,
    ));
    cases.push(req_case(
        "req_statement_create",
        RequestBody::StatementCreate(sample_statement_create()),
        &sample_statement_create(),
    ));
    cases.push(req_case(
        "req_relation_create",
        RequestBody::RelationCreate(sample_relation_create()),
        &sample_relation_create(),
    ));
    let schema_upload = SchemaUploadRequest {
        schema_document: "namespace org\ndefine entity_type Person { attributes {} }\n".into(),
        dry_run: false,
        allow_breaking: false,
        request_id: RID,
    };
    cases.push(req_case(
        "req_schema_upload",
        RequestBody::SchemaUpload(schema_upload.clone()),
        &schema_upload,
    ));
    cases.push(req_case(
        "req_query",
        RequestBody::Query(sample_query()),
        &sample_query(),
    ));
    let materialize = MaterializeProceduralRequest {
        agent_id: AGENT,
        context_filter: 0,
        top_k: 20,
        min_confidence: 0.5,
        categories: vec!["tone".into()],
        request_id: RID,
    };
    cases.push(req_case(
        "req_materialize_procedural",
        RequestBody::MaterializeProcedural(materialize.clone()),
        &materialize,
    ));

    // ---- Handshake responses ----
    cases.push(resp_case(
        "resp_welcome",
        ResponseBody::Welcome(sample_welcome()),
        &sample_welcome(),
    ));
    let auth_ok = AuthOkPayload {
        agent_id: AGENT,
        bound_shard_id: 5,
        permissions: AgentPermissions {
            can_encode: true,
            can_recall: true,
            can_plan: true,
            can_reason: true,
            can_forget: true,
            can_admin: false,
        },
        server_time_unix_nanos: 1_700_000_000_000_000_000,
    };
    cases.push(resp_case(
        "resp_auth_ok",
        ResponseBody::AuthOk(auth_ok),
        &auth_ok,
    ));

    // ---- Memory substrate responses ----
    cases.push(resp_case(
        "resp_encode",
        ResponseBody::Encode(sample_encode_response()),
        &sample_encode_response(),
    ));
    let recall_resp = RecallResponseFrame {
        answer_kind: AnswerKindWire::None,
        memories: Vec::new(),
        is_final: true,
        cumulative_count: 0,
        estimated_remaining: Some(0),
    };
    cases.push(resp_case(
        "resp_recall",
        ResponseBody::Recall(recall_resp.clone()),
        &recall_resp,
    ));
    let forget_resp = ForgetResponse {
        memory_id: mid(),
        was_already_forgotten: false,
        edges_removed: 2,
    };
    cases.push(resp_case(
        "resp_forget",
        ResponseBody::Forget(forget_resp),
        &forget_resp,
    ));

    // ---- Typed-graph responses ----
    let entity_create_resp = EntityCreateResponse { entity_id: EID };
    cases.push(resp_case(
        "resp_entity_create",
        ResponseBody::EntityCreate(entity_create_resp),
        &entity_create_resp,
    ));
    let statement_create_resp = StatementCreateResponse {
        statement_id: RID,
        auto_superseded: [0u8; 16],
        chain_root: RID,
    };
    cases.push(resp_case(
        "resp_statement_create",
        ResponseBody::StatementCreate(statement_create_resp),
        &statement_create_resp,
    ));
    let relation_create_resp = RelationCreateResponse { relation_id: RID };
    cases.push(resp_case(
        "resp_relation_create",
        ResponseBody::RelationCreate(relation_create_resp),
        &relation_create_resp,
    ));
    let schema_upload_resp = SchemaUploadResponse {
        namespace: "org".into(),
        schema_version: 1,
        validation_errors: Vec::new(),
        backward_compatible: true,
        migration_summary_blob: Vec::new(),
    };
    cases.push(resp_case(
        "resp_schema_upload",
        ResponseBody::SchemaUpload(schema_upload_resp.clone()),
        &schema_upload_resp,
    ));
    let query_resp = QueryResponse {
        items: Vec::new(),
        total_latency_ms: 12.5,
        retriever_outcomes: Vec::new(),
    };
    cases.push(resp_case(
        "resp_query",
        ResponseBody::Query(query_resp.clone()),
        &query_resp,
    ));
    let materialize_resp = MaterializeProceduralResponse {
        system_block: "## Behaviors\n- step 1".into(),
        statement_ids: vec![RID],
        total_candidates: 1,
        trimmed_by_budget: false,
    };
    cases.push(resp_case(
        "resp_materialize_procedural",
        ResponseBody::MaterializeProcedural(materialize_resp.clone()),
        &materialize_resp,
    ));

    // ---- Read-side typed-graph responses ----
    cases.push(resp_case(
        "resp_entity_get",
        ResponseBody::EntityGet(sample_entity_get()),
        &sample_entity_get(),
    ));
    cases.push(resp_case(
        "resp_entity_list",
        ResponseBody::EntityList(sample_entity_list()),
        &sample_entity_list(),
    ));
    cases.push(resp_case(
        "resp_entity_resolve",
        ResponseBody::EntityResolve(sample_entity_resolve()),
        &sample_entity_resolve(),
    ));
    cases.push(resp_case(
        "resp_statement_get",
        ResponseBody::StatementGet(sample_statement_get()),
        &sample_statement_get(),
    ));
    cases.push(resp_case(
        "resp_statement_list",
        ResponseBody::StatementList(sample_statement_list()),
        &sample_statement_list(),
    ));
    cases.push(resp_case(
        "resp_relation_list",
        ResponseBody::RelationListFrom(sample_relation_list()),
        &sample_relation_list(),
    ));

    // ---- Cognitive read-side responses ----
    cases.push(resp_case(
        "resp_plan",
        ResponseBody::Plan(sample_plan()),
        &sample_plan(),
    ));
    cases.push(resp_case(
        "resp_reason",
        ResponseBody::Reason(sample_reason()),
        &sample_reason(),
    ));
    cases.push(resp_case(
        "resp_link",
        ResponseBody::Link(sample_link()),
        &sample_link(),
    ));

    // ---- Transaction responses ----
    let txn_begin_resp = TxnBeginResponse {
        txn_id: RID,
        timeout_seconds: 30,
        started_at_unix_nanos: 1_700_000_000_000_000_000,
    };
    cases.push(resp_case(
        "resp_txn_begin",
        ResponseBody::TxnBegin(txn_begin_resp),
        &txn_begin_resp,
    ));
    let txn_commit_resp = TxnCommitResponse {
        txn_id: RID,
        committed_at_unix_nanos: 1_700_000_001_000_000_000,
        operations_applied: 3,
    };
    cases.push(resp_case(
        "resp_txn_commit",
        ResponseBody::TxnCommit(txn_commit_resp),
        &txn_commit_resp,
    ));
    let txn_abort_resp = TxnAbortResponse {
        txn_id: RID,
        operations_discarded: 2,
    };
    cases.push(resp_case(
        "resp_txn_abort",
        ResponseBody::TxnAbort(txn_abort_resp),
        &txn_abort_resp,
    ));

    // ---- Capabilities + subscription event ----
    cases.push(resp_case(
        "resp_get_capabilities",
        ResponseBody::GetCapabilities(sample_get_capabilities()),
        &sample_get_capabilities(),
    ));
    cases.push(resp_case(
        "resp_subscribe_event",
        ResponseBody::SubscribeEvent(sample_subscribe_event()),
        &sample_subscribe_event(),
    ));

    // ---- Keepalive responses ----
    let pong_resp = PongResponse {
        client_timestamp_unix_nanos: 1_700_000_000_000_000_000,
        server_timestamp_unix_nanos: 1_700_000_000_500_000_000,
    };
    cases.push(resp_case(
        "resp_pong",
        ResponseBody::Pong(pong_resp),
        &pong_resp,
    ));
    let server_ping_resp = ServerPingResponse {
        server_timestamp_unix_nanos: 1_700_000_000_000_000_000,
    };
    cases.push(resp_case(
        "resp_server_ping",
        ResponseBody::ServerPing(server_ping_resp),
        &server_ping_resp,
    ));

    // ---- ERROR responses: cover every category ----
    for (name, code, category) in [
        (
            "resp_error_protocol",
            ErrorCode::BadMagic,
            ErrorCategory::Protocol,
        ),
        (
            "resp_error_authentication",
            ErrorCode::Unauthenticated,
            ErrorCategory::Authentication,
        ),
        (
            "resp_error_authorization",
            ErrorCode::PermissionDenied,
            ErrorCategory::Authorization,
        ),
        (
            "resp_error_validation",
            ErrorCode::InvalidArgument,
            ErrorCategory::Validation,
        ),
        (
            "resp_error_not_found",
            ErrorCode::MemoryNotFound,
            ErrorCategory::NotFound,
        ),
        (
            "resp_error_conflict",
            ErrorCode::IdempotencyConflict,
            ErrorCategory::Conflict,
        ),
        (
            "resp_error_resource_exhausted",
            ErrorCode::OutOfSlots,
            ErrorCategory::ResourceExhausted,
        ),
        (
            "resp_error_internal",
            ErrorCode::Internal,
            ErrorCategory::Internal,
        ),
        (
            "resp_error_unavailable",
            ErrorCode::ShardUnavailable,
            ErrorCategory::Unavailable,
        ),
    ] {
        let err = ErrorResponse {
            code: ErrorCodeWire::from(code),
            category: ErrorCategoryWire::from(category),
            message: "fixed error message".into(),
            details: Some(ErrorDetails {
                field: Some("top_k".into()),
                expected: Some("[1, 1000]".into()),
                actual: Some("5000".into()),
            }),
            retry_after_ms: None,
        };
        cases.push(resp_case(name, ResponseBody::Error(err.clone()), &err));
    }

    // ---- Full frames (header + payload), incl. the vector-trailer case ----
    cases.push(frame_case(
        "frame_hello",
        Opcode::Hello,
        0x00,
        0,
        RequestBody::Hello(sample_hello()).encode(),
    ));
    cases.push(frame_case(
        "frame_welcome",
        Opcode::Welcome,
        0x00,
        0,
        ResponseBody::Welcome(sample_welcome()).encode(),
    ));
    cases.push(frame_case(
        "frame_encode",
        Opcode::EncodeReq,
        0x00,
        2,
        RequestBody::Encode(sample_encode()).encode(),
    ));
    // ENCODE_VECTOR_DIRECT: the encoded payload is CBOR followed by a raw
    // little-endian f32 trailer (appended by RequestBody::encode). This is
    // the one case where the wire payload is NOT pure CBOR.
    cases.push(frame_case(
        "frame_encode_vector_direct",
        Opcode::EncodeVectorDirectReq,
        0x00,
        3,
        RequestBody::EncodeVectorDirect(sample_encode_vector_direct()).encode(),
    ));
    // Error frame on a per-op stream.
    let err_frame_body = ResponseBody::Error(ErrorResponse {
        code: ErrorCodeWire::from(ErrorCode::MemoryNotFound),
        category: ErrorCategoryWire::from(ErrorCategory::NotFound),
        message: "fixed error message".into(),
        details: None,
        retry_after_ms: None,
    });
    cases.push(frame_case(
        "frame_error",
        Opcode::Error,
        0x00,
        2,
        err_frame_body.encode(),
    ));
    // Final streaming RECALL_RESP frame (EOS flag set in the header).
    cases.push(frame_case(
        "frame_recall_eos",
        Opcode::RecallResp,
        0x80,
        2,
        ResponseBody::Recall(RecallResponseFrame {
            answer_kind: AnswerKindWire::None,
            memories: Vec::new(),
            is_final: true,
            cumulative_count: 0,
            estimated_remaining: Some(0),
        })
        .encode(),
    ));

    cases
}

// ---------------------------------------------------------------------------
// Coverage drift guard
// ---------------------------------------------------------------------------

/// Opcode families and the representative member exercised by the corpus.
///
/// Brain's wire surface has many opcodes; the corpus pins one representative
/// per family plus every error category and the vector-trailer special case.
/// New opcode FAMILIES MUST be added here and given a case above. Individual
/// opcodes within an already-covered family are checked structurally by the
/// in-crate `RequestBody` / `ResponseBody` round-trip unit tests.
fn required_families() -> Vec<(&'static str, Opcode)> {
    vec![
        ("handshake.hello", Opcode::Hello),
        ("handshake.welcome", Opcode::Welcome),
        ("handshake.auth", Opcode::Auth),
        ("handshake.auth_ok", Opcode::AuthOk),
        ("memory.encode", Opcode::EncodeReq),
        ("memory.encode_resp", Opcode::EncodeResp),
        ("memory.encode_vector_direct", Opcode::EncodeVectorDirectReq),
        ("memory.recall", Opcode::RecallReq),
        ("memory.recall_resp", Opcode::RecallResp),
        ("memory.forget", Opcode::ForgetReq),
        ("memory.forget_resp", Opcode::ForgetResp),
        ("graph.entity_create", Opcode::EntityCreateReq),
        ("graph.entity_create_resp", Opcode::EntityCreateResp),
        ("graph.statement_create", Opcode::StatementCreateReq),
        ("graph.statement_create_resp", Opcode::StatementCreateResp),
        ("graph.relation_create", Opcode::RelationCreateReq),
        ("graph.relation_create_resp", Opcode::RelationCreateResp),
        ("schema.upload", Opcode::SchemaUploadReq),
        ("schema.upload_resp", Opcode::SchemaUploadResp),
        ("query.query", Opcode::QueryReq),
        ("query.query_resp", Opcode::QueryResp),
        ("procedural.materialize", Opcode::MaterializeProceduralReq),
        (
            "procedural.materialize_resp",
            Opcode::MaterializeProceduralResp,
        ),
        ("graph.entity_get_resp", Opcode::EntityGetResp),
        ("graph.entity_list_resp", Opcode::EntityListResp),
        ("graph.entity_resolve_resp", Opcode::EntityResolveResp),
        ("graph.statement_get_resp", Opcode::StatementGetResp),
        ("graph.statement_list_resp", Opcode::StatementListResp),
        (
            "graph.relation_list_from_resp",
            Opcode::RelationListFromResp,
        ),
        ("cognitive.plan_resp", Opcode::PlanResp),
        ("cognitive.reason_resp", Opcode::ReasonResp),
        ("cognitive.link_resp", Opcode::LinkResp),
        ("txn.begin_resp", Opcode::TxnBeginResp),
        ("txn.commit_resp", Opcode::TxnCommitResp),
        ("txn.abort_resp", Opcode::TxnAbortResp),
        ("capabilities.get_resp", Opcode::GetCapabilitiesResp),
        ("subscribe.event", Opcode::SubscribeEvent),
        ("keepalive.pong", Opcode::Pong),
        ("keepalive.server_ping", Opcode::ServerPing),
        ("error", Opcode::Error),
    ]
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("conformance")
        .join("corpus")
}

fn blessing() -> bool {
    std::env::var("BRAIN_CONFORMANCE_BLESS").as_deref() == Ok("1")
}

#[derive(Serialize)]
struct ManifestEntry {
    name: String,
    opcode: String,
    kind: String,
    payload_len: usize,
}

#[test]
fn wire_encoding_matches_golden_corpus_bytes() {
    let dir = corpus_dir();
    let cases = corpus();
    let bless = blessing();

    if bless {
        fs::create_dir_all(&dir).expect("create corpus dir");
    }

    let mut failures: Vec<String> = Vec::new();
    let mut manifest: Vec<ManifestEntry> = Vec::new();

    for case in &cases {
        let bin_path = dir.join(format!("{}.bin", case.name));
        let json_path = dir.join(format!("{}.json", case.name));

        if bless {
            fs::write(&bin_path, &case.bytes).expect("write golden .bin");
            fs::write(&json_path, format!("{}\n", case.json)).expect("write golden .json");
        } else {
            match fs::read(&bin_path) {
                Ok(golden) => {
                    if golden != case.bytes {
                        failures.push(format!(
                            "{}: encoded bytes ({}) != golden ({}). Re-bless if intentional.",
                            case.name,
                            case.bytes.len(),
                            golden.len()
                        ));
                    }
                    if let Err(reason) = (case.roundtrip)(&golden) {
                        failures.push(format!("{}: {reason}", case.name));
                    }
                }
                Err(_) => failures.push(format!(
                    "{}: missing golden fixture {}. \
                     Run with BRAIN_CONFORMANCE_BLESS=1 to generate.",
                    case.name,
                    bin_path.display()
                )),
            }
        }

        manifest.push(ManifestEntry {
            name: case.name.to_string(),
            opcode: format!("0x{:04X}", case.opcode.as_u16()),
            kind: case.kind.as_str().to_string(),
            payload_len: case.bytes.len(),
        });
    }

    // Determinism self-check: rebuilding and re-encoding must be byte-identical.
    for case in &cases {
        let again = (case.reencode)();
        if again != case.bytes {
            failures.push(format!(
                "{}: nondeterministic encoding (re-encode produced different bytes)",
                case.name
            ));
        }
    }

    // Coverage drift guard: every required family must have at least one case
    // for its opcode.
    let mut covered = std::collections::BTreeSet::new();
    for case in &cases {
        covered.insert(case.opcode.as_u16());
    }
    for (family, op) in required_families() {
        if !covered.contains(&op.as_u16()) {
            failures.push(format!(
                "coverage gap: family '{family}' (0x{:04X}) has no corpus case",
                op.as_u16()
            ));
        }
    }

    if bless {
        let manifest_json = serde_json::to_string_pretty(&manifest).expect("serialize manifest");
        fs::write(dir.join("index.json"), format!("{manifest_json}\n")).expect("write index.json");
        eprintln!("blessed {} fixtures into {}", cases.len(), dir.display());
    } else {
        let index_path = dir.join("index.json");
        match fs::read_to_string(&index_path) {
            Ok(s) => {
                let expected =
                    serde_json::to_string_pretty(&manifest).expect("serialize manifest") + "\n";
                if s != expected {
                    failures.push(
                        "index.json out of date. Re-bless with BRAIN_CONFORMANCE_BLESS=1."
                            .to_string(),
                    );
                }
            }
            Err(_) => failures.push(format!(
                "missing {}. Run with BRAIN_CONFORMANCE_BLESS=1.",
                index_path.display()
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "conformance corpus failures:\n{}",
        failures.join("\n")
    );
}
