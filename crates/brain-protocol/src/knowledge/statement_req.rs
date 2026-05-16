//! Statement-op request payloads. Spec §28/06.
//!
//! Mirrors the value-side `brain_core::knowledge` types but uses
//! wire-domain primitives so the rkyv derive fires without coupling
//! `brain-core` to rkyv. Conversion lives in [`crate::knowledge::statement_resp`]
//! alongside `StatementView`.

use rkyv::{Archive, Deserialize, Serialize};

use crate::request::WireUuid;

// ---------------------------------------------------------------------------
// Shared types (used by requests + StatementView in statement_resp.rs).
// ---------------------------------------------------------------------------

/// Wire counterpart to `brain_core::knowledge::StatementKind`. Spec
/// §28/06 §2.1. Discriminants are offset by 1 vs `StatementKind` so
/// `0` can mean "no filter" in [`StatementListRequest::kind`].
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum StatementKindWire {
    Fact = 1,
    Preference = 2,
    Event = 3,
}

/// Wire counterpart to `brain_core::knowledge::StatementValue`. Spec
/// §28/06 §2.2.
///
/// `Blob` is capped at 64 KiB by the handler (spec
/// §28_knowledge_wire_protocol/04_validation.md §3.2).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum StatementValueWire {
    Text(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    UnixNanos(u64),
    Blob(Vec<u8>),
}

// `From` impls for ergonomic `.object_value(...)` setters on the SDK
// builders (17.8). Local-type rule means these must live alongside
// the enum definition.
impl From<String> for StatementValueWire {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}
impl From<&str> for StatementValueWire {
    fn from(s: &str) -> Self {
        Self::Text(s.to_string())
    }
}
impl From<i64> for StatementValueWire {
    fn from(i: i64) -> Self {
        Self::Integer(i)
    }
}
impl From<f64> for StatementValueWire {
    fn from(f: f64) -> Self {
        Self::Float(f)
    }
}
impl From<bool> for StatementValueWire {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}
impl From<Vec<u8>> for StatementValueWire {
    fn from(b: Vec<u8>) -> Self {
        Self::Blob(b)
    }
}

/// Wire counterpart to `brain_core::knowledge::StatementObject`. Spec
/// §28/06 §2.2.
///
/// `MemoryRef` carries the raw 16-byte `MemoryId` packed form (spec
/// §02/03). All other variants use `WireUuid` ([u8; 16]).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum StatementObjectWire {
    EntityRef(WireUuid),
    Value(StatementValueWire),
    MemoryRef([u8; 16]),
    StatementRef(WireUuid),
}

impl StatementObjectWire {
    /// Stable discriminant byte (0..=3) for fast contradiction probes
    /// without full deserialisation. Aligns with
    /// `brain_core::knowledge::StatementObject::discriminant()`.
    #[must_use]
    pub const fn discriminant(&self) -> u8 {
        match self {
            Self::EntityRef(_) => 0,
            Self::Value(_) => 1,
            Self::MemoryRef(_) => 2,
            Self::StatementRef(_) => 3,
        }
    }
}

/// Wire counterpart to `brain_core::knowledge::EvidenceRef`. Spec
/// §28/06 §2.3.
///
/// `Inline` MUST contain ≤ 8 MemoryIds. Each entry is the raw 16-byte
/// `MemoryId` packed form (spec §02/03). Confidence + timestamp +
/// extractor metadata that brain-core's `EvidenceEntry` carries is NOT
/// re-sent over the wire — the handler supplies them server-side from
/// the request context. Phase-22 add-evidence ops carry the metadata
/// explicitly via a follow-up structured payload.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum EvidenceRefWire {
    Inline(Vec<[u8; 16]>),
    Overflow(WireUuid),
}

// ---------------------------------------------------------------------------
// Request structs.
// ---------------------------------------------------------------------------

/// `STATEMENT_CREATE` (`0x0140`). Spec §28/06 §3.1.
///
/// Server allocates the `StatementId`; the request does NOT carry one.
///
/// Predicate is the canonical `"namespace:name"` form; the handler
/// resolves it via `brain_metadata::predicate_lookup_by_qname`.
///
/// `valid_from_unix_nanos`, `valid_to_unix_nanos`, `event_at_unix_nanos`:
/// `0` = absent. `event_at_unix_nanos` MUST be non-zero iff
/// `kind == Event`.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementCreateRequest {
    pub kind: StatementKindWire,
    pub subject: WireUuid,
    pub predicate: String,
    pub object: StatementObjectWire,
    pub confidence: f32,
    pub evidence: EvidenceRefWire,
    pub extractor_id: u32,
    pub valid_from_unix_nanos: u64,
    pub valid_to_unix_nanos: u64,
    pub event_at_unix_nanos: u64,
    pub schema_version: u32,
    pub request_id: WireUuid,
}

/// `STATEMENT_GET` (`0x0141`). Spec §28/06 §4.1.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementGetRequest {
    pub statement_id: WireUuid,
    /// If `true` and the row is superseded, the server returns the
    /// current statement in the chain (with
    /// `returned_via_supersession = true` in the response).
    pub follow_supersession: bool,
}

/// `STATEMENT_SUPERSEDE` (`0x0142`). Spec §28/06 §5.1.
///
/// Server runs CREATE for `new_statement` then links the old + new
/// atomically inside one redb txn.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementSupersedeRequest {
    pub old_statement_id: WireUuid,
    pub new_statement: StatementCreateRequest,
    pub request_id: WireUuid,
}

/// `STATEMENT_TOMBSTONE` (`0x0143`). Spec §28/06 §6.1.
///
/// `reason` byte values: `1=SourceMemoryForgotten / 2=UserRequest /
/// 3=SchemaInvalidation / 4=ExtractorRetraction`. `reason_message`
/// is capped at 4 KiB by the validator.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementTombstoneRequest {
    pub statement_id: WireUuid,
    pub reason: u8,
    pub reason_message: String,
    pub request_id: WireUuid,
}

/// `STATEMENT_RETRACT` (`0x0144`). Spec §28/06 §7.1.
///
/// Hard delete: tombstones immediately + schedules zero-out after
/// the grace period. Distinct from `STATEMENT_TOMBSTONE` in that
/// retracted statements are excluded from `STATEMENT_HISTORY`.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementRetractRequest {
    pub statement_id: WireUuid,
    pub reason: u8,
    pub reason_message: String,
    pub request_id: WireUuid,
}

/// `STATEMENT_HISTORY` (`0x0145`). Spec §28/06 §8.1.
///
/// `anchor_id` may be a `StatementId` (any member of the chain) or
/// a chain-root id — server resolves per spec §19/01 §4.1.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementHistoryRequest {
    pub anchor_id: WireUuid,
    pub include_tombstoned: bool,
}

/// `STATEMENT_LIST` (`0x0146`). Spec §28/06 §9.1.
///
/// Empty fields:
/// - `subject == [0;16]` → no subject filter.
/// - `predicate == ""` → no predicate filter.
/// - `kind == 0` → no kind filter; otherwise matches `StatementKindWire`.
/// - `time_range_*_unix_nanos == 0` → no time bound.
///
/// `limit` must be in `1..=1000`. `cursor` is opaque (phase 23
/// streaming).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementListRequest {
    pub subject: WireUuid,
    pub predicate: String,
    pub kind: u8,
    pub min_confidence: f32,
    pub time_range_start_unix_nanos: u64,
    pub time_range_end_unix_nanos: u64,
    pub only_current: bool,
    pub include_tombstoned: bool,
    pub limit: u32,
    pub cursor: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{
        StatementCreateResponse, StatementGetResponse, StatementHistoryResponseFrame,
        StatementListResponseFrame, StatementRetractResponse, StatementSupersedeResponse,
        StatementTombstoneResponse, StatementView,
    };
    use crate::opcode::Opcode;
    use crate::request::RequestBody;
    use crate::response::ResponseBody;

    fn sample_uuid(seed: u8) -> WireUuid {
        let mut u = [0u8; 16];
        for (i, b) in u.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        u
    }

    fn sample_create_request(object: StatementObjectWire) -> StatementCreateRequest {
        StatementCreateRequest {
            kind: StatementKindWire::Fact,
            subject: sample_uuid(1),
            predicate: "test:role".into(),
            object,
            confidence: 0.9,
            evidence: EvidenceRefWire::Inline(vec![[1u8; 16], [2u8; 16]]),
            extractor_id: 0,
            valid_from_unix_nanos: 1_700_000_000_000_000_000,
            valid_to_unix_nanos: 0,
            event_at_unix_nanos: 0,
            schema_version: 1,
            request_id: sample_uuid(2),
        }
    }

    fn sample_view() -> StatementView {
        StatementView {
            statement_id: sample_uuid(3),
            kind: StatementKindWire::Fact,
            subject: sample_uuid(4),
            subject_pending_audit_id: [0u8; 16],
            predicate: "test:role".into(),
            object: StatementObjectWire::EntityRef(sample_uuid(5)),
            confidence: 0.85,
            evidence: EvidenceRefWire::Inline(vec![[7u8; 16]]),
            extractor_id: 0,
            extracted_at_unix_nanos: 1_700_000_000_000_000_000,
            schema_version: 1,
            valid_from_unix_nanos: 1_700_000_000_000_000_000,
            valid_to_unix_nanos: 0,
            event_at_unix_nanos: 0,
            version: 1,
            superseded_by: [0u8; 16],
            supersedes: [0u8; 16],
            chain_root: sample_uuid(3),
            tombstoned: false,
            tombstoned_at_unix_nanos: 0,
            tombstone_reason: 0,
            flags: 0,
        }
    }

    fn req_round_trip(body: RequestBody) {
        let bytes = body.encode();
        let decoded = RequestBody::decode(body.opcode(), &bytes)
            .unwrap_or_else(|e| panic!("decode failed for {:?}: {e}", body.opcode()));
        assert_eq!(decoded, body);
    }

    fn resp_round_trip(body: ResponseBody) {
        let bytes = body.encode();
        let decoded = ResponseBody::decode(body.opcode(), &bytes)
            .unwrap_or_else(|e| panic!("decode failed for {:?}: {e}", body.opcode()));
        assert_eq!(decoded, body);
    }

    // ----- Opcode assignments -----

    #[test]
    fn statement_opcode_byte_assignments() {
        assert_eq!(Opcode::StatementCreateReq.as_u16(), 0x0140);
        assert_eq!(Opcode::StatementCreateResp.as_u16(), 0x01C0);
        assert_eq!(Opcode::StatementGetReq.as_u16(), 0x0141);
        assert_eq!(Opcode::StatementGetResp.as_u16(), 0x01C1);
        assert_eq!(Opcode::StatementSupersedeReq.as_u16(), 0x0142);
        assert_eq!(Opcode::StatementSupersedeResp.as_u16(), 0x01C2);
        assert_eq!(Opcode::StatementTombstoneReq.as_u16(), 0x0143);
        assert_eq!(Opcode::StatementTombstoneResp.as_u16(), 0x01C3);
        assert_eq!(Opcode::StatementRetractReq.as_u16(), 0x0144);
        assert_eq!(Opcode::StatementRetractResp.as_u16(), 0x01C4);
        assert_eq!(Opcode::StatementHistoryReq.as_u16(), 0x0145);
        assert_eq!(Opcode::StatementHistoryResp.as_u16(), 0x01C5);
        assert_eq!(Opcode::StatementListReq.as_u16(), 0x0146);
        assert_eq!(Opcode::StatementListResp.as_u16(), 0x01C6);

        assert!(Opcode::StatementCreateReq.is_knowledge());
        assert!(Opcode::StatementCreateReq.is_request());
        assert!(Opcode::StatementCreateResp.is_response());
    }

    // ----- Requests -----

    #[test]
    fn statement_create_request_roundtrip_all_object_variants() {
        let cases = [
            StatementObjectWire::EntityRef(sample_uuid(10)),
            StatementObjectWire::Value(StatementValueWire::Text("hello".into())),
            StatementObjectWire::Value(StatementValueWire::Integer(-7)),
            StatementObjectWire::Value(StatementValueWire::Float(3.14)),
            StatementObjectWire::Value(StatementValueWire::Bool(true)),
            StatementObjectWire::Value(StatementValueWire::UnixNanos(42)),
            StatementObjectWire::Value(StatementValueWire::Blob(vec![0xDE, 0xAD])),
            StatementObjectWire::MemoryRef([3u8; 16]),
            StatementObjectWire::StatementRef(sample_uuid(11)),
        ];
        for o in cases {
            req_round_trip(RequestBody::StatementCreate(sample_create_request(o)));
        }
    }

    #[test]
    fn statement_create_request_overflow_evidence() {
        let mut req = sample_create_request(StatementObjectWire::EntityRef(sample_uuid(12)));
        req.evidence = EvidenceRefWire::Overflow(sample_uuid(13));
        req_round_trip(RequestBody::StatementCreate(req));
    }

    #[test]
    fn statement_get_request_roundtrip_both_flags() {
        for follow in [true, false] {
            req_round_trip(RequestBody::StatementGet(StatementGetRequest {
                statement_id: sample_uuid(20),
                follow_supersession: follow,
            }));
        }
    }

    #[test]
    fn statement_supersede_request_roundtrip() {
        req_round_trip(RequestBody::StatementSupersede(StatementSupersedeRequest {
            old_statement_id: sample_uuid(30),
            new_statement: sample_create_request(StatementObjectWire::Value(
                StatementValueWire::Text("new value".into()),
            )),
            request_id: sample_uuid(31),
        }));
    }

    #[test]
    fn statement_tombstone_request_roundtrip() {
        req_round_trip(RequestBody::StatementTombstone(StatementTombstoneRequest {
            statement_id: sample_uuid(40),
            reason: 2,
            reason_message: "user request".into(),
            request_id: sample_uuid(41),
        }));
    }

    #[test]
    fn statement_retract_request_roundtrip() {
        req_round_trip(RequestBody::StatementRetract(StatementRetractRequest {
            statement_id: sample_uuid(50),
            reason: 4,
            reason_message: "extractor retraction".into(),
            request_id: sample_uuid(51),
        }));
    }

    #[test]
    fn statement_history_request_roundtrip() {
        req_round_trip(RequestBody::StatementHistory(StatementHistoryRequest {
            anchor_id: sample_uuid(60),
            include_tombstoned: true,
        }));
    }

    #[test]
    fn statement_list_request_roundtrip() {
        // All filter fields populated.
        req_round_trip(RequestBody::StatementList(StatementListRequest {
            subject: sample_uuid(70),
            predicate: "test:role".into(),
            kind: 1,
            min_confidence: 0.5,
            time_range_start_unix_nanos: 1_000,
            time_range_end_unix_nanos: 2_000,
            only_current: true,
            include_tombstoned: false,
            limit: 100,
            cursor: vec![1, 2, 3],
        }));
        // Empty-filter case.
        req_round_trip(RequestBody::StatementList(StatementListRequest {
            subject: [0u8; 16],
            predicate: String::new(),
            kind: 0,
            min_confidence: 0.0,
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            only_current: false,
            include_tombstoned: false,
            limit: 100,
            cursor: Vec::new(),
        }));
    }

    // ----- Responses -----

    #[test]
    fn statement_responses_roundtrip() {
        resp_round_trip(ResponseBody::StatementCreate(StatementCreateResponse {
            statement_id: sample_uuid(80),
            auto_superseded: [0u8; 16],
            chain_root: sample_uuid(80),
        }));
        resp_round_trip(ResponseBody::StatementGet(StatementGetResponse {
            statement: sample_view(),
            returned_via_supersession: false,
        }));
        resp_round_trip(ResponseBody::StatementSupersede(
            StatementSupersedeResponse {
                new_statement_id: sample_uuid(81),
                chain_root: sample_uuid(82),
                version: 2,
            },
        ));
        resp_round_trip(ResponseBody::StatementTombstone(
            StatementTombstoneResponse {
                tombstoned_at_unix_nanos: 1_700_000_000_000_000_000,
            },
        ));
        resp_round_trip(ResponseBody::StatementRetract(StatementRetractResponse {
            retracted_at_unix_nanos: 1_700_000_000_000_000_000,
            will_zero_at_unix_nanos: 1_702_592_000_000_000_000,
        }));
        resp_round_trip(ResponseBody::StatementHistory(
            StatementHistoryResponseFrame {
                items: vec![sample_view()],
                chain_root: sample_uuid(83),
                total_versions: 1,
                is_final: true,
            },
        ));
        resp_round_trip(ResponseBody::StatementList(StatementListResponseFrame {
            items: vec![sample_view()],
            next_cursor: Vec::new(),
            cumulative_count: 1,
            is_final: true,
        }));
    }
}

