//! Request-frame payload codecs.
//!
//! One variant of [`RequestBody`] per server-bound opcode. Structured
//! fields are CBOR-encoded; the raw embedding blob for
//! `ENCODE_VECTOR_DIRECT_REQ` lives in the trailing raw section of the
//! payload and is composed at the [`crate::Frame`] layer — it is *not*
//! part of the CBOR-encoded bytes this module produces.
//!
//! ## Wire-domain types
//!
//! Each request struct uses raw representations (`u128` for `MemoryId`,
//! `[u8; 16]` for UUID-shaped IDs, `u8`-mapped enums) so the wire types
//! stay decoupled from `brain-core` value types. Conversion between
//! these wire types and `brain_core` domain types is the operation
//! handler's responsibility.

// `PlanState` and `ObservationInput` use `By*` variant naming that
// mirrors the spec's discriminator phrasing; clippy flags the family as
// same-prefixed. The module-level allow covers it without spreading
// attribute noise.
#![allow(clippy::enum_variant_names)]

use crate::codec::cbor::{from_cbor_bytes, to_cbor_bytes};
use crate::codec::opcode::Opcode;
use crate::connection::handshake::{AuthPayload, HelloPayload};
use crate::error::ProtocolError;

// ---------------------------------------------------------------------------
// Helper aliases for spec-domain primitive types as carried on the wire.
// ---------------------------------------------------------------------------

/// 16-byte UUID-shaped identifier (`AgentId`, `RequestId`, `TxnId`).
pub type WireUuid = [u8; 16];

/// Wire-side `ContextId` — 8 bytes / `u64`.
pub type WireContextId = u64;

/// Packed `MemoryId` (shard 16 + slot 48 + version 32 + reserved 32,
/// all rolled into a `u128`).
pub type WireMemoryId = u128;

// Per-op-family request payload structs live in `crate::ops` and
// `crate::connection`. Bring them in here so the `RequestBody` variants
// can address them by short name, and re-export them so external
// callers can still address them as `brain_protocol::envelope::request::X`.
pub use crate::connection::stream::{
    ByeRequest, CancelStreamRequest, ClientPongRequest, PingRequest,
};
pub use crate::ops::admin::*;
pub use crate::ops::capabilities::*;
pub use crate::ops::entity::*;
pub use crate::ops::extractor::*;
pub use crate::ops::graph::*;
pub use crate::ops::memory::*;
pub use crate::ops::procedural::*;
pub use crate::ops::query::*;
pub use crate::ops::relation::*;
pub use crate::ops::statement::*;
pub use crate::ops::subscribe::*;
pub use crate::ops::txn::*;
pub use crate::schema::ops::*;
pub use crate::shared::enums::*;
pub use crate::shared::primitives::*;

/// One variant per server-bound opcode. The variant carries the
/// CBOR-encoded structured payload; raw vector blobs (for opcodes
/// that include them) are appended by the [`crate::Frame`] layer as the
/// trailing raw section, not by this enum.
#[derive(Clone, Debug, PartialEq)]
pub enum RequestBody {
    /// Opening handshake frame (connection-level, stream 0).
    Hello(HelloPayload),
    /// Authentication frame following WELCOME.
    Auth(AuthPayload),
    Encode(EncodeRequest),
    EncodeVectorDirect(EncodeVectorDirectRequest),
    Recall(RecallRequest),
    Plan(PlanRequest),
    Reason(ReasonRequest),
    Forget(ForgetRequest),
    Link(LinkRequest),
    Unlink(UnlinkRequest),
    MemoryList(MemoryListRequest),
    MemoryInspect(MemoryInspectRequest),
    GraphFetch(GraphFetchRequest),
    Subscribe(SubscribeRequest),
    Unsubscribe(UnsubscribeRequest),
    GetCapabilities(GetCapabilitiesRequest),
    TxnBegin(TxnBeginRequest),
    TxnCommit(TxnCommitRequest),
    TxnAbort(TxnAbortRequest),
    CancelStream(CancelStreamRequest),
    Ping(PingRequest),
    ClientPong(ClientPongRequest),
    Bye(ByeRequest),
    AdminStats(AdminStatsRequest),
    AdminSnapshot(AdminSnapshotRequest),
    AdminRestore(AdminRestoreRequest),
    AdminIntegrityCheck(AdminIntegrityCheckRequest),
    AdminMigrateEmbeddings(AdminMigrateEmbeddingsRequest),
    AdminCreateContext(AdminCreateContextRequest),
    AdminRenameContext(AdminRenameContextRequest),
    AdminMoveMemory(AdminMoveMemoryRequest),
    AdminReclassify(AdminReclassifyRequest),
    AdminListTombstoned(AdminListTombstonedRequest),
    AdminListPendingContradictions(AdminListPendingContradictionsRequest),
    AdminBackfill(AdminBackfillRequest),
    AdminBackfillCancel(AdminBackfillCancelRequest),

    // Typed-graph namespace.
    EntityCreate(EntityCreateRequest),
    EntityGet(EntityGetRequest),
    EntityUpdate(EntityUpdateRequest),
    EntityRename(EntityRenameRequest),
    EntityMerge(EntityMergeRequest),
    EntityUnmerge(EntityUnmergeRequest),
    EntityResolve(EntityResolveRequest),
    EntityList(EntityListRequest),
    EntityTombstone(EntityTombstoneRequest),

    // Statement ops.
    StatementCreate(StatementCreateRequest),
    StatementGet(StatementGetRequest),
    StatementSupersede(StatementSupersedeRequest),
    StatementTombstone(StatementTombstoneRequest),
    StatementRetract(StatementRetractRequest),
    StatementHistory(StatementHistoryRequest),
    StatementList(StatementListRequest),

    // Relation ops.
    RelationCreate(RelationCreateRequest),
    RelationGet(RelationGetRequest),
    RelationSupersede(RelationSupersedeRequest),
    RelationTombstone(RelationTombstoneRequest),
    RelationListFrom(RelationListFromRequest),
    RelationListTo(RelationListToRequest),
    RelationTraverse(RelationTraverseRequest),

    // Schema ops.
    SchemaUpload(SchemaUploadRequest),
    SchemaGet(SchemaGetRequest),
    SchemaList(SchemaListRequest),
    SchemaValidate(SchemaValidateRequest),
    SchemaReplace(SchemaReplaceRequest),

    // Extractor introspection (read-only).
    ExtractorList(ExtractorListRequest),

    // Retrieval query ops.
    QueryExplain(QueryExplainRequest),
    QueryTrace(QueryTraceRequest),

    // Procedural-memory materialization. Reads an agent's stored
    // `brain:behavior_*` Preferences and renders a system block for
    // LLM prompt injection.
    MaterializeProcedural(MaterializeProceduralRequest),
}

impl RequestBody {
    /// The opcode this body corresponds to.
    #[must_use]
    pub fn opcode(&self) -> Opcode {
        match self {
            Self::Hello(_) => Opcode::Hello,
            Self::Auth(_) => Opcode::Auth,
            Self::Encode(_) => Opcode::EncodeReq,
            Self::EncodeVectorDirect(_) => Opcode::EncodeVectorDirectReq,
            Self::Recall(_) => Opcode::RecallReq,
            Self::Plan(_) => Opcode::PlanReq,
            Self::Reason(_) => Opcode::ReasonReq,
            Self::Forget(_) => Opcode::ForgetReq,
            Self::Link(_) => Opcode::LinkReq,
            Self::Unlink(_) => Opcode::UnlinkReq,
            Self::MemoryList(_) => Opcode::MemoryListReq,
            Self::MemoryInspect(_) => Opcode::MemoryInspectReq,
            Self::GraphFetch(_) => Opcode::GraphFetchReq,
            Self::Subscribe(_) => Opcode::SubscribeReq,
            Self::Unsubscribe(_) => Opcode::UnsubscribeReq,
            Self::GetCapabilities(_) => Opcode::GetCapabilitiesReq,
            Self::TxnBegin(_) => Opcode::TxnBegin,
            Self::TxnCommit(_) => Opcode::TxnCommit,
            Self::TxnAbort(_) => Opcode::TxnAbort,
            Self::CancelStream(_) => Opcode::CancelStream,
            Self::Ping(_) => Opcode::Ping,
            Self::ClientPong(_) => Opcode::ClientPong,
            Self::Bye(_) => Opcode::Bye,
            Self::AdminStats(_) => Opcode::AdminStatsReq,
            Self::AdminSnapshot(_) => Opcode::AdminSnapshotReq,
            Self::AdminRestore(_) => Opcode::AdminRestoreReq,
            Self::AdminIntegrityCheck(_) => Opcode::AdminIntegrityCheckReq,
            Self::AdminMigrateEmbeddings(_) => Opcode::AdminMigrateEmbeddingsReq,
            Self::AdminCreateContext(_) => Opcode::AdminCreateContextReq,
            Self::AdminRenameContext(_) => Opcode::AdminRenameContextReq,
            Self::AdminMoveMemory(_) => Opcode::AdminMoveMemoryReq,
            Self::AdminReclassify(_) => Opcode::AdminReclassifyReq,
            Self::AdminListTombstoned(_) => Opcode::AdminListTombstonedReq,
            Self::AdminListPendingContradictions(_) => Opcode::AdminListPendingContradictionsReq,
            Self::AdminBackfill(_) => Opcode::AdminBackfillReq,
            Self::AdminBackfillCancel(_) => Opcode::AdminBackfillCancelReq,
            Self::EntityCreate(_) => Opcode::EntityCreateReq,
            Self::EntityGet(_) => Opcode::EntityGetReq,
            Self::EntityUpdate(_) => Opcode::EntityUpdateReq,
            Self::EntityRename(_) => Opcode::EntityRenameReq,
            Self::EntityMerge(_) => Opcode::EntityMergeReq,
            Self::EntityUnmerge(_) => Opcode::EntityUnmergeReq,
            Self::EntityResolve(_) => Opcode::EntityResolveReq,
            Self::EntityList(_) => Opcode::EntityListReq,
            Self::EntityTombstone(_) => Opcode::EntityTombstoneReq,
            Self::StatementCreate(_) => Opcode::StatementCreateReq,
            Self::StatementGet(_) => Opcode::StatementGetReq,
            Self::StatementSupersede(_) => Opcode::StatementSupersedeReq,
            Self::StatementTombstone(_) => Opcode::StatementTombstoneReq,
            Self::StatementRetract(_) => Opcode::StatementRetractReq,
            Self::StatementHistory(_) => Opcode::StatementHistoryReq,
            Self::StatementList(_) => Opcode::StatementListReq,
            Self::RelationCreate(_) => Opcode::RelationCreateReq,
            Self::RelationGet(_) => Opcode::RelationGetReq,
            Self::RelationSupersede(_) => Opcode::RelationSupersedeReq,
            Self::RelationTombstone(_) => Opcode::RelationTombstoneReq,
            Self::RelationListFrom(_) => Opcode::RelationListFromReq,
            Self::RelationListTo(_) => Opcode::RelationListToReq,
            Self::RelationTraverse(_) => Opcode::RelationTraverseReq,
            Self::SchemaUpload(_) => Opcode::SchemaUploadReq,
            Self::SchemaGet(_) => Opcode::SchemaGetReq,
            Self::SchemaList(_) => Opcode::SchemaListReq,
            Self::SchemaValidate(_) => Opcode::SchemaValidateReq,
            Self::SchemaReplace(_) => Opcode::SchemaReplaceReq,
            Self::ExtractorList(_) => Opcode::ExtractorListReq,
            Self::QueryExplain(_) => Opcode::QueryExplainReq,
            Self::QueryTrace(_) => Opcode::QueryTraceReq,
            Self::MaterializeProcedural(_) => Opcode::MaterializeProceduralReq,
        }
    }

    /// Encode the structured body to bytes via CBOR. The returned vector
    /// is suitable for placement in a [`crate::Frame::payload`]; vector
    /// blobs (where this opcode supports them) are appended by callers.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Hello(r) => to_cbor_bytes(r),
            Self::Auth(r) => to_cbor_bytes(r),
            Self::Encode(r) => to_cbor_bytes(r),
            Self::EncodeVectorDirect(r) => {
                let mut p = to_cbor_bytes(r);
                p.extend_from_slice(&crate::codec::cbor::f32_slice_to_le_bytes(&r.vector));
                p
            }
            Self::Recall(r) => to_cbor_bytes(r),
            Self::Plan(r) => to_cbor_bytes(r),
            Self::Reason(r) => to_cbor_bytes(r),
            Self::Forget(r) => to_cbor_bytes(r),
            Self::Link(r) => to_cbor_bytes(r),
            Self::Unlink(r) => to_cbor_bytes(r),
            Self::MemoryList(r) => to_cbor_bytes(r),
            Self::MemoryInspect(r) => to_cbor_bytes(r),
            Self::GraphFetch(r) => to_cbor_bytes(r),
            Self::Subscribe(r) => to_cbor_bytes(r),
            Self::Unsubscribe(r) => to_cbor_bytes(r),
            Self::GetCapabilities(r) => to_cbor_bytes(r),
            Self::TxnBegin(r) => to_cbor_bytes(r),
            Self::TxnCommit(r) => to_cbor_bytes(r),
            Self::TxnAbort(r) => to_cbor_bytes(r),
            Self::CancelStream(r) => to_cbor_bytes(r),
            Self::Ping(r) => to_cbor_bytes(r),
            Self::ClientPong(r) => to_cbor_bytes(r),
            Self::Bye(r) => to_cbor_bytes(r),
            Self::AdminStats(r) => to_cbor_bytes(r),
            Self::AdminSnapshot(r) => to_cbor_bytes(r),
            Self::AdminRestore(r) => to_cbor_bytes(r),
            Self::AdminIntegrityCheck(r) => to_cbor_bytes(r),
            Self::AdminMigrateEmbeddings(r) => to_cbor_bytes(r),
            Self::AdminCreateContext(r) => to_cbor_bytes(r),
            Self::AdminRenameContext(r) => to_cbor_bytes(r),
            Self::AdminMoveMemory(r) => to_cbor_bytes(r),
            Self::AdminReclassify(r) => to_cbor_bytes(r),
            Self::AdminListTombstoned(r) => to_cbor_bytes(r),
            Self::AdminListPendingContradictions(r) => to_cbor_bytes(r),
            Self::AdminBackfill(r) => to_cbor_bytes(r),
            Self::AdminBackfillCancel(r) => to_cbor_bytes(r),
            Self::EntityCreate(r) => to_cbor_bytes(r),
            Self::EntityGet(r) => to_cbor_bytes(r),
            Self::EntityUpdate(r) => to_cbor_bytes(r),
            Self::EntityRename(r) => to_cbor_bytes(r),
            Self::EntityMerge(r) => to_cbor_bytes(r),
            Self::EntityUnmerge(r) => to_cbor_bytes(r),
            Self::EntityResolve(r) => to_cbor_bytes(r),
            Self::EntityList(r) => to_cbor_bytes(r),
            Self::EntityTombstone(r) => to_cbor_bytes(r),
            Self::StatementCreate(r) => to_cbor_bytes(r),
            Self::StatementGet(r) => to_cbor_bytes(r),
            Self::StatementSupersede(r) => to_cbor_bytes(r),
            Self::StatementTombstone(r) => to_cbor_bytes(r),
            Self::StatementRetract(r) => to_cbor_bytes(r),
            Self::StatementHistory(r) => to_cbor_bytes(r),
            Self::StatementList(r) => to_cbor_bytes(r),
            Self::RelationCreate(r) => to_cbor_bytes(r),
            Self::RelationGet(r) => to_cbor_bytes(r),
            Self::RelationSupersede(r) => to_cbor_bytes(r),
            Self::RelationTombstone(r) => to_cbor_bytes(r),
            Self::RelationListFrom(r) => to_cbor_bytes(r),
            Self::RelationListTo(r) => to_cbor_bytes(r),
            Self::RelationTraverse(r) => to_cbor_bytes(r),
            Self::SchemaUpload(r) => to_cbor_bytes(r),
            Self::SchemaGet(r) => to_cbor_bytes(r),
            Self::SchemaList(r) => to_cbor_bytes(r),
            Self::SchemaValidate(r) => to_cbor_bytes(r),
            Self::SchemaReplace(r) => to_cbor_bytes(r),
            Self::ExtractorList(r) => to_cbor_bytes(r),
            Self::QueryExplain(r) => to_cbor_bytes(r),
            Self::QueryTrace(r) => to_cbor_bytes(r),
            Self::MaterializeProcedural(r) => to_cbor_bytes(r),
        }
    }

    /// Decode `bytes` as the request body for the given server-bound
    /// `opcode`. Returns [`ProtocolError::UnknownOpcode`] for opcodes that
    /// don't carry a request body (responses, error frames).
    pub fn decode(opcode: Opcode, bytes: &[u8]) -> Result<Self, ProtocolError> {
        Ok(match opcode {
            Opcode::Hello => Self::Hello(from_cbor_bytes(bytes)?),
            Opcode::Auth => Self::Auth(from_cbor_bytes(bytes)?),
            Opcode::EncodeReq => Self::Encode(from_cbor_bytes(bytes)?),
            Opcode::EncodeVectorDirectReq => {
                let (mut req, consumed) = crate::codec::cbor::from_cbor_prefix::<
                    crate::ops::memory::EncodeVectorDirectRequest,
                >(bytes)?;
                req.vector = crate::codec::cbor::le_bytes_to_f32_vec(&bytes[consumed..])?;
                Self::EncodeVectorDirect(req)
            }
            Opcode::RecallReq => Self::Recall(from_cbor_bytes(bytes)?),
            Opcode::PlanReq => Self::Plan(from_cbor_bytes(bytes)?),
            Opcode::ReasonReq => Self::Reason(from_cbor_bytes(bytes)?),
            Opcode::ForgetReq => Self::Forget(from_cbor_bytes(bytes)?),
            Opcode::LinkReq => Self::Link(from_cbor_bytes(bytes)?),
            Opcode::UnlinkReq => Self::Unlink(from_cbor_bytes(bytes)?),
            Opcode::MemoryListReq => Self::MemoryList(from_cbor_bytes(bytes)?),
            Opcode::MemoryInspectReq => Self::MemoryInspect(from_cbor_bytes(bytes)?),
            Opcode::GraphFetchReq => Self::GraphFetch(from_cbor_bytes(bytes)?),
            Opcode::SubscribeReq => Self::Subscribe(from_cbor_bytes(bytes)?),
            Opcode::UnsubscribeReq => Self::Unsubscribe(from_cbor_bytes(bytes)?),
            Opcode::GetCapabilitiesReq => Self::GetCapabilities(from_cbor_bytes(bytes)?),
            Opcode::TxnBegin => Self::TxnBegin(from_cbor_bytes(bytes)?),
            Opcode::TxnCommit => Self::TxnCommit(from_cbor_bytes(bytes)?),
            Opcode::TxnAbort => Self::TxnAbort(from_cbor_bytes(bytes)?),
            Opcode::CancelStream => Self::CancelStream(from_cbor_bytes(bytes)?),
            Opcode::Ping => Self::Ping(from_cbor_bytes(bytes)?),
            Opcode::ClientPong => Self::ClientPong(from_cbor_bytes(bytes)?),
            Opcode::Bye => Self::Bye(from_cbor_bytes(bytes)?),
            Opcode::AdminStatsReq => Self::AdminStats(from_cbor_bytes(bytes)?),
            Opcode::AdminSnapshotReq => Self::AdminSnapshot(from_cbor_bytes(bytes)?),
            Opcode::AdminRestoreReq => Self::AdminRestore(from_cbor_bytes(bytes)?),
            Opcode::AdminIntegrityCheckReq => Self::AdminIntegrityCheck(from_cbor_bytes(bytes)?),
            Opcode::AdminMigrateEmbeddingsReq => {
                Self::AdminMigrateEmbeddings(from_cbor_bytes(bytes)?)
            }
            Opcode::AdminCreateContextReq => Self::AdminCreateContext(from_cbor_bytes(bytes)?),
            Opcode::AdminRenameContextReq => Self::AdminRenameContext(from_cbor_bytes(bytes)?),
            Opcode::AdminMoveMemoryReq => Self::AdminMoveMemory(from_cbor_bytes(bytes)?),
            Opcode::AdminReclassifyReq => Self::AdminReclassify(from_cbor_bytes(bytes)?),
            Opcode::AdminListTombstonedReq => Self::AdminListTombstoned(from_cbor_bytes(bytes)?),
            Opcode::AdminListPendingContradictionsReq => {
                Self::AdminListPendingContradictions(from_cbor_bytes(bytes)?)
            }
            Opcode::AdminBackfillReq => Self::AdminBackfill(from_cbor_bytes(bytes)?),
            Opcode::AdminBackfillCancelReq => Self::AdminBackfillCancel(from_cbor_bytes(bytes)?),
            Opcode::EntityCreateReq => Self::EntityCreate(from_cbor_bytes(bytes)?),
            Opcode::EntityGetReq => Self::EntityGet(from_cbor_bytes(bytes)?),
            Opcode::EntityUpdateReq => Self::EntityUpdate(from_cbor_bytes(bytes)?),
            Opcode::EntityRenameReq => Self::EntityRename(from_cbor_bytes(bytes)?),
            Opcode::EntityMergeReq => Self::EntityMerge(from_cbor_bytes(bytes)?),
            Opcode::EntityUnmergeReq => Self::EntityUnmerge(from_cbor_bytes(bytes)?),
            Opcode::EntityResolveReq => Self::EntityResolve(from_cbor_bytes(bytes)?),
            Opcode::EntityListReq => Self::EntityList(from_cbor_bytes(bytes)?),
            Opcode::EntityTombstoneReq => Self::EntityTombstone(from_cbor_bytes(bytes)?),
            Opcode::StatementCreateReq => Self::StatementCreate(from_cbor_bytes(bytes)?),
            Opcode::StatementGetReq => Self::StatementGet(from_cbor_bytes(bytes)?),
            Opcode::StatementSupersedeReq => Self::StatementSupersede(from_cbor_bytes(bytes)?),
            Opcode::StatementTombstoneReq => Self::StatementTombstone(from_cbor_bytes(bytes)?),
            Opcode::StatementRetractReq => Self::StatementRetract(from_cbor_bytes(bytes)?),
            Opcode::StatementHistoryReq => Self::StatementHistory(from_cbor_bytes(bytes)?),
            Opcode::StatementListReq => Self::StatementList(from_cbor_bytes(bytes)?),
            Opcode::RelationCreateReq => Self::RelationCreate(from_cbor_bytes(bytes)?),
            Opcode::RelationGetReq => Self::RelationGet(from_cbor_bytes(bytes)?),
            Opcode::RelationSupersedeReq => Self::RelationSupersede(from_cbor_bytes(bytes)?),
            Opcode::RelationTombstoneReq => Self::RelationTombstone(from_cbor_bytes(bytes)?),
            Opcode::RelationListFromReq => Self::RelationListFrom(from_cbor_bytes(bytes)?),
            Opcode::RelationListToReq => Self::RelationListTo(from_cbor_bytes(bytes)?),
            Opcode::RelationTraverseReq => Self::RelationTraverse(from_cbor_bytes(bytes)?),
            Opcode::SchemaUploadReq => Self::SchemaUpload(from_cbor_bytes(bytes)?),
            Opcode::SchemaGetReq => Self::SchemaGet(from_cbor_bytes(bytes)?),
            Opcode::SchemaListReq => Self::SchemaList(from_cbor_bytes(bytes)?),
            Opcode::SchemaValidateReq => Self::SchemaValidate(from_cbor_bytes(bytes)?),
            Opcode::SchemaReplaceReq => Self::SchemaReplace(from_cbor_bytes(bytes)?),
            Opcode::ExtractorListReq => Self::ExtractorList(from_cbor_bytes(bytes)?),
            Opcode::QueryExplainReq => Self::QueryExplain(from_cbor_bytes(bytes)?),
            Opcode::QueryTraceReq => Self::QueryTrace(from_cbor_bytes(bytes)?),
            Opcode::MaterializeProceduralReq => {
                Self::MaterializeProcedural(from_cbor_bytes(bytes)?)
            }
            other => return Err(ProtocolError::UnknownOpcode(other.as_u16())),
        })
    }
}

/// Borrow the effective-identity selector (`act_as`) carried by a
/// request body, if the op is one of the three data-plane verbs that
/// support acting on behalf of another `(namespace, agent_id)`:
/// `Encode`, `Recall`, and `Forget`. Every other variant returns
/// `None` — those ops always run as the connection's own key-bound
/// identity and carry no `act_as` field on the wire.
///
/// This is the single point the server consults to decide whether a
/// request wants to override its effective identity; keeping it here
/// (next to `RequestBody`) means new act-as-capable ops are added in
/// exactly one place.
///
/// # Examples
///
/// ```
/// use brain_protocol::{act_as_of, RequestBody, EncodeRequest, ActAs, WaitMode};
///
/// let no_override = RequestBody::Encode(EncodeRequest {
///     text: "hi".into(),
///     context_id: 0,
///     request_id: [0; 16],
///     txn_id: None,
///     occurred_at_unix_nanos: None,
///     act_as: None,
///     wait: WaitMode::Ack,
///     allow_duplicates: false,
/// });
/// assert!(act_as_of(&no_override).is_none());
///
/// let with_override = RequestBody::Encode(EncodeRequest {
///     text: "hi".into(),
///     context_id: 0,
///     request_id: [0; 16],
///     txn_id: None,
///     occurred_at_unix_nanos: None,
///     act_as: Some(ActAs { namespace: "acme".into(), agent_id: [1; 16] }),
///     wait: WaitMode::Ack,
///     allow_duplicates: false,
/// });
/// assert_eq!(act_as_of(&with_override).map(|a| a.namespace.as_str()), Some("acme"));
/// ```
#[must_use]
pub fn act_as_of(body: &RequestBody) -> Option<&ActAs> {
    match body {
        RequestBody::Encode(r) => r.act_as.as_ref(),
        RequestBody::Recall(r) => r.act_as.as_ref(),
        RequestBody::Forget(r) => r.act_as.as_ref(),
        RequestBody::Plan(r) => r.act_as.as_ref(),
        RequestBody::Reason(r) => r.act_as.as_ref(),
        RequestBody::Link(r) => r.act_as.as_ref(),
        RequestBody::Unlink(r) => r.act_as.as_ref(),
        RequestBody::MemoryList(r) => r.act_as.as_ref(),
        RequestBody::MemoryInspect(r) => r.act_as.as_ref(),
        RequestBody::GraphFetch(r) => r.act_as.as_ref(),
        RequestBody::EntityCreate(r) => r.act_as.as_ref(),
        RequestBody::EntityGet(r) => r.act_as.as_ref(),
        RequestBody::EntityList(r) => r.act_as.as_ref(),
        RequestBody::EntityResolve(r) => r.act_as.as_ref(),
        RequestBody::StatementCreate(r) => r.act_as.as_ref(),
        RequestBody::StatementGet(r) => r.act_as.as_ref(),
        RequestBody::StatementList(r) => r.act_as.as_ref(),
        RequestBody::RelationCreate(r) => r.act_as.as_ref(),
        RequestBody::RelationGet(r) => r.act_as.as_ref(),
        RequestBody::RelationListFrom(r) => r.act_as.as_ref(),
        RequestBody::RelationListTo(r) => r.act_as.as_ref(),
        RequestBody::RelationTraverse(r) => r.act_as.as_ref(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a `RequestBody` through encode → decode and assert
    /// equality. Used by every per-variant test.
    fn round_trip(body: RequestBody) {
        let bytes = body.encode();
        let decoded = RequestBody::decode(body.opcode(), &bytes)
            .unwrap_or_else(|e| panic!("decode failed for {:?}: {e}", body.opcode()));
        assert_eq!(decoded, body);
    }

    fn sample_uuid(seed: u8) -> WireUuid {
        let mut u = [0u8; 16];
        for (i, b) in u.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        u
    }

    fn sample_memory_id() -> WireMemoryId {
        // Equivalent of `MemoryId::pack(7, 0x12_3456, 42)`.
        ((7u128) << 72) | ((42u128) << 56) | 0x12_3456_u128
    }

    #[test]
    fn encode_round_trips() {
        round_trip(RequestBody::Encode(EncodeRequest {
            text: "hello brain".into(),
            context_id: 1_u64,
            request_id: sample_uuid(2),
            txn_id: Some(sample_uuid(3)),
            occurred_at_unix_nanos: Some(1_700_000_000_000_000_000),
            act_as: None,
            wait: WaitMode::Ack,
            allow_duplicates: false,
        }));
    }

    #[test]
    fn encode_round_trips_with_act_as() {
        round_trip(RequestBody::Encode(EncodeRequest {
            text: "hello brain".into(),
            context_id: 1_u64,
            request_id: sample_uuid(2),
            txn_id: None,
            occurred_at_unix_nanos: None,
            act_as: Some(ActAs {
                namespace: "acme".into(),
                agent_id: sample_uuid(9),
            }),
            wait: WaitMode::Ack,
            allow_duplicates: false,
        }));
    }

    #[test]
    fn encode_vector_direct_round_trips() {
        // A unit-norm 4-element vector is enough for the wire round
        // trip; the server-side normalisation check lives in the
        // handler and is exercised elsewhere.
        round_trip(RequestBody::EncodeVectorDirect(EncodeVectorDirectRequest {
            text: "hello brain".into(),
            vector: vec![1.0, 0.0, 0.0, 0.0],
            model_fingerprint: [0xAB; 16],
            context_id: 1_u64,
            kind: MemoryKindWire::Episodic,
            salience_hint: 0.25,
            edges: vec![EdgeRequest {
                target: sample_memory_id(),
                kind: EdgeKindWire::Caused,
                weight: 0.9,
            }],
            request_id: sample_uuid(2),
            txn_id: Some(sample_uuid(3)),
            deduplicate: true,
        }));
    }

    #[test]
    fn recall_round_trips() {
        round_trip(RequestBody::Recall(RecallRequest {
            cue_text: "what about budgets".into(),
            subject_name: "Alice".into(),
            max_results: 10,
            confidence_threshold: 0.3,
            context_filter: Some(vec![1_u64, 2_u64]),
            age_bound_unix_nanos: Some(1_700_000_000_000_000_000),
            as_of_record_time_unix_nanos: Some(1_710_000_000_000_000_000),
            kind_filter: Some(vec![MemoryKindWire::Episodic, MemoryKindWire::Semantic]),
            salience_floor: 0.1,
            include_edges: true,
            include_graph: false,
            include_text: true,
            request_id: Some(sample_uuid(7)),
            txn_id: None,
            trace: true,
            act_as: None,
        }));
    }

    #[test]
    fn plan_round_trips_with_each_state_variant() {
        for start in [
            PlanState::ByMemoryId(sample_memory_id()),
            PlanState::ByText("origin".into()),
            PlanState::ByVector {
                offset: 16,
                dim: 384,
            },
        ] {
            round_trip(RequestBody::Plan(PlanRequest {
                start: start.clone(),
                goal: PlanState::ByText("destination".into()),
                budget: PlanBudget {
                    max_steps: 10,
                    max_wall_time_ms: 1_000,
                    max_branches_explored: 100,
                },
                strategy_hint: Some(PlanStrategy::AStar),
                context_filter: None,
                request_id: None,
                txn_id: None,
                act_as: None,
            }));
        }
    }

    #[test]
    fn reason_round_trips_with_each_observation_variant() {
        for obs in [
            ObservationInput::ByMemoryId(sample_memory_id()),
            ObservationInput::ByText("an event".into()),
        ] {
            round_trip(RequestBody::Reason(ReasonRequest {
                observation: obs,
                depth: 5,
                confidence_threshold: 0.4,
                context_filter: None,
                max_inferences: 50,
                budget_wall_time_ms: 5_000,
                request_id: None,
                txn_id: None,
                act_as: None,
            }));
        }
    }

    #[test]
    fn forget_round_trips() {
        for mode in [ForgetMode::Soft, ForgetMode::Hard] {
            round_trip(RequestBody::Forget(ForgetRequest {
                memory_id: sample_memory_id(),
                mode,
                request_id: sample_uuid(8),
                txn_id: None,
                act_as: None,
            }));
        }
    }

    #[test]
    fn forget_round_trips_with_act_as() {
        round_trip(RequestBody::Forget(ForgetRequest {
            memory_id: sample_memory_id(),
            mode: ForgetMode::Hard,
            request_id: sample_uuid(8),
            txn_id: None,
            act_as: Some(ActAs {
                namespace: "acme".into(),
                agent_id: sample_uuid(9),
            }),
        }));
    }

    #[test]
    fn subscribe_round_trips() {
        round_trip(RequestBody::Subscribe(SubscribeRequest {
            filter: SubscriptionFilter {
                contexts: Some(vec![9_u64]),
                kinds: None,
                similar_to: Some(SimilarityFilter {
                    reference_memory_id: sample_memory_id(),
                    threshold: 0.85,
                }),
                agents: None,
            },
            include_history: true,
            from_lsn: Some(42),
            max_inflight: 16,
        }));
    }

    #[test]
    fn unsubscribe_round_trips() {
        round_trip(RequestBody::Unsubscribe(UnsubscribeRequest {
            target_stream_id: 7,
        }));
    }

    #[test]
    fn txn_lifecycle_round_trips() {
        let id = sample_uuid(10);
        round_trip(RequestBody::TxnBegin(TxnBeginRequest {
            txn_id: id,
            timeout_seconds: 60,
        }));
        round_trip(RequestBody::TxnCommit(TxnCommitRequest { txn_id: id }));
        round_trip(RequestBody::TxnAbort(TxnAbortRequest { txn_id: id }));
    }

    #[test]
    fn cancel_stream_round_trips() {
        for reason in [
            CancellationReason::ClientUnneeded,
            CancellationReason::Timeout,
            CancellationReason::Other("downstream cancelled".into()),
        ] {
            round_trip(RequestBody::CancelStream(CancelStreamRequest {
                target_stream_id: 9,
                reason,
            }));
        }
    }

    #[test]
    fn get_capabilities_request_round_trips() {
        round_trip(RequestBody::GetCapabilities(GetCapabilitiesRequest {}));
    }

    #[test]
    fn keepalive_and_bye_round_trip() {
        round_trip(RequestBody::Ping(PingRequest {
            client_timestamp_unix_nanos: 123_456_789,
        }));
        round_trip(RequestBody::ClientPong(ClientPongRequest {
            server_timestamp_unix_nanos: 1,
            client_timestamp_unix_nanos: 2,
        }));
        round_trip(RequestBody::Bye(ByeRequest {
            reason: Some("done".into()),
        }));
        round_trip(RequestBody::Bye(ByeRequest { reason: None }));
    }

    #[test]
    fn admin_round_trips() {
        round_trip(RequestBody::AdminStats(AdminStatsRequest {
            detail: StatsDetail::PerShard,
        }));
        round_trip(RequestBody::AdminSnapshot(AdminSnapshotRequest {
            snapshot_name: "nightly".into(),
            target_path: Some("/var/brain/snapshots/2026-05-10".into()),
            include_wal: true,
            request_id: sample_uuid(11),
        }));
        round_trip(RequestBody::AdminRestore(AdminRestoreRequest {
            snapshot_name: "nightly".into(),
            target_shard: Some(2),
            request_id: sample_uuid(12),
        }));
        round_trip(RequestBody::AdminIntegrityCheck(
            AdminIntegrityCheckRequest {
                scope: CheckScope::PerShard(vec![0, 1, 2]),
                repair_if_possible: false,
            },
        ));
        round_trip(RequestBody::AdminIntegrityCheck(
            AdminIntegrityCheckRequest {
                scope: CheckScope::QuickSample,
                repair_if_possible: true,
            },
        ));
        round_trip(RequestBody::AdminMigrateEmbeddings(
            AdminMigrateEmbeddingsRequest {
                target_model: ModelIdentifier {
                    name: "bge-large-en-v1.5".into(),
                    fingerprint: sample_uuid(13),
                },
                batch_size: 100,
                rate_limit_qps: 0,
            },
        ));
        round_trip(RequestBody::AdminCreateContext(AdminCreateContextRequest {
            name: "personal".into(),
            description: "personal notes".into(),
            request_id: sample_uuid(14),
        }));
        round_trip(RequestBody::AdminRenameContext(AdminRenameContextRequest {
            context_id: 15_u64,
            new_name: "renamed".into(),
        }));
        round_trip(RequestBody::AdminMoveMemory(AdminMoveMemoryRequest {
            memory_id: sample_memory_id(),
            new_context_id: 16_u64,
        }));
        round_trip(RequestBody::AdminReclassify(AdminReclassifyRequest {
            memory_id: sample_memory_id(),
            new_kind: MemoryKindWire::Consolidated,
        }));
        round_trip(RequestBody::AdminListTombstoned(
            AdminListTombstonedRequest {
                context_id: Some(17_u64),
                max_age_seconds: 3600,
                limit: 100,
            },
        ));
        round_trip(RequestBody::AdminListPendingContradictions(
            AdminListPendingContradictionsRequest { limit: 50 },
        ));
        round_trip(RequestBody::AdminBackfill(AdminBackfillRequest {
            scope: BackfillScope::All,
            extractor_ids: vec![1, 2, 3],
            dry_run: true,
            request_id: sample_uuid(21),
        }));
        round_trip(RequestBody::AdminBackfill(AdminBackfillRequest {
            scope: BackfillScope::MemoryRange {
                start: sample_memory_id(),
                end_inclusive: sample_memory_id().saturating_add(1024),
            },
            extractor_ids: vec![7],
            dry_run: false,
            request_id: sample_uuid(22),
        }));
        round_trip(RequestBody::AdminBackfillCancel(
            AdminBackfillCancelRequest {
                backfill_id: sample_uuid(23),
                request_id: sample_uuid(24),
            },
        ));
    }

    #[test]
    fn handshake_request_bodies_round_trip() {
        use crate::connection::handshake::{
            AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload, MtlsClaim,
        };

        for body in [
            RequestBody::Hello(HelloPayload {
                client_id: "example-client/1.0".into(),
                supported_versions: vec![crate::VERSION],
                capabilities: HelloCapabilities {
                    streaming: true,
                    compression_zstd: false,
                    server_push: false,
                },
                client_session_token: None,
            }),
            RequestBody::Auth(AuthPayload {
                method: AuthMethod::Token,
                credentials: AuthCredentials::Token(b"opaque".to_vec()),
            }),
            RequestBody::Auth(AuthPayload {
                method: AuthMethod::Mtls,
                credentials: AuthCredentials::Mtls(MtlsClaim {
                    cert_fingerprint: [9u8; 32],
                    asserted_subject: "CN=client".into(),
                }),
            }),
        ] {
            let bytes = body.encode();
            let decoded = RequestBody::decode(body.opcode(), &bytes).unwrap();
            assert_eq!(decoded, body);
        }
    }

    #[test]
    fn act_as_of_returns_selector_for_supported_ops() {
        let selector = ActAs {
            namespace: "acme".into(),
            agent_id: sample_uuid(9),
        };

        let encode = RequestBody::Encode(EncodeRequest {
            text: "x".into(),
            context_id: 0,
            request_id: sample_uuid(1),
            txn_id: None,
            occurred_at_unix_nanos: None,
            act_as: Some(selector.clone()),
            wait: WaitMode::Ack,
            allow_duplicates: false,
        });
        assert_eq!(act_as_of(&encode), Some(&selector));

        let recall = RequestBody::Recall(RecallRequest {
            cue_text: "x".into(),
            subject_name: String::new(),
            max_results: 1,
            confidence_threshold: 0.0,
            context_filter: None,
            age_bound_unix_nanos: None,
            as_of_record_time_unix_nanos: None,
            kind_filter: None,
            salience_floor: 0.0,
            include_edges: false,
            include_graph: false,
            include_text: false,
            request_id: None,
            txn_id: None,
            trace: false,
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&recall), Some(&selector));

        let forget = RequestBody::Forget(ForgetRequest {
            memory_id: sample_memory_id(),
            mode: ForgetMode::Soft,
            request_id: sample_uuid(1),
            txn_id: None,
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&forget), Some(&selector));

        let plan = RequestBody::Plan(PlanRequest {
            start: PlanState::ByText("a".into()),
            goal: PlanState::ByText("b".into()),
            budget: PlanBudget {
                max_steps: 1,
                max_wall_time_ms: 1,
                max_branches_explored: 1,
            },
            strategy_hint: None,
            context_filter: None,
            request_id: None,
            txn_id: None,
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&plan), Some(&selector));

        let reason = RequestBody::Reason(ReasonRequest {
            observation: ObservationInput::ByText("x".into()),
            depth: 1,
            confidence_threshold: 0.0,
            context_filter: None,
            max_inferences: 1,
            budget_wall_time_ms: 1,
            request_id: None,
            txn_id: None,
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&reason), Some(&selector));

        let link = RequestBody::Link(LinkRequest {
            source: sample_memory_id(),
            target: sample_memory_id(),
            kind: EdgeKindWire::Caused,
            weight: 1.0,
            request_id: sample_uuid(1),
            txn_id: None,
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&link), Some(&selector));

        let unlink = RequestBody::Unlink(UnlinkRequest {
            source: sample_memory_id(),
            target: sample_memory_id(),
            kind: EdgeKindWire::Caused,
            request_id: sample_uuid(1),
            txn_id: None,
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&unlink), Some(&selector));

        let entity_create = RequestBody::EntityCreate(EntityCreateRequest {
            entity_type_id: 1,
            canonical_name: "Ada".into(),
            aliases: Vec::new(),
            attributes_blob: Vec::new(),
            request_id: sample_uuid(1),
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&entity_create), Some(&selector));

        let entity_resolve = RequestBody::EntityResolve(EntityResolveRequest {
            candidate_name: "Ada".into(),
            context: String::new(),
            entity_type_hint: 0,
            allow_create: false,
            request_id: sample_uuid(1),
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&entity_resolve), Some(&selector));

        let entity_get = RequestBody::EntityGet(EntityGetRequest {
            entity_id: sample_uuid(1),
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&entity_get), Some(&selector));

        let entity_list = RequestBody::EntityList(EntityListRequest {
            entity_type_id: 0,
            name_prefix: String::new(),
            mention_count_min: 0,
            include_tombstoned: false,
            include_merged: false,
            limit: 100,
            cursor: Vec::new(),
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&entity_list), Some(&selector));

        let statement_create = RequestBody::StatementCreate(StatementCreateRequest {
            kind: StatementKindWire::Fact,
            subject: sample_uuid(1),
            predicate: "p".into(),
            object: StatementObjectWire::Value(StatementValueWire::Text("v".into())),
            confidence: 1.0,
            evidence: EvidenceRefWire::Inline(Vec::new()),
            extractor_id: 0,
            valid_from_unix_nanos: 0,
            valid_to_unix_nanos: 0,
            event_at_unix_nanos: 0,
            schema_version: 0,
            request_id: sample_uuid(1),
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&statement_create), Some(&selector));

        let relation_create = RequestBody::RelationCreate(RelationCreateRequest {
            relation_type: "r".into(),
            from_entity: sample_uuid(1),
            to_entity: sample_uuid(2),
            properties_blob: Vec::new(),
            evidence: EvidenceRefWire::Inline(Vec::new()),
            extractor_id: 0,
            confidence: 1.0,
            valid_from_unix_nanos: 0,
            valid_to_unix_nanos: 0,
            request_id: sample_uuid(1),
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&relation_create), Some(&selector));

        let relation_traverse = RequestBody::RelationTraverse(RelationTraverseRequest {
            start_entity: sample_uuid(1),
            relation_types: Vec::new(),
            direction: 0,
            max_depth: 3,
            max_nodes: 100,
            time_at_unix_nanos: 0,
            include_superseded: false,
            request_id: sample_uuid(1),
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&relation_traverse), Some(&selector));

        let statement_get = RequestBody::StatementGet(StatementGetRequest {
            statement_id: sample_uuid(1),
            follow_supersession: false,
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&statement_get), Some(&selector));

        let statement_list = RequestBody::StatementList(StatementListRequest {
            subject: sample_uuid(1),
            predicate: String::new(),
            kind: 0,
            min_confidence: 0.0,
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            only_current: false,
            include_tombstoned: false,
            limit: 100,
            cursor: Vec::new(),
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&statement_list), Some(&selector));

        let relation_get = RequestBody::RelationGet(RelationGetRequest {
            relation_id: sample_uuid(1),
            follow_supersession: false,
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&relation_get), Some(&selector));

        let relation_list_from = RequestBody::RelationListFrom(RelationListFromRequest {
            from_entity: sample_uuid(1),
            relation_type_filter: String::new(),
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            include_superseded: false,
            include_tombstoned: false,
            limit: 100,
            cursor: Vec::new(),
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&relation_list_from), Some(&selector));

        let relation_list_to = RequestBody::RelationListTo(RelationListToRequest {
            to_entity: sample_uuid(1),
            relation_type_filter: String::new(),
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            include_superseded: false,
            include_tombstoned: false,
            limit: 100,
            cursor: Vec::new(),
            act_as: Some(selector.clone()),
        });
        assert_eq!(act_as_of(&relation_list_to), Some(&selector));
    }

    #[test]
    fn act_as_of_returns_none_when_absent_or_unsupported() {
        // Supported op, but no override set.
        let encode = RequestBody::Encode(EncodeRequest {
            text: "x".into(),
            context_id: 0,
            request_id: sample_uuid(1),
            txn_id: None,
            occurred_at_unix_nanos: None,
            act_as: None,
            wait: WaitMode::Ack,
            allow_duplicates: false,
        });
        assert!(act_as_of(&encode).is_none());

        // Op that does not carry an `act_as` field at all.
        let ping = RequestBody::Ping(PingRequest {
            client_timestamp_unix_nanos: 0,
        });
        assert!(act_as_of(&ping).is_none());
    }

    #[test]
    fn opcode_matches_variant() {
        // Cross-check that every variant reports its expected opcode.
        let cases: &[(RequestBody, Opcode)] = &[
            (
                RequestBody::Ping(PingRequest {
                    client_timestamp_unix_nanos: 0,
                }),
                Opcode::Ping,
            ),
            (RequestBody::Bye(ByeRequest { reason: None }), Opcode::Bye),
            (
                RequestBody::Unsubscribe(UnsubscribeRequest {
                    target_stream_id: 0,
                }),
                Opcode::UnsubscribeReq,
            ),
        ];
        for (body, opcode) in cases {
            assert_eq!(body.opcode(), *opcode);
        }
    }

    #[test]
    fn decode_with_response_opcode_returns_unknown() {
        // Response opcodes don't carry request bodies. Feeding one to
        // `RequestBody::decode` must error rather than panic.
        let any_bytes = vec![0u8; 8];
        let err = RequestBody::decode(Opcode::EncodeResp, &any_bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::UnknownOpcode(_)));
    }

    #[test]
    fn decode_garbage_returns_malformed() {
        let garbage = vec![0xAAu8; 64];
        let err = RequestBody::decode(Opcode::EncodeReq, &garbage).unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedPayload(_)));
    }

    #[test]
    fn schema_replace_request_round_trips() {
        round_trip(RequestBody::SchemaReplace(SchemaReplaceRequest {
            schema_document: "namespace acme\ndefine entity_type Widget { attributes {} }\n".into(),
            force_drop_existing: true,
            request_id: [0xAB; 16],
        }));
    }

    // Wire fuzz: arbitrary bytes fed to RecallReq / EncodeReq decode
    // must never panic. The wire path is the trust boundary; a panic
    // here is a remote-DOS vector.

    use proptest::collection::vec as pvec;
    use proptest::prelude::*;

    proptest! {
        // 256 cases — exhausts the byte-discriminant space many
        // times over while staying well under 1 second wall-time.
        #![proptest_config(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        })]

        #[test]
        fn arbitrary_bytes_decode_never_panics_recall(bytes in pvec(any::<u8>(), 0..=512)) {
            // Catch UB-free: the function must return Result, never
            // unwind. We don't care which Result variant — just
            // that no panic escapes.
            let _ = RequestBody::decode(Opcode::RecallReq, &bytes);
        }

        #[test]
        fn arbitrary_bytes_decode_never_panics_encode(bytes in pvec(any::<u8>(), 0..=512)) {
            let _ = RequestBody::decode(Opcode::EncodeReq, &bytes);
        }
    }
}
