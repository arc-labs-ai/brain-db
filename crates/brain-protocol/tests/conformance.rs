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
    AuthCredentials, AuthMethod, AuthOkPayload, AuthPayload, HelloCapabilities, HelloPayload,
    ServerFeatures, SpacePermissions, WelcomePayload,
};
use brain_protocol::envelope::error::{ErrorDetails, ErrorResponse};
use brain_protocol::envelope::response::{ErrorCategoryWire, ErrorCodeWire};
use brain_protocol::error::{ErrorCategory, ErrorCode};
use brain_protocol::ops::capabilities::{
    Capabilities, GetCapabilitiesRequest, GetCapabilitiesResponse,
};
use brain_protocol::ops::query::{
    FusionConfigWire, QueryExplainRequest, QueryExplainResponse, QueryRequest, QueryTraceRequest,
    QueryTraceResponse, RetrieverSelectionWire, RetrieverWire, TimeRangeWire,
};
use brain_protocol::{
    ActAs, AnswerKindWire, ByeRequest, CancelStreamAck, CancelStreamRequest, CancellationReason,
    ClientPongRequest, EdgeKindWire, EncodeGraphEdge, EncodeGraphNode, EncodeRequest,
    EncodeResponse, EncodeStageArtifact, EncodeStageGraph, EncodeStageKeywordField,
    EncodeStageRecord, EncodeTrace, EncodeTraceArtifacts, EncodeTraceDedup, EncodeTraceEntity,
    EncodeTraceIndex, EncodeTraceRelation, EncodeTraceStage, EncodeTraceStageStatus,
    EncodeTraceStatement, EncodeVectorDirectRequest, EntityCreateRequest, EntityCreateResponse,
    EntityGetRequest, EntityGetResponse, EntityListItem, EntityListRequest,
    EntityListResponseFrame, EntityMergeRequest, EntityMergeResponse, EntityRenameRequest,
    EntityRenameResponse, EntityResolveRequest, EntityResolveResponse, EntityTombstoneRequest,
    EntityTombstoneResponse, EntityUnmergeRequest, EntityUnmergeResponse, EntityUpdateRequest,
    EntityUpdateResponse, EntityView, EventType, EvidenceRefWire, ExtractorListItem,
    ExtractorListRequest, ExtractorListResponseFrame, ForgetMode, ForgetRequest, ForgetResponse,
    Frame, GraphEdge, GraphFetchRequest, GraphFetchResponseFrame, GraphNode, InferenceKind,
    InferenceStep, LinkRequest, LinkResponse, MaterializeProceduralRequest,
    MaterializeProceduralResponse, MemoryInspectRequest, MemoryInspectResponse, MemoryKindWire,
    MemoryListDirWire, MemoryListItem, MemoryListRequest, MemoryListResponseFrame,
    MemoryListSortWire, MemoryListTimeAxisWire, ObservationInput, Opcode, PingRequest, PlanBudget,
    PlanRequest, PlanResponseFrame, PlanState, PlanStatus, PlanStep, PlanTrace, PlanTraceDirection,
    PlanTraceMeetingPoint, PlanTraceNode, PongResponse, RankedItemKindWire, ReasonRequest,
    ReasonResponseFrame, ReasonStatus, ReasonTrace, ReasonTraceBase, ReasonTraceCandidate,
    ReasonTraceCentroid, ReasonTraceEdgeCandidate, ReasonTraceIdWithText,
    ReasonTraceScoreBreakdown, ReasonTraceScoredId, ReasonTraceWalk, RecallRequest,
    RecallResponseFrame, RecallTrace, RecallTraceDroppedId, RecallTraceFilterChain,
    RecallTraceRerank, RecallTraceRetriever, RecallTraceRetrieverStatus, RelationCreateRequest,
    RelationCreateResponse, RelationGetRequest, RelationGetResponse, RelationListFromRequest,
    RelationListFromResponseFrame, RelationListToRequest, RelationListToResponseFrame,
    RelationSupersedeRequest, RelationSupersedeResponse, RelationTombstoneRequest,
    RelationTombstoneResponse, RelationTraverseRequest, RelationTraverseResponseFrame,
    RelationView, RequestBody, ResolutionOutcomeWire, ResponseBody, RetrieverNameWire,
    SchemaGetRequest, SchemaGetResponse, SchemaListItemWire, SchemaListRequest,
    SchemaListResponseFrame, SchemaReplaceRequest, SchemaReplaceResponse, SchemaUploadRequest,
    SchemaUploadResponse, SchemaValidateRequest, SchemaValidateResponse, SchemaValidationErrorWire,
    ServerPingResponse, SessionCreateRequest, SessionCreateResponse, SessionDeleteRequest,
    SessionDeleteResponse, SessionListRequest, SessionListResponse, SessionView, SimilarityFilter,
    SpaceCreateRequest, SpaceCreateResponse, SpaceDeleteRequest, SpaceDeleteResponse,
    SpaceListRequest, SpaceListResponse, SpaceView, StageKind, StatementCreateRequest,
    StatementCreateResponse, StatementGetRequest, StatementGetResponse, StatementHistoryRequest,
    StatementHistoryResponseFrame, StatementKindWire, StatementListRequest,
    StatementListResponseFrame, StatementObjectWire, StatementRetractRequest,
    StatementRetractResponse, StatementSupersedeRequest, StatementSupersedeResponse,
    StatementTombstoneRequest, StatementTombstoneResponse, StatementValueWire, StatementView,
    SubscribeRequest, SubscriptionEvent, SubscriptionFilter, TransitionKind, TraversalPathWire,
    TraversalStepWire, TxnAbortRequest, TxnAbortResponse, TxnBeginRequest, TxnBeginResponse,
    TxnCommitRequest, TxnCommitResponse, UnlinkRequest, UnlinkResponse, UnsubscribeRequest,
    UnsubscribeResponse,
};

// Fixed byte patterns. No clock, no randomness — fixtures are reproducible.
const RID: [u8; 16] = [0x11; 16];
const SPACE: [u8; 16] = [0x22; 16];
/// Structured wire space selector string (client-supplied). The server
/// derives the 16-byte storage id from it; the corpus pins the CBOR of
/// the string form.
const SPACE_STR: &str = "support-bot:user123";
const FP: [u8; 16] = [0x33; 16];
const EID: [u8; 16] = [0x44; 16];
const SID: [u8; 16] = [0x55; 16];
/// Fixed statement event time — 2026-01-01T00:00:00Z in unix nanos.
const EVENT_AT: u64 = 1_767_225_600_000_000_000;

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
        client_connection_token: None,
    }
}

fn sample_welcome() -> WelcomePayload {
    WelcomePayload {
        server_id: "brain-server/conformance".into(),
        chosen_version: 1,
        connection_id: SID,
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        server_features: ServerFeatures {
            max_payload_size: 16 * 1024 * 1024 - 1,
            max_concurrent_streams: 1024,
            idle_timeout_seconds: 300,
            auth_methods: vec![AuthMethod::Token, AuthMethod::Mtls],
        },
    }
}

fn sample_encode() -> EncodeRequest {
    EncodeRequest {
        text: "the sky is blue".into(),
        session_id: 1,
        request_id: RID,
        txn_id: None,
        occurred_at_unix_nanos: Some(1_700_000_000_000_000_000),
        act_as: None,
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    }
}

/// An ENCODE carrying an `act_as` effective-identity selector. Exercises
/// the impersonation wire path from a trusted service principal.
fn sample_encode_act_as() -> EncodeRequest {
    EncodeRequest {
        text: "on behalf of a tenant".into(),
        session_id: 1,
        request_id: RID,
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    }
}

fn sample_encode_vector_direct() -> EncodeVectorDirectRequest {
    EncodeVectorDirectRequest {
        text: "precomputed".into(),
        vector: vec![1.0, 0.5, -0.25, 0.125],
        model_fingerprint: FP,
        session_id: 1,
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
        space_id: SPACE,
        session_id: 1,
        kind: MemoryKindWire::Episodic,
        created_at_unix_nanos: 1_700_000_000_000_000_000,
        edges_out_count: 1,
        embedding_model_fp: FP,
        pending_stages: vec![StageKind::AutoEdge],
        has_active_schema: true,
        trace: None,
    }
}

/// An ENCODE response carrying a populated `trace` — the synchronous
/// write-analysis timeline plus the artifacts the write produced. Mirrors
/// `resp_recall_trace`: exercises the opt-in `trace = true` wire path.
fn sample_encode_response_trace() -> EncodeResponse {
    let mut resp = sample_encode_response();
    resp.trace = Some(EncodeTrace {
        stages: vec![
            EncodeTraceStage {
                name: "validate".into(),
                status: EncodeTraceStageStatus::Ok,
                latency_us: 3,
                detail: String::new(),
                artifact: None,
            },
            EncodeTraceStage {
                name: "embed".into(),
                status: EncodeTraceStageStatus::Ok,
                latency_us: 1200,
                detail: "dim=384".into(),
                artifact: Some(EncodeStageArtifact {
                    vector: vec![0.1, -0.2, 0.3, 0.4],
                    ..Default::default()
                }),
            },
            EncodeTraceStage {
                name: "reserve".into(),
                status: EncodeTraceStageStatus::Ok,
                latency_us: 5,
                detail: String::new(),
                artifact: None,
            },
            EncodeTraceStage {
                name: "persist".into(),
                status: EncodeTraceStageStatus::Ok,
                latency_us: 800,
                detail: "lsn=42".into(),
                artifact: Some(EncodeStageArtifact {
                    record: Some(EncodeStageRecord {
                        memory_id: mid().to_be_bytes(),
                        kind: 0,
                        salience: 0.5,
                        created_at_unix_nanos: 1_700_000_000_000_000_000,
                        occurred_at_unix_nanos: 0,
                        vector_dim: 384,
                        text_len: 42,
                        lsn: 42,
                    }),
                    ..Default::default()
                }),
            },
            EncodeTraceStage {
                name: "extractor".into(),
                status: EncodeTraceStageStatus::Ok,
                latency_us: 42_000,
                detail: "entities=2 statements=1 relations=0 audit=Succeeded".into(),
                artifact: Some(EncodeStageArtifact {
                    hype_questions: vec![
                        "Who works on brain?".into(),
                        "What does niraj work on?".into(),
                    ],
                    keyword_fields: vec![EncodeStageKeywordField {
                        field: "memory_text".into(),
                        terms: vec!["niraj".into(), "brain".into(), "works".into()],
                    }],
                    graph: Some(EncodeStageGraph {
                        nodes: vec![
                            EncodeGraphNode {
                                id: EID,
                                name: "niraj".into(),
                                kind: "entity".into(),
                                type_qname: "org:person".into(),
                            },
                            EncodeGraphNode {
                                id: mid().to_be_bytes(),
                                name: "brain".into(),
                                kind: "entity".into(),
                                type_qname: "org:project".into(),
                            },
                        ],
                        edges: vec![EncodeGraphEdge {
                            source: EID,
                            target: mid().to_be_bytes(),
                            predicate: "org:works_on".into(),
                            kind: "statement".into(),
                            confidence: 0.9,
                            event_at_unix_nanos: Some(EVENT_AT),
                        }],
                    }),
                    ..Default::default()
                }),
            },
            EncodeTraceStage {
                name: "auto_edge".into(),
                status: EncodeTraceStageStatus::Timeout,
                latency_us: 0,
                detail: "stage did not complete within the trace wait window".into(),
                artifact: None,
            },
        ],
        artifacts: EncodeTraceArtifacts {
            entities: vec![EncodeTraceEntity {
                id: EID,
                name: "brain".into(),
                type_qname: "org:project".into(),
            }],
            statements: vec![EncodeTraceStatement {
                id: mid().to_be_bytes(),
                subject_name: "niraj".into(),
                predicate: "org:works_on".into(),
                object_name: "brain".into(),
                confidence: 0.9,
                event_at_unix_nanos: Some(EVENT_AT),
            }],
            relations: vec![EncodeTraceRelation {
                source_name: "niraj".into(),
                predicate: "org:member_of".into(),
                target_name: "arc-labs".into(),
            }],
            indexes: vec![
                EncodeTraceIndex {
                    name: "memory_hnsw".into(),
                    status: EncodeTraceStageStatus::Ok,
                },
                EncodeTraceIndex {
                    name: "memory_text".into(),
                    status: EncodeTraceStageStatus::Ok,
                },
                EncodeTraceIndex {
                    name: "statement_text".into(),
                    status: EncodeTraceStageStatus::Ok,
                },
            ],
            dedup: EncodeTraceDedup {
                was_deduplicated: false,
                matched_memory_id: None,
            },
        },
        total_latency_us: 44_010,
    });
    resp
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
        session_id: 7,
        request_id: RID,
        act_as: None,
    }
}

fn sample_relation_create() -> RelationCreateRequest {
    RelationCreateRequest {
        relation_type: "org:mentors".into(),
        from_entity: EID,
        to_entity: SPACE,
        properties_blob: Vec::new(),
        evidence: EvidenceRefWire::Inline(vec![mid().to_be_bytes()]),
        extractor_id: 0,
        confidence: 0.9,
        valid_from_unix_nanos: 1_700_000_000_000_000_000,
        valid_to_unix_nanos: 0,
        session_id: 9,
        request_id: RID,
        act_as: None,
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
        resolved_from: vec![[7u8; 16], [8u8; 16]],
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
        to_entity: SPACE,
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
        trace: None,
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
        trace: None,
    }
}

/// Final frame of a `trace = true` PLAN: carries a populated `PlanTrace`
/// with the full bidirectional-BFS visited map (both directions) and every
/// meeting point found, including the one dropped by the `max_paths` cap.
fn sample_plan_trace() -> PlanResponseFrame {
    let mut resp = sample_plan();
    resp.trace = Some(PlanTrace {
        explored: vec![
            PlanTraceNode {
                memory_id: mid(),
                text: "origin: the trip begins in paris".into(),
                direction: PlanTraceDirection::Forward,
                depth: 0,
                parent_edge: None,
                alignment_score: None,
            },
            PlanTraceNode {
                memory_id: mid(),
                text: "destination: the trip ends in rome".into(),
                direction: PlanTraceDirection::Backward,
                depth: 1,
                parent_edge: Some(mid()),
                alignment_score: Some(0.62),
            },
        ],
        meeting_points: vec![
            PlanTraceMeetingPoint {
                memory_id: mid(),
                text: "layover in milan connects both legs".into(),
                included_in_result: true,
            },
            PlanTraceMeetingPoint {
                memory_id: mid(),
                text: "layover in zurich, discarded by the max_paths cap".into(),
                included_in_result: false,
            },
        ],
    });
    resp
}

/// Final frame of a `trace = true` REASON: carries a populated
/// `ReasonTrace` un-collapsing the base candidate set, the outward evidence
/// walk's considered/dropped edges, the per-item score breakdown, and the
/// centroid computation outcome.
fn sample_reason_trace() -> ReasonResponseFrame {
    let mut resp = sample_reason();
    resp.trace = Some(ReasonTrace {
        base: ReasonTraceBase {
            candidates: vec![ReasonTraceCandidate {
                memory_id: mid(),
                text: "the sky is blue".into(),
                score: 0.9,
            }],
        },
        walk: ReasonTraceWalk {
            considered: vec![ReasonTraceEdgeCandidate {
                memory_id: mid(),
                text: "the sky turned dark before the storm".into(),
                edge_kind: EdgeKindWire::Caused,
                depth: 1,
                from_memory_id: mid(),
                raw_score: 0.7,
            }],
            dropped_by_edge_kind: Vec::new(),
            dropped_by_tombstone: vec![ReasonTraceIdWithText {
                memory_id: mid(),
                text: "the sky was blue yesterday, later retracted".into(),
            }],
            dropped_by_visited: Vec::new(),
            dropped_by_confidence: vec![ReasonTraceScoredId {
                memory_id: mid(),
                text: "some clouds were visible in the distance".into(),
                score: 0.2,
            }],
            dropped_by_max_supporting: Vec::new(),
            dropped_by_max_contradicting: vec![ReasonTraceIdWithText {
                memory_id: mid(),
                text: "the sky is actually green, per one outlier report".into(),
            }],
        },
        scoring: vec![ReasonTraceScoreBreakdown {
            memory_id: mid(),
            text: "the sky is blue".into(),
            base_similarity: 0.9,
            decay: 0.95,
            weight_product: 1.0,
            alignment: 0.8,
            analogical_fit: 1.15,
            final_score: 0.7866,
        }],
        centroid: ReasonTraceCentroid {
            computed: false,
            skipped_reason: Some("singleton base".into()),
        },
    });
    resp
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

fn sample_extractor_list() -> ExtractorListResponseFrame {
    ExtractorListResponseFrame {
        items: vec![ExtractorListItem {
            extractor_id: 7,
            namespace: "org".into(),
            name: "org.default".into(),
            kind: 2,
            schema_version: 1,
            created_at_unix_nanos: 1_700_000_000_000_000_000,
        }],
        total: 1,
        is_final: true,
    }
}

fn sample_memory_inspect_request() -> MemoryInspectRequest {
    MemoryInspectRequest {
        memory_id: mid().to_be_bytes(),
        act_as: None,
    }
}

fn sample_memory_inspect_response() -> MemoryInspectResponse {
    MemoryInspectResponse {
        found: true,
        memory_id: mid().to_be_bytes(),
        text: "niraj works on brain".into(),
        artifact: EncodeStageArtifact {
            vector: vec![0.1, -0.2, 0.3, 0.4],
            record: Some(EncodeStageRecord {
                memory_id: mid().to_be_bytes(),
                kind: 0,
                salience: 0.5,
                created_at_unix_nanos: 1_700_000_000_000_000_000,
                occurred_at_unix_nanos: 0,
                vector_dim: 384,
                text_len: 20,
                lsn: 42,
            }),
            hype_questions: vec!["Who works on brain?".into()],
            keyword_fields: vec![EncodeStageKeywordField {
                field: "memory_text".into(),
                terms: vec!["niraj".into(), "brain".into(), "works".into()],
            }],
            graph: Some(EncodeStageGraph {
                nodes: vec![EncodeGraphNode {
                    id: EID,
                    name: "niraj".into(),
                    kind: "entity".into(),
                    type_qname: "org:person".into(),
                }],
                edges: vec![EncodeGraphEdge {
                    source: EID,
                    target: mid().to_be_bytes(),
                    predicate: "org:works_on".into(),
                    kind: "statement".into(),
                    confidence: 0.9,
                    // Left undated on purpose: this fixture pins the
                    // omitted-key encoding of an event-less edge, the
                    // counterpart to the dated edge in `resp_encode_trace`.
                    event_at_unix_nanos: None,
                }],
            }),
        },
    }
}

fn sample_memory_list_request() -> MemoryListRequest {
    MemoryListRequest {
        sort: MemoryListSortWire::Created,
        dir: MemoryListDirWire::Desc,
        limit: 50,
        // Non-empty on purpose: `cursor` is `Vec<u8>` (no `serde_bytes`), so
        // it must encode as a CBOR array of ints, not a byte string. The
        // byte values straddle the CBOR 1-byte/2-byte int boundary (>= 24)
        // so the array encoding is unmistakably distinct from a byte string.
        cursor: vec![0x2a, 0x00, 0xff, 0x18, 0x7b],
        kinds: vec![MemoryKindWire::Episodic],
        include_tombstoned: false,
        time_axis: MemoryListTimeAxisWire::Created,
        from_unix_nanos: 0,
        to_unix_nanos: 0,
        salience_min: 0.0,
        salience_max: 1.0,
        text_contains: String::new(),
        act_as: None,
    }
}

fn sample_memory_list_response() -> MemoryListResponseFrame {
    MemoryListResponseFrame {
        items: vec![MemoryListItem {
            memory_id: EID,
            space_id: SPACE,
            session_id: 1,
            text: "the sky is blue".into(),
            kind: 0,
            state: 0,
            created_at_unix_nanos: 1_700_000_000_000_000_000,
            occurred_at_unix_nanos: 0,
            last_accessed_at_unix_nanos: 1_700_000_001_000_000_000,
            salience: 0.5,
            access_count: 3,
            source_request_id: RID,
            statement_count: 0,
            entity_count: 0,
            relation_count: 0,
        }],
        // Non-empty: exercises the `Vec<u8>` array-of-ints encoding on the
        // response side too (a byte-string encoding would mismatch this
        // golden — see `sample_memory_list_request`).
        next_cursor: vec![0x2a, 0x00, 0xff, 0x18, 0x7b],
        cumulative_count: 1,
        is_final: true,
    }
}

fn sample_graph_fetch_request() -> GraphFetchRequest {
    GraphFetchRequest {
        limit: 200,
        // Non-empty on purpose: `cursor` is `Vec<u8>` (no `serde_bytes`), so
        // it must encode as a CBOR array of ints, not a byte string — same
        // cross-language contract as MEMORY_LIST's cursor.
        cursor: vec![0x2a, 0x00, 0xff, 0x18, 0x7b],
        include_statements: true,
        include_memories: true,
        include_memory_edges: true,
        include_tombstoned: false,
        act_as: None,
    }
}

fn sample_graph_fetch_response() -> GraphFetchResponseFrame {
    // Two memory ids for the memory layer; distinct from EID/RID so the
    // node-kind byte is what disambiguates the id-space, not the bytes.
    const MEM_A: [u8; 16] = [0x66; 16];
    const MEM_B: [u8; 16] = [0x77; 16];
    GraphFetchResponseFrame {
        nodes: vec![
            GraphNode {
                id: EID,
                kind: 0,
                label: "Sarah Chen".into(),
                type_qname: "brain:Person".into(),
            },
            GraphNode {
                id: RID,
                kind: 0,
                label: "Aurora Robotics".into(),
                type_qname: "brain:Organization".into(),
            },
            GraphNode {
                id: MEM_A,
                kind: 2,
                label: "Sarah Chen works at Aurora Robotics".into(),
                type_qname: String::new(),
            },
            GraphNode {
                id: MEM_B,
                kind: 2,
                label: "Sarah joined Aurora in March".into(),
                type_qname: String::new(),
            },
        ],
        edges: vec![
            GraphEdge {
                from_id: EID,
                to_id: RID,
                kind: 0,
                label: "brain:works_at".into(),
            },
            GraphEdge {
                from_id: MEM_A,
                to_id: EID,
                kind: 3,
                label: String::new(),
            },
            // Memory↔memory builtin edges: one symmetric (SimilarTo = 7),
            // one directional (FollowedBy = 5). The kind byte alone tells
            // them apart — there is no companion field.
            GraphEdge {
                from_id: MEM_A,
                to_id: MEM_B,
                kind: 7,
                label: "similar_to".into(),
            },
            GraphEdge {
                from_id: MEM_A,
                to_id: MEM_B,
                kind: 5,
                label: "followed_by".into(),
            },
        ],
        // Non-empty: exercises the `Vec<u8>` array-of-ints cursor on the
        // response side too.
        next_cursor: vec![0x2a, 0x00, 0xff, 0x18, 0x7b],
        is_final: true,
    }
}

fn sample_subscribe_event() -> SubscriptionEvent {
    SubscriptionEvent {
        event_type: EventType::Encoded,
        memory_id: mid(),
        session_id: 1,
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
        credentials: AuthCredentials::Token(b"opaque-token".to_vec()),
    };
    cases.push(req_case("req_auth", RequestBody::Auth(auth.clone()), &auth));

    // ---- Memory substrate requests ----
    cases.push(req_case(
        "req_encode",
        RequestBody::Encode(sample_encode()),
        &sample_encode(),
    ));
    // ENCODE opting into the synchronous write-analysis trace.
    let encode_trace = EncodeRequest {
        wait: brain_protocol::WaitMode::Derived,
        ..sample_encode()
    };
    cases.push(req_case(
        "req_encode_trace",
        RequestBody::Encode(encode_trace.clone()),
        &encode_trace,
    ));
    // ENCODE opting out of content dedup. `allow_duplicates` is skip-when-false,
    // so this is the only fixture that carries the key on the wire — it pins the
    // opt-out path so an SDK that forgets to emit the flag drifts loudly.
    let encode_allow_dups = EncodeRequest {
        allow_duplicates: true,
        ..sample_encode()
    };
    cases.push(req_case(
        "req_encode_allow_duplicates",
        RequestBody::Encode(encode_allow_dups.clone()),
        &encode_allow_dups,
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
        trace: true,
        cue_text: "what color is the sky".into(),
        subject_name: "sky".into(),
        max_results: 10,
        confidence_threshold: 0.3,
        session_filter: Some(vec![1]),
        age_bound_unix_nanos: None,
        as_of_record_time_unix_nanos: Some(1_710_000_000_000_000_000),
        kind_filter: Some(vec![MemoryKindWire::Episodic]),
        salience_floor: 0.1,
        include_edges: true,
        include_graph: false,
        include_text: true,
        request_id: Some(RID),
        txn_id: None,
        act_as: None,
    };
    cases.push(req_case(
        "req_recall",
        RequestBody::Recall(recall.clone()),
        &recall,
    ));
    let recall_act_as = RecallRequest {
        trace: false,
        cue_text: "what color is the sky".into(),
        subject_name: "sky".into(),
        max_results: 10,
        confidence_threshold: 0.3,
        session_filter: Some(vec![1]),
        age_bound_unix_nanos: None,
        as_of_record_time_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: false,
        request_id: Some(RID),
        txn_id: None,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
    };
    cases.push(req_case(
        "req_recall_act_as",
        RequestBody::Recall(recall_act_as.clone()),
        &recall_act_as,
    ));
    let forget = ForgetRequest {
        memory_id: mid(),
        mode: ForgetMode::Soft,
        request_id: RID,
        txn_id: None,
        act_as: None,
    };
    cases.push(req_case(
        "req_forget",
        RequestBody::Forget(forget.clone()),
        &forget,
    ));
    let forget_act_as = ForgetRequest {
        memory_id: mid(),
        mode: ForgetMode::Soft,
        request_id: RID,
        txn_id: None,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
    };
    cases.push(req_case(
        "req_forget_act_as",
        RequestBody::Forget(forget_act_as.clone()),
        &forget_act_as,
    ));
    cases.push(req_case(
        "req_encode_act_as",
        RequestBody::Encode(sample_encode_act_as()),
        &sample_encode_act_as(),
    ));
    // PLAN on behalf of a tenant — a data-plane read verb that now
    // carries the shared `act_as` selector.
    let plan_act_as = PlanRequest {
        start: PlanState::ByText("origin".into()),
        goal: PlanState::ByText("destination".into()),
        budget: PlanBudget {
            max_steps: 8,
            max_wall_time_ms: 1_000,
            max_branches_explored: 64,
        },
        strategy_hint: None,
        session_filter: None,
        request_id: Some(RID),
        txn_id: None,
        trace: true,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
    };
    cases.push(req_case(
        "req_plan_act_as",
        RequestBody::Plan(plan_act_as.clone()),
        &plan_act_as,
    ));
    // REASON on behalf of a tenant.
    let reason_act_as = ReasonRequest {
        observation: ObservationInput::ByText("the cat sat".into()),
        depth: 3,
        confidence_threshold: 0.5,
        session_filter: None,
        max_inferences: 5,
        budget_wall_time_ms: 1_000,
        request_id: Some(RID),
        txn_id: None,
        trace: true,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
    };
    cases.push(req_case(
        "req_reason_act_as",
        RequestBody::Reason(reason_act_as.clone()),
        &reason_act_as,
    ));

    // ---- Typed-graph requests ----
    let entity_create = EntityCreateRequest {
        entity_type_id: 1,
        canonical_name: "Ada".into(),
        aliases: vec!["Ada L.".into()],
        attributes_blob: b"role=engineer".to_vec(),
        session_id: 0,
        request_id: RID,
        act_as: None,
    };
    cases.push(req_case(
        "req_entity_create",
        RequestBody::EntityCreate(entity_create.clone()),
        &entity_create,
    ));
    // Typed-graph write on behalf of a tenant — proves `act_as` rides
    // the entity-create map when the principal is a multi-tenant gateway.
    let entity_create_act_as = EntityCreateRequest {
        entity_type_id: 1,
        canonical_name: "Ada".into(),
        aliases: vec!["Ada L.".into()],
        attributes_blob: b"role=engineer".to_vec(),
        session_id: 0,
        request_id: RID,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
    };
    cases.push(req_case(
        "req_entity_create_act_as",
        RequestBody::EntityCreate(entity_create_act_as.clone()),
        &entity_create_act_as,
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
    let materialize = MaterializeProceduralRequest {
        space_id: SPACE,
        session_filter: Some(vec![7]),
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

    // ---- Query introspection (QUERY_EXPLAIN / QUERY_TRACE) ----
    //
    // These two nest a whole QueryRequest, and that struct is where both SDK
    // bugs found in the July audit lived: `session_filter` was missing from all
    // three (the server defaults a missing Option to None, so it failed
    // silently), and `RetrieverWire` was encoded as its discriminant integer
    // rather than the variant-name string serde actually emits.
    //
    // The sample is chosen to pin exactly those two. `session_filter` is
    // populated rather than None, so an SDK that omits the field produces
    // different bytes instead of an accidentally-matching short map. And
    // `retrievers` is Explicit rather than Auto: Auto is a bare string in
    // either encoding, which is precisely why the pre-existing SDK tests using
    // it caught nothing.
    let query = QueryRequest {
        text: "who mentored whom".into(),
        entity_anchor: Some(EID),
        kind_filter: vec![0, 1],
        predicate_filter: vec!["org:works_on".into()],
        session_filter: Some(vec![7, 9]),
        time_filter: Some(TimeRangeWire {
            from_unix_ms: Some(1_700_000_000_000),
            to_unix_ms: None,
        }),
        as_of_record_time_unix_nanos: Some(1_710_000_000_000_000_000),
        confidence_min: Some(0.25),
        include_tombstoned: false,
        include_superseded: true,
        limit: 20,
        retrievers: RetrieverSelectionWire::Explicit(vec![
            RetrieverWire::Semantic,
            RetrieverWire::Graph,
        ]),
        fusion_config: Some(FusionConfigWire {
            k: 60,
            semantic_weight: 0.5,
            lexical_weight: 0.25,
            graph_weight: 0.25,
        }),
        request_id: RID,
    };
    let query_explain = QueryExplainRequest {
        query: query.clone(),
    };
    cases.push(req_case(
        "req_query_explain",
        RequestBody::QueryExplain(query_explain.clone()),
        &query_explain,
    ));
    let query_trace = QueryTraceRequest {
        query: query.clone(),
    };
    cases.push(req_case(
        "req_query_trace",
        RequestBody::QueryTrace(query_trace.clone()),
        &query_trace,
    ));

    // ---- Memory enumeration (MEMORY_LIST) ----
    cases.push(req_case(
        "req_memory_list",
        RequestBody::MemoryList(sample_memory_list_request()),
        &sample_memory_list_request(),
    ));

    // ---- Per-memory inspection (MEMORY_INSPECT) ----
    cases.push(req_case(
        "req_memory_inspect",
        RequestBody::MemoryInspect(sample_memory_inspect_request()),
        &sample_memory_inspect_request(),
    ));

    // ---- Typed-graph export (GRAPH_FETCH) ----
    cases.push(req_case(
        "req_graph_fetch",
        RequestBody::GraphFetch(sample_graph_fetch_request()),
        &sample_graph_fetch_request(),
    ));

    // ---- Handshake responses ----
    cases.push(resp_case(
        "resp_welcome",
        ResponseBody::Welcome(sample_welcome()),
        &sample_welcome(),
    ));
    let auth_ok = AuthOkPayload {
        space_id: SPACE,
        bound_shard_id: 5,
        permissions: SpacePermissions {
            can_encode: true,
            can_recall: true,
            can_plan: true,
            can_reason: true,
            can_forget: true,
            can_admin: false,
            can_act_as: false,
        },
        namespace: "acme".to_string(),
        server_time_unix_nanos: 1_700_000_000_000_000_000,
    };
    cases.push(resp_case(
        "resp_auth_ok",
        ResponseBody::AuthOk(auth_ok.clone()),
        &auth_ok,
    ));
    // A trusted service principal that holds the `can_act_as` grant —
    // the edge/gateway identity that fronts many tenants.
    let auth_ok_act_as = AuthOkPayload {
        space_id: SPACE,
        bound_shard_id: 5,
        permissions: SpacePermissions {
            can_encode: true,
            can_recall: true,
            can_plan: true,
            can_reason: true,
            can_forget: true,
            can_admin: false,
            can_act_as: true,
        },
        namespace: "acme".to_string(),
        server_time_unix_nanos: 1_700_000_000_000_000_000,
    };
    cases.push(resp_case(
        "resp_auth_ok_act_as",
        ResponseBody::AuthOk(auth_ok_act_as.clone()),
        &auth_ok_act_as,
    ));

    // ---- Memory substrate responses ----
    cases.push(resp_case(
        "resp_encode",
        ResponseBody::Encode(sample_encode_response()),
        &sample_encode_response(),
    ));
    // Response to a `trace = true` ENCODE: carries the full synchronous
    // write-analysis timeline plus the produced artifacts.
    cases.push(resp_case(
        "resp_encode_trace",
        ResponseBody::Encode(sample_encode_response_trace()),
        &sample_encode_response_trace(),
    ));
    let recall_resp = RecallResponseFrame {
        trace: None,
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
    // Final frame of a `trace = true` recall: carries a populated
    // `RecallTrace` mirroring the read pipeline's per-stage observability.
    let recall_trace_resp = RecallResponseFrame {
        answer_kind: AnswerKindWire::None,
        memories: Vec::new(),
        is_final: true,
        cumulative_count: 0,
        estimated_remaining: Some(0),
        trace: Some(RecallTrace {
            retrievers: vec![
                RecallTraceRetriever {
                    name: RetrieverNameWire::Semantic,
                    status: RecallTraceRetrieverStatus::Success,
                    status_detail: String::new(),
                    latency_ms: 1.5,
                    candidate_count: 12,
                    candidates: Vec::new(),
                },
                RecallTraceRetriever {
                    name: RetrieverNameWire::Graph,
                    status: RecallTraceRetrieverStatus::Skipped,
                    status_detail: "no anchor".into(),
                    latency_ms: 0.0,
                    candidate_count: 0,
                    candidates: Vec::new(),
                },
            ],
            filter_chain: RecallTraceFilterChain {
                before: 12,
                after_type: 12,
                after_temporal: 10,
                after_confidence: 8,
                after_tombstone: 8,
                after_supersession: 7,
                after_as_of: 7,
                after_limit: 5,
                dropped_by_type: Vec::new(),
                dropped_by_temporal: Vec::new(),
                dropped_by_confidence: Vec::new(),
                dropped_by_tombstone: Vec::new(),
                dropped_by_supersession: vec![RecallTraceDroppedId {
                    kind: RankedItemKindWire::Statement,
                    id: mid(),
                }],
                dropped_by_as_of: vec![RecallTraceDroppedId {
                    kind: RankedItemKindWire::Relation,
                    id: mid(),
                }],
                dropped_by_limit: vec![RecallTraceDroppedId {
                    kind: RankedItemKindWire::Memory,
                    id: mid(),
                }],
            },
            rerank: Some(RecallTraceRerank {
                applied: true,
                candidates: 5,
                latency_ms: 2.25,
                before_order: Vec::new(),
                after_order: Vec::new(),
            }),
            total_latency_ms: 4.75,
            fusion: None,
        }),
    };
    cases.push(resp_case(
        "resp_recall_trace",
        ResponseBody::Recall(recall_trace_resp.clone()),
        &recall_trace_resp,
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

    // ---- Query introspection responses ----
    let query_explain_resp = QueryExplainResponse {
        plan_text: "semantic ∪ graph -> rrf(k=60) -> limit 20".into(),
        estimated_cost_ms: 1.25,
    };
    cases.push(resp_case(
        "resp_query_explain",
        ResponseBody::QueryExplain(query_explain_resp.clone()),
        &query_explain_resp,
    ));
    // `total_latency_ms` is f64 here where most timing fields are f32 — the
    // corpus is the only thing that pins which, since both round-trip fine
    // inside an SDK that picks the wrong one consistently.
    let query_trace_resp = QueryTraceResponse {
        trace_text: "semantic 0.4ms -> graph 1.1ms -> fuse 0.2ms".into(),
        total_latency_ms: 2.5,
    };
    cases.push(resp_case(
        "resp_query_trace",
        ResponseBody::QueryTrace(query_trace_resp.clone()),
        &query_trace_resp,
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

    cases.push(resp_case(
        "resp_memory_list",
        ResponseBody::MemoryList(sample_memory_list_response()),
        &sample_memory_list_response(),
    ));
    cases.push(resp_case(
        "resp_memory_inspect",
        ResponseBody::MemoryInspect(sample_memory_inspect_response()),
        &sample_memory_inspect_response(),
    ));

    cases.push(resp_case(
        "resp_graph_fetch",
        ResponseBody::GraphFetch(sample_graph_fetch_response()),
        &sample_graph_fetch_response(),
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
    // Final frame of a `trace = true` PLAN/REASON: exercises the opt-in
    // per-stage trace wire path, mirroring `resp_recall_trace`.
    cases.push(resp_case(
        "resp_plan_trace",
        ResponseBody::Plan(sample_plan_trace()),
        &sample_plan_trace(),
    ));
    cases.push(resp_case(
        "resp_reason_trace",
        ResponseBody::Reason(sample_reason_trace()),
        &sample_reason_trace(),
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

    // ---- Extractor introspection ----
    let extractor_list_req = ExtractorListRequest {};
    cases.push(req_case(
        "req_extractor_list",
        RequestBody::ExtractorList(extractor_list_req),
        &extractor_list_req,
    ));
    cases.push(resp_case(
        "resp_extractor_list",
        ResponseBody::ExtractorList(sample_extractor_list()),
        &sample_extractor_list(),
    ));

    // ---- Space & session registry ----
    let space_create_req = SpaceCreateRequest {
        metadata: Some(vec![0x01, 0x02, 0x03]),
        request_id: RID,
        act_as: None,
    };
    cases.push(req_case(
        "req_space_create",
        RequestBody::SpaceCreate(space_create_req.clone()),
        &space_create_req,
    ));
    let space_create_resp = SpaceCreateResponse {
        space_id: SPACE_STR.into(),
        created: true,
        created_at_unix_nanos: 1_700_000_000_000_000_000,
        last_active_unix_nanos: 1_700_000_000_000_000_000,
        memory_count: 0,
        session_count: 1,
    };
    cases.push(resp_case(
        "resp_space_create",
        ResponseBody::SpaceCreate(space_create_resp.clone()),
        &space_create_resp,
    ));
    let space_list_req = SpaceListRequest {
        limit: 100,
        act_as: None,
    };
    cases.push(req_case(
        "req_space_list",
        RequestBody::SpaceList(space_list_req.clone()),
        &space_list_req,
    ));
    let space_list_resp = SpaceListResponse {
        spaces: vec![SpaceView {
            space_id: SPACE_STR.into(),
            created_at_unix_nanos: 1_700_000_000_000_000_000,
            last_active_unix_nanos: 1_700_000_000_500_000_000,
            memory_count: 42,
            session_count: 3,
        }],
        cross_shard_complete: false,
    };
    cases.push(resp_case(
        "resp_space_list",
        ResponseBody::SpaceList(space_list_resp.clone()),
        &space_list_resp,
    ));
    let space_delete_req = SpaceDeleteRequest {
        request_id: RID,
        act_as: None,
    };
    cases.push(req_case(
        "req_space_delete",
        RequestBody::SpaceDelete(space_delete_req.clone()),
        &space_delete_req,
    ));
    let space_delete_resp = SpaceDeleteResponse {
        space_id: SPACE_STR.into(),
        existed: true,
        memories_forgotten: 42,
    };
    cases.push(resp_case(
        "resp_space_delete",
        ResponseBody::SpaceDelete(space_delete_resp.clone()),
        &space_delete_resp,
    ));
    let session_create_req = SessionCreateRequest {
        session_id: 7,
        title: Some("project alpha".into()),
        request_id: RID,
        act_as: None,
    };
    cases.push(req_case(
        "req_session_create",
        RequestBody::SessionCreate(session_create_req.clone()),
        &session_create_req,
    ));
    let session_create_resp = SessionCreateResponse {
        space_id: SPACE,
        session_id: 7,
        created: true,
        created_at_unix_nanos: 1_700_000_000_000_000_000,
        last_active_unix_nanos: 1_700_000_000_000_000_000,
        memory_count: 0,
    };
    cases.push(resp_case(
        "resp_session_create",
        ResponseBody::SessionCreate(session_create_resp.clone()),
        &session_create_resp,
    ));
    let session_list_req = SessionListRequest {
        limit: 50,
        act_as: None,
    };
    cases.push(req_case(
        "req_session_list",
        RequestBody::SessionList(session_list_req.clone()),
        &session_list_req,
    ));
    let session_list_resp = SessionListResponse {
        space_id: SPACE,
        sessions: vec![SessionView {
            session_id: 7,
            created_at_unix_nanos: 1_700_000_000_000_000_000,
            last_active_unix_nanos: 1_700_000_000_900_000_000,
            title: Some("project alpha".into()),
            memory_count: 12,
        }],
    };
    cases.push(resp_case(
        "resp_session_list",
        ResponseBody::SessionList(session_list_resp.clone()),
        &session_list_resp,
    ));
    let session_delete_req = SessionDeleteRequest {
        session_id: 7,
        hard: false,
        request_id: RID,
        act_as: None,
    };
    cases.push(req_case(
        "req_session_delete",
        RequestBody::SessionDelete(session_delete_req.clone()),
        &session_delete_req,
    ));
    let session_delete_resp = SessionDeleteResponse {
        space_id: SPACE,
        session_id: 7,
        existed: true,
        memories_forgotten: 12,
    };
    cases.push(resp_case(
        "resp_session_delete",
        ResponseBody::SessionDelete(session_delete_resp.clone()),
        &session_delete_resp,
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

    // Dedicated ActAsDenied case: a per-request-identity denial, distinct
    // from the generic PermissionDenied authorization error above.
    let act_as_denied = ErrorResponse {
        code: ErrorCodeWire::from(ErrorCode::ActAsDenied),
        category: ErrorCategoryWire::from(ErrorCategory::Authorization),
        message: "principal not entitled to act_as".into(),
        details: None,
        retry_after_ms: None,
    };
    cases.push(resp_case(
        "resp_error_act_as_denied",
        ResponseBody::Error(act_as_denied.clone()),
        &act_as_denied,
    ));

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
            trace: None,
            answer_kind: AnswerKindWire::None,
            memories: Vec::new(),
            is_final: true,
            cumulative_count: 0,
            estimated_remaining: Some(0),
        })
        .encode(),
    ));

    const ENT_DUP: [u8; 16] = [0x66; 16];
    const ENT_AUDIT: [u8; 16] = [0x77; 16];
    const ENT_STALE: [u8; 16] = [0x88; 16];

    // ---- Entity requests ----

    // ENTITY_GET (0x0131). Only two fields, and the interesting one is `act_as`:
    // it is `skip_serializing_if = "Option::is_none"`, so this is the sole vector
    // proving the key is spelled `act_as` and carries a two-string map on a
    // *read* op. (The None-is-omitted half of that contract is already pinned by
    // req_entity_create vs req_entity_create_act_as.)
    let entity_get = EntityGetRequest {
        entity_id: EID,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
    };
    cases.push(req_case(
        "req_entity_get",
        RequestBody::EntityGet(entity_get.clone()),
        &entity_get,
    ));

    // ENTITY_UPDATE (0x0132). Full-desired-state write, so every mutable field
    // is populated. Pins two things nothing else does: `aliases` is a 3-element
    // ordered list (a decoder that sorts or dedupes them re-encodes differently),
    // and `attributes_blob` is a bare `Vec<u8>` with NO `serde_bytes` attribute —
    // it must encode as a CBOR *array of integers*, not a byte string. An SDK
    // that maps every blob field to bytes produces different bytes here.
    // `entity_id` (EID) and `request_id` (RID) differ so a transposition shows.
    let entity_update = EntityUpdateRequest {
        entity_id: EID,
        canonical_name: "Ada Lovelace".into(),
        aliases: vec![
            "Ada L.".into(),
            "A. Lovelace".into(),
            "Countess of Lovelace".into(),
        ],
        attributes_blob: b"role=mathematician;team=analytical-engine".to_vec(),
        request_id: RID,
    };
    cases.push(req_case(
        "req_entity_update",
        RequestBody::EntityUpdate(entity_update.clone()),
        &entity_update,
    ));

    // ENTITY_RENAME (0x0133). Pins `move_to_alias` — a bool whose only
    // server-accepted value is `true` (the handler rejects `false` with
    // "not yet supported"), which is also the non-default bool, so an SDK that
    // drops the field or defaults it to `false` cannot match these bytes.
    // The new canonical name is distinct from the update case's so the two
    // string fields can't be confused.
    let entity_rename = EntityRenameRequest {
        entity_id: EID,
        new_canonical_name: "Augusta Ada King".into(),
        move_to_alias: true,
        request_id: RID,
    };
    cases.push(req_case(
        "req_entity_rename",
        RequestBody::EntityRename(entity_rename.clone()),
        &entity_rename,
    ));

    // ENTITY_MERGE (0x0134). Three distinct 16-byte ids in one payload
    // (survivor / merged / request_id) — the only entity vector that can catch a
    // survivor-vs-merged swap, which is a silent data-destroying bug on the
    // server. `confidence` is f32: 0.87 is not representable in binary16, so the
    // golden bytes pin a 4-byte CBOR float and an SDK widening to f64 fails.
    let entity_merge = EntityMergeRequest {
        survivor: EID,
        merged: ENT_DUP,
        confidence: 0.87,
        reason: "same github handle and work email domain".into(),
        request_id: RID,
    };
    cases.push(req_case(
        "req_entity_merge",
        RequestBody::EntityMerge(entity_merge.clone()),
        &entity_merge,
    ));

    // ENTITY_UNMERGE (0x0135). Pins the direction of the operation: the field is
    // `merged_entity` (the absorbed record, ENT_DUP), never the survivor. An SDK
    // that names it after the survivor still round-trips against itself.
    let entity_unmerge = EntityUnmergeRequest {
        merged_entity: ENT_DUP,
        request_id: RID,
    };
    cases.push(req_case(
        "req_entity_unmerge",
        RequestBody::EntityUnmerge(entity_unmerge),
        &entity_unmerge,
    ));

    // ENTITY_RESOLVE (0x0136). Two adjacent free-text fields with clearly
    // different content (a swap is visible), a non-zero `entity_type_hint`
    // (0 means "no hint", so a dropped field would look like the legal
    // cross-type path and silently change server behaviour), `allow_create =
    // true` (non-default — an SDK defaulting it to false turns a create-on-miss
    // into a NotFound), and `act_as` present so tenant-scoped resolution is
    // pinned on the one op where cross-tenant leakage would be worst.
    let entity_resolve = EntityResolveRequest {
        candidate_name: "Ada L.".into(),
        resolution_context: "standup notes: mentioned alongside Charles Babbage".into(),
        entity_type_hint: 7,
        allow_create: true,
        request_id: RID,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
    };
    cases.push(req_case(
        "req_entity_resolve",
        RequestBody::EntityResolve(entity_resolve.clone()),
        &entity_resolve,
    ));

    // ENTITY_LIST (0x0137). The widest entity request: eight fields, three of
    // them numeric and two of them bool.
    //   - entity_type_id / mention_count_min / limit hold 4 / 3 / 250 — three
    //     distinct u32s, so any pairwise transposition changes the bytes.
    //   - include_tombstoned = true, include_merged = false — deliberately
    //     *different* so a swap of the two adjacent bools is visible.
    //   - `cursor` is a non-empty `Vec<u8>` with no `serde_bytes`: like
    //     attributes_blob it must encode as a CBOR array of integers. This is
    //     the only vector in the corpus that pins an opaque continuation token,
    //     and the only one that pins it as an array rather than a byte string.
    //   - `act_as` present: ENTITY_LIST enumerates a whole tenant keyspace.
    // NOTE: the v1 handler currently rejects a non-empty cursor
    // ("pagination lands in phase 16.7.6"). The corpus is a wire-encoding
    // oracle and never runs the handler, and an empty cursor would pin nothing
    // about the field's encoding, so it is populated here on purpose.
    let entity_list = EntityListRequest {
        entity_type_id: 4,
        name_prefix: "Ada".into(),
        mention_count_min: 3,
        include_tombstoned: true,
        include_merged: false,
        limit: 250,
        cursor: b"page:2:ada-lovelace".to_vec(),
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
    };
    cases.push(req_case(
        "req_entity_list",
        RequestBody::EntityList(entity_list.clone()),
        &entity_list,
    ));

    // ENTITY_TOMBSTONE (0x0138). Uses a *third* entity id (ENT_STALE) so this
    // case cannot be satisfied by a decoder that hard-codes EID, and carries a
    // non-empty `reason` — the reason string is folded into the idempotency
    // request hash on the server, so an SDK that drops it makes two semantically
    // different tombstones collide in the 24h idempotency cache.
    let entity_tombstone = EntityTombstoneRequest {
        entity_id: ENT_STALE,
        reason: "bad extraction: matched a product name, not a person".into(),
        request_id: RID,
    };
    cases.push(req_case(
        "req_entity_tombstone",
        RequestBody::EntityTombstone(entity_tombstone.clone()),
        &entity_tombstone,
    ));

    // ---- Entity responses ----

    // ENTITY_UPDATE reply (0x01B2). Carries the post-update EntityView. This is
    // a *second, fully distinct* EntityView in the corpus: resp_entity_get's
    // sample uses a single alias, mention_count 3, entity_type_id 1 and
    // embedding_version 1, so a decoder that mixed up any two of the four u32s
    // (entity_type_id / mention_count / embedding_version / flags) would still
    // match it. Here they are 4 / 17 / 5 / 0 — all distinct — and the two
    // timestamps are far apart rather than one second apart, so a
    // created/updated swap is obvious in the bytes.
    let entity_after_update = EntityView {
        entity_id: EID,
        entity_type_id: 4,
        canonical_name: "Ada Lovelace".into(),
        normalized_name: "ada lovelace".into(),
        aliases: vec![
            "Ada L.".into(),
            "A. Lovelace".into(),
            "Countess of Lovelace".into(),
        ],
        attributes_blob: b"role=mathematician;team=analytical-engine".to_vec(),
        mention_count: 17,
        created_at_unix_nanos: 1_759_000_000_000_000_000,
        updated_at_unix_nanos: EVENT_AT,
        merged_into: [0u8; 16],
        embedding_version: 5,
        flags: 0,
    };
    let entity_update_resp = EntityUpdateResponse {
        entity: entity_after_update,
    };
    cases.push(resp_case(
        "resp_entity_update",
        ResponseBody::EntityUpdate(entity_update_resp.clone()),
        &entity_update_resp,
    ));

    // ENTITY_RENAME reply (0x01B3). Pins the observable rename contract that no
    // other case does: the *old* canonical name has been appended to `aliases`
    // as the last element (move_to_alias), `canonical_name` / `normalized_name`
    // are the new pair, and `embedding_version` has been bumped by exactly one
    // relative to the update case (5 -> 6) because the canonical name changed.
    // A 4-element alias list also distinguishes this view from every other one.
    let entity_after_rename = EntityView {
        entity_id: EID,
        entity_type_id: 4,
        canonical_name: "Augusta Ada King".into(),
        normalized_name: "augusta ada king".into(),
        aliases: vec![
            "Ada L.".into(),
            "A. Lovelace".into(),
            "Countess of Lovelace".into(),
            // Old canonical name, moved into aliases by the rename.
            "Ada Lovelace".into(),
        ],
        attributes_blob: b"role=mathematician;team=analytical-engine".to_vec(),
        mention_count: 17,
        created_at_unix_nanos: 1_759_000_000_000_000_000,
        updated_at_unix_nanos: 1_767_312_000_000_000_000,
        merged_into: [0u8; 16],
        embedding_version: 6,
        flags: 0,
    };
    let entity_rename_resp = EntityRenameResponse {
        entity: entity_after_rename,
    };
    cases.push(resp_case(
        "resp_entity_rename",
        ResponseBody::EntityRename(entity_rename_resp.clone()),
        &entity_rename_resp,
    ));

    // ENTITY_MERGE reply (0x01B4). `audit_id` is a MergeId, not an EntityId —
    // ENT_AUDIT is distinct from both merge participants so an SDK echoing back
    // the survivor or the merged id instead of the audit row fails here.
    // `grace_period_seconds` carries the server's real default (7 days) rather
    // than 0, so a dropped field is not indistinguishable from "no grace".
    let entity_merge_resp = EntityMergeResponse {
        audit_id: ENT_AUDIT,
        grace_period_seconds: 604_800,
    };
    cases.push(resp_case(
        "resp_entity_merge",
        ResponseBody::EntityMerge(entity_merge_resp),
        &entity_merge_resp,
    ));

    // ENTITY_UNMERGE reply (0x01B5). Single field, and the pin is which id it
    // is: the handler returns the *restored* (formerly merged-away) entity,
    // ENT_DUP — not the survivor. Pairs with req_entity_unmerge so the two
    // halves of the round trip name the same id.
    let entity_unmerge_resp = EntityUnmergeResponse {
        restored_entity_id: ENT_DUP,
    };
    cases.push(resp_case(
        "resp_entity_unmerge",
        ResponseBody::EntityUnmerge(entity_unmerge_resp),
        &entity_unmerge_resp,
    ));

    // ENTITY_TOMBSTONE reply (0x01B8). Single u64 nanosecond timestamp. The
    // value is large enough (2026-01-02) that it needs a full 8-byte CBOR
    // integer, so an SDK truncating to u32 / i32 or emitting unix *seconds* or
    // *millis* cannot match. Distinct from EVENT_AT for the same reason.
    let entity_tombstone_resp = EntityTombstoneResponse {
        tombstoned_at_unix_nanos: 1_767_312_000_000_000_000,
    };
    cases.push(resp_case(
        "resp_entity_tombstone",
        ResponseBody::EntityTombstone(entity_tombstone_resp),
        &entity_tombstone_resp,
    ));

    // ============================================================================
    // OPTIONAL — drop this block if you want to stay strictly on the 13 opcodes.
    //
    // Gap found while drafting: across the ENTIRE corpus, `EntityView.merged_into`
    // is always [0; 16] and `EntityView.flags` is always 0. Nothing pins the
    // merged-entity read shape, and `flags` is exactly the "silently defaults to
    // the same bytes" case the corpus exists to catch (bit 0 = TOMBSTONED,
    // bit 1 = MERGED, per brain_metadata::tables::entity::flags).
    //
    // None of my 13 opcodes can carry that coherently — ENTITY_UPDATE /
    // ENTITY_RENAME both return an active entity, and merge / unmerge / tombstone
    // return no view at all. This is a second ENTITY_GET_RESP case (a distinct
    // case name, not a replacement for resp_entity_get) that fills the hole:
    // reading a record that has been merged into EID and tombstoned as part of
    // the cleanup, so both flag bits and merged_into are non-zero.
    // ============================================================================
    let entity_merged_away = EntityView {
        entity_id: ENT_DUP,
        entity_type_id: 4,
        canonical_name: "A. Lovelace".into(),
        normalized_name: "a. lovelace".into(),
        aliases: vec!["Lovelace, A.".into()],
        attributes_blob: b"role=mathematician".to_vec(),
        mention_count: 2,
        created_at_unix_nanos: 1_759_500_000_000_000_000,
        updated_at_unix_nanos: 1_767_312_000_000_000_000,
        // Non-zero: this record redirects to EID.
        merged_into: EID,
        embedding_version: 3,
        // TOMBSTONED | MERGED — the only non-zero `flags` in the corpus.
        flags: 3,
    };
    let entity_get_merged_resp = EntityGetResponse {
        entity: entity_merged_away,
    };
    cases.push(resp_case(
        "resp_entity_get_merged",
        ResponseBody::EntityGet(entity_get_merged_resp.clone()),
        &entity_get_merged_resp,
    ));

    const REL_ID: [u8; 16] = [
        0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e,
        0x6f,
    ];
    const REL_OLD_ID: [u8; 16] = [0x71; 16];
    const REL_NEW_ID: [u8; 16] = [0x72; 16];
    const REL_TO_EID: [u8; 16] = [0x73; 16];
    const REL_EVIDENCE_OVERFLOW_ID: [u8; 16] = [0x74; 16];
    const STMT_ID: [u8; 16] = [
        0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e,
        0x8f,
    ];
    const STMT_OLD_ID: [u8; 16] = [0x91; 16];
    const STMT_NEW_ID: [u8; 16] = [0x92; 16];
    const STMT_AUDIT_ID: [u8; 16] = [0x93; 16];
    const STMT_EVIDENCE_OVERFLOW_ID: [u8; 16] = [0x94; 16];
    // Shared `act_as` for the relation/statement ops that route it.
    let graph_act_as = ActAs {
        namespace: "tenant-acme".into(),
        space_id: SPACE_STR.into(),
    };

    // =====================================================================
    // Relation requests
    // =====================================================================

    // RELATION_GET (0x0151). Pins the two-field read request plus `act_as`
    // on a *read* op — `follow_supersession` is `true` (the non-default
    // value), so an SDK that hardcodes or drops the flag produces different
    // bytes instead of an accidentally-matching `false`.
    let relation_get = RelationGetRequest {
        relation_id: REL_ID,
        follow_supersession: true,
        act_as: Some(graph_act_as.clone()),
    };
    cases.push(req_case(
        "req_relation_get",
        RequestBody::RelationGet(relation_get.clone()),
        &relation_get,
    ));

    // RELATION_SUPERSEDE (0x0152). The only case that pins a *nested*
    // `RelationCreateRequest` inside another request map. The inner
    // `request_id` differs from the outer one on purpose: an SDK that flattens
    // the two, or reuses the outer id for the inner map, is caught. This is
    // also the only relation case with a non-empty `properties_blob`, a
    // non-zero `extractor_id`, and a non-zero `valid_to_unix_nanos` (the
    // existing `req_relation_create` leaves all three at their sentinels).
    // NOTE: `properties_blob` is a plain `Vec<u8>` with no `serde_bytes`
    // attribute, so it encodes as a CBOR *array of ints*, not a byte string.
    let relation_supersede = RelationSupersedeRequest {
        old_relation_id: REL_OLD_ID,
        new_relation: RelationCreateRequest {
            relation_type: "org:reports_to".into(),
            from_entity: EID,
            to_entity: REL_TO_EID,
            properties_blob: b"since=2024-03-01;weight=0.80".to_vec(),
            evidence: EvidenceRefWire::Inline(vec![mid().to_be_bytes(), REL_ID]),
            extractor_id: 3,
            confidence: 0.72,
            valid_from_unix_nanos: EVENT_AT,
            valid_to_unix_nanos: EVENT_AT + 86_400_000_000_000,
            session_id: 11,
            request_id: REL_NEW_ID,
            act_as: Some(graph_act_as.clone()),
        },
        request_id: RID,
    };
    cases.push(req_case(
        "req_relation_supersede",
        RequestBody::RelationSupersede(relation_supersede.clone()),
        &relation_supersede,
    ));

    // RELATION_TOMBSTONE (0x0153). Pins that the relation tombstone reason is
    // a free-text `String` — unlike STATEMENT_TOMBSTONE, which splits it into
    // a `reason: u8` code plus a `reason_message: String`. The two families
    // look alike enough that an SDK can easily model relation's as a code.
    let relation_tombstone = RelationTombstoneRequest {
        relation_id: REL_ID,
        reason: "superseded by a corrected extraction run".into(),
        request_id: RID,
    };
    cases.push(req_case(
        "req_relation_tombstone",
        RequestBody::RelationTombstone(relation_tombstone.clone()),
        &relation_tombstone,
    ));

    // RELATION_LIST_FROM (0x0154). Pins the eight-field filter map with every
    // filter engaged: both time bounds non-zero and unequal, the two adjacent
    // bools holding *different* values (so a transposition is visible), a
    // non-default `limit`, and a non-empty opaque `cursor`.
    let relation_list_from = RelationListFromRequest {
        from_entity: EID,
        relation_type_filter: "org:mentors".into(),
        time_range_start_unix_nanos: EVENT_AT,
        time_range_end_unix_nanos: EVENT_AT + 3_600_000_000_000,
        include_superseded: true,
        include_tombstoned: false,
        limit: 250,
        cursor: vec![0x01, 0x9a, 0x2f, 0x00],
        act_as: Some(graph_act_as.clone()),
    };
    cases.push(req_case(
        "req_relation_list_from",
        RequestBody::RelationListFrom(relation_list_from.clone()),
        &relation_list_from,
    ));

    // RELATION_LIST_TO (0x0155). Structurally identical to LIST_FROM except
    // the anchor field is named `to_entity`. Every scalar here is deliberately
    // *different* from the LIST_FROM case (swapped bools, different filter
    // string / limit / cursor / time window), so an SDK that encodes one type
    // with the other's field name — or reuses one request builder for both —
    // produces different bytes rather than a coincidental match.
    let relation_list_to = RelationListToRequest {
        to_entity: REL_TO_EID,
        relation_type_filter: "org:reports_to".into(),
        time_range_start_unix_nanos: EVENT_AT - 7_200_000_000_000,
        time_range_end_unix_nanos: EVENT_AT + 1_800_000_000_000,
        include_superseded: false,
        include_tombstoned: true,
        limit: 64,
        cursor: vec![0x02, 0x7f, 0x11],
        act_as: Some(graph_act_as.clone()),
    };
    cases.push(req_case(
        "req_relation_list_to",
        RequestBody::RelationListTo(relation_list_to.clone()),
        &relation_list_to,
    ));

    // RELATION_TRAVERSE (0x0156). The only relation request carrying a
    // `Vec<String>`; it is non-empty and multi-element so element ordering is
    // pinned. `direction` is `2` (= Both), not the `0` (= Outgoing) default —
    // a decoder that silently defaults the direction byte is caught. `max_depth`
    // and `max_nodes` are distinct non-default values (4 / 500) so a
    // transposition of the two adjacent u32s is visible.
    let relation_traverse = RelationTraverseRequest {
        start_entity: EID,
        relation_types: vec!["org:mentors".into(), "org:reports_to".into()],
        direction: 2,
        max_depth: 4,
        max_nodes: 500,
        time_at_unix_nanos: EVENT_AT,
        include_superseded: true,
        request_id: RID,
        act_as: Some(graph_act_as.clone()),
    };
    cases.push(req_case(
        "req_relation_traverse",
        RequestBody::RelationTraverse(relation_traverse.clone()),
        &relation_traverse,
    ));

    // =====================================================================
    // Relation responses
    // =====================================================================

    // Two contrasting `RelationView`s. The existing `sample_relation_view()`
    // leaves `properties_blob`, `extractor_id`, `valid_to`, `superseded_by`,
    // `supersedes`, `tombstoned*` and `flags` all at their sentinels, so those
    // seven fields are pinned nowhere in the corpus today. Between these two
    // every one of them carries a non-sentinel value in at least one view, and
    // the two views disagree on `tombstoned` / `flags` / `evidence` variant so
    // the fields are proven to be per-item rather than frame-level.
    //
    // `rel_view_tip` is a chain tip that was later tombstoned by a FORGET
    // cascade; `rel_view_prior` is the superseded predecessor.
    let rel_view_tip = RelationView {
        relation_id: REL_NEW_ID,
        chain_root: REL_ID,
        relation_type: "org:reports_to".into(),
        from_entity: EID,
        to_entity: REL_TO_EID,
        properties_blob: b"since=2024-03-01;weight=0.80".to_vec(),
        // Only place in the corpus that pins the second `EvidenceRefWire`
        // variant. `Inline` is variant 0, so an SDK that hardcodes the tag
        // round-trips the existing cases fine and fails here.
        evidence: EvidenceRefWire::Overflow(REL_EVIDENCE_OVERFLOW_ID),
        extractor_id: 3,
        extracted_at_unix_nanos: EVENT_AT,
        confidence: 0.72,
        valid_from_unix_nanos: EVENT_AT,
        valid_to_unix_nanos: EVENT_AT + 86_400_000_000_000,
        version: 3,
        superseded_by: [0u8; 16],
        supersedes: REL_OLD_ID,
        tombstoned: true,
        tombstoned_at_unix_nanos: EVENT_AT + 172_800_000_000_000,
        // bit 0 = is_symmetric.
        flags: 1,
    };
    let rel_view_prior = RelationView {
        relation_id: REL_OLD_ID,
        chain_root: REL_ID,
        relation_type: "org:mentors".into(),
        // Reversed endpoints relative to `rel_view_tip`: this is the LIST_TO
        // direction, and swapping `from_entity` / `to_entity` must change bytes.
        from_entity: REL_TO_EID,
        to_entity: EID,
        properties_blob: b"since=2023-11-14".to_vec(),
        evidence: EvidenceRefWire::Inline(vec![mid().to_be_bytes(), REL_ID]),
        extractor_id: 7,
        extracted_at_unix_nanos: EVENT_AT - 3_600_000_000_000,
        confidence: 0.55,
        valid_from_unix_nanos: EVENT_AT - 7_200_000_000_000,
        valid_to_unix_nanos: EVENT_AT,
        version: 2,
        superseded_by: REL_NEW_ID,
        supersedes: REL_ID,
        tombstoned: false,
        tombstoned_at_unix_nanos: 0,
        flags: 0,
    };

    // RELATION_GET reply (0x01D1). Pins the `{relation, returned_via_supersession}`
    // wrapper — an SDK that returns the bare `RelationView` for GET (easy to do,
    // since LIST_FROM/LIST_TO nest views in `items`) fails here.
    // `returned_via_supersession` is `true`, the non-default value.
    let relation_get_resp = RelationGetResponse {
        relation: rel_view_tip.clone(),
        returned_via_supersession: true,
    };
    cases.push(resp_case(
        "resp_relation_get",
        ResponseBody::RelationGet(relation_get_resp.clone()),
        &relation_get_resp,
    ));

    // RELATION_SUPERSEDE reply (0x01D2). Two fields of different types; the
    // `version` is 3 rather than 0/1 so a decoder that defaults it is caught.
    let relation_supersede_resp = RelationSupersedeResponse {
        new_relation_id: REL_NEW_ID,
        version: 3,
    };
    cases.push(resp_case(
        "resp_relation_supersede",
        ResponseBody::RelationSupersede(relation_supersede_resp),
        &relation_supersede_resp,
    ));

    // RELATION_TOMBSTONE reply (0x01D3). Single-field map — pins that the
    // response is a one-key CBOR map, not a bare integer.
    let relation_tombstone_resp = RelationTombstoneResponse {
        tombstoned_at_unix_nanos: EVENT_AT + 172_800_000_000_000,
    };
    cases.push(resp_case(
        "resp_relation_tombstone",
        ResponseBody::RelationTombstone(relation_tombstone_resp),
        &relation_tombstone_resp,
    ));

    // RELATION_LIST_TO reply (0x01D5). The only relation frame in the corpus
    // with more than one item, so per-item ordering is pinned, and the only one
    // with `is_final: false` + a non-empty `next_cursor` — i.e. a genuine
    // mid-stream frame. The existing `resp_relation_list` (LIST_FROM) is a
    // single-item terminal frame with an empty cursor, so a client that assumes
    // "one frame, always final" only breaks here.
    let relation_list_to_resp = RelationListToResponseFrame {
        items: vec![rel_view_tip.clone(), rel_view_prior.clone()],
        next_cursor: vec![0x03, 0xa1, 0x5c, 0x08],
        cumulative_count: 2,
        is_final: false,
    };
    cases.push(resp_case(
        "resp_relation_list_to",
        ResponseBody::RelationListTo(relation_list_to_resp.clone()),
        &relation_list_to_resp,
    ));

    // RELATION_TRAVERSE reply (0x01D6). The only case pinning the doubly-nested
    // `Vec<TraversalPathWire>` → `Vec<TraversalStepWire>` shape (paths, then
    // steps within a path). Two paths of unequal length prove the outer/inner
    // nesting is not flattened; `depth` is 1,2 within the first path so the
    // per-step counter can't be a constant. `truncated: true` is the
    // non-default value.
    let relation_traverse_resp = RelationTraverseResponseFrame {
        paths: vec![
            TraversalPathWire {
                steps: vec![
                    TraversalStepWire {
                        relation_id: REL_ID,
                        from: EID,
                        to: REL_TO_EID,
                        relation_type: "org:mentors".into(),
                        depth: 1,
                    },
                    TraversalStepWire {
                        relation_id: REL_NEW_ID,
                        from: REL_TO_EID,
                        to: SID,
                        relation_type: "org:reports_to".into(),
                        depth: 2,
                    },
                ],
            },
            TraversalPathWire {
                steps: vec![TraversalStepWire {
                    relation_id: REL_OLD_ID,
                    from: EID,
                    to: SID,
                    relation_type: "org:collaborates_with".into(),
                    depth: 1,
                }],
            },
        ],
        total_paths: 2,
        truncated: true,
        is_final: true,
    };
    cases.push(resp_case(
        "resp_relation_traverse",
        ResponseBody::RelationTraverse(relation_traverse_resp.clone()),
        &relation_traverse_resp,
    ));

    // =====================================================================
    // Statement requests
    // =====================================================================

    // STATEMENT_GET (0x0141). Same three-field shape as RELATION_GET but keyed
    // on `statement_id`; `follow_supersession` is `true` so the flag is pinned
    // at its non-default value.
    let statement_get_req = StatementGetRequest {
        statement_id: STMT_ID,
        follow_supersession: true,
        act_as: Some(graph_act_as.clone()),
    };
    cases.push(req_case(
        "req_statement_get",
        RequestBody::StatementGet(statement_get_req.clone()),
        &statement_get_req,
    ));

    // STATEMENT_SUPERSEDE (0x0142). Nests a full `StatementCreateRequest`, and
    // is the case that pins the awkward parts of that struct which
    // `req_statement_create` leaves at defaults:
    //
    //   * `kind: Event` with a matching non-zero `event_at_unix_nanos` — the
    //     protocol invariant is "event_at non-zero iff kind == Event", and the
    //     existing create case is a `Fact` with `event_at == 0`, so this is the
    //     only place the coupling is pinned.
    //   * `object: Value(Blob(..))` — `StatementValueWire::Blob` holds a plain
    //     `Vec<u8>` with NO `serde_bytes` attribute, so it encodes as a CBOR
    //     ARRAY OF INTS, not a byte string. Every SDK will be tempted to emit a
    //     bstr here. This is the single most valuable byte in the block.
    //   * the doubly-nested tagged enum `StatementObjectWire::Value` →
    //     `StatementValueWire::Blob`, i.e. externally-tagged inside
    //     externally-tagged: `{"Value": {"Blob": [202,254,208,13]}}`.
    //   * non-zero `extractor_id`, non-zero `valid_to`, `schema_version` 4.
    //
    // The inner `request_id` differs from the outer one so a flattening SDK is
    // caught.
    let statement_supersede = StatementSupersedeRequest {
        old_statement_id: STMT_OLD_ID,
        new_statement: StatementCreateRequest {
            kind: StatementKindWire::Event,
            subject: EID,
            predicate: "org:shipped_release".into(),
            object: StatementObjectWire::Value(StatementValueWire::Blob(vec![
                0xCA, 0xFE, 0xD0, 0x0D,
            ])),
            confidence: 0.83,
            evidence: EvidenceRefWire::Inline(vec![mid().to_be_bytes(), STMT_OLD_ID]),
            extractor_id: 5,
            valid_from_unix_nanos: EVENT_AT,
            valid_to_unix_nanos: EVENT_AT + 604_800_000_000_000,
            event_at_unix_nanos: EVENT_AT + 1_800_000_000_000,
            schema_version: 4,
            session_id: 11,
            request_id: STMT_NEW_ID,
            act_as: Some(graph_act_as.clone()),
        },
        request_id: RID,
    };
    cases.push(req_case(
        "req_statement_supersede",
        RequestBody::StatementSupersede(statement_supersede.clone()),
        &statement_supersede,
    ));

    // STATEMENT_TOMBSTONE (0x0143). Pins the split reason encoding: a `u8`
    // code plus a separate free-text `reason_message`. `reason = 3`
    // (SchemaInvalidation) is deliberately not `1`/`2`, so an SDK that
    // hardcodes or drops the code byte is caught.
    let statement_tombstone = StatementTombstoneRequest {
        statement_id: STMT_ID,
        reason: 3,
        reason_message: "predicate org:works_on removed in schema v4".into(),
        request_id: RID,
    };
    cases.push(req_case(
        "req_statement_tombstone",
        RequestBody::StatementTombstone(statement_tombstone.clone()),
        &statement_tombstone,
    ));

    // STATEMENT_RETRACT (0x0144). Field-for-field identical in shape to
    // STATEMENT_TOMBSTONE, which is exactly the hazard: an SDK can route one
    // opcode's payload through the other's encoder and never notice. Every
    // value here differs from the tombstone case (different statement id,
    // `reason = 2` (UserRequest) not 3, different message), so a crossed wire
    // changes the bytes.
    let statement_retract = StatementRetractRequest {
        statement_id: STMT_OLD_ID,
        reason: 2,
        reason_message: "user asked to delete the dietary-preference statement".into(),
        request_id: RID,
    };
    cases.push(req_case(
        "req_statement_retract",
        RequestBody::StatementRetract(statement_retract.clone()),
        &statement_retract,
    ));

    // STATEMENT_HISTORY (0x0145). The smallest request in the family — two
    // fields, no `request_id` and no `act_as`. Pins exactly that: an SDK that
    // assumes every typed-graph request carries a `request_id` emits a
    // three-key map and fails. `include_tombstoned` is `true`, the non-default.
    let statement_history_req = StatementHistoryRequest {
        anchor_id: STMT_OLD_ID,
        include_tombstoned: true,
    };
    cases.push(req_case(
        "req_statement_history",
        RequestBody::StatementHistory(statement_history_req),
        &statement_history_req,
    ));

    // STATEMENT_LIST (0x0146). Ten-field filter map with every filter engaged.
    // `kind = 2` pins the offset-by-one raw filter byte (`0` = no filter,
    // non-zero = `core_byte + 1`, so `2` selects Preference) — an SDK that
    // passes the enum's 0-based storage byte straight through sends `1` and is
    // caught. The two adjacent bools hold different values, both time bounds
    // are non-zero and unequal, and `cursor` is non-empty.
    let statement_list_req = StatementListRequest {
        subject: EID,
        predicate: "org:works_on".into(),
        kind: 2,
        min_confidence: 0.35,
        time_range_start_unix_nanos: EVENT_AT,
        time_range_end_unix_nanos: EVENT_AT + 2_592_000_000_000_000,
        only_current: false,
        include_tombstoned: true,
        limit: 250,
        cursor: vec![0x04, 0xbe, 0x21, 0x09],
        act_as: Some(graph_act_as.clone()),
    };
    cases.push(req_case(
        "req_statement_list",
        RequestBody::StatementList(statement_list_req.clone()),
        &statement_list_req,
    ));

    // =====================================================================
    // Statement responses
    // =====================================================================

    // STATEMENT_SUPERSEDE reply (0x01C2). Three fields where the two ids are
    // distinct and `version` is 4 — the statement supersede reply carries a
    // `chain_root` that the relation one does not, so an SDK sharing a decoder
    // between the two families is caught.
    let statement_supersede_resp = StatementSupersedeResponse {
        new_statement_id: STMT_NEW_ID,
        chain_root: STMT_OLD_ID,
        version: 4,
    };
    cases.push(resp_case(
        "resp_statement_supersede",
        ResponseBody::StatementSupersede(statement_supersede_resp),
        &statement_supersede_resp,
    ));

    // STATEMENT_TOMBSTONE reply (0x01C3). Single-field map. Its timestamp is
    // deliberately different from the relation tombstone reply's so the two
    // one-key maps are not byte-identical.
    let statement_tombstone_resp = StatementTombstoneResponse {
        tombstoned_at_unix_nanos: EVENT_AT + 3_600_000_000_000,
    };
    cases.push(resp_case(
        "resp_statement_tombstone",
        ResponseBody::StatementTombstone(statement_tombstone_resp),
        &statement_tombstone_resp,
    ));

    // STATEMENT_RETRACT reply (0x01C4). Two adjacent u64 nanos fields; the
    // second is the first + 30 days, so they are distinct and a transposition
    // is visible. Nothing else in the corpus pins `will_zero_at_unix_nanos`.
    let statement_retract_resp = StatementRetractResponse {
        retracted_at_unix_nanos: EVENT_AT + 7_200_000_000_000,
        will_zero_at_unix_nanos: EVENT_AT + 7_200_000_000_000 + 2_592_000_000_000_000,
    };
    cases.push(resp_case(
        "resp_statement_retract",
        ResponseBody::StatementRetract(statement_retract_resp),
        &statement_retract_resp,
    ));

    // Three `StatementView`s forming one supersession chain, used by the
    // STATEMENT_HISTORY frame below. The existing `sample_statement_view()`
    // pins only the "resolved entity subject / Fact / Value(Text) / not
    // superseded / not tombstoned" shape. These three cover everything it
    // misses, and they disagree with each other on every enum-valued field so
    // nothing can be frame-level or hardcoded:
    //
    //   subject shape:  Pending (flags=1)  |  Memory (flags=2)  |  Entity (flags=0)
    //   kind:           Preference         |  Event             |  Custom(9)
    //   object:         Value(UnixNanos)   |  StatementRef      |  MemoryRef
    //   evidence:       Overflow           |  Inline(2)         |  Inline(1)
    //   is_stateful:    true               |  false             |  true
    //
    // v1 — chain root. Pending subject: `subject` is the `[0;16]` sentinel and
    // the audit id lives in `subject_pending_audit_id`, with `flags & 1` set.
    // This tri-state (`flags` bit 0 = pending, bit 1 = memory subject, neither
    // = entity) is pinned nowhere else in the corpus. Also the only view with a
    // non-zero `tombstone_reason`.
    let stmt_view_v1 = StatementView {
        statement_id: STMT_OLD_ID,
        kind: StatementKindWire::Preference,
        subject: [0u8; 16],
        subject_pending_audit_id: STMT_AUDIT_ID,
        predicate: "org:preferred_editor".into(),
        // `UnixNanos` and `Integer` are both bare CBOR ints on the wire —
        // only the variant-name key distinguishes them. An SDK that collapses
        // the two numeric variants is caught here and nowhere else.
        object: StatementObjectWire::Value(StatementValueWire::UnixNanos(EVENT_AT)),
        confidence: 0.55,
        evidence: EvidenceRefWire::Overflow(STMT_EVIDENCE_OVERFLOW_ID),
        extractor_id: 5,
        extracted_at_unix_nanos: EVENT_AT - 86_400_000_000_000,
        schema_version: 3,
        valid_from_unix_nanos: EVENT_AT - 86_400_000_000_000,
        valid_to_unix_nanos: EVENT_AT,
        event_at_unix_nanos: 0,
        version: 1,
        superseded_by: STMT_NEW_ID,
        supersedes: [0u8; 16],
        chain_root: STMT_OLD_ID,
        tombstoned: true,
        tombstoned_at_unix_nanos: EVENT_AT + 3_600_000_000_000,
        // 3 = SchemaInvalidation.
        tombstone_reason: 3,
        // bit 0 = pending subject.
        flags: 1,
        is_stateful: true,
    };
    // v2 — middle of the chain. Memory subject (`flags & 2`), so the 16 bytes
    // in `subject` are a packed `MemoryId`, not an `EntityId`. Kind is `Event`
    // with the matching non-zero `event_at_unix_nanos` — the read-side half of
    // the invariant pinned by `req_statement_supersede`. Object is
    // `StatementRef`, the last (index 3) `StatementObjectWire` variant.
    let stmt_view_v2 = StatementView {
        statement_id: STMT_NEW_ID,
        kind: StatementKindWire::Event,
        subject: mid().to_be_bytes(),
        subject_pending_audit_id: [0u8; 16],
        predicate: "org:shipped_release".into(),
        object: StatementObjectWire::StatementRef(STMT_OLD_ID),
        confidence: 0.83,
        evidence: EvidenceRefWire::Inline(vec![mid().to_be_bytes(), STMT_OLD_ID]),
        extractor_id: 7,
        extracted_at_unix_nanos: EVENT_AT,
        schema_version: 4,
        valid_from_unix_nanos: EVENT_AT,
        valid_to_unix_nanos: EVENT_AT + 604_800_000_000_000,
        event_at_unix_nanos: EVENT_AT + 1_800_000_000_000,
        version: 2,
        superseded_by: STMT_ID,
        supersedes: STMT_OLD_ID,
        chain_root: STMT_OLD_ID,
        tombstoned: false,
        tombstoned_at_unix_nanos: 0,
        tombstone_reason: 0,
        // bit 1 = memory subject.
        flags: 2,
        is_stateful: false,
    };
    // v3 — current tip. Resolved entity subject (`flags == 0`). Kind is
    // `Custom(9)`: the only NEWTYPE variant of `StatementKindWire`, which
    // serde emits as the map `{"Custom": 9}` while the six built-ins are bare
    // strings. Every SDK hand-models this enum, and one that encodes the kind
    // as an integer discriminant round-trips the built-ins fine and fails only
    // here. Object is `MemoryRef` (variant 2, a raw packed `MemoryId`).
    let stmt_view_v3 = StatementView {
        statement_id: STMT_ID,
        kind: StatementKindWire::Custom(9),
        subject: EID,
        subject_pending_audit_id: [0u8; 16],
        predicate: "org:custom_signal".into(),
        object: StatementObjectWire::MemoryRef(mid().to_be_bytes()),
        confidence: 0.99,
        evidence: EvidenceRefWire::Inline(vec![mid().to_be_bytes()]),
        extractor_id: 11,
        extracted_at_unix_nanos: EVENT_AT + 3_600_000_000_000,
        schema_version: 5,
        valid_from_unix_nanos: EVENT_AT + 3_600_000_000_000,
        valid_to_unix_nanos: EVENT_AT + 1_209_600_000_000_000,
        event_at_unix_nanos: 0,
        version: 3,
        superseded_by: [0u8; 16],
        supersedes: STMT_NEW_ID,
        chain_root: STMT_OLD_ID,
        tombstoned: false,
        tombstoned_at_unix_nanos: 0,
        tombstone_reason: 0,
        flags: 0,
        is_stateful: true,
    };

    // STATEMENT_HISTORY reply (0x01C5). Pins the one response frame that
    // carries `chain_root` + `total_versions` alongside `items` (LIST carries
    // `next_cursor` + `cumulative_count` instead — the two frames are easy to
    // conflate). Three items in ascending `version` order pin the ordering
    // contract, and carry the enum/subject/object coverage described above.
    let statement_history_resp = StatementHistoryResponseFrame {
        items: vec![
            stmt_view_v1.clone(),
            stmt_view_v2.clone(),
            stmt_view_v3.clone(),
        ],
        chain_root: STMT_OLD_ID,
        total_versions: 3,
        is_final: true,
    };
    cases.push(resp_case(
        "resp_statement_history",
        ResponseBody::StatementHistory(statement_history_resp.clone()),
        &statement_history_resp,
    ));

    // ---- Fixed ids for the schema / txn / subscription / connection cases ----
    //
    // Distinct from RID / SPACE / EID / SID so a fixture that echoes the
    // wrong id field fails loudly rather than matching by coincidence.
    /// Transaction handle. Distinct from RID so an SDK that confuses
    /// `txn_id` with `request_id` produces different bytes.
    const TXN_ID: [u8; 16] = [0x88; 16];
    /// Second space id — pins the *list* shape of `SubscriptionFilter::spaces`
    /// (a one-element list and a scalar are easy to conflate).
    const SPACE_ALT: [u8; 16] = [0x99; 16];
    /// Two distinct packed memory ids for the LINK / UNLINK triple, so a
    /// source/target transposition is visible on the wire. (`sample_link()`
    /// uses `mid()` for both endpoints and therefore cannot catch it.)
    const LINK_SRC: u128 = (5u128 << 72) | (13u128 << 56) | 0x0A_BCDE_u128;
    const LINK_DST: u128 = (9u128 << 72) | (31u128 << 56) | 0x0F_EDCB_u128;

    // ---- Schema requests ----
    //
    // `SCHEMA_UPLOAD` already has a fixture; these four pin the remaining
    // schema verbs. `version` is 3 rather than 0 so the "specific version"
    // path is pinned — `0` means "active version" and an SDK that drops the
    // field entirely would still decode as the default.
    let schema_get_req = SchemaGetRequest {
        namespace: "org".into(),
        version: 3,
    };
    cases.push(req_case(
        "req_schema_get",
        RequestBody::SchemaGet(schema_get_req.clone()),
        &schema_get_req,
    ));
    // The only fixture carrying a non-empty *request* cursor: `cursor` is a
    // `Vec<u8>` and `limit == 0` means unlimited, so a paginating client that
    // drops either field silently changes the query rather than erroring.
    let schema_list_req = SchemaListRequest {
        namespace: "org".into(),
        limit: 25,
        cursor: vec![0x9a, 0x02, 0x00, 0x00, 0x03],
    };
    cases.push(req_case(
        "req_schema_list",
        RequestBody::SchemaList(schema_list_req.clone()),
        &schema_list_req,
    ));
    // Single-field request. The document text differs from `req_schema_upload`'s
    // so a fixture that cross-wires the two schema-document verbs is visible.
    let schema_validate_req = SchemaValidateRequest {
        schema_document: "namespace org\ndefine relation_type mentors { from Person to Person }\n"
            .into(),
    };
    cases.push(req_case(
        "req_schema_validate",
        RequestBody::SchemaValidate(schema_validate_req.clone()),
        &schema_validate_req,
    ));
    // Destructive counterpart to SCHEMA_UPLOAD. `force_drop_existing` MUST be
    // `true` (the handler rejects `false`), so this is the one fixture that
    // pins the confirmation flag in its only legal state — an SDK that omits
    // it would have the server read `false` and reject the call at runtime.
    let schema_replace_req = SchemaReplaceRequest {
        schema_document:
            "namespace org\ndefine entity_type Person { attributes { joined_at: timestamp } }\n"
                .into(),
        force_drop_existing: true,
        request_id: RID,
    };
    cases.push(req_case(
        "req_schema_replace",
        RequestBody::SchemaReplace(schema_replace_req.clone()),
        &schema_replace_req,
    ));

    // ---- Schema responses ----
    //
    // `source_blob` is the parsed-AST JSON and `schema_document` the verbatim
    // DSL: two length-prefixed byte-ish fields adjacent in the map. They carry
    // deliberately different content so an SDK that reads one into the other
    // fails. `schema_version` (3) and `validator_version` (2) are distinct
    // small u32s for the same reason.
    let schema_get_resp = SchemaGetResponse {
        namespace: "org".into(),
        schema_version: 3,
        schema_document: "namespace org\ndefine entity_type Person { attributes {} }\n".into(),
        source_blob: br#"{"namespace":"org","entity_types":["Person"]}"#.to_vec(),
        uploaded_at_unix_nanos: EVENT_AT,
        validator_version: 2,
    };
    cases.push(resp_case(
        "resp_schema_get",
        ResponseBody::SchemaGet(schema_get_resp.clone()),
        &schema_get_resp,
    ));
    // Two items with *different* `has_source_text` pin the bool in both
    // states, and `total` (3) deliberately exceeds `items.len()` (2) with
    // `is_final == false` + a non-empty `next_cursor`, so the mid-stream
    // pagination shape is pinned rather than the terminal single-page one
    // every other list fixture uses.
    let schema_list_resp = SchemaListResponseFrame {
        namespace: "org".into(),
        items: vec![
            SchemaListItemWire {
                schema_version: 3,
                uploaded_at_unix_nanos: EVENT_AT,
                validator_version: 2,
                has_source_text: true,
            },
            SchemaListItemWire {
                schema_version: 2,
                uploaded_at_unix_nanos: EVENT_AT - 86_400_000_000_000,
                validator_version: 1,
                has_source_text: false,
            },
        ],
        total: 3,
        next_cursor: vec![0x9a, 0x02, 0x00, 0x00, 0x02],
        is_final: false,
    };
    cases.push(resp_case(
        "resp_schema_list",
        ResponseBody::SchemaList(schema_list_resp.clone()),
        &schema_list_resp,
    ));
    // The only fixture that populates `SchemaValidationErrorWire` — the nested
    // struct shared by SCHEMA_UPLOAD_RESP / SCHEMA_VALIDATE_RESP /
    // SCHEMA_REPLACE_RESP, which is empty in every existing fixture. `line`
    // (4) / `column` (17) / `length` (11) are three adjacent u32s with
    // distinct values so a transposition can't hide.
    let schema_validate_resp = SchemaValidateResponse {
        namespace: "org".into(),
        would_be_version: 0,
        validation_errors: vec![SchemaValidationErrorWire {
            code: "UnknownAttributeType".into(),
            message: "unknown attribute type 'timestampz' on Person.joined_at".into(),
            line: 4,
            column: 17,
            length: 11,
            severity: 2,
        }],
    };
    cases.push(resp_case(
        "resp_schema_validate",
        ResponseBody::SchemaValidate(schema_validate_resp.clone()),
        &schema_validate_resp,
    ));
    // `dropped_count` is the field unique to REPLACE — the count of
    // schema-declared rows removed before the new document landed. 17 is
    // deliberately unlike the version numbers around it.
    let schema_replace_resp = SchemaReplaceResponse {
        namespace: "org".into(),
        schema_version: 4,
        dropped_count: 17,
        validation_errors: Vec::new(),
    };
    cases.push(resp_case(
        "resp_schema_replace",
        ResponseBody::SchemaReplace(schema_replace_resp.clone()),
        &schema_replace_resp,
    ));

    // ---- Transaction requests ----
    //
    // The three `*_RESP` bodies already have fixtures; these pin the
    // client→server side. `timeout_seconds` is 45 (not the 30 the response
    // fixture carries) and `txn_id` is TXN_ID (not RID), so a fixture that
    // copies the response's values instead of encoding the request's differs.
    let txn_begin_req = TxnBeginRequest {
        txn_id: TXN_ID,
        timeout_seconds: 45,
    };
    cases.push(req_case(
        "req_txn_begin",
        RequestBody::TxnBegin(txn_begin_req),
        &txn_begin_req,
    ));
    // COMMIT and ABORT are byte-identical single-field maps; the opcode is the
    // whole discriminator. Both fixtures exist precisely so an SDK that routes
    // one to the other's opcode is caught by the manifest, not by the bytes.
    let txn_commit_req = TxnCommitRequest { txn_id: TXN_ID };
    cases.push(req_case(
        "req_txn_commit",
        RequestBody::TxnCommit(txn_commit_req),
        &txn_commit_req,
    ));
    let txn_abort_req = TxnAbortRequest { txn_id: TXN_ID };
    cases.push(req_case(
        "req_txn_abort",
        RequestBody::TxnAbort(txn_abort_req),
        &txn_abort_req,
    ));

    // ---- Subscription ----
    //
    // `SubscriptionFilter` has five independent optional fields and every one
    // is `Some` here. That is the point: a `None` optional is omitted from the
    // CBOR map entirely, so an SDK that drops the field and an SDK that sends
    // `None` are byte-identical — only a populated filter distinguishes them.
    //
    // Specifics: `kinds` uses Semantic/Consolidated (not Episodic = 0, the
    // default a silently-defaulting decoder would produce); `spaces` goes
    // through the bespoke `opt_vec_byte_array16` codec and carries two entries
    // so the list framing is pinned; `similar_to` is the only nested struct in
    // the filter and carries a non-round threshold.
    let subscribe_req = SubscribeRequest {
        filter: SubscriptionFilter {
            session_filter: Some(vec![7, 9]),
            kinds: Some(vec![MemoryKindWire::Semantic, MemoryKindWire::Consolidated]),
            similar_to: Some(SimilarityFilter {
                reference_memory_id: mid(),
                threshold: 0.72,
            }),
            spaces: Some(vec![SPACE, SPACE_ALT]),
            memory_ids: Some(vec![LINK_SRC, LINK_DST]),
        },
        include_history: true,
        from_lsn: Some(4_096),
        max_inflight: 64,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
    };
    cases.push(req_case(
        "req_subscribe",
        RequestBody::Subscribe(subscribe_req.clone()),
        &subscribe_req,
    ));
    // Stream id 11 (not 0/1) — a stream-control op whose only field is the
    // *target* stream, which is distinct from the frame header's own stream id.
    let unsubscribe_req = UnsubscribeRequest {
        target_stream_id: 11,
    };
    cases.push(req_case(
        "req_unsubscribe",
        RequestBody::Unsubscribe(unsubscribe_req),
        &unsubscribe_req,
    ));
    // `final_lsn` is the resume point a client feeds back into
    // `SubscribeRequest::from_lsn`; it is deliberately *not* equal to the
    // 4_096 that fixture carries, so the two aren't confusable.
    let unsubscribe_resp = UnsubscribeResponse {
        target_stream_id: 11,
        final_lsn: 4_211,
    };
    cases.push(resp_case(
        "resp_unsubscribe",
        ResponseBody::Unsubscribe(unsubscribe_resp),
        &unsubscribe_resp,
    ));

    // ---- Keepalive / connection-control requests ----
    //
    // PING carries one client clock reading; the matching PONG_RESP fixture
    // carries two. A distinct sub-second value keeps the two files from
    // sharing bytes.
    let ping_req = PingRequest {
        client_timestamp_unix_nanos: 1_700_000_000_250_000_000,
    };
    cases.push(req_case("req_ping", RequestBody::Ping(ping_req), &ping_req));
    // CLIENT_PONG lists `server_timestamp` FIRST and `client_timestamp`
    // second — the reverse of `PongResponse`. The two values are distinct so
    // an SDK that copies the PONG field order encodes visibly wrong bytes.
    let client_pong_req = ClientPongRequest {
        server_timestamp_unix_nanos: 1_700_000_000_000_000_000,
        client_timestamp_unix_nanos: 1_700_000_000_750_000_000,
    };
    cases.push(req_case(
        "req_client_pong",
        RequestBody::ClientPong(client_pong_req),
        &client_pong_req,
    ));
    // BYE's `reason` is `Option<String>` with no skip attribute, so `None`
    // still occupies a map slot. `Some` is used anyway: it pins the string
    // arm, which is the one a decoder can get wrong.
    let bye_req = ByeRequest {
        reason: Some("client shutting down: rolling deploy".into()),
    };
    cases.push(req_case(
        "req_bye",
        RequestBody::Bye(bye_req.clone()),
        &bye_req,
    ));
    // `reason` is a tagged enum whose payload-bearing variant is the form an
    // SDK is most likely to encode as a bare string (like the unit variants).
    // This is the only fixture pinning `CancellationReason::Other(_)`.
    let cancel_stream_req = CancelStreamRequest {
        target_stream_id: 13,
        reason: CancellationReason::Other("client budget exhausted".into()),
    };
    cases.push(req_case(
        "req_cancel_stream",
        RequestBody::CancelStream(cancel_stream_req.clone()),
        &cancel_stream_req,
    ));
    // Structurally empty request — the payload is an empty CBOR map, NOT a
    // zero-length body or a CBOR null. That distinction is exactly what an
    // SDK gets wrong when it "optimizes away" a fieldless request.
    let get_capabilities_req = GetCapabilitiesRequest {};
    cases.push(req_case(
        "req_get_capabilities",
        RequestBody::GetCapabilities(get_capabilities_req),
        &get_capabilities_req,
    ));

    // ---- LINK / UNLINK requests ----
    //
    // `Contradicts` is the one edge kind whose weight range is [-1, 1]; the
    // negative weight pins the signed f32 path that every other weight field
    // in the corpus (all non-negative) leaves untested. `txn_id` is `Some`
    // and distinct from `request_id`, so the optional-uuid codec
    // (`opt_byte_array16`) is exercised in its present form, and the two
    // 16-byte ids can't be swapped unnoticed.
    let link_req = LinkRequest {
        source: LINK_SRC,
        target: LINK_DST,
        kind: EdgeKindWire::Contradicts,
        weight: -0.75,
        request_id: RID,
        txn_id: Some(TXN_ID),
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
    };
    cases.push(req_case(
        "req_link",
        RequestBody::Link(link_req.clone()),
        &link_req,
    ));
    // UNLINK is LINK minus `weight`. `PartOf` (= 7) is the highest edge-kind
    // discriminant, so a decoder that truncates or mis-maps the repr(u8) enum
    // is caught here rather than at the low end.
    let unlink_req = UnlinkRequest {
        source: LINK_SRC,
        target: LINK_DST,
        kind: EdgeKindWire::PartOf,
        request_id: RID,
        txn_id: Some(TXN_ID),
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            space_id: SPACE_STR.into(),
        }),
    };
    cases.push(req_case(
        "req_unlink",
        RequestBody::Unlink(unlink_req.clone()),
        &unlink_req,
    ));

    // ---- LINK / UNLINK + stream-control responses ----
    //
    // Counterpart to `resp_link`, but for the idempotent removal verb:
    // `removed` distinguishes "edge existed and was deleted" from
    // "no-op". Endpoints match `req_unlink` so a client can diff the pair.
    let unlink_resp = UnlinkResponse {
        source: LINK_SRC,
        target: LINK_DST,
        kind: EdgeKindWire::PartOf,
        removed: true,
    };
    cases.push(resp_case(
        "resp_unlink",
        ResponseBody::Unlink(unlink_resp),
        &unlink_resp,
    ));
    // ENCODE_VECTOR_DIRECT_RESP reuses the `EncodeResponse` *shape* under a
    // separate opcode (0x00AA). Every value differs from `resp_encode`'s —
    // notably `was_deduplicated: true` and a two-entry `pending_stages` —
    // so a client that routes 0x00AA to the 0x00A2 decoder (or vice versa)
    // cannot pass both fixtures with one hard-coded payload. Unlike the
    // *request*, this body is plain CBOR with no trailing f32 section.
    let encode_vector_direct_resp = EncodeResponse {
        memory_id: LINK_DST,
        was_deduplicated: true,
        salience: 0.82,
        auto_edges_added: 3,
        lsn: 4_711,
        space_id: SPACE,
        session_id: 9,
        kind: MemoryKindWire::Semantic,
        created_at_unix_nanos: EVENT_AT,
        edges_out_count: 5,
        embedding_model_fp: FP,
        pending_stages: vec![StageKind::Extractor, StageKind::Hype],
        has_active_schema: true,
        trace: None,
    };
    cases.push(resp_case(
        "resp_encode_vector_direct",
        ResponseBody::EncodeVectorDirect(encode_vector_direct_resp.clone()),
        &encode_vector_direct_resp,
    ));
    // Server ack for `req_cancel_stream` — same `target_stream_id` (13) so the
    // request/ack pair reads as one exchange, plus the server clock reading
    // that is the ack's only other field.
    let cancel_stream_ack = CancelStreamAck {
        target_stream_id: 13,
        cancelled_at_unix_nanos: 1_700_000_000_125_000_000,
    };
    cases.push(resp_case(
        "resp_cancel_stream_ack",
        ResponseBody::CancelStreamAck(cancel_stream_ack),
        &cancel_stream_ack,
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
        ("memory.list", Opcode::MemoryListReq),
        ("memory.list_resp", Opcode::MemoryListResp),
        ("graph.fetch", Opcode::GraphFetchReq),
        ("graph.fetch_resp", Opcode::GraphFetchResp),
        ("graph.entity_create", Opcode::EntityCreateReq),
        ("graph.entity_create_resp", Opcode::EntityCreateResp),
        ("graph.statement_create", Opcode::StatementCreateReq),
        ("graph.statement_create_resp", Opcode::StatementCreateResp),
        ("graph.relation_create", Opcode::RelationCreateReq),
        ("graph.relation_create_resp", Opcode::RelationCreateResp),
        ("schema.upload", Opcode::SchemaUploadReq),
        ("schema.upload_resp", Opcode::SchemaUploadResp),
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
        ("extractor.list_req", Opcode::ExtractorListReq),
        ("extractor.list_resp", Opcode::ExtractorListResp),
        ("registry.space_create", Opcode::SpaceCreateReq),
        ("registry.space_create_resp", Opcode::SpaceCreateResp),
        ("registry.space_list", Opcode::SpaceListReq),
        ("registry.space_list_resp", Opcode::SpaceListResp),
        ("registry.space_delete", Opcode::SpaceDeleteReq),
        ("registry.space_delete_resp", Opcode::SpaceDeleteResp),
        ("registry.session_create", Opcode::SessionCreateReq),
        ("registry.session_create_resp", Opcode::SessionCreateResp),
        ("registry.session_list", Opcode::SessionListReq),
        ("registry.session_list_resp", Opcode::SessionListResp),
        ("registry.session_delete", Opcode::SessionDeleteReq),
        ("registry.session_delete_resp", Opcode::SessionDeleteResp),
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
