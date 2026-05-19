//! Top-level dispatch. Routes a wire `RequestBody` to its handler
//! and returns a wire `ResponseBody`. Spec §09/01 §1: each operation
//! is a request-response interaction (or streaming for SUBSCRIBE).
//!
//! The `match req { … }` is exhaustive over `RequestBody`'s variants.
//! When Phase 1's wire shape gains a new variant, this file fails to
//! compile until the corresponding arm is added — the bug-prevention
//! guarantee we want.
//!
//! Stub handlers return `OpError::NotYetImplemented`; sub-tasks 7.3
//! through 7.10 replace each stub with a real implementation.

use brain_core::AgentId;
use brain_protocol::request::RequestBody;
use brain_protocol::response::ResponseBody;

use crate::context::OpsContext;
use crate::error::OpError;

/// Per-request caller context. Carries the auth-time agent (from
/// `ConnPhase::Established.agent`) so handlers can stamp it onto
/// the Ops they build — separate from the wire request because the
/// agent is auth metadata, not request data. Future fields might
/// include permissions, request id for tracing, etc.
#[derive(Debug, Clone, Copy)]
pub struct RequestCaller {
    /// Authenticated agent for this request. `AgentId::default()`
    /// means "unauthenticated / test path"; the writer treats that
    /// as a substrate-wide event with no agent filter applicability.
    pub agent_id: AgentId,
}

impl RequestCaller {
    /// Construct a caller with the given agent.
    #[must_use]
    pub fn new(agent_id: AgentId) -> Self {
        Self { agent_id }
    }

    /// The substrate-wide / test-only default. Used by paths that
    /// don't yet wire connection auth (in-process unit tests).
    #[must_use]
    pub fn anonymous() -> Self {
        Self {
            agent_id: AgentId::default(),
        }
    }
}

pub async fn dispatch(
    req: RequestBody,
    caller: RequestCaller,
    ctx: &OpsContext,
) -> Result<ResponseBody, OpError> {
    // Per-request override: stamp the caller's agent onto a clone
    // of the shared ctx so handlers that build writer Ops can pull
    // it via `ctx.executor.caller_agent` without taking another
    // function param. The clone is cheap — every field is Arc'd.
    let per_request_ctx = if caller.agent_id == brain_core::AgentId::default() {
        // Anonymous caller — no override needed; reuse the shared
        // ctx (zero-cost on the hot path that doesn't actually
        // authenticate).
        None
    } else {
        let mut owned = ctx.clone();
        owned.executor = owned.executor.with_caller_agent(caller.agent_id);
        Some(owned)
    };
    let ctx = per_request_ctx.as_ref().unwrap_or(ctx);
    match req {
        // -----------------------------------------------------------
        // Cognitive primitives — real handlers land in 7.3-7.7.
        // Handlers read `ctx.executor.caller_agent` to populate
        // `agent_id` on the writer Ops they build; the per-request
        // clone above ensures they see the auth-time value, not the
        // shared per-shard default.
        // -----------------------------------------------------------
        RequestBody::Encode(r) => crate::encode::handle_encode(r, ctx)
            .await
            .map(ResponseBody::Encode),

        RequestBody::EncodeVectorDirect(_) => Err(OpError::NotYetImplemented(
            "EncodeVectorDirect — future phase",
        )),

        RequestBody::Recall(r) => crate::recall::handle_recall(r, ctx)
            .await
            .map(ResponseBody::Recall),

        RequestBody::Plan(r) => crate::plan::handle_plan(r, ctx)
            .await
            .map(ResponseBody::Plan),

        RequestBody::Reason(r) => crate::reason::handle_reason(r, ctx)
            .await
            .map(ResponseBody::Reason),

        RequestBody::Forget(r) => crate::forget::handle_forget(r, ctx)
            .await
            .map(ResponseBody::Forget),

        // -----------------------------------------------------------
        // LINK / UNLINK — 7.8.
        // -----------------------------------------------------------
        RequestBody::Link(r) => crate::link::handle_link(r, ctx)
            .await
            .map(ResponseBody::Link),

        RequestBody::Unlink(r) => crate::link::handle_unlink(r, ctx)
            .await
            .map(ResponseBody::Unlink),

        // -----------------------------------------------------------
        // Streaming — 7.10. First-event shape only; subsequent
        // events ride a broadcast channel.
        // -----------------------------------------------------------
        RequestBody::Subscribe(r) => crate::subscribe::handle_subscribe(r, ctx)
            .await
            .map(ResponseBody::SubscribeEvent),

        RequestBody::Unsubscribe(r) => crate::subscribe::handle_unsubscribe(r, ctx)
            .await
            .map(ResponseBody::Unsubscribe),

        // -----------------------------------------------------------
        // Transactions — 7.9.
        // -----------------------------------------------------------
        RequestBody::TxnBegin(r) => crate::txn::handle_txn_begin(r, ctx)
            .await
            .map(ResponseBody::TxnBegin),

        RequestBody::TxnCommit(r) => crate::txn::handle_txn_commit(r, ctx)
            .await
            .map(ResponseBody::TxnCommit),

        RequestBody::TxnAbort(r) => crate::txn::handle_txn_abort(r, ctx)
            .await
            .map(ResponseBody::TxnAbort),

        // -----------------------------------------------------------
        // Connection lifecycle — brain-server (Phase 9) owns these.
        // -----------------------------------------------------------
        RequestBody::Hello(_)
        | RequestBody::Auth(_)
        | RequestBody::Bye(_)
        | RequestBody::Ping(_)
        | RequestBody::ClientPong(_)
        | RequestBody::CancelStream(_) => Err(OpError::NotYetImplemented(
            "connection-lifecycle op — Phase 9 (server)",
        )),

        // -----------------------------------------------------------
        // Admin ops — Phase 8 (workers) / Phase 9 (server) own these.
        // -----------------------------------------------------------
        RequestBody::AdminStats(_)
        | RequestBody::AdminSnapshot(_)
        | RequestBody::AdminRestore(_)
        | RequestBody::AdminIntegrityCheck(_)
        | RequestBody::AdminMigrateEmbeddings(_)
        | RequestBody::AdminCreateContext(_)
        | RequestBody::AdminRenameContext(_)
        | RequestBody::AdminMoveMemory(_)
        | RequestBody::AdminReclassify(_)
        | RequestBody::AdminListTombstoned(_) => {
            Err(OpError::NotYetImplemented("admin op — Phase 8 / 9"))
        }

        // -----------------------------------------------------------
        // Knowledge layer — Phase 16+ (spec §28/00).
        // -----------------------------------------------------------
        RequestBody::EntityCreate(r) => crate::knowledge_entity::handle_entity_create(r, ctx)
            .await
            .map(ResponseBody::EntityCreate),

        RequestBody::EntityGet(r) => crate::knowledge_entity::handle_entity_get(r, ctx)
            .await
            .map(ResponseBody::EntityGet),

        RequestBody::EntityUpdate(r) => crate::knowledge_entity::handle_entity_update(r, ctx)
            .await
            .map(ResponseBody::EntityUpdate),

        RequestBody::EntityRename(r) => crate::knowledge_entity::handle_entity_rename(r, ctx)
            .await
            .map(ResponseBody::EntityRename),

        RequestBody::EntityMerge(r) => crate::knowledge_entity::handle_entity_merge(r, ctx)
            .await
            .map(ResponseBody::EntityMerge),

        RequestBody::EntityUnmerge(r) => crate::knowledge_entity::handle_entity_unmerge(r, ctx)
            .await
            .map(ResponseBody::EntityUnmerge),

        RequestBody::EntityResolve(r) => crate::knowledge_entity::handle_entity_resolve(r, ctx)
            .await
            .map(ResponseBody::EntityResolve),

        RequestBody::EntityList(r) => crate::knowledge_entity::handle_entity_list(r, ctx)
            .await
            .map(ResponseBody::EntityList),

        RequestBody::EntityTombstone(r) => crate::knowledge_entity::handle_entity_tombstone(r, ctx)
            .await
            .map(ResponseBody::EntityTombstone),

        // Statement ops — phase 17.7. Spec §28/06.
        RequestBody::StatementCreate(r) => {
            crate::knowledge_statement::handle_statement_create(r, ctx)
                .await
                .map(ResponseBody::StatementCreate)
        }
        RequestBody::StatementGet(r) => crate::knowledge_statement::handle_statement_get(r, ctx)
            .await
            .map(ResponseBody::StatementGet),
        RequestBody::StatementSupersede(r) => {
            crate::knowledge_statement::handle_statement_supersede(r, ctx)
                .await
                .map(ResponseBody::StatementSupersede)
        }
        RequestBody::StatementTombstone(r) => {
            crate::knowledge_statement::handle_statement_tombstone(r, ctx)
                .await
                .map(ResponseBody::StatementTombstone)
        }
        RequestBody::StatementRetract(r) => {
            crate::knowledge_statement::handle_statement_retract(r, ctx)
                .await
                .map(ResponseBody::StatementRetract)
        }
        RequestBody::StatementHistory(r) => {
            crate::knowledge_statement::handle_statement_history(r, ctx)
                .await
                .map(ResponseBody::StatementHistory)
        }
        RequestBody::StatementList(r) => crate::knowledge_statement::handle_statement_list(r, ctx)
            .await
            .map(ResponseBody::StatementList),

        // Relation ops — phase 18.7. Spec §28/07.
        RequestBody::RelationCreate(r) => crate::knowledge_relation::handle_relation_create(r, ctx)
            .await
            .map(ResponseBody::RelationCreate),
        RequestBody::RelationGet(r) => crate::knowledge_relation::handle_relation_get(r, ctx)
            .await
            .map(ResponseBody::RelationGet),
        RequestBody::RelationSupersede(r) => {
            crate::knowledge_relation::handle_relation_supersede(r, ctx)
                .await
                .map(ResponseBody::RelationSupersede)
        }
        RequestBody::RelationTombstone(r) => {
            crate::knowledge_relation::handle_relation_tombstone(r, ctx)
                .await
                .map(ResponseBody::RelationTombstone)
        }
        RequestBody::RelationListFrom(r) => {
            crate::knowledge_relation::handle_relation_list_from(r, ctx)
                .await
                .map(ResponseBody::RelationListFrom)
        }
        RequestBody::RelationListTo(r) => {
            crate::knowledge_relation::handle_relation_list_to(r, ctx)
                .await
                .map(ResponseBody::RelationListTo)
        }
        RequestBody::RelationTraverse(r) => {
            crate::knowledge_relation::handle_relation_traverse(r, ctx)
                .await
                .map(ResponseBody::RelationTraverse)
        }

        // Schema ops — phase 19.6. Spec §28/05.
        RequestBody::SchemaUpload(r) => crate::knowledge_schema::handle_schema_upload(r, ctx)
            .await
            .map(ResponseBody::SchemaUpload),
        RequestBody::SchemaGet(r) => crate::knowledge_schema::handle_schema_get(r, ctx)
            .await
            .map(ResponseBody::SchemaGet),
        RequestBody::SchemaList(r) => crate::knowledge_schema::handle_schema_list(r, ctx)
            .await
            .map(ResponseBody::SchemaList),
        RequestBody::SchemaValidate(r) => crate::knowledge_schema::handle_schema_validate(r, ctx)
            .await
            .map(ResponseBody::SchemaValidate),

        // Extractor governance ops — phase 20.8. Spec §28/05 §6-§7.
        RequestBody::ExtractorList(r) => crate::knowledge_extractor::handle_extractor_list(r, ctx)
            .await
            .map(ResponseBody::ExtractorList),
        RequestBody::ExtractorDisable(r) => {
            crate::knowledge_extractor::handle_extractor_disable(r, ctx)
                .await
                .map(ResponseBody::ExtractorDisable)
        }
        RequestBody::ExtractorEnable(r) => {
            crate::knowledge_extractor::handle_extractor_enable(r, ctx)
                .await
                .map(ResponseBody::ExtractorEnable)
        }

        // Hybrid query ops — phase 23.9. Spec §24 + §28/04.
        RequestBody::Query(r) => crate::knowledge_query::handle_query(r, ctx)
            .await
            .map(ResponseBody::Query),
        RequestBody::QueryExplain(r) => crate::knowledge_query::handle_query_explain(r, ctx)
            .await
            .map(ResponseBody::QueryExplain),
        RequestBody::QueryTrace(r) => crate::knowledge_query::handle_query_trace(r, ctx)
            .await
            .map(ResponseBody::QueryTrace),
        RequestBody::RecallHybrid(r) => crate::knowledge_query::handle_recall_hybrid(r, ctx)
            .await
            .map(ResponseBody::RecallHybrid),
    }
}
