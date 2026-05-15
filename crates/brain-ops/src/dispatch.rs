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

use brain_protocol::request::RequestBody;
use brain_protocol::response::ResponseBody;

use crate::context::OpsContext;
use crate::error::OpError;

pub async fn dispatch(req: RequestBody, ctx: &OpsContext) -> Result<ResponseBody, OpError> {
    match req {
        // -----------------------------------------------------------
        // Cognitive primitives — real handlers land in 7.3-7.7.
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
    }
}
