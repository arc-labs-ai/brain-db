//! Relation-op request payloads.
//!
//! Mirrors the value-side `brain_core::Relation` /
//! `RelationType` but uses wire-domain primitives so the wire types
//! stay decoupled from brain-core. Conversion lives in
//! [`crate::responses::relation`] alongside `RelationView`.

use crate::envelope::request::WireUuid;
use crate::ops::statement::EvidenceRefWire;

// ---------------------------------------------------------------------------
// Request structs.
// ---------------------------------------------------------------------------

/// `RELATION_CREATE` (`0x0150`).
///
/// Server allocates `relation_id`. `relation_type` is the canonical
/// `"namespace:name"` form; handler resolves via
/// `brain_metadata::relation_type_lookup_by_qname`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationCreateRequest {
    pub relation_type: String,
    #[serde(with = "serde_bytes")]
    pub from_entity: WireUuid,
    #[serde(with = "serde_bytes")]
    pub to_entity: WireUuid,
    pub properties_blob: Vec<u8>,
    pub evidence: EvidenceRefWire,
    pub extractor_id: u32,
    pub confidence: f32,
    pub valid_from_unix_nanos: u64,
    pub valid_to_unix_nanos: u64,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `RELATION_GET` (`0x0151`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RelationGetRequest {
    #[serde(with = "serde_bytes")]
    pub relation_id: WireUuid,
    pub follow_supersession: bool,
}

/// `RELATION_SUPERSEDE` (`0x0152`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationSupersedeRequest {
    #[serde(with = "serde_bytes")]
    pub old_relation_id: WireUuid,
    pub new_relation: RelationCreateRequest,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `RELATION_TOMBSTONE` (`0x0153`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationTombstoneRequest {
    #[serde(with = "serde_bytes")]
    pub relation_id: WireUuid,
    pub reason: String,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `RELATION_LIST_FROM` (`0x0154`).
///
/// `relation_type_filter == ""` → any type.
/// `time_range_*_unix_nanos == 0` → no time bound.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationListFromRequest {
    #[serde(with = "serde_bytes")]
    pub from_entity: WireUuid,
    pub relation_type_filter: String,
    pub time_range_start_unix_nanos: u64,
    pub time_range_end_unix_nanos: u64,
    pub include_superseded: bool,
    pub include_tombstoned: bool,
    pub limit: u32,
    pub cursor: Vec<u8>,
}

/// `RELATION_LIST_TO` (`0x0155`).
///
/// Identical shape to LIST_FROM but filters on `to_entity`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationListToRequest {
    #[serde(with = "serde_bytes")]
    pub to_entity: WireUuid,
    pub relation_type_filter: String,
    pub time_range_start_unix_nanos: u64,
    pub time_range_end_unix_nanos: u64,
    pub include_superseded: bool,
    pub include_tombstoned: bool,
    pub limit: u32,
    pub cursor: Vec<u8>,
}

/// `RELATION_TRAVERSE` (`0x0156`).
///
/// `direction`: `0` = Outgoing / `1` = Incoming / `2` = Both.
/// `max_depth` clamped to `MAX_DEPTH = 5`.
/// `max_nodes` ≤ 1000.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationTraverseRequest {
    #[serde(with = "serde_bytes")]
    pub start_entity: WireUuid,
    pub relation_types: Vec<String>,
    pub direction: u8,
    pub max_depth: u32,
    pub max_nodes: u32,
    pub time_at_unix_nanos: u64,
    pub include_superseded: bool,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

// ---------------------------------------------------------------------------
// Tests live in `statement_req.rs`-style colocation; round-trip tests
// for the relation opcodes lives in `relation_resp.rs::tests`
// (response + request share the test harness with the same
// req_round_trip / resp_round_trip helpers).
// ---------------------------------------------------------------------------

// ============================================================
// Response payloads
// ============================================================

use brain_core::{
    EntityId, ExtractorId, MemoryId, Relation, RelationId, RelationType, RelationTypeId,
};

// ---------------------------------------------------------------------------
// RelationView — read-side projection.
// ---------------------------------------------------------------------------

/// Wire-domain projection of `brain_core::Relation`.
/// Optional value-side fields collapse to sentinel zero
/// (`[0; 16]` / `0`).
///
/// `relation_type` is the canonical `"namespace:name"` form —
/// the server resolves `RelationTypeId` ↔ string via the registry
/// at projection time.
///
/// `flags`: bit 0 = `is_symmetric` (mirrored from the row).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationView {
    #[serde(with = "serde_bytes")]
    pub relation_id: WireUuid,
    #[serde(with = "serde_bytes")]
    pub chain_root: WireUuid,
    pub relation_type: String,
    #[serde(with = "serde_bytes")]
    pub from_entity: WireUuid,
    #[serde(with = "serde_bytes")]
    pub to_entity: WireUuid,
    pub properties_blob: Vec<u8>,
    pub evidence: EvidenceRefWire,
    pub extractor_id: u32,
    pub extracted_at_unix_nanos: u64,
    pub confidence: f32,
    pub valid_from_unix_nanos: u64,
    pub valid_to_unix_nanos: u64,
    pub version: u32,
    #[serde(with = "serde_bytes")]
    pub superseded_by: WireUuid,
    #[serde(with = "serde_bytes")]
    pub supersedes: WireUuid,
    pub tombstoned: bool,
    pub tombstoned_at_unix_nanos: u64,
    pub flags: u32,
}

impl RelationView {
    /// Build the wire view from a brain-core `Relation`. The handler
    /// resolves `relation_type_qname` via the registry.
    #[must_use]
    pub fn from_relation(r: &Relation, relation_type_qname: String) -> Self {
        let memory_ids: Vec<[u8; 16]> = r.evidence.iter().map(|m| m.to_be_bytes()).collect();
        let flags = if r.is_symmetric { 1u32 } else { 0u32 };
        Self {
            relation_id: r.id.to_bytes(),
            chain_root: r.chain_root.to_bytes(),
            relation_type: relation_type_qname,
            from_entity: r.from_entity.to_bytes(),
            to_entity: r.to_entity.to_bytes(),
            properties_blob: r.properties_blob.clone(),
            evidence: EvidenceRefWire::Inline(memory_ids),
            extractor_id: r.extractor_id.raw(),
            extracted_at_unix_nanos: r.extracted_at_unix_nanos,
            confidence: r.confidence,
            valid_from_unix_nanos: r.valid_from_unix_nanos.unwrap_or(0),
            valid_to_unix_nanos: r.valid_to_unix_nanos.unwrap_or(0),
            version: r.version,
            superseded_by: r
                .superseded_by
                .map(RelationId::to_bytes)
                .unwrap_or([0u8; 16]),
            supersedes: r.supersedes.map(RelationId::to_bytes).unwrap_or([0u8; 16]),
            tombstoned: r.tombstoned,
            tombstoned_at_unix_nanos: r.tombstoned_at_unix_nanos.unwrap_or(0),
            flags,
        }
    }

    /// Project a wire view back to a brain-core `Relation`. The caller
    /// supplies the resolved `relation_type` id (string canonical form
    /// is the wire transport, not the storage primitive).
    pub fn to_relation(
        &self,
        relation_type: RelationTypeId,
    ) -> Result<Relation, RelationWireError> {
        let evidence = match &self.evidence {
            EvidenceRefWire::Inline(memory_ids) => memory_ids
                .iter()
                .map(|b| MemoryId::from_be_bytes(*b))
                .collect::<Vec<_>>(),
            EvidenceRefWire::Overflow(_) => {
                return Err(RelationWireError::OverflowEvidenceNotSupported)
            }
        };

        let opt_id = |b: [u8; 16]| {
            if b == [0u8; 16] {
                None
            } else {
                Some(RelationId::from_bytes(b))
            }
        };
        let opt_nz = |v: u64| if v == 0 { None } else { Some(v) };

        Ok(Relation {
            id: RelationId::from_bytes(self.relation_id),
            relation_type,
            from_entity: EntityId::from_bytes(self.from_entity),
            to_entity: EntityId::from_bytes(self.to_entity),
            properties_blob: self.properties_blob.clone(),
            confidence: self.confidence,
            evidence,
            extractor_id: ExtractorId::from(self.extractor_id),
            extracted_at_unix_nanos: self.extracted_at_unix_nanos,
            valid_from_unix_nanos: opt_nz(self.valid_from_unix_nanos),
            valid_to_unix_nanos: opt_nz(self.valid_to_unix_nanos),
            version: self.version,
            superseded_by: opt_id(self.superseded_by),
            supersedes: opt_id(self.supersedes),
            chain_root: RelationId::from_bytes(self.chain_root),
            tombstoned: self.tombstoned,
            tombstoned_at_unix_nanos: opt_nz(self.tombstoned_at_unix_nanos),
            is_symmetric: self.flags & 1 != 0,
        })
    }
}

#[derive(thiserror::Error, Debug)]
pub enum RelationWireError {
    #[error("RELATION evidence overflow not supported in v1")]
    OverflowEvidenceNotSupported,
}

// ---------------------------------------------------------------------------
// Traversal wire shape.
// ---------------------------------------------------------------------------

/// One step in a `RELATION_TRAVERSE` path. Mirrors
/// `brain_metadata::relation::traversal::TraversalStep` but uses wire
/// primitives + canonical type string.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TraversalStepWire {
    #[serde(with = "serde_bytes")]
    pub relation_id: WireUuid,
    #[serde(with = "serde_bytes")]
    pub from: WireUuid,
    #[serde(with = "serde_bytes")]
    pub to: WireUuid,
    pub relation_type: String,
    pub depth: u32,
}

/// One full path. The traversal returns N of these in a single
/// response frame.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TraversalPathWire {
    pub steps: Vec<TraversalStepWire>,
}

// ---------------------------------------------------------------------------
// Response structs.
// ---------------------------------------------------------------------------

/// Reply to `RELATION_CREATE` (`0x01D0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RelationCreateResponse {
    #[serde(with = "serde_bytes")]
    pub relation_id: WireUuid,
}

/// Reply to `RELATION_GET` (`0x01D1`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationGetResponse {
    pub relation: RelationView,
    pub returned_via_supersession: bool,
}

/// Reply to `RELATION_SUPERSEDE` (`0x01D2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RelationSupersedeResponse {
    #[serde(with = "serde_bytes")]
    pub new_relation_id: WireUuid,
    pub version: u32,
}

/// Reply to `RELATION_TOMBSTONE` (`0x01D3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RelationTombstoneResponse {
    pub tombstoned_at_unix_nanos: u64,
}

/// Reply to `RELATION_LIST_FROM` (`0x01D4`) — single-frame snapshot
/// in v1 (cursor pagination + true streaming is a follow-up).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationListFromResponseFrame {
    pub items: Vec<RelationView>,
    pub next_cursor: Vec<u8>,
    pub cumulative_count: u32,
    pub is_final: bool,
}

impl RelationListFromResponseFrame {
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.is_final
    }
}

/// Reply to `RELATION_LIST_TO` (`0x01D5`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationListToResponseFrame {
    pub items: Vec<RelationView>,
    pub next_cursor: Vec<u8>,
    pub cumulative_count: u32,
    pub is_final: bool,
}

impl RelationListToResponseFrame {
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.is_final
    }
}

/// Reply to `RELATION_TRAVERSE` (`0x01D6`) — single-frame snapshot
/// carrying `Vec<TraversalPathWire>`; the per-frame streaming variant
/// is a follow-up.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationTraverseResponseFrame {
    pub paths: Vec<TraversalPathWire>,
    pub total_paths: u32,
    pub truncated: bool,
    pub is_final: bool,
}

impl RelationTraverseResponseFrame {
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.is_final
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers — RelationType ↔ wire qname (handler-resolved).
// ---------------------------------------------------------------------------

/// Canonical qname for a `RelationType` row. Pure helper.
#[must_use]
pub fn relation_type_canonical(rt: &RelationType) -> String {
    rt.canonical()
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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

    fn sample_create_request() -> RelationCreateRequest {
        RelationCreateRequest {
            relation_type: "test:knows".into(),
            from_entity: sample_uuid(1),
            to_entity: sample_uuid(2),
            properties_blob: Vec::new(),
            evidence: EvidenceRefWire::Inline(vec![[7u8; 16]]),
            extractor_id: 0,
            confidence: 0.9,
            valid_from_unix_nanos: 0,
            valid_to_unix_nanos: 0,
            request_id: sample_uuid(3),
        }
    }

    fn sample_view() -> RelationView {
        RelationView {
            relation_id: sample_uuid(10),
            chain_root: sample_uuid(10),
            relation_type: "test:knows".into(),
            from_entity: sample_uuid(11),
            to_entity: sample_uuid(12),
            properties_blob: Vec::new(),
            evidence: EvidenceRefWire::Inline(vec![[5u8; 16]]),
            extractor_id: 0,
            extracted_at_unix_nanos: 1_700_000_000_000_000_000,
            confidence: 0.9,
            valid_from_unix_nanos: 0,
            valid_to_unix_nanos: 0,
            version: 1,
            superseded_by: [0u8; 16],
            supersedes: [0u8; 16],
            tombstoned: false,
            tombstoned_at_unix_nanos: 0,
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
    fn relation_opcode_byte_assignments() {
        assert_eq!(Opcode::RelationCreateReq.as_u16(), 0x0150);
        assert_eq!(Opcode::RelationCreateResp.as_u16(), 0x01D0);
        assert_eq!(Opcode::RelationGetReq.as_u16(), 0x0151);
        assert_eq!(Opcode::RelationGetResp.as_u16(), 0x01D1);
        assert_eq!(Opcode::RelationSupersedeReq.as_u16(), 0x0152);
        assert_eq!(Opcode::RelationSupersedeResp.as_u16(), 0x01D2);
        assert_eq!(Opcode::RelationTombstoneReq.as_u16(), 0x0153);
        assert_eq!(Opcode::RelationTombstoneResp.as_u16(), 0x01D3);
        assert_eq!(Opcode::RelationListFromReq.as_u16(), 0x0154);
        assert_eq!(Opcode::RelationListFromResp.as_u16(), 0x01D4);
        assert_eq!(Opcode::RelationListToReq.as_u16(), 0x0155);
        assert_eq!(Opcode::RelationListToResp.as_u16(), 0x01D5);
        assert_eq!(Opcode::RelationTraverseReq.as_u16(), 0x0156);
        assert_eq!(Opcode::RelationTraverseResp.as_u16(), 0x01D6);

        assert!(Opcode::RelationCreateReq.is_typed_graph());
        assert!(Opcode::RelationCreateReq.is_request());
        assert!(Opcode::RelationCreateResp.is_response());
    }

    // ----- Requests -----

    #[test]
    fn relation_create_request_roundtrip() {
        req_round_trip(RequestBody::RelationCreate(sample_create_request()));
    }

    #[test]
    fn relation_get_request_roundtrip() {
        for follow in [true, false] {
            req_round_trip(RequestBody::RelationGet(RelationGetRequest {
                relation_id: sample_uuid(20),
                follow_supersession: follow,
            }));
        }
    }

    #[test]
    fn relation_supersede_request_roundtrip() {
        req_round_trip(RequestBody::RelationSupersede(RelationSupersedeRequest {
            old_relation_id: sample_uuid(30),
            new_relation: sample_create_request(),
            request_id: sample_uuid(31),
        }));
    }

    #[test]
    fn relation_tombstone_request_roundtrip() {
        req_round_trip(RequestBody::RelationTombstone(RelationTombstoneRequest {
            relation_id: sample_uuid(40),
            reason: "test reason".into(),
            request_id: sample_uuid(41),
        }));
    }

    #[test]
    fn relation_list_from_request_roundtrip() {
        req_round_trip(RequestBody::RelationListFrom(RelationListFromRequest {
            from_entity: sample_uuid(50),
            relation_type_filter: "test:knows".into(),
            time_range_start_unix_nanos: 1,
            time_range_end_unix_nanos: 100,
            include_superseded: false,
            include_tombstoned: false,
            limit: 100,
            cursor: Vec::new(),
        }));
    }

    #[test]
    fn relation_list_to_request_roundtrip() {
        req_round_trip(RequestBody::RelationListTo(RelationListToRequest {
            to_entity: sample_uuid(60),
            relation_type_filter: String::new(),
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            include_superseded: false,
            include_tombstoned: false,
            limit: 100,
            cursor: Vec::new(),
        }));
    }

    #[test]
    fn relation_traverse_request_roundtrip() {
        req_round_trip(RequestBody::RelationTraverse(RelationTraverseRequest {
            start_entity: sample_uuid(70),
            relation_types: vec!["test:knows".into()],
            direction: 0,
            max_depth: 3,
            max_nodes: 100,
            time_at_unix_nanos: 0,
            include_superseded: false,
            request_id: sample_uuid(71),
        }));
    }

    // ----- Responses -----

    #[test]
    fn relation_responses_roundtrip() {
        resp_round_trip(ResponseBody::RelationCreate(RelationCreateResponse {
            relation_id: sample_uuid(80),
        }));
        resp_round_trip(ResponseBody::RelationGet(RelationGetResponse {
            relation: sample_view(),
            returned_via_supersession: false,
        }));
        resp_round_trip(ResponseBody::RelationSupersede(RelationSupersedeResponse {
            new_relation_id: sample_uuid(81),
            version: 2,
        }));
        resp_round_trip(ResponseBody::RelationTombstone(RelationTombstoneResponse {
            tombstoned_at_unix_nanos: 1_700_000_000_000_000_000,
        }));
        resp_round_trip(ResponseBody::RelationListFrom(
            RelationListFromResponseFrame {
                items: vec![sample_view()],
                next_cursor: Vec::new(),
                cumulative_count: 1,
                is_final: true,
            },
        ));
        resp_round_trip(ResponseBody::RelationListTo(RelationListToResponseFrame {
            items: vec![sample_view()],
            next_cursor: Vec::new(),
            cumulative_count: 1,
            is_final: true,
        }));
        resp_round_trip(ResponseBody::RelationTraverse(
            RelationTraverseResponseFrame {
                paths: vec![TraversalPathWire {
                    steps: vec![TraversalStepWire {
                        relation_id: sample_uuid(90),
                        from: sample_uuid(91),
                        to: sample_uuid(92),
                        relation_type: "test:knows".into(),
                        depth: 1,
                    }],
                }],
                total_paths: 1,
                truncated: false,
                is_final: true,
            },
        ));
    }

    // ----- View conversion -----

    #[test]
    fn view_round_trip() {
        let view = sample_view();
        let r = view.to_relation(RelationTypeId::from(7)).unwrap();
        let view2 = RelationView::from_relation(&r, "test:knows".into());
        // The wire view doesn't preserve `RelationTypeId`; it carries
        // the canonical string. Verify the rest of the fields match
        // and the predicate string is preserved.
        assert_eq!(view, view2);
    }

    #[test]
    fn view_symmetric_flag() {
        let mut view = sample_view();
        view.flags = 1;
        let r = view.to_relation(RelationTypeId::from(7)).unwrap();
        assert!(r.is_symmetric);
        let view2 = RelationView::from_relation(&r, "test:knows".into());
        assert_eq!(view2.flags & 1, 1);
    }

    #[test]
    fn view_overflow_evidence_rejected() {
        let mut view = sample_view();
        view.evidence = EvidenceRefWire::Overflow(sample_uuid(99));
        let err = view.to_relation(RelationTypeId::from(7)).unwrap_err();
        matches!(err, RelationWireError::OverflowEvidenceNotSupported)
            .then_some(())
            .expect("expected OverflowEvidenceNotSupported");
    }
}
