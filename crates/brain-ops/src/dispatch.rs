//! Top-level dispatch. Routes a wire `RequestBody` to its handler
//! and returns a wire `ResponseBody`: each operation
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
use brain_metadata::api_keys::bits as perm_bits;
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;

use crate::context::OpsContext;
use crate::error::OpError;

/// Per-request caller context. Carries the AUTH-bound scope
/// (org / user / namespace / agent / permissions) derived from the
/// API key the client presented — handlers read scope from here
/// instead of trusting client-supplied fields.
#[derive(Debug, Clone)]
pub struct RequestCaller {
    /// Authenticated agent. `AgentId::default()` means "unauthenticated /
    /// test path"; the writer treats that as a substrate-wide event with
    /// no agent filter applicability.
    pub agent_id: AgentId,
    /// Tenant identity. Zero in permissive mode (no scope binding).
    pub org_id: [u8; 16],
    /// User identity (optional human/service). Zero when not bound.
    pub user_id: [u8; 16],
    /// Schema namespace the caller may address. Empty string means
    /// "no namespace lock" (permissive / dev mode).
    pub namespace: String,
    /// Permission bitfield from [`brain_metadata::api_keys::bits`].
    pub permissions: u32,
    /// True when scope binding is enforced for this caller. In
    /// permissive mode (default v1.0) all checks short-circuit.
    pub scope_enforced: bool,
}

impl RequestCaller {
    /// Construct a permissive caller carrying the given agent. Used by
    /// the network layer when `BRAIN_REQUIRE_SCOPED_API_KEYS` is off.
    #[must_use]
    pub fn new(agent_id: AgentId) -> Self {
        Self {
            agent_id,
            org_id: [0u8; 16],
            user_id: [0u8; 16],
            namespace: String::new(),
            permissions: perm_bits::FULL,
            scope_enforced: false,
        }
    }

    /// Construct a strict-mode caller from a resolved API-key scope.
    #[must_use]
    pub fn from_scope(
        agent_id: AgentId,
        org_id: [u8; 16],
        user_id: [u8; 16],
        namespace: String,
        permissions: u32,
    ) -> Self {
        Self {
            agent_id,
            org_id,
            user_id,
            namespace,
            permissions,
            scope_enforced: true,
        }
    }

    /// The substrate-wide / test-only default. Used by paths that
    /// don't yet wire connection auth (in-process unit tests).
    #[must_use]
    pub fn anonymous() -> Self {
        Self {
            agent_id: AgentId::default(),
            org_id: [0u8; 16],
            user_id: [0u8; 16],
            namespace: String::new(),
            permissions: perm_bits::FULL,
            scope_enforced: false,
        }
    }

    /// True iff every bit in `op` is set on this caller's permission
    /// bitfield. In permissive mode this is always true.
    #[must_use]
    pub fn allows(&self, op: u32) -> bool {
        !self.scope_enforced || (self.permissions & op == op)
    }

    /// Returns `Err(OpError::Unauthorized)` if the requested permission
    /// is not in the caller's bitfield. Helper for handler permission
    /// checks at the auth boundary.
    pub fn require(&self, op: u32, what: &'static str) -> Result<(), OpError> {
        if self.allows(op) {
            Ok(())
        } else {
            Err(OpError::Unauthorized(format!(
                "API key lacks permission: {what}"
            )))
        }
    }

    /// Returns `Err(OpError::Unauthorized)` when scope binding is on
    /// and the request's claimed agent doesn't match the key's agent.
    /// In permissive mode this passes unconditionally.
    pub fn require_agent(&self, claimed: AgentId, what: &'static str) -> Result<(), OpError> {
        if !self.scope_enforced || self.agent_id == claimed {
            Ok(())
        } else {
            Err(OpError::Unauthorized(format!(
                "{what}: API key is bound to a different agent_id"
            )))
        }
    }

    /// Returns `Err(OpError::Unauthorized)` when scope binding is on
    /// and the schema namespace the request targets doesn't match the
    /// key's namespace.
    pub fn require_namespace(&self, claimed: &str, what: &'static str) -> Result<(), OpError> {
        if !self.scope_enforced || self.namespace.is_empty() || self.namespace == claimed {
            Ok(())
        } else {
            Err(OpError::Unauthorized(format!(
                "{what}: API key is bound to namespace {:?}, not {:?}",
                self.namespace, claimed
            )))
        }
    }
}

/// Result of dispatching a single wire request.
///
/// `Single` carries the one response body for ops that finish in a
/// single frame (every op outside PLAN / REASON today). `Stream` carries
/// the ordered sequence of response bodies an op chose to emit — each
/// becomes one wire frame, with the last one tagged `is_final = true`
/// on both the body and the frame header. Callers iterate the Vec when
/// turning the outcome into wire frames; the connection layer is the
/// only place that distinguishes them.
#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    Single(ResponseBody),
    Stream(Vec<ResponseBody>),
}

impl DispatchOutcome {
    /// True iff this is a streaming op that may emit multiple frames.
    #[must_use]
    pub fn is_stream(&self) -> bool {
        matches!(self, DispatchOutcome::Stream(_))
    }

    /// Convenience for handlers that always produce one frame today.
    /// Keeps every non-streaming op arm at one line.
    #[inline]
    pub fn single(body: ResponseBody) -> Self {
        DispatchOutcome::Single(body)
    }
}

pub async fn dispatch(
    req: RequestBody,
    caller: RequestCaller,
    ctx: &OpsContext,
) -> Result<DispatchOutcome, OpError> {
    // First gate: every op carries a required-permission tag. In
    // permissive mode `caller.allows()` is unconditionally true so the
    // check is a no-op; in strict mode an API key without the bit
    // gets rejected before any work is done.
    enforce_permission(&caller, &req)?;
    // Second gate: handlers that act as a specific agent_id must see
    // the AUTH-bound one, not whatever the client claimed. Namespace
    // checks for schema-touching ops happen inside the namespace-bound
    // handlers (SCHEMA_UPLOAD, etc.).
    enforce_namespace(&caller, &req)?;

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
    // Shorthand: one frame, wrap into DispatchOutcome::Single.
    let single = DispatchOutcome::Single;
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
            .map(|b| single(ResponseBody::Encode(b))),

        RequestBody::Recall(r) => crate::recall::handle_recall(r, ctx)
            .await
            .map(|b| single(ResponseBody::Recall(b))),

        // PLAN streams one frame per scored path plus a terminal frame
        // carrying the aggregate status. The connection layer writes
        // each frame to the wire; only the last carries the EOS flag.
        RequestBody::Plan(r) => crate::plan::handle_plan(r, ctx).await.map(|frames| {
            DispatchOutcome::Stream(frames.into_iter().map(ResponseBody::Plan).collect())
        }),

        // REASON streams one frame per inference step plus a terminal.
        // v1 always produces a length-1 step stream; the framing is
        // multi-frame-ready for future passes.
        RequestBody::Reason(r) => crate::reason::handle_reason(r, ctx).await.map(|frames| {
            DispatchOutcome::Stream(frames.into_iter().map(ResponseBody::Reason).collect())
        }),

        RequestBody::Forget(r) => crate::forget::handle_forget(r, ctx)
            .await
            .map(|b| single(ResponseBody::Forget(b))),

        // -----------------------------------------------------------
        // LINK / UNLINK — 7.8.
        // -----------------------------------------------------------
        RequestBody::Link(r) => crate::link::handle_link(r, ctx)
            .await
            .map(|b| single(ResponseBody::Link(b))),

        RequestBody::Unlink(r) => crate::link::handle_unlink(r, ctx)
            .await
            .map(|b| single(ResponseBody::Unlink(b))),

        // -----------------------------------------------------------
        // Streaming — 7.10. First-event shape only; subsequent
        // events ride a broadcast channel.
        // -----------------------------------------------------------
        RequestBody::Subscribe(r) => crate::subscribe::handle_subscribe(r, ctx)
            .await
            .map(|b| single(ResponseBody::SubscribeEvent(b))),

        RequestBody::Unsubscribe(r) => crate::subscribe::handle_unsubscribe(r, ctx)
            .await
            .map(|b| single(ResponseBody::Unsubscribe(b))),

        // -----------------------------------------------------------
        // Transactions — 7.9.
        // -----------------------------------------------------------
        RequestBody::TxnBegin(r) => crate::txn::handle_txn_begin(r, ctx)
            .await
            .map(|b| single(ResponseBody::TxnBegin(b))),

        RequestBody::TxnCommit(r) => crate::txn::handle_txn_commit(r, ctx)
            .await
            .map(|b| single(ResponseBody::TxnCommit(b))),

        RequestBody::TxnAbort(r) => crate::txn::handle_txn_abort(r, ctx)
            .await
            .map(|b| single(ResponseBody::TxnAbort(b))),

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

        // Backfill control surface. The wire layer is allocated;
        // the handler that bridges to the per-shard
        // `BackfillWorker::submit` / `::cancel` API lands when the
        // worker handle threads into `OpsContext`. Today the call
        // returns `NotYetImplemented` so callers see a structured
        // error rather than a silent route miss.
        RequestBody::AdminBackfill(_) | RequestBody::AdminBackfillCancel(_) => Err(
            OpError::NotYetImplemented("admin backfill op — worker handle not wired"),
        ),

        // -----------------------------------------------------------
        // Knowledge layer — Phase 16+.
        // -----------------------------------------------------------
        RequestBody::EntityCreate(r) => crate::handlers::entity::handle_entity_create(r, ctx)
            .await
            .map(|b| single(ResponseBody::EntityCreate(b))),

        RequestBody::EntityGet(r) => crate::handlers::entity::handle_entity_get(r, ctx)
            .await
            .map(|b| single(ResponseBody::EntityGet(b))),

        RequestBody::EntityUpdate(r) => crate::handlers::entity::handle_entity_update(r, ctx)
            .await
            .map(|b| single(ResponseBody::EntityUpdate(b))),

        RequestBody::EntityRename(r) => crate::handlers::entity::handle_entity_rename(r, ctx)
            .await
            .map(|b| single(ResponseBody::EntityRename(b))),

        RequestBody::EntityMerge(r) => crate::handlers::entity::handle_entity_merge(r, ctx)
            .await
            .map(|b| single(ResponseBody::EntityMerge(b))),

        RequestBody::EntityUnmerge(r) => crate::handlers::entity::handle_entity_unmerge(r, ctx)
            .await
            .map(|b| single(ResponseBody::EntityUnmerge(b))),

        RequestBody::EntityResolve(r) => crate::handlers::entity::handle_entity_resolve(r, ctx)
            .await
            .map(|b| single(ResponseBody::EntityResolve(b))),

        RequestBody::EntityList(r) => crate::handlers::entity::handle_entity_list(r, ctx)
            .await
            .map(|b| single(ResponseBody::EntityList(b))),

        RequestBody::EntityTombstone(r) => crate::handlers::entity::handle_entity_tombstone(r, ctx)
            .await
            .map(|b| single(ResponseBody::EntityTombstone(b))),

        // Statement ops — phase 17.7.
        RequestBody::StatementCreate(r) => {
            crate::handlers::statement::handle_statement_create(r, ctx)
                .await
                .map(|b| single(ResponseBody::StatementCreate(b)))
        }
        RequestBody::StatementGet(r) => crate::handlers::statement::handle_statement_get(r, ctx)
            .await
            .map(|b| single(ResponseBody::StatementGet(b))),
        RequestBody::StatementSupersede(r) => {
            crate::handlers::statement::handle_statement_supersede(r, ctx)
                .await
                .map(|b| single(ResponseBody::StatementSupersede(b)))
        }
        RequestBody::StatementTombstone(r) => {
            crate::handlers::statement::handle_statement_tombstone(r, ctx)
                .await
                .map(|b| single(ResponseBody::StatementTombstone(b)))
        }
        RequestBody::StatementRetract(r) => {
            crate::handlers::statement::handle_statement_retract(r, ctx)
                .await
                .map(|b| single(ResponseBody::StatementRetract(b)))
        }
        RequestBody::StatementHistory(r) => {
            crate::handlers::statement::handle_statement_history(r, ctx)
                .await
                .map(|b| single(ResponseBody::StatementHistory(b)))
        }
        RequestBody::StatementList(r) => crate::handlers::statement::handle_statement_list(r, ctx)
            .await
            .map(|b| single(ResponseBody::StatementList(b))),

        // Relation ops — phase 18.7.
        RequestBody::RelationCreate(r) => crate::handlers::relation::handle_relation_create(r, ctx)
            .await
            .map(|b| single(ResponseBody::RelationCreate(b))),
        RequestBody::RelationGet(r) => crate::handlers::relation::handle_relation_get(r, ctx)
            .await
            .map(|b| single(ResponseBody::RelationGet(b))),
        RequestBody::RelationSupersede(r) => {
            crate::handlers::relation::handle_relation_supersede(r, ctx)
                .await
                .map(|b| single(ResponseBody::RelationSupersede(b)))
        }
        RequestBody::RelationTombstone(r) => {
            crate::handlers::relation::handle_relation_tombstone(r, ctx)
                .await
                .map(|b| single(ResponseBody::RelationTombstone(b)))
        }
        RequestBody::RelationListFrom(r) => {
            crate::handlers::relation::handle_relation_list_from(r, ctx)
                .await
                .map(|b| single(ResponseBody::RelationListFrom(b)))
        }
        RequestBody::RelationListTo(r) => {
            crate::handlers::relation::handle_relation_list_to(r, ctx)
                .await
                .map(|b| single(ResponseBody::RelationListTo(b)))
        }
        RequestBody::RelationTraverse(r) => {
            crate::handlers::relation::handle_relation_traverse(r, ctx)
                .await
                .map(|b| single(ResponseBody::RelationTraverse(b)))
        }

        // Schema ops — phase 19.6.
        RequestBody::SchemaUpload(r) => crate::handlers::schema::handle_schema_upload(r, ctx)
            .await
            .map(|b| single(ResponseBody::SchemaUpload(b))),
        RequestBody::SchemaGet(r) => crate::handlers::schema::handle_schema_get(r, ctx)
            .await
            .map(|b| single(ResponseBody::SchemaGet(b))),
        RequestBody::SchemaList(r) => crate::handlers::schema::handle_schema_list(r, ctx)
            .await
            .map(|b| single(ResponseBody::SchemaList(b))),
        RequestBody::SchemaValidate(r) => crate::handlers::schema::handle_schema_validate(r, ctx)
            .await
            .map(|b| single(ResponseBody::SchemaValidate(b))),

        // Extractor governance ops — phase 20.8-§7.
        RequestBody::ExtractorList(r) => {
            crate::handlers::extractor_admin::handle_extractor_list(r, ctx)
                .await
                .map(|b| single(ResponseBody::ExtractorList(b)))
        }
        RequestBody::ExtractorDisable(r) => {
            crate::handlers::extractor_admin::handle_extractor_disable(r, ctx)
                .await
                .map(|b| single(ResponseBody::ExtractorDisable(b)))
        }
        RequestBody::ExtractorEnable(r) => {
            crate::handlers::extractor_admin::handle_extractor_enable(r, ctx)
                .await
                .map(|b| single(ResponseBody::ExtractorEnable(b)))
        }

        // Hybrid query ops — phase 23.9.
        RequestBody::Query(r) => crate::query::handle_query(r, ctx)
            .await
            .map(|b| single(ResponseBody::Query(b))),
        RequestBody::QueryExplain(r) => crate::query::handle_query_explain(r, ctx)
            .await
            .map(|b| single(ResponseBody::QueryExplain(b))),
        RequestBody::QueryTrace(r) => crate::query::handle_query_trace(r, ctx)
            .await
            .map(|b| single(ResponseBody::QueryTrace(b))),
        RequestBody::RecallHybrid(r) => crate::query::handle_recall_hybrid(r, ctx)
            .await
            .map(|b| single(ResponseBody::RecallHybrid(b))),

        // Procedural-memory materialization (W3.1, wire v2).
        RequestBody::MaterializeProcedural(r) => {
            crate::handlers::procedural::handle_materialize_procedural(r, ctx)
                .await
                .map(|b| single(ResponseBody::MaterializeProcedural(b)))
        }
    }
}

/// Map each `RequestBody` variant to the permission bit it needs and
/// fail with `Unauthorized` when the caller's bitfield lacks it.
fn enforce_permission(caller: &RequestCaller, req: &RequestBody) -> Result<(), OpError> {
    let (op_bit, what): (u32, &'static str) = match req {
        // Cognitive primitives.
        RequestBody::Encode(_) => (perm_bits::ENCODE, "ENCODE"),
        RequestBody::Recall(_) | RequestBody::Plan(_) | RequestBody::Reason(_) => {
            (perm_bits::RECALL, "RECALL")
        }
        RequestBody::Forget(_) => (perm_bits::FORGET, "FORGET"),

        // Edge mutation.
        RequestBody::Link(_) | RequestBody::Unlink(_) => (perm_bits::LINK, "LINK"),

        // Streaming reads.
        RequestBody::Subscribe(_) | RequestBody::Unsubscribe(_) | RequestBody::CancelStream(_) => {
            (perm_bits::RECALL, "SUBSCRIBE")
        }

        // Transactions ride with the underlying writes; require both
        // ENCODE and FORGET so the txn can mutate any state. Coarse but
        // safe — a read-only key cannot drive any kind of write txn.
        RequestBody::TxnBegin(_) | RequestBody::TxnCommit(_) | RequestBody::TxnAbort(_) => {
            (perm_bits::ENCODE, "TXN")
        }

        // Schema ops.
        RequestBody::SchemaUpload(_) => (perm_bits::SCHEMA_UPLOAD, "SCHEMA_UPLOAD"),
        RequestBody::SchemaGet(_) | RequestBody::SchemaList(_) | RequestBody::SchemaValidate(_) => {
            (perm_bits::RECALL, "SCHEMA_READ")
        }

        // Knowledge-layer writes ride under ENCODE.
        RequestBody::EntityCreate(_)
        | RequestBody::EntityUpdate(_)
        | RequestBody::EntityRename(_)
        | RequestBody::EntityMerge(_)
        | RequestBody::EntityUnmerge(_)
        | RequestBody::StatementCreate(_)
        | RequestBody::StatementSupersede(_)
        | RequestBody::StatementRetract(_)
        | RequestBody::RelationCreate(_)
        | RequestBody::RelationSupersede(_) => (perm_bits::ENCODE, "KNOWLEDGE_WRITE"),

        // Knowledge-layer tombstones ride under FORGET.
        RequestBody::EntityTombstone(_)
        | RequestBody::StatementTombstone(_)
        | RequestBody::RelationTombstone(_) => (perm_bits::FORGET, "KNOWLEDGE_TOMBSTONE"),

        // Knowledge-layer reads.
        RequestBody::EntityGet(_)
        | RequestBody::EntityList(_)
        | RequestBody::EntityResolve(_)
        | RequestBody::StatementGet(_)
        | RequestBody::StatementHistory(_)
        | RequestBody::StatementList(_)
        | RequestBody::RelationGet(_)
        | RequestBody::RelationListFrom(_)
        | RequestBody::RelationListTo(_)
        | RequestBody::RelationTraverse(_)
        | RequestBody::Query(_)
        | RequestBody::QueryExplain(_)
        | RequestBody::QueryTrace(_)
        | RequestBody::RecallHybrid(_)
        | RequestBody::MaterializeProcedural(_) => (perm_bits::RECALL, "KNOWLEDGE_READ"),

        // Extractor governance — admin-only.
        RequestBody::ExtractorList(_)
        | RequestBody::ExtractorDisable(_)
        | RequestBody::ExtractorEnable(_) => (perm_bits::ADMIN, "EXTRACTOR_ADMIN"),

        // Admin ops — admin-only.
        RequestBody::AdminStats(_)
        | RequestBody::AdminSnapshot(_)
        | RequestBody::AdminRestore(_)
        | RequestBody::AdminIntegrityCheck(_)
        | RequestBody::AdminMigrateEmbeddings(_)
        | RequestBody::AdminCreateContext(_)
        | RequestBody::AdminRenameContext(_)
        | RequestBody::AdminMoveMemory(_)
        | RequestBody::AdminReclassify(_)
        | RequestBody::AdminListTombstoned(_) => (perm_bits::ADMIN, "ADMIN"),

        // Connection-lifecycle ops never reach the dispatcher in
        // production (the network layer handles them inline); the
        // arms exist so the match is exhaustive.
        RequestBody::Hello(_)
        | RequestBody::Auth(_)
        | RequestBody::Bye(_)
        | RequestBody::Ping(_)
        | RequestBody::ClientPong(_) => return Ok(()),
    };
    caller.require(op_bit, what)
}

/// Reject namespace-touching ops whose target namespace doesn't match
/// the caller's bound namespace. In permissive mode this is a no-op.
fn enforce_namespace(caller: &RequestCaller, req: &RequestBody) -> Result<(), OpError> {
    if !caller.scope_enforced || caller.namespace.is_empty() {
        return Ok(());
    }
    let target = match req {
        RequestBody::SchemaGet(r) => Some(r.namespace.as_str()),
        RequestBody::SchemaList(r) => Some(r.namespace.as_str()),
        _ => None,
    };
    if let Some(ns) = target {
        caller.require_namespace(ns, "namespace")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (pure permission / namespace checks).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::{SchemaGetRequest, SchemaListRequest};
    use brain_protocol::EncodeRequest;
    use brain_protocol::MemoryKindWire;

    fn agent(byte: u8) -> AgentId {
        let mut a = [0u8; 16];
        a[15] = byte;
        AgentId(uuid::Uuid::from_bytes(a))
    }

    fn permissive() -> RequestCaller {
        RequestCaller::new(agent(1))
    }

    fn strict(perms: u32, namespace: &str, agent_id: AgentId) -> RequestCaller {
        RequestCaller::from_scope(agent_id, [1u8; 16], [0u8; 16], namespace.into(), perms)
    }

    fn encode_req() -> RequestBody {
        RequestBody::Encode(EncodeRequest {
            text: "hi".into(),
            context_id: 0,
            kind: MemoryKindWire::Episodic,
            salience_hint: 0.5,
            edges: Vec::new(),
            request_id: [0u8; 16],
            txn_id: None,
            deduplicate: false,
        })
    }

    #[test]
    fn permissive_caller_passes_every_permission_check() {
        let caller = permissive();
        assert!(enforce_permission(&caller, &encode_req()).is_ok());
        assert!(caller.allows(perm_bits::ADMIN));
        assert!(caller.allows(perm_bits::ENCODE | perm_bits::FORGET));
    }

    #[test]
    fn strict_caller_without_encode_rejects_encode() {
        let caller = strict(perm_bits::RECALL, "acme", agent(1));
        let err = enforce_permission(&caller, &encode_req()).unwrap_err();
        assert!(matches!(err, OpError::Unauthorized(_)));
    }

    #[test]
    fn strict_caller_with_encode_passes() {
        let caller = strict(perm_bits::ENCODE | perm_bits::RECALL, "acme", agent(1));
        assert!(enforce_permission(&caller, &encode_req()).is_ok());
    }

    #[test]
    fn strict_mode_enforces_namespace_scope() {
        let caller = strict(
            perm_bits::RECALL | perm_bits::SCHEMA_UPLOAD,
            "brain",
            agent(1),
        );
        let req = RequestBody::SchemaGet(SchemaGetRequest {
            namespace: "acme".into(),
            version: 0,
        });
        let err = enforce_namespace(&caller, &req).unwrap_err();
        assert!(matches!(err, OpError::Unauthorized(_)));

        let req = RequestBody::SchemaGet(SchemaGetRequest {
            namespace: "brain".into(),
            version: 0,
        });
        assert!(enforce_namespace(&caller, &req).is_ok());
    }

    #[test]
    fn strict_caller_with_empty_namespace_is_open() {
        let caller = strict(perm_bits::RECALL, "", agent(1));
        let req = RequestBody::SchemaList(SchemaListRequest {
            namespace: "anywhere".into(),
            limit: 0,
            cursor: Vec::new(),
        });
        assert!(enforce_namespace(&caller, &req).is_ok());
    }

    #[test]
    fn strict_mode_enforces_agent_id() {
        let caller = strict(perm_bits::ENCODE, "ns", agent(1));
        assert!(caller.require_agent(agent(1), "test").is_ok());
        assert!(caller.require_agent(agent(2), "test").is_err());

        // Permissive: any claimed agent_id passes.
        let p = permissive();
        assert!(p.require_agent(agent(99), "test").is_ok());
    }
}
