//! Relation-op request payloads.
//!
//! Mirrors the value-side `brain_core::Relation` /
//! `RelationType` but uses wire-domain primitives so rkyv derives
//! fire without coupling brain-core to rkyv. Conversion lives in
//! [`crate::responses::relation`] alongside `RelationView`.

use rkyv::{Archive, Deserialize, Serialize};

use crate::requests::statement::EvidenceRefWire;
use crate::request::WireUuid;

// ---------------------------------------------------------------------------
// Request structs.
// ---------------------------------------------------------------------------

/// `RELATION_CREATE` (`0x0150`).
///
/// Server allocates `relation_id`. `relation_type` is the canonical
/// `"namespace:name"` form; handler resolves via
/// `brain_metadata::relation_type_lookup_by_qname`.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RelationCreateRequest {
    pub relation_type: String,
    pub from_entity: WireUuid,
    pub to_entity: WireUuid,
    pub properties_blob: Vec<u8>,
    pub evidence: EvidenceRefWire,
    pub extractor_id: u32,
    pub confidence: f32,
    pub valid_from_unix_nanos: u64,
    pub valid_to_unix_nanos: u64,
    pub request_id: WireUuid,
}

/// `RELATION_GET` (`0x0151`).
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RelationGetRequest {
    pub relation_id: WireUuid,
    pub follow_supersession: bool,
}

/// `RELATION_SUPERSEDE` (`0x0152`).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RelationSupersedeRequest {
    pub old_relation_id: WireUuid,
    pub new_relation: RelationCreateRequest,
    pub request_id: WireUuid,
}

/// `RELATION_TOMBSTONE` (`0x0153`).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RelationTombstoneRequest {
    pub relation_id: WireUuid,
    pub reason: String,
    pub request_id: WireUuid,
}

/// `RELATION_LIST_FROM` (`0x0154`).
///
/// `relation_type_filter == ""` → any type.
/// `time_range_*_unix_nanos == 0` → no time bound.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RelationListFromRequest {
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
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RelationListToRequest {
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
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RelationTraverseRequest {
    pub start_entity: WireUuid,
    pub relation_types: Vec<String>,
    pub direction: u8,
    pub max_depth: u32,
    pub max_nodes: u32,
    pub time_at_unix_nanos: u64,
    pub include_superseded: bool,
    pub request_id: WireUuid,
}

// ---------------------------------------------------------------------------
// Tests live in `statement_req.rs`-style colocation; round-trip tests
// for the relation opcodes lives in `relation_resp.rs::tests`
// (response + request share the test harness with the same
// req_round_trip / resp_round_trip helpers).
// ---------------------------------------------------------------------------
