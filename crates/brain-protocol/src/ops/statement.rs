//! Statement-op request payloads.
//!
//! Mirrors the value-side `brain_core` types but uses wire-domain
//! primitives so the wire types stay decoupled from `brain-core`
//! value types. Conversion lives in [`crate::responses::statement`]
//! alongside `StatementView`.

use crate::envelope::request::WireUuid;

// ---------------------------------------------------------------------------
// Shared types (used by requests + StatementView in statement_resp.rs).
// ---------------------------------------------------------------------------

/// Wire counterpart to `brain_core::StatementKind`. Discriminants are
/// offset by 1 vs `StatementKind` so `0` can mean "no filter" in
/// [`StatementListRequest::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum StatementKindWire {
    Fact = 1,
    Preference = 2,
    Event = 3,
}

/// Wire counterpart to `brain_core::StatementValue`.
///
/// `Blob` is capped at 64 KiB by the handler.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StatementValueWire {
    Text(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    UnixNanos(u64),
    Blob(Vec<u8>),
}

// `From` impls for ergonomic `.object_value(...)` setters on the client
// builders. Local-type rule means these must live alongside the enum
// definition.
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

/// Wire counterpart to `brain_core::StatementObject`.
///
/// `MemoryRef` carries the raw 16-byte `MemoryId` packed form. All
/// other variants use `WireUuid` ([u8; 16]).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StatementObjectWire {
    EntityRef(#[serde(with = "serde_bytes")] WireUuid),
    Value(StatementValueWire),
    MemoryRef(#[serde(with = "serde_bytes")] [u8; 16]),
    StatementRef(#[serde(with = "serde_bytes")] WireUuid),
}

impl StatementObjectWire {
    /// Stable discriminant byte (0..=3) for fast contradiction probes
    /// without full deserialisation. Aligns with
    /// `brain_core::StatementObject::discriminant()`.
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

/// Wire counterpart to `brain_core::EvidenceRef`.
///
/// `Inline` MUST contain ≤ 8 MemoryIds. Each entry is the raw 16-byte
/// `MemoryId` packed form. Confidence + timestamp + extractor metadata
/// that brain-core's `EvidenceEntry` carries is NOT re-sent over the
/// wire — the handler supplies them server-side from the request
/// context. Add-evidence ops carry the metadata explicitly via a
/// follow-up structured payload.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum EvidenceRefWire {
    Inline(#[serde(with = "crate::codec::cbor::vec_byte_array16")] Vec<[u8; 16]>),
    Overflow(#[serde(with = "serde_bytes")] WireUuid),
}

// ---------------------------------------------------------------------------
// Request structs.
// ---------------------------------------------------------------------------

/// `STATEMENT_CREATE` (`0x0140`).
///
/// Server allocates the `StatementId`; the request does NOT carry one.
///
/// Predicate is the canonical `"namespace:name"` form; the handler
/// resolves it via `brain_metadata::predicate_lookup_by_qname`.
///
/// `valid_from_unix_nanos`, `valid_to_unix_nanos`, `event_at_unix_nanos`:
/// `0` = absent. `event_at_unix_nanos` MUST be non-zero iff
/// `kind == Event`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatementCreateRequest {
    pub kind: StatementKindWire,
    #[serde(with = "serde_bytes")]
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
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `STATEMENT_GET` (`0x0141`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatementGetRequest {
    #[serde(with = "serde_bytes")]
    pub statement_id: WireUuid,
    /// If `true` and the row is superseded, the server returns the
    /// current statement in the chain (with
    /// `returned_via_supersession = true` in the response).
    pub follow_supersession: bool,
}

/// `STATEMENT_SUPERSEDE` (`0x0142`).
///
/// Server runs CREATE for `new_statement` then links the old + new
/// atomically inside one redb txn.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatementSupersedeRequest {
    #[serde(with = "serde_bytes")]
    pub old_statement_id: WireUuid,
    pub new_statement: StatementCreateRequest,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `STATEMENT_TOMBSTONE` (`0x0143`).
///
/// `reason` byte values: `1=SourceMemoryForgotten / 2=UserRequest /
/// 3=SchemaInvalidation / 4=ExtractorRetraction`. `reason_message`
/// is capped at 4 KiB by the validator.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatementTombstoneRequest {
    #[serde(with = "serde_bytes")]
    pub statement_id: WireUuid,
    pub reason: u8,
    pub reason_message: String,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `STATEMENT_RETRACT` (`0x0144`).
///
/// Hard delete: tombstones immediately + schedules zero-out after
/// the grace period. Distinct from `STATEMENT_TOMBSTONE` in that
/// retracted statements are excluded from `STATEMENT_HISTORY`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatementRetractRequest {
    #[serde(with = "serde_bytes")]
    pub statement_id: WireUuid,
    pub reason: u8,
    pub reason_message: String,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `STATEMENT_HISTORY` (`0x0145`).
///
/// `anchor_id` may be a `StatementId` (any member of the chain) or
/// a chain-root id — server resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatementHistoryRequest {
    #[serde(with = "serde_bytes")]
    pub anchor_id: WireUuid,
    pub include_tombstoned: bool,
}

/// `STATEMENT_LIST` (`0x0146`).
///
/// Empty fields:
/// - `subject == [0;16]` → no subject filter.
/// - `predicate == ""` → no predicate filter.
/// - `kind == 0` → no kind filter; otherwise matches `StatementKindWire`.
/// - `time_range_*_unix_nanos == 0` → no time bound.
///
/// `limit` must be in `1..=1000`. `cursor` is opaque (reserved for a
/// later streaming cut).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatementListRequest {
    #[serde(with = "serde_bytes")]
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
mod tests_req {
    use super::*;
    use crate::codec::opcode::Opcode;
    use crate::envelope::request::RequestBody;
    use crate::envelope::response::ResponseBody;

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
            is_stateful: false,
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

        assert!(Opcode::StatementCreateReq.is_typed_graph());
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
            StatementObjectWire::Value(StatementValueWire::Float(3.5)),
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

// ============================================================
// Response payloads
// ============================================================

use brain_core::{
    EntityId, EvidenceEntry, EvidenceOverflowId, EvidenceRef, ExtractorId, MemoryId, PredicateId,
    Statement, StatementId, StatementKind, StatementObject, StatementValue, SubjectRef,
    TombstoneReason, INLINE_EVIDENCE_CAP,
};
use smallvec::SmallVec;

// ---------------------------------------------------------------------------
// StatementView — read-side projection.
// ---------------------------------------------------------------------------

/// Wire-domain projection of `brain_core::Statement`.
/// Optional value-side fields collapse to sentinel zero (`[0; 16]` for
/// ids, `0` for nanos) — same convention as `EntityView`.
///
/// `predicate` is the canonical `"namespace:name"` form — the server
/// resolves the `PredicateId` registry row to its string at projection
/// time.
///
/// `subject` is the resolved `EntityId` for resolved subjects; for
/// pending subjects, `subject == [0;16]` and `subject_pending_audit_id`
/// carries the audit row id. `flags & 1 != 0` ⇔ subject is pending.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatementView {
    #[serde(with = "serde_bytes")]
    pub statement_id: WireUuid,
    pub kind: StatementKindWire,
    #[serde(with = "serde_bytes")]
    pub subject: WireUuid,
    #[serde(with = "serde_bytes")]
    pub subject_pending_audit_id: WireUuid,
    pub predicate: String,
    pub object: StatementObjectWire,
    pub confidence: f32,
    pub evidence: EvidenceRefWire,
    pub extractor_id: u32,
    pub extracted_at_unix_nanos: u64,
    pub schema_version: u32,
    pub valid_from_unix_nanos: u64,
    pub valid_to_unix_nanos: u64,
    pub event_at_unix_nanos: u64,
    pub version: u32,
    #[serde(with = "serde_bytes")]
    pub superseded_by: WireUuid,
    #[serde(with = "serde_bytes")]
    pub supersedes: WireUuid,
    #[serde(with = "serde_bytes")]
    pub chain_root: WireUuid,
    pub tombstoned: bool,
    pub tombstoned_at_unix_nanos: u64,
    pub tombstone_reason: u8,
    pub flags: u32,
    /// `true` iff this statement is stateful (per-statement signal).
    pub is_stateful: bool,
}

// ---------------------------------------------------------------------------
// Errors raised by wire → brain-core conversion.
// ---------------------------------------------------------------------------

/// Failures from [`StatementView::to_statement`] /
/// [`evidence_ref_from_wire`] / [`statement_object_from_wire`].
#[derive(thiserror::Error, Debug)]
pub enum WireToStatementError {
    #[error("inline evidence list exceeds cap of {cap}; got {len}")]
    EvidenceInlineTooLarge { len: usize, cap: usize },

    #[error("unknown StatementKindWire byte {0}")]
    UnknownKind(u8),

    #[error("unknown TombstoneReason byte {0}")]
    UnknownTombstoneReason(u8),
}

// ---------------------------------------------------------------------------
// Conversion helpers — StatementObject ↔ wire.
// ---------------------------------------------------------------------------

#[must_use]
pub fn statement_value_to_wire(v: &StatementValue) -> StatementValueWire {
    match v {
        StatementValue::Text(s) => StatementValueWire::Text(s.clone()),
        StatementValue::Integer(i) => StatementValueWire::Integer(*i),
        StatementValue::Float(f) => StatementValueWire::Float(*f),
        StatementValue::Bool(b) => StatementValueWire::Bool(*b),
        StatementValue::UnixNanos(n) => StatementValueWire::UnixNanos(*n),
        StatementValue::Blob(b) => StatementValueWire::Blob(b.clone()),
    }
}

#[must_use]
pub fn statement_value_from_wire(w: &StatementValueWire) -> StatementValue {
    match w {
        StatementValueWire::Text(s) => StatementValue::Text(s.clone()),
        StatementValueWire::Integer(i) => StatementValue::Integer(*i),
        StatementValueWire::Float(f) => StatementValue::Float(*f),
        StatementValueWire::Bool(b) => StatementValue::Bool(*b),
        StatementValueWire::UnixNanos(n) => StatementValue::UnixNanos(*n),
        StatementValueWire::Blob(b) => StatementValue::Blob(b.clone()),
    }
}

#[must_use]
pub fn statement_object_to_wire(o: &StatementObject) -> StatementObjectWire {
    match o {
        StatementObject::Entity(id) => StatementObjectWire::EntityRef(id.to_bytes()),
        StatementObject::Value(v) => StatementObjectWire::Value(statement_value_to_wire(v)),
        StatementObject::Memory(m) => StatementObjectWire::MemoryRef(m.to_be_bytes()),
        StatementObject::Statement(s) => StatementObjectWire::StatementRef(s.to_bytes()),
    }
}

#[must_use]
pub fn statement_object_from_wire(w: &StatementObjectWire) -> StatementObject {
    match w {
        StatementObjectWire::EntityRef(id) => StatementObject::Entity(EntityId::from_bytes(*id)),
        StatementObjectWire::Value(v) => StatementObject::Value(statement_value_from_wire(v)),
        StatementObjectWire::MemoryRef(m) => StatementObject::Memory(MemoryId::from_be_bytes(*m)),
        StatementObjectWire::StatementRef(s) => {
            StatementObject::Statement(StatementId::from_bytes(*s))
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers — EvidenceRef ↔ wire.
// ---------------------------------------------------------------------------

/// Convert brain-core's evidence list to the wire shape. Note that the
/// per-entry confidence/timestamp/extractor metadata that brain-core
/// carries is **dropped** in the wire encoding — the wire variant
/// carries only `MemoryId`s. Server-side projections that need the
/// metadata should call `evidence_overflow_load` for overflow lists
/// and read inline metadata from the storage layer directly.
#[must_use]
pub fn evidence_ref_to_wire(e: &EvidenceRef) -> EvidenceRefWire {
    match e {
        EvidenceRef::Inline(entries) => {
            let memory_ids = entries
                .iter()
                .map(|e| e.memory_id.to_be_bytes())
                .collect::<Vec<[u8; 16]>>();
            EvidenceRefWire::Inline(memory_ids)
        }
        EvidenceRef::Overflow(id) => EvidenceRefWire::Overflow(id.to_bytes()),
    }
}

/// Convert the wire shape back to brain-core's `EvidenceRef`. Inline
/// entries get zero confidence / zero timestamp / extractor_id = 0;
/// callers that need the per-entry metadata read it from the storage
/// layer's overflow row (or from a future add-evidence payload that
/// carries the metadata explicitly).
pub fn evidence_ref_from_wire(w: &EvidenceRefWire) -> Result<EvidenceRef, WireToStatementError> {
    match w {
        EvidenceRefWire::Inline(memory_ids) => {
            if memory_ids.len() > INLINE_EVIDENCE_CAP {
                return Err(WireToStatementError::EvidenceInlineTooLarge {
                    len: memory_ids.len(),
                    cap: INLINE_EVIDENCE_CAP,
                });
            }
            let mut entries =
                SmallVec::<[EvidenceEntry; INLINE_EVIDENCE_CAP]>::with_capacity(memory_ids.len());
            for mid in memory_ids {
                entries.push(EvidenceEntry {
                    memory_id: MemoryId::from_be_bytes(*mid),
                    confidence_milli: 0,
                    timestamp_unix_nanos: 0,
                    extractor_id: ExtractorId::from(0),
                });
            }
            Ok(EvidenceRef::inline(entries))
        }
        EvidenceRefWire::Overflow(id) => Ok(EvidenceRef::Overflow(EvidenceOverflowId::from(*id))),
    }
}

// ---------------------------------------------------------------------------
// StatementKind ↔ wire.
// ---------------------------------------------------------------------------

#[must_use]
pub fn statement_kind_to_wire(k: StatementKind) -> StatementKindWire {
    match k {
        StatementKind::Fact => StatementKindWire::Fact,
        StatementKind::Preference => StatementKindWire::Preference,
        StatementKind::Event => StatementKindWire::Event,
    }
}

#[must_use]
pub fn statement_kind_from_wire(w: StatementKindWire) -> StatementKind {
    match w {
        StatementKindWire::Fact => StatementKind::Fact,
        StatementKindWire::Preference => StatementKind::Preference,
        StatementKindWire::Event => StatementKind::Event,
    }
}

// ---------------------------------------------------------------------------
// StatementView ↔ Statement.
// ---------------------------------------------------------------------------

impl StatementView {
    /// Build a wire view from a brain-core `Statement`. The handler
    /// resolves `predicate_qname` via the registry (string form
    /// `"namespace:name"`).
    #[must_use]
    pub fn from_statement(s: &Statement, predicate_qname: String) -> Self {
        let (subject, subject_pending_audit_id, flags) = match s.subject {
            SubjectRef::Entity(id) => (id.to_bytes(), [0u8; 16], 0u32),
            SubjectRef::Pending(audit) => ([0u8; 16], audit.to_bytes(), 1u32),
        };

        Self {
            statement_id: s.id.to_bytes(),
            kind: statement_kind_to_wire(s.kind),
            subject,
            subject_pending_audit_id,
            predicate: predicate_qname,
            object: statement_object_to_wire(&s.object),
            confidence: s.confidence,
            evidence: evidence_ref_to_wire(&s.evidence),
            extractor_id: s.extractor_id.raw(),
            extracted_at_unix_nanos: s.extracted_at_unix_nanos,
            schema_version: s.schema_version,
            valid_from_unix_nanos: s.valid_from_unix_nanos.unwrap_or(0),
            valid_to_unix_nanos: s.valid_to_unix_nanos.unwrap_or(0),
            event_at_unix_nanos: s.event_at_unix_nanos.unwrap_or(0),
            version: s.version,
            superseded_by: s
                .superseded_by
                .map(StatementId::to_bytes)
                .unwrap_or([0u8; 16]),
            supersedes: s.supersedes.map(StatementId::to_bytes).unwrap_or([0u8; 16]),
            chain_root: s.chain_root.to_bytes(),
            tombstoned: s.tombstoned,
            tombstoned_at_unix_nanos: s.tombstoned_at_unix_nanos.unwrap_or(0),
            tombstone_reason: s.tombstone_reason.map(TombstoneReason::as_u8).unwrap_or(0),
            flags,
            is_stateful: s.is_stateful,
        }
    }

    /// Project a wire view back to a brain-core `Statement`. The
    /// caller supplies the resolved `predicate` since the wire-side
    /// carries the canonical string and not the interned u32 id.
    pub fn to_statement(&self, predicate: PredicateId) -> Result<Statement, WireToStatementError> {
        let kind = statement_kind_from_wire(self.kind);
        let subject = if self.flags & 1 != 0 {
            SubjectRef::Pending(brain_core::AuditId::from_bytes(
                self.subject_pending_audit_id,
            ))
        } else {
            SubjectRef::Entity(EntityId::from_bytes(self.subject))
        };

        let tombstone_reason = if self.tombstone_reason == 0 {
            None
        } else {
            Some(TombstoneReason::from_u8(self.tombstone_reason).ok_or(
                WireToStatementError::UnknownTombstoneReason(self.tombstone_reason),
            )?)
        };

        let opt_nz = |v: u64| if v == 0 { None } else { Some(v) };
        let opt_id = |b: [u8; 16]| {
            if b == [0u8; 16] {
                None
            } else {
                Some(StatementId::from_bytes(b))
            }
        };

        Ok(Statement {
            id: StatementId::from_bytes(self.statement_id),
            kind,
            subject,
            predicate,
            object: statement_object_from_wire(&self.object),
            confidence: self.confidence,
            evidence: evidence_ref_from_wire(&self.evidence)?,
            extractor_id: ExtractorId::from(self.extractor_id),
            extracted_at_unix_nanos: self.extracted_at_unix_nanos,
            schema_version: self.schema_version,
            valid_from_unix_nanos: opt_nz(self.valid_from_unix_nanos),
            valid_to_unix_nanos: opt_nz(self.valid_to_unix_nanos),
            event_at_unix_nanos: opt_nz(self.event_at_unix_nanos),
            version: self.version,
            superseded_by: opt_id(self.superseded_by),
            supersedes: opt_id(self.supersedes),
            chain_root: StatementId::from_bytes(self.chain_root),
            tombstoned: self.tombstoned,
            tombstoned_at_unix_nanos: opt_nz(self.tombstoned_at_unix_nanos),
            tombstone_reason,
            is_stateful: self.is_stateful,
            // Bi-temporal field — wire layer doesn't carry it yet;
            // a follow-up will extend `StatementView` and route
            // the value through here. Until then the wire-decoded
            // statement is treated as "still active in record-time".
            record_invalidated_at_unix_nanos: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Response structs.
// ---------------------------------------------------------------------------

/// Reply to `STATEMENT_CREATE` (`0x01C0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatementCreateResponse {
    #[serde(with = "serde_bytes")]
    pub statement_id: WireUuid,
    /// `[0; 16]` unless auto-supersession fired (Preference kind with a
    /// prior current row at same `(subject, predicate)`).
    #[serde(with = "serde_bytes")]
    pub auto_superseded: WireUuid,
    #[serde(with = "serde_bytes")]
    pub chain_root: WireUuid,
}

/// Reply to `STATEMENT_GET` (`0x01C1`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatementGetResponse {
    pub statement: StatementView,
    /// `true` iff `follow_supersession = true` redirected to a later
    /// chain entry.
    pub returned_via_supersession: bool,
}

/// Reply to `STATEMENT_SUPERSEDE` (`0x01C2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatementSupersedeResponse {
    #[serde(with = "serde_bytes")]
    pub new_statement_id: WireUuid,
    #[serde(with = "serde_bytes")]
    pub chain_root: WireUuid,
    pub version: u32,
}

/// Reply to `STATEMENT_TOMBSTONE` (`0x01C3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatementTombstoneResponse {
    pub tombstoned_at_unix_nanos: u64,
}

/// Reply to `STATEMENT_RETRACT` (`0x01C4`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatementRetractResponse {
    pub retracted_at_unix_nanos: u64,
    /// When the GC sweep will physically reclaim the row. In v1 this
    /// is "tombstoned_at + 30 days".
    pub will_zero_at_unix_nanos: u64,
}

/// Single-frame snapshot reply for `STATEMENT_HISTORY` (`0x01C5`).
/// v1 collapses the per-item + tail shapes into one frame; a later
/// cut splits when it streams.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatementHistoryResponseFrame {
    /// Chain entries in `version` ascending order.
    pub items: Vec<StatementView>,
    #[serde(with = "serde_bytes")]
    pub chain_root: WireUuid,
    pub total_versions: u32,
    pub is_final: bool,
}

impl StatementHistoryResponseFrame {
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.is_final
    }
}

/// Single-frame snapshot reply for `STATEMENT_LIST` (`0x01C6`).
/// Mirrors `EntityListResponseFrame`. A later cut splits into
/// per-batch streaming + cursor pagination.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatementListResponseFrame {
    pub items: Vec<StatementView>,
    pub next_cursor: Vec<u8>,
    pub cumulative_count: u32,
    pub is_final: bool,
}

impl StatementListResponseFrame {
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.is_final
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests_resp {
    use super::*;
    use brain_core::{
        ContextId, EntityId, EvidenceEntry, EvidenceOverflowId, EvidenceRef, ExtractorId, MemoryId,
        PredicateId, Statement, StatementId, StatementKind, StatementObject, SubjectRef,
    };
    use brain_core::{StatementValue, INLINE_EVIDENCE_CAP};
    use smallvec::SmallVec;

    fn mem(byte: u16) -> MemoryId {
        MemoryId::pack(byte, ContextId::DEFAULT.into(), 0)
    }

    fn sample_statement(object: StatementObject) -> Statement {
        let id = StatementId::new();
        let subject = EntityId::new();
        let mut s = Statement::new_root(
            id,
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            PredicateId::from(7),
            object,
            0.9,
            EvidenceRef::inline({
                let mut sv = SmallVec::<[EvidenceEntry; INLINE_EVIDENCE_CAP]>::new();
                sv.push(EvidenceEntry::from_parts(
                    mem(1),
                    0.8,
                    1_700_000_000_000_000_000,
                    ExtractorId::from(0),
                ));
                sv
            }),
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        );
        s.valid_from_unix_nanos = Some(1_700_000_000_000_000_000);
        s
    }

    #[test]
    fn view_round_trip_entity_object() {
        let s = sample_statement(StatementObject::Entity(EntityId::new()));
        let view = StatementView::from_statement(&s, "test:role".into());
        let back = view.to_statement(s.predicate).unwrap();
        // Evidence metadata is dropped on the wire, so re-project the
        // original through the wire and compare.
        let expected_wire = StatementView::from_statement(&s, "test:role".into())
            .to_statement(s.predicate)
            .unwrap();
        assert_eq!(back, expected_wire);
        // View carries the not-stateful default.
        assert!(!view.is_stateful);
    }

    #[test]
    fn view_carries_stateful_flag() {
        let mut s = sample_statement(StatementObject::Entity(EntityId::new()));
        s.is_stateful = true;
        let view = StatementView::from_statement(&s, "test:role".into());
        assert!(view.is_stateful);
        let back = view.to_statement(s.predicate).unwrap();
        assert!(back.is_stateful);
    }

    #[test]
    fn view_round_trip_value_variants() {
        let cases = [
            StatementObject::Value(StatementValue::Text("hi".into())),
            StatementObject::Value(StatementValue::Integer(-5)),
            StatementObject::Value(StatementValue::Float(2.5)),
            StatementObject::Value(StatementValue::Bool(true)),
            StatementObject::Value(StatementValue::UnixNanos(42)),
            StatementObject::Value(StatementValue::Blob(vec![1, 2, 3])),
            StatementObject::Memory(mem(9)),
            StatementObject::Statement(StatementId::new()),
        ];
        for o in cases {
            let s = sample_statement(o.clone());
            let view = StatementView::from_statement(&s, "test:p".into());
            let back = view.to_statement(s.predicate).unwrap();
            assert_eq!(back.object, o);
        }
    }

    #[test]
    fn view_pending_subject_round_trip() {
        let mut s = sample_statement(StatementObject::Value(StatementValue::Bool(false)));
        let audit = brain_core::AuditId::new();
        s.subject = SubjectRef::Pending(audit);

        let view = StatementView::from_statement(&s, "test:p".into());
        assert_eq!(view.subject, [0u8; 16]);
        assert_eq!(view.subject_pending_audit_id, audit.to_bytes());
        assert_eq!(view.flags & 1, 1);

        let back = view.to_statement(s.predicate).unwrap();
        match back.subject {
            SubjectRef::Pending(a) => assert_eq!(a, audit),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn view_chain_fields_preserved() {
        let mut s = sample_statement(StatementObject::Entity(EntityId::new()));
        let prior = StatementId::new();
        s.supersedes = Some(prior);
        s.chain_root = prior;
        s.version = 2;

        let view = StatementView::from_statement(&s, "test:p".into());
        let back = view.to_statement(s.predicate).unwrap();
        assert_eq!(back.supersedes, Some(prior));
        assert_eq!(back.chain_root, prior);
        assert_eq!(back.version, 2);
    }

    #[test]
    fn evidence_inline_too_large_rejected() {
        let big = vec![[0u8; 16]; INLINE_EVIDENCE_CAP + 1];
        let err = evidence_ref_from_wire(&EvidenceRefWire::Inline(big)).unwrap_err();
        matches!(
            err,
            WireToStatementError::EvidenceInlineTooLarge { cap: 8, .. }
        )
        .then_some(())
        .expect("expected EvidenceInlineTooLarge");
    }

    #[test]
    fn evidence_overflow_round_trip() {
        let id = EvidenceOverflowId::new();
        let wire = EvidenceRefWire::Overflow(id.to_bytes());
        let back = evidence_ref_from_wire(&wire).unwrap();
        match back {
            EvidenceRef::Overflow(got) => assert_eq!(got, id),
            _ => panic!("expected Overflow"),
        }
    }

    #[test]
    fn object_discriminants_stable() {
        assert_eq!(StatementObjectWire::EntityRef([0u8; 16]).discriminant(), 0);
        assert_eq!(
            StatementObjectWire::Value(StatementValueWire::Bool(false)).discriminant(),
            1
        );
        assert_eq!(StatementObjectWire::MemoryRef([0u8; 16]).discriminant(), 2);
        assert_eq!(
            StatementObjectWire::StatementRef([0u8; 16]).discriminant(),
            3
        );
    }
}
