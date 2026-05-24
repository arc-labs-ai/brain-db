//! Response-frame payload codecs.
//!
//! One variant of [`ResponseBody`] per client-bound opcode. Mirrors
//! `crate::request` exactly: rkyv-archivable structs for the structured
//! fields, raw vector blobs (where applicable) appended at the
//! [`crate::Frame`] layer.
//!
//! ## Streaming
//!
//! Several opcodes stream multiple response frames over a single stream
//! (`RECALL_RESP`, `PLAN_RESP`, `REASON_RESP`, `SUBSCRIBE_EVENT`,
//! `ADMIN_MIGRATE_EMBEDDINGS_RESP`, `ADMIN_LIST_TOMBSTONED_RESP`). Each
//! emitted frame is one variant payload; the *last* frame of a stream
//! sets the header's `EOS` flag and the body's `is_final = true`.
//! [`ResponseBody::is_final`] surfaces the body-side signal so a
//! Frame-layer dispatcher can cross-check against the header.
//!
//! ## ERROR-frame mirror enums
//!
//! The ERROR body ties to `ErrorCode` / `ErrorCategory`. Those enums
//! live in [`crate::error`] and are intentionally `#[non_exhaustive]`
//! for forward-compat. We mirror them here as plain rkyv-archivable
//! enums so wire encoding/decoding is closed, and convert at the
//! boundary via `From` impls.

use crate::codec::opcode::Opcode;
use crate::codec::rkyv::{from_rkyv_bytes, to_rkyv_bytes};
use crate::connection::handshake::{AuthOkPayload, WelcomePayload};
use crate::error::ProtocolError;

// ---------------------------------------------------------------------------
// Helper enums shared by multiple response bodies.
// ---------------------------------------------------------------------------

// Per-op-family response payload structs live under `crate::ops`,
// `crate::connection`, `crate::schema`, `crate::shared`, and
// `crate::envelope::error`. Re-exported here so external callers continue
// to address them as `brain_protocol::envelope::response::EncodeResponse`
// etc.
pub use crate::connection::stream::{CancelStreamAck, PongResponse, ServerPingResponse};
pub use crate::envelope::error::{ErrorDetails, ErrorResponse};
pub use crate::ops::admin::*;
pub use crate::ops::entity::*;
pub use crate::ops::extractor::*;
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

/// One variant per client-bound opcode. Mirrors
/// [`crate::envelope::request::RequestBody`]; raw vector blobs (where applicable)
/// live in the trailing section of [`crate::Frame::payload`] and are
/// not part of the rkyv-encoded bytes this module produces.
#[derive(Clone, Debug, PartialEq)]
pub enum ResponseBody {
    /// Server reply to HELLO (connection-level, stream 0).
    Welcome(WelcomePayload),
    /// Server confirmation of authentication.
    AuthOk(AuthOkPayload),
    Encode(EncodeResponse),
    Recall(RecallResponseFrame),
    Plan(PlanResponseFrame),
    Reason(ReasonResponseFrame),
    Forget(ForgetResponse),
    Link(LinkResponse),
    Unlink(UnlinkResponse),
    SubscribeEvent(SubscriptionEvent),
    Unsubscribe(UnsubscribeResponse),
    TxnBegin(TxnBeginResponse),
    TxnCommit(TxnCommitResponse),
    TxnAbort(TxnAbortResponse),
    CancelStreamAck(CancelStreamAck),
    Pong(PongResponse),
    ServerPing(ServerPingResponse),
    AdminStats(AdminStatsResponse),
    AdminSnapshot(AdminSnapshotResponse),
    AdminRestore(AdminRestoreResponse),
    AdminIntegrityCheck(AdminIntegrityCheckResponse),
    AdminMigrateEmbeddings(AdminMigrateEmbeddingsResponseFrame),
    AdminCreateContext(AdminCreateContextResponse),
    AdminRenameContext(AdminRenameContextResponse),
    AdminMoveMemory(AdminMoveMemoryResponse),
    AdminReclassify(AdminReclassifyResponse),
    AdminListTombstoned(AdminListTombstonedResponseFrame),
    AdminBackfill(AdminBackfillResponse),
    AdminBackfillCancel(AdminBackfillCancelResponse),

    // Typed-graph namespace.
    EntityCreate(EntityCreateResponse),
    EntityGet(EntityGetResponse),
    EntityUpdate(EntityUpdateResponse),
    EntityRename(EntityRenameResponse),
    EntityMerge(EntityMergeResponse),
    EntityUnmerge(EntityUnmergeResponse),
    EntityResolve(EntityResolveResponse),
    /// Streaming response — per-item or tail. Wire opcode is the same
    /// (`0x01B7`); the body's `is_final()` discriminates.
    EntityList(EntityListResponseFrame),
    EntityTombstone(EntityTombstoneResponse),

    // Statement ops.
    StatementCreate(StatementCreateResponse),
    StatementGet(StatementGetResponse),
    StatementSupersede(StatementSupersedeResponse),
    StatementTombstone(StatementTombstoneResponse),
    StatementRetract(StatementRetractResponse),
    /// Single-frame snapshot in v1; a later cut splits into streaming.
    StatementHistory(StatementHistoryResponseFrame),
    /// Single-frame snapshot in v1; a later cut splits into streaming.
    StatementList(StatementListResponseFrame),

    // Relation ops.
    RelationCreate(RelationCreateResponse),
    RelationGet(RelationGetResponse),
    RelationSupersede(RelationSupersedeResponse),
    RelationTombstone(RelationTombstoneResponse),
    /// Single-frame snapshot in v1; a later cut splits into streaming.
    RelationListFrom(RelationListFromResponseFrame),
    /// Single-frame snapshot in v1; a later cut splits into streaming.
    RelationListTo(RelationListToResponseFrame),
    /// Single-frame snapshot in v1; a later cut splits into streaming.
    RelationTraverse(RelationTraverseResponseFrame),

    // Schema ops.
    SchemaUpload(SchemaUploadResponse),
    SchemaGet(SchemaGetResponse),
    /// Single-frame snapshot in v1; a later cut may split into streaming.
    SchemaList(SchemaListResponseFrame),
    SchemaValidate(SchemaValidateResponse),

    // Extractor governance ops.
    /// Single-frame snapshot in v1.
    ExtractorList(ExtractorListResponseFrame),
    ExtractorDisable(ExtractorDisableResponse),
    ExtractorEnable(ExtractorEnableResponse),

    // Hybrid query ops.
    Query(QueryResponse),
    QueryExplain(QueryExplainResponse),
    QueryTrace(QueryTraceResponse),
    RecallHybrid(RecallHybridResponse),

    // Procedural-memory materialization. Carries the rendered system
    // block plus the statement ids that contributed.
    MaterializeProcedural(MaterializeProceduralResponse),

    Error(ErrorResponse),
}

impl ResponseBody {
    /// The opcode this body corresponds to.
    #[must_use]
    pub fn opcode(&self) -> Opcode {
        match self {
            Self::Welcome(_) => Opcode::Welcome,
            Self::AuthOk(_) => Opcode::AuthOk,
            Self::Encode(_) => Opcode::EncodeResp,
            Self::Recall(_) => Opcode::RecallResp,
            Self::Plan(_) => Opcode::PlanResp,
            Self::Reason(_) => Opcode::ReasonResp,
            Self::Forget(_) => Opcode::ForgetResp,
            Self::Link(_) => Opcode::LinkResp,
            Self::Unlink(_) => Opcode::UnlinkResp,
            Self::SubscribeEvent(_) => Opcode::SubscribeEvent,
            Self::Unsubscribe(_) => Opcode::UnsubscribeResp,
            Self::TxnBegin(_) => Opcode::TxnBeginResp,
            Self::TxnCommit(_) => Opcode::TxnCommitResp,
            Self::TxnAbort(_) => Opcode::TxnAbortResp,
            Self::CancelStreamAck(_) => Opcode::CancelStreamAck,
            Self::Pong(_) => Opcode::Pong,
            Self::ServerPing(_) => Opcode::ServerPing,
            Self::AdminStats(_) => Opcode::AdminStatsResp,
            Self::AdminSnapshot(_) => Opcode::AdminSnapshotResp,
            Self::AdminRestore(_) => Opcode::AdminRestoreResp,
            Self::AdminIntegrityCheck(_) => Opcode::AdminIntegrityCheckResp,
            Self::AdminMigrateEmbeddings(_) => Opcode::AdminMigrateEmbeddingsResp,
            Self::AdminCreateContext(_) => Opcode::AdminCreateContextResp,
            Self::AdminRenameContext(_) => Opcode::AdminRenameContextResp,
            Self::AdminMoveMemory(_) => Opcode::AdminMoveMemoryResp,
            Self::AdminReclassify(_) => Opcode::AdminReclassifyResp,
            Self::AdminListTombstoned(_) => Opcode::AdminListTombstonedResp,
            Self::AdminBackfill(_) => Opcode::AdminBackfillResp,
            Self::AdminBackfillCancel(_) => Opcode::AdminBackfillCancelResp,
            Self::EntityCreate(_) => Opcode::EntityCreateResp,
            Self::EntityGet(_) => Opcode::EntityGetResp,
            Self::EntityUpdate(_) => Opcode::EntityUpdateResp,
            Self::EntityRename(_) => Opcode::EntityRenameResp,
            Self::EntityMerge(_) => Opcode::EntityMergeResp,
            Self::EntityUnmerge(_) => Opcode::EntityUnmergeResp,
            Self::EntityResolve(_) => Opcode::EntityResolveResp,
            Self::EntityList(_) => Opcode::EntityListResp,
            Self::EntityTombstone(_) => Opcode::EntityTombstoneResp,
            Self::StatementCreate(_) => Opcode::StatementCreateResp,
            Self::StatementGet(_) => Opcode::StatementGetResp,
            Self::StatementSupersede(_) => Opcode::StatementSupersedeResp,
            Self::StatementTombstone(_) => Opcode::StatementTombstoneResp,
            Self::StatementRetract(_) => Opcode::StatementRetractResp,
            Self::StatementHistory(_) => Opcode::StatementHistoryResp,
            Self::StatementList(_) => Opcode::StatementListResp,
            Self::RelationCreate(_) => Opcode::RelationCreateResp,
            Self::RelationGet(_) => Opcode::RelationGetResp,
            Self::RelationSupersede(_) => Opcode::RelationSupersedeResp,
            Self::RelationTombstone(_) => Opcode::RelationTombstoneResp,
            Self::RelationListFrom(_) => Opcode::RelationListFromResp,
            Self::RelationListTo(_) => Opcode::RelationListToResp,
            Self::RelationTraverse(_) => Opcode::RelationTraverseResp,
            Self::SchemaUpload(_) => Opcode::SchemaUploadResp,
            Self::SchemaGet(_) => Opcode::SchemaGetResp,
            Self::SchemaList(_) => Opcode::SchemaListResp,
            Self::SchemaValidate(_) => Opcode::SchemaValidateResp,
            Self::ExtractorList(_) => Opcode::ExtractorListResp,
            Self::ExtractorDisable(_) => Opcode::ExtractorDisableResp,
            Self::ExtractorEnable(_) => Opcode::ExtractorEnableResp,
            Self::Query(_) => Opcode::QueryResp,
            Self::QueryExplain(_) => Opcode::QueryExplainResp,
            Self::QueryTrace(_) => Opcode::QueryTraceResp,
            Self::RecallHybrid(_) => Opcode::RecallHybridResp,
            Self::MaterializeProcedural(_) => Opcode::MaterializeProceduralResp,
            Self::Error(_) => Opcode::Error,
        }
    }

    /// `Some(is_final)` for streaming variants (recall / plan / reason /
    /// admin-migrate / admin-list-tombstoned). `None` for unary or
    /// open-ended variants — subscription events have no body-side
    /// `is_final` (the EOS flag in the frame header carries it).
    #[must_use]
    pub fn is_final(&self) -> Option<bool> {
        match self {
            Self::Recall(r) => Some(r.is_final),
            Self::Plan(r) => Some(r.is_final),
            Self::Reason(r) => Some(r.is_final),
            Self::AdminMigrateEmbeddings(r) => Some(r.is_final),
            Self::AdminListTombstoned(r) => Some(r.is_final),
            Self::StatementHistory(r) => Some(r.is_final),
            Self::StatementList(r) => Some(r.is_final),
            Self::RelationListFrom(r) => Some(r.is_final),
            Self::RelationListTo(r) => Some(r.is_final),
            Self::RelationTraverse(r) => Some(r.is_final),
            Self::SchemaList(r) => Some(r.is_final),
            Self::ExtractorList(r) => Some(r.is_final),
            _ => None,
        }
    }

    /// Encode the structured body to bytes via rkyv. Vector blobs (where
    /// supported) are appended by callers at the [`crate::Frame`] layer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Welcome(r) => to_rkyv_bytes(r),
            Self::AuthOk(r) => to_rkyv_bytes(r),
            Self::Encode(r) => to_rkyv_bytes(r),
            Self::Recall(r) => to_rkyv_bytes(r),
            Self::Plan(r) => to_rkyv_bytes(r),
            Self::Reason(r) => to_rkyv_bytes(r),
            Self::Forget(r) => to_rkyv_bytes(r),
            Self::Link(r) => to_rkyv_bytes(r),
            Self::Unlink(r) => to_rkyv_bytes(r),
            Self::SubscribeEvent(r) => to_rkyv_bytes(r),
            Self::Unsubscribe(r) => to_rkyv_bytes(r),
            Self::TxnBegin(r) => to_rkyv_bytes(r),
            Self::TxnCommit(r) => to_rkyv_bytes(r),
            Self::TxnAbort(r) => to_rkyv_bytes(r),
            Self::CancelStreamAck(r) => to_rkyv_bytes(r),
            Self::Pong(r) => to_rkyv_bytes(r),
            Self::ServerPing(r) => to_rkyv_bytes(r),
            Self::AdminStats(r) => to_rkyv_bytes(r),
            Self::AdminSnapshot(r) => to_rkyv_bytes(r),
            Self::AdminRestore(r) => to_rkyv_bytes(r),
            Self::AdminIntegrityCheck(r) => to_rkyv_bytes(r),
            Self::AdminMigrateEmbeddings(r) => to_rkyv_bytes(r),
            Self::AdminCreateContext(r) => to_rkyv_bytes(r),
            Self::AdminRenameContext(r) => to_rkyv_bytes(r),
            Self::AdminMoveMemory(r) => to_rkyv_bytes(r),
            Self::AdminReclassify(r) => to_rkyv_bytes(r),
            Self::AdminListTombstoned(r) => to_rkyv_bytes(r),
            Self::AdminBackfill(r) => to_rkyv_bytes(r),
            Self::AdminBackfillCancel(r) => to_rkyv_bytes(r),
            Self::EntityCreate(r) => to_rkyv_bytes(r),
            Self::EntityGet(r) => to_rkyv_bytes(r),
            Self::EntityUpdate(r) => to_rkyv_bytes(r),
            Self::EntityRename(r) => to_rkyv_bytes(r),
            Self::EntityMerge(r) => to_rkyv_bytes(r),
            Self::EntityUnmerge(r) => to_rkyv_bytes(r),
            Self::EntityResolve(r) => to_rkyv_bytes(r),
            Self::EntityList(r) => to_rkyv_bytes(r),
            Self::EntityTombstone(r) => to_rkyv_bytes(r),
            Self::StatementCreate(r) => to_rkyv_bytes(r),
            Self::StatementGet(r) => to_rkyv_bytes(r),
            Self::StatementSupersede(r) => to_rkyv_bytes(r),
            Self::StatementTombstone(r) => to_rkyv_bytes(r),
            Self::StatementRetract(r) => to_rkyv_bytes(r),
            Self::StatementHistory(r) => to_rkyv_bytes(r),
            Self::StatementList(r) => to_rkyv_bytes(r),
            Self::RelationCreate(r) => to_rkyv_bytes(r),
            Self::RelationGet(r) => to_rkyv_bytes(r),
            Self::RelationSupersede(r) => to_rkyv_bytes(r),
            Self::RelationTombstone(r) => to_rkyv_bytes(r),
            Self::RelationListFrom(r) => to_rkyv_bytes(r),
            Self::RelationListTo(r) => to_rkyv_bytes(r),
            Self::RelationTraverse(r) => to_rkyv_bytes(r),
            Self::SchemaUpload(r) => to_rkyv_bytes(r),
            Self::SchemaGet(r) => to_rkyv_bytes(r),
            Self::SchemaList(r) => to_rkyv_bytes(r),
            Self::SchemaValidate(r) => to_rkyv_bytes(r),
            Self::ExtractorList(r) => to_rkyv_bytes(r),
            Self::ExtractorDisable(r) => to_rkyv_bytes(r),
            Self::ExtractorEnable(r) => to_rkyv_bytes(r),
            Self::Query(r) => to_rkyv_bytes(r),
            Self::QueryExplain(r) => to_rkyv_bytes(r),
            Self::QueryTrace(r) => to_rkyv_bytes(r),
            Self::RecallHybrid(r) => to_rkyv_bytes(r),
            Self::MaterializeProcedural(r) => to_rkyv_bytes(r),
            Self::Error(r) => to_rkyv_bytes(r),
        }
    }

    /// Decode `bytes` as the response body for `opcode`. Returns
    /// [`ProtocolError::UnknownOpcode`] if `opcode` doesn't carry a
    /// response body (request opcodes).
    pub fn decode(opcode: Opcode, bytes: &[u8]) -> Result<Self, ProtocolError> {
        Ok(match opcode {
            Opcode::Welcome => Self::Welcome(from_rkyv_bytes(bytes)?),
            Opcode::AuthOk => Self::AuthOk(from_rkyv_bytes(bytes)?),
            Opcode::EncodeResp => Self::Encode(from_rkyv_bytes(bytes)?),
            Opcode::RecallResp => Self::Recall(from_rkyv_bytes(bytes)?),
            Opcode::PlanResp => Self::Plan(from_rkyv_bytes(bytes)?),
            Opcode::ReasonResp => Self::Reason(from_rkyv_bytes(bytes)?),
            Opcode::ForgetResp => Self::Forget(from_rkyv_bytes(bytes)?),
            Opcode::LinkResp => Self::Link(from_rkyv_bytes(bytes)?),
            Opcode::UnlinkResp => Self::Unlink(from_rkyv_bytes(bytes)?),
            Opcode::SubscribeEvent => Self::SubscribeEvent(from_rkyv_bytes(bytes)?),
            Opcode::UnsubscribeResp => Self::Unsubscribe(from_rkyv_bytes(bytes)?),
            Opcode::TxnBeginResp => Self::TxnBegin(from_rkyv_bytes(bytes)?),
            Opcode::TxnCommitResp => Self::TxnCommit(from_rkyv_bytes(bytes)?),
            Opcode::TxnAbortResp => Self::TxnAbort(from_rkyv_bytes(bytes)?),
            Opcode::CancelStreamAck => Self::CancelStreamAck(from_rkyv_bytes(bytes)?),
            Opcode::Pong => Self::Pong(from_rkyv_bytes(bytes)?),
            Opcode::ServerPing => Self::ServerPing(from_rkyv_bytes(bytes)?),
            Opcode::AdminStatsResp => Self::AdminStats(from_rkyv_bytes(bytes)?),
            Opcode::AdminSnapshotResp => Self::AdminSnapshot(from_rkyv_bytes(bytes)?),
            Opcode::AdminRestoreResp => Self::AdminRestore(from_rkyv_bytes(bytes)?),
            Opcode::AdminIntegrityCheckResp => Self::AdminIntegrityCheck(from_rkyv_bytes(bytes)?),
            Opcode::AdminMigrateEmbeddingsResp => {
                Self::AdminMigrateEmbeddings(from_rkyv_bytes(bytes)?)
            }
            Opcode::AdminCreateContextResp => Self::AdminCreateContext(from_rkyv_bytes(bytes)?),
            Opcode::AdminRenameContextResp => Self::AdminRenameContext(from_rkyv_bytes(bytes)?),
            Opcode::AdminMoveMemoryResp => Self::AdminMoveMemory(from_rkyv_bytes(bytes)?),
            Opcode::AdminReclassifyResp => Self::AdminReclassify(from_rkyv_bytes(bytes)?),
            Opcode::AdminListTombstonedResp => Self::AdminListTombstoned(from_rkyv_bytes(bytes)?),
            Opcode::AdminBackfillResp => Self::AdminBackfill(from_rkyv_bytes(bytes)?),
            Opcode::AdminBackfillCancelResp => Self::AdminBackfillCancel(from_rkyv_bytes(bytes)?),
            Opcode::EntityCreateResp => Self::EntityCreate(from_rkyv_bytes(bytes)?),
            Opcode::EntityGetResp => Self::EntityGet(from_rkyv_bytes(bytes)?),
            Opcode::EntityUpdateResp => Self::EntityUpdate(from_rkyv_bytes(bytes)?),
            Opcode::EntityRenameResp => Self::EntityRename(from_rkyv_bytes(bytes)?),
            Opcode::EntityMergeResp => Self::EntityMerge(from_rkyv_bytes(bytes)?),
            Opcode::EntityUnmergeResp => Self::EntityUnmerge(from_rkyv_bytes(bytes)?),
            Opcode::EntityResolveResp => Self::EntityResolve(from_rkyv_bytes(bytes)?),
            Opcode::EntityListResp => Self::EntityList(from_rkyv_bytes(bytes)?),
            Opcode::EntityTombstoneResp => Self::EntityTombstone(from_rkyv_bytes(bytes)?),
            Opcode::StatementCreateResp => Self::StatementCreate(from_rkyv_bytes(bytes)?),
            Opcode::StatementGetResp => Self::StatementGet(from_rkyv_bytes(bytes)?),
            Opcode::StatementSupersedeResp => Self::StatementSupersede(from_rkyv_bytes(bytes)?),
            Opcode::StatementTombstoneResp => Self::StatementTombstone(from_rkyv_bytes(bytes)?),
            Opcode::StatementRetractResp => Self::StatementRetract(from_rkyv_bytes(bytes)?),
            Opcode::StatementHistoryResp => Self::StatementHistory(from_rkyv_bytes(bytes)?),
            Opcode::StatementListResp => Self::StatementList(from_rkyv_bytes(bytes)?),
            Opcode::RelationCreateResp => Self::RelationCreate(from_rkyv_bytes(bytes)?),
            Opcode::RelationGetResp => Self::RelationGet(from_rkyv_bytes(bytes)?),
            Opcode::RelationSupersedeResp => Self::RelationSupersede(from_rkyv_bytes(bytes)?),
            Opcode::RelationTombstoneResp => Self::RelationTombstone(from_rkyv_bytes(bytes)?),
            Opcode::RelationListFromResp => Self::RelationListFrom(from_rkyv_bytes(bytes)?),
            Opcode::RelationListToResp => Self::RelationListTo(from_rkyv_bytes(bytes)?),
            Opcode::RelationTraverseResp => Self::RelationTraverse(from_rkyv_bytes(bytes)?),
            Opcode::SchemaUploadResp => Self::SchemaUpload(from_rkyv_bytes(bytes)?),
            Opcode::SchemaGetResp => Self::SchemaGet(from_rkyv_bytes(bytes)?),
            Opcode::SchemaListResp => Self::SchemaList(from_rkyv_bytes(bytes)?),
            Opcode::SchemaValidateResp => Self::SchemaValidate(from_rkyv_bytes(bytes)?),
            Opcode::ExtractorListResp => Self::ExtractorList(from_rkyv_bytes(bytes)?),
            Opcode::ExtractorDisableResp => Self::ExtractorDisable(from_rkyv_bytes(bytes)?),
            Opcode::ExtractorEnableResp => Self::ExtractorEnable(from_rkyv_bytes(bytes)?),
            Opcode::QueryResp => Self::Query(from_rkyv_bytes(bytes)?),
            Opcode::QueryExplainResp => Self::QueryExplain(from_rkyv_bytes(bytes)?),
            Opcode::QueryTraceResp => Self::QueryTrace(from_rkyv_bytes(bytes)?),
            Opcode::RecallHybridResp => Self::RecallHybrid(from_rkyv_bytes(bytes)?),
            Opcode::MaterializeProceduralResp => {
                Self::MaterializeProcedural(from_rkyv_bytes(bytes)?)
            }
            Opcode::Error => Self::Error(from_rkyv_bytes(bytes)?),
            other => return Err(ProtocolError::UnknownOpcode(other.as_u16())),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::request::{
        EdgeKindWire, ForgetMode, MemoryKindWire, WireMemoryId, WireUuid,
    };
    use crate::error::{ErrorCategory, ErrorCode};

    fn round_trip(body: ResponseBody) {
        let bytes = body.encode();
        let decoded = ResponseBody::decode(body.opcode(), &bytes)
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
        ((7u128) << 72) | ((42u128) << 56) | 0x12_3456_u128
    }

    #[test]
    fn encode_response_round_trips() {
        round_trip(ResponseBody::Encode(EncodeResponse {
            memory_id: sample_memory_id(),
            was_deduplicated: false,
            salience: 0.5,
            auto_edges_added: 3,
            lsn: 42,
            agent_id: [0xAA; 16],
            context_id: 7,
            kind: MemoryKindWire::Episodic,
            created_at_unix_nanos: 1_700_000_000_000_000_000,
            edges_out_count: 3,
            embedding_model_fp: [0xBB; 16],
            pending_stages: vec![
                StageKind::AutoEdge,
                StageKind::TemporalEdge,
                StageKind::Extractor,
            ],
            has_active_schema: true,
            has_llm_extractor: true,
        }));
    }

    #[test]
    fn recall_response_round_trips() {
        round_trip(ResponseBody::Recall(RecallResponseFrame {
            results: vec![MemoryResult {
                memory_id: sample_memory_id(),
                text: "first result".into(),
                similarity_score: 0.92,
                confidence: 0.85,
                salience: 0.5,
                kind: MemoryKindWire::Episodic,
                context_id: 1_u64,
                created_at_unix_nanos: 1_700_000_000_000_000_000,
                last_accessed_at_unix_nanos: 1_700_000_001_000_000_000,
                contributing_retrievers: vec![
                    crate::shared::enums::RetrieverNameWire::Semantic,
                    crate::shared::enums::RetrieverNameWire::Lexical,
                ],
                fused_score: 0.0164,
                edges: Some(vec![EdgeView {
                    target: sample_memory_id(),
                    kind: EdgeKindWire::Caused,
                    weight: 0.9,
                }]),
                salience_initial: 0.7,
                access_count: 5,
                lsn: 100,
                flags: 0x1,
                consolidated_at_unix_nanos: None,
                edges_out_count: 2,
                edges_in_count: 3,
                graph: None,
            }],
            is_final: false,
            cumulative_count: 1,
            estimated_remaining: Some(9),
        }));
    }

    #[test]
    fn plan_response_round_trips_each_transition() {
        for transition in [
            TransitionKind::Initial,
            TransitionKind::Causal,
            TransitionKind::Temporal,
            TransitionKind::Similarity,
            TransitionKind::Other("custom".into()),
        ] {
            round_trip(ResponseBody::Plan(PlanResponseFrame {
                steps: vec![PlanStep {
                    step_index: 0,
                    memory_id: sample_memory_id(),
                    text: "first step".into(),
                    transition_kind: transition.clone(),
                    confidence: 0.7,
                    estimated_distance_to_goal: 1.0,
                }],
                is_final: false,
                plan_status: None,
            }));
        }
        round_trip(ResponseBody::Plan(PlanResponseFrame {
            steps: vec![],
            is_final: true,
            plan_status: Some(PlanStatus::GoalReached),
        }));
    }

    #[test]
    fn reason_response_round_trips() {
        round_trip(ResponseBody::Reason(ReasonResponseFrame {
            inferences: vec![InferenceStep {
                step_index: 0,
                claim: "A causes B".into(),
                supporting_memories: vec![sample_memory_id()],
                contradicting_memories: vec![],
                confidence: 0.8,
                inference_kind: InferenceKind::CausalExplanation,
            }],
            is_final: false,
            reason_status: None,
        }));
        round_trip(ResponseBody::Reason(ReasonResponseFrame {
            inferences: vec![],
            is_final: true,
            reason_status: Some(ReasonStatus::Complete),
        }));
    }

    #[test]
    fn forget_response_round_trips() {
        round_trip(ResponseBody::Forget(ForgetResponse {
            memory_id: sample_memory_id(),
            was_already_forgotten: false,
            edges_removed: 4,
        }));
    }

    #[test]
    fn subscribe_event_round_trips() {
        round_trip(ResponseBody::SubscribeEvent(SubscriptionEvent {
            event_type: EventType::Encoded,
            memory_id: sample_memory_id(),
            context_id: 2_u64,
            text: "new memory".into(),
            kind: MemoryKindWire::Episodic,
            salience: 0.5,
            timestamp_unix_nanos: 1_700_000_000_000_000_000,
            lsn: 1234,
            knowledge_payload: None,
            edge_payload: None,
            stage_kind: None,
            stage_outcome: None,
            stage_payload: None,
        }));
    }

    #[test]
    fn unsubscribe_response_round_trips() {
        round_trip(ResponseBody::Unsubscribe(UnsubscribeResponse {
            target_stream_id: 7,
            final_lsn: 9999,
        }));
    }

    #[test]
    fn txn_responses_round_trip() {
        let id = sample_uuid(3);
        round_trip(ResponseBody::TxnBegin(TxnBeginResponse {
            txn_id: id,
            timeout_seconds: 60,
            started_at_unix_nanos: 1,
        }));
        round_trip(ResponseBody::TxnCommit(TxnCommitResponse {
            txn_id: id,
            committed_at_unix_nanos: 2,
            operations_applied: 5,
        }));
        round_trip(ResponseBody::TxnAbort(TxnAbortResponse {
            txn_id: id,
            operations_discarded: 5,
        }));
    }

    #[test]
    fn cancel_stream_ack_round_trips() {
        round_trip(ResponseBody::CancelStreamAck(CancelStreamAck {
            target_stream_id: 1,
            cancelled_at_unix_nanos: 99,
        }));
    }

    #[test]
    fn keepalive_responses_round_trip() {
        round_trip(ResponseBody::Pong(PongResponse {
            client_timestamp_unix_nanos: 1,
            server_timestamp_unix_nanos: 2,
        }));
        round_trip(ResponseBody::ServerPing(ServerPingResponse {
            server_timestamp_unix_nanos: 3,
        }));
    }

    #[test]
    fn admin_responses_round_trip() {
        round_trip(ResponseBody::AdminStats(AdminStatsResponse {
            summary: StatsSummary {
                total_memories: 1_000_000,
                total_active_memories: 999_000,
                total_tombstoned_memories: 1_000,
                total_contexts: 10,
                encode_qps: 100.5,
                recall_qps: 50.25,
                p99_encode_latency_ms: 2.0,
                p99_recall_latency_ms: 5.0,
                resident_memory_bytes: 1024 * 1024 * 1024,
                disk_used_bytes: 10_u64.pow(10),
            },
            per_shard: Some(vec![ShardStats {
                shard_id: 0,
                memory_count: 100_000,
                salience_distribution: SalienceHistogram { buckets: [10; 10] },
                wal_segment_count: 5,
                last_checkpoint_lsn: 1_000_000,
                arena_used_bytes: 1024 * 1024,
            }]),
            per_context: Some(vec![ContextStats {
                context_id: 4_u64,
                name: "default".into(),
                memory_count: 100,
                last_encoded_at_unix_nanos: 1,
                last_recalled_at_unix_nanos: 2,
            }]),
            server_uptime_seconds: 3600,
            server_version: "0.1.0".into(),
        }));
        round_trip(ResponseBody::AdminSnapshot(AdminSnapshotResponse {
            snapshot_id: sample_uuid(5),
            snapshot_name: "nightly".into(),
            snapshot_path: "/var/brain/snapshots/2026-05-10".into(),
            started_at_unix_nanos: 1,
            completed_at_unix_nanos: 2,
            bytes_written: 1_000_000,
            used_reflink: true,
        }));
        round_trip(ResponseBody::AdminRestore(AdminRestoreResponse {
            snapshot_name: "nightly".into(),
            shards_restored: vec![0, 1, 2],
            completed_at_unix_nanos: 3,
            memories_restored: 1_000_000,
        }));
        round_trip(ResponseBody::AdminIntegrityCheck(
            AdminIntegrityCheckResponse {
                scope: crate::envelope::request::CheckScope::Full,
                issues_found: vec![IntegrityIssue {
                    issue_type: IntegrityIssueType::VectorCorruption,
                    affected_memory_id: Some(sample_memory_id()),
                    affected_shard_id: Some(0),
                    description: "vector failed norm check".into(),
                    repaired: false,
                }],
                issues_repaired: 0,
                completed_at_unix_nanos: 4,
            },
        ));
        round_trip(ResponseBody::AdminMigrateEmbeddings(
            AdminMigrateEmbeddingsResponseFrame {
                is_final: false,
                progress: MigrationProgress {
                    total_memories: 100_000,
                    migrated_so_far: 25_000,
                    failed_so_far: 0,
                    current_qps: 1000.0,
                    estimated_remaining_seconds: 75,
                },
                status: None,
            },
        ));
        round_trip(ResponseBody::AdminMigrateEmbeddings(
            AdminMigrateEmbeddingsResponseFrame {
                is_final: true,
                progress: MigrationProgress {
                    total_memories: 100_000,
                    migrated_so_far: 100_000,
                    failed_so_far: 0,
                    current_qps: 0.0,
                    estimated_remaining_seconds: 0,
                },
                status: Some(MigrationStatus::Completed),
            },
        ));
        round_trip(ResponseBody::AdminCreateContext(
            AdminCreateContextResponse {
                context_id: 6_u64,
                name: "personal".into(),
            },
        ));
        round_trip(ResponseBody::AdminRenameContext(
            AdminRenameContextResponse {
                context_id: 7_u64,
                new_name: "renamed".into(),
                old_name: "original".into(),
            },
        ));
        round_trip(ResponseBody::AdminMoveMemory(AdminMoveMemoryResponse {
            memory_id: sample_memory_id(),
            new_context_id: 8_u64,
            old_context_id: 9_u64,
        }));
        round_trip(ResponseBody::AdminReclassify(AdminReclassifyResponse {
            memory_id: sample_memory_id(),
            new_kind: MemoryKindWire::Consolidated,
            old_kind: MemoryKindWire::Episodic,
        }));
        round_trip(ResponseBody::AdminListTombstoned(
            AdminListTombstonedResponseFrame {
                memory: TombstonedMemoryInfo {
                    memory_id: sample_memory_id(),
                    text: "forgotten".into(),
                    forgot_at_unix_nanos: 5,
                    forget_mode: ForgetMode::Soft,
                    age_seconds: 3600,
                    eligible_for_reclaim: false,
                },
                is_final: false,
            },
        ));
        round_trip(ResponseBody::AdminBackfill(AdminBackfillResponse {
            backfill_id: sample_uuid(31),
            progress: BackfillProgress {
                running: true,
                completed: 12,
                failed: 1,
                skipped_already_completed: 4,
                last_processed_memory_id_present: true,
                last_processed_memory_id: sample_memory_id(),
            },
        }));
        round_trip(ResponseBody::AdminBackfillCancel(
            AdminBackfillCancelResponse {
                backfill_id: sample_uuid(32),
                cancelled: true,
                progress: BackfillProgress::idle(),
            },
        ));
        round_trip(ResponseBody::AdminBackfillCancel(
            AdminBackfillCancelResponse {
                backfill_id: sample_uuid(33),
                cancelled: false,
                progress: BackfillProgress::idle(),
            },
        ));
    }

    #[test]
    fn error_response_round_trips() {
        round_trip(ResponseBody::Error(ErrorResponse {
            code: ErrorCodeWire::InvalidArgument,
            category: ErrorCategoryWire::Validation,
            message: "field 'top_k' out of range".into(),
            details: Some(ErrorDetails {
                field: Some("top_k".into()),
                expected: Some("[1, 1000]".into()),
                actual: Some("5000".into()),
            }),
            retry_after_ms: None,
        }));
    }

    #[test]
    fn streaming_sequence_round_trips() {
        // a streaming response is a sequence of
        // frames, only the last of which has is_final=true. Round-trip a
        // 3-frame sequence and verify ordering survives.
        let seq: Vec<ResponseBody> = vec![
            ResponseBody::Recall(RecallResponseFrame {
                results: vec![],
                is_final: false,
                cumulative_count: 0,
                estimated_remaining: Some(10),
            }),
            ResponseBody::Recall(RecallResponseFrame {
                results: vec![],
                is_final: false,
                cumulative_count: 5,
                estimated_remaining: Some(5),
            }),
            ResponseBody::Recall(RecallResponseFrame {
                results: vec![],
                is_final: true,
                cumulative_count: 10,
                estimated_remaining: Some(0),
            }),
        ];
        let encoded: Vec<Vec<u8>> = seq.iter().map(ResponseBody::encode).collect();
        let decoded: Vec<ResponseBody> = encoded
            .iter()
            .map(|b| ResponseBody::decode(Opcode::RecallResp, b).expect("decode streaming frame"))
            .collect();
        assert_eq!(decoded, seq);
        // Ordering: only the third frame is final.
        assert_eq!(
            decoded
                .iter()
                .map(ResponseBody::is_final)
                .collect::<Vec<_>>(),
            vec![Some(false), Some(false), Some(true)],
        );
    }

    #[test]
    fn is_final_signals_streaming_variants() {
        // Streaming variants report Some(...).
        assert_eq!(
            ResponseBody::Recall(RecallResponseFrame {
                results: vec![],
                is_final: true,
                cumulative_count: 0,
                estimated_remaining: None,
            })
            .is_final(),
            Some(true)
        );
        // Unary variants report None.
        assert_eq!(
            ResponseBody::Pong(PongResponse {
                client_timestamp_unix_nanos: 0,
                server_timestamp_unix_nanos: 0,
            })
            .is_final(),
            None
        );
        // Subscription events are open-ended; body has no is_final field.
        assert_eq!(
            ResponseBody::SubscribeEvent(SubscriptionEvent {
                event_type: EventType::Encoded,
                memory_id: 0,
                context_id: 0,
                text: String::new(),
                kind: MemoryKindWire::Episodic,
                salience: 0.0,
                timestamp_unix_nanos: 0,
                lsn: 0,
                knowledge_payload: None,
                edge_payload: None,
                stage_kind: None,
                stage_outcome: None,
                stage_payload: None,
            })
            .is_final(),
            None
        );
    }

    #[test]
    fn decode_with_request_opcode_returns_unknown() {
        let any_bytes = vec![0u8; 8];
        let err = ResponseBody::decode(Opcode::EncodeReq, &any_bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::UnknownOpcode(_)));
    }

    #[test]
    fn decode_garbage_returns_malformed() {
        let garbage = vec![0xAAu8; 64];
        let err = ResponseBody::decode(Opcode::EncodeResp, &garbage).unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedPayload(_)));
    }

    #[test]
    fn handshake_response_bodies_round_trip() {
        use crate::connection::handshake::{
            AgentPermissions, AuthMethod, AuthOkPayload, HelloCapabilities, ServerFeatures,
            WelcomePayload,
        };

        let welcome = ResponseBody::Welcome(WelcomePayload {
            server_id: "brain-server/0.5.0".into(),
            chosen_version: 1,
            session_id: sample_uuid(20),
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
        });
        let auth_ok = ResponseBody::AuthOk(AuthOkPayload {
            agent_id: sample_uuid(21),
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
        });

        for body in [welcome, auth_ok] {
            let bytes = body.encode();
            let decoded = ResponseBody::decode(body.opcode(), &bytes).unwrap();
            assert_eq!(decoded, body);
        }
    }

    #[test]
    fn error_code_wire_round_trips_through_canonical() {
        // ErrorCode → ErrorCodeWire → ErrorCode is the identity for every
        // code (sanity-check on the From mappings).
        for code in [
            ErrorCode::BadMagic,
            ErrorCode::Unauthenticated,
            ErrorCode::PermissionDenied,
            ErrorCode::InvalidArgument,
            ErrorCode::MemoryNotFound,
            ErrorCode::IdempotencyConflict,
            ErrorCode::OutOfSlots,
            ErrorCode::Internal,
            ErrorCode::ShardUnavailable,
        ] {
            let wire: ErrorCodeWire = code.into();
            let back: ErrorCode = wire.into();
            assert_eq!(back, code);
        }
        for cat in [
            ErrorCategory::Protocol,
            ErrorCategory::Authentication,
            ErrorCategory::Authorization,
            ErrorCategory::Validation,
            ErrorCategory::NotFound,
            ErrorCategory::Conflict,
            ErrorCategory::ResourceExhausted,
            ErrorCategory::Internal,
            ErrorCategory::Unavailable,
        ] {
            let wire: ErrorCategoryWire = cat.into();
            let back: ErrorCategory = wire.into();
            assert_eq!(back, cat);
        }
    }
}
