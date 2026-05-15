//! Knowledge-layer wire types — namespace `0x01xx` (spec §28/00).
//!
//! Phase 16.6c lands the four entity opcodes:
//!
//! - `ENTITY_CREATE`  (0x0130) / `ENTITY_CREATE_RESP`  (0x01B0)
//! - `ENTITY_GET`     (0x0131) / `ENTITY_GET_RESP`     (0x01B1)
//! - `ENTITY_UPDATE`  (0x0132) / `ENTITY_UPDATE_RESP`  (0x01B2)
//! - `ENTITY_RENAME`  (0x0133) / `ENTITY_RENAME_RESP`  (0x01B3)
//!
//! Other §28 opcodes (schema / statement / relation / query / admin /
//! extractor) land in phases 17–24.
//!
//! Each request and response is an rkyv-archivable struct, following
//! the substrate convention (see `crate::requests::cognitive`).

pub mod entity_req;
pub mod entity_resp;
pub mod events;
pub mod statement_req;
pub mod statement_resp;

pub use entity_req::{
    EntityCreateRequest, EntityGetRequest, EntityListRequest, EntityMergeRequest,
    EntityRenameRequest, EntityResolveRequest, EntityTombstoneRequest, EntityUnmergeRequest,
    EntityUpdateRequest,
};
pub use entity_resp::{
    EntityCreateResponse, EntityGetResponse, EntityListItem, EntityListResponseFrame,
    EntityMergeResponse, EntityRenameResponse, EntityResolveResponse, EntityTombstoneResponse,
    EntityUnmergeResponse, EntityUpdateResponse, EntityView, ResolutionOutcomeWire,
};
pub use events::{
    EntityCreatedEvent, EntityMergedEvent, EntityRenamedEvent, EntityTombstonedEvent,
    EntityUnmergedEvent, EntityUpdatedEvent, ExtractionCompletedEvent, ExtractionFailedEvent,
    KnowledgeEventPayload, RelationCreatedEvent, RelationSupersededEvent, SchemaUpdatedEvent,
    StatementCreatedEvent, StatementSupersededEvent, StatementTombstonedEvent,
};
pub use statement_req::{
    EvidenceRefWire, StatementCreateRequest, StatementGetRequest, StatementHistoryRequest,
    StatementKindWire, StatementListRequest, StatementObjectWire, StatementRetractRequest,
    StatementSupersedeRequest, StatementTombstoneRequest, StatementValueWire,
};
pub use statement_resp::{
    evidence_ref_from_wire, evidence_ref_to_wire, statement_kind_from_wire,
    statement_kind_to_wire, statement_object_from_wire, statement_object_to_wire,
    statement_value_from_wire, statement_value_to_wire, StatementCreateResponse,
    StatementGetResponse, StatementHistoryResponseFrame, StatementListResponseFrame,
    StatementRetractResponse, StatementSupersedeResponse, StatementTombstoneResponse,
    StatementView, WireToStatementError,
};
