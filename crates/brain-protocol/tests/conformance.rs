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
    ActAs, AnswerKindWire, EdgeKindWire, EncodeGraphEdge, EncodeGraphNode, EncodeRequest,
    EncodeResponse, EncodeStageArtifact, EncodeStageGraph, EncodeStageKeywordField,
    EncodeStageRecord, EncodeTrace, EncodeTraceArtifacts, EncodeTraceDedup, EncodeTraceEntity,
    EncodeTraceIndex, EncodeTraceRelation, EncodeTraceStage, EncodeTraceStageStatus,
    EncodeTraceStatement, EncodeVectorDirectRequest, EntityCreateRequest, EntityCreateResponse,
    EntityGetResponse, EntityListItem, EntityListResponseFrame, EntityResolveResponse, EntityView,
    EventType, EvidenceRefWire, ExtractorListItem, ExtractorListRequest,
    ExtractorListResponseFrame, ForgetMode, ForgetRequest, ForgetResponse, Frame, GraphEdge,
    GraphFetchRequest, GraphFetchResponseFrame, GraphNode, InferenceKind, InferenceStep,
    LinkResponse, MaterializeProceduralRequest, MaterializeProceduralResponse,
    MemoryInspectRequest, MemoryInspectResponse, MemoryKindWire, MemoryListDirWire, MemoryListItem,
    MemoryListRequest, MemoryListResponseFrame, MemoryListSortWire, MemoryListTimeAxisWire,
    ObservationInput, Opcode, PlanBudget, PlanRequest, PlanResponseFrame, PlanState, PlanStatus,
    PlanStep, PlanTrace, PlanTraceDirection, PlanTraceMeetingPoint, PlanTraceNode, PongResponse,
    RankedItemKindWire, ReasonRequest, ReasonResponseFrame, ReasonStatus, ReasonTrace,
    ReasonTraceBase, ReasonTraceCandidate, ReasonTraceCentroid, ReasonTraceEdgeCandidate,
    ReasonTraceIdWithText, ReasonTraceScoreBreakdown, ReasonTraceScoredId, ReasonTraceWalk,
    RecallRequest, RecallResponseFrame, RecallTrace, RecallTraceDroppedId, RecallTraceFilterChain,
    RecallTraceRerank, RecallTraceRetriever, RecallTraceRetrieverStatus, RelationCreateRequest,
    RelationCreateResponse, RelationListFromResponseFrame, RelationView, RequestBody,
    ResolutionOutcomeWire, ResponseBody, RetrieverNameWire, SchemaUploadRequest,
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
            auth_methods: vec![AuthMethod::Token, AuthMethod::Mtls],
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
        context_id: 1,
        request_id: RID,
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            agent_id: AGENT,
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
        request_id: RID,
        act_as: None,
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
        context_filter: Some(vec![1]),
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
            agent_id: AGENT,
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
            agent_id: AGENT,
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
        context_filter: None,
        request_id: Some(RID),
        txn_id: None,
        trace: true,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            agent_id: AGENT,
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
        context_filter: None,
        max_inferences: 5,
        budget_wall_time_ms: 1_000,
        request_id: Some(RID),
        txn_id: None,
        trace: true,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            agent_id: AGENT,
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
        request_id: RID,
        act_as: Some(ActAs {
            namespace: "tenant-acme".into(),
            agent_id: AGENT,
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
        agent_id: AGENT,
        bound_shard_id: 5,
        permissions: AgentPermissions {
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
        agent_id: AGENT,
        bound_shard_id: 5,
        permissions: AgentPermissions {
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
