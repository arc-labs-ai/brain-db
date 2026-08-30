//! Top-level dispatch. Routes a wire `RequestBody` to its handler
//! and returns a wire `ResponseBody`: each operation
//! is a request-response interaction (or streaming for SUBSCRIBE).
//!
//! The `match req { … }` is exhaustive over `RequestBody`'s variants.
//! When the wire shape gains a new variant, this file fails to
//! compile until the corresponding arm is added — the bug-prevention
//! guarantee we want.
//!
//! The dispatcher is complete: every data-plane and typed-graph
//! variant routes to a real handler. `OpError::NotYetImplemented` now
//! marks only the two arms this crate deliberately does not own — the
//! connection-lifecycle ops handled by the network layer
//! (`brain-server`) and, on non-Linux builds, the SUBSCRIBE stub that
//! stands in for the io_uring-backed streaming path.

use brain_core::SpaceId;
use brain_metadata::api_keys::bits as perm_bits;
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;

use crate::context::OpsContext;
use crate::error::OpError;

/// Per-request caller context. Carries the AUTH-bound scope
/// (org / user / namespace / space / permissions) derived from the
/// API key the client presented — handlers read scope from here
/// instead of trusting client-supplied fields. Every caller is fully
/// scoped: there is no anonymous or permissive variant. Production
/// builds one only via [`RequestCaller::from_scope`] (a resolved key);
/// tests use [`RequestCaller::for_tests`].
#[derive(Debug, Clone)]
pub struct RequestCaller {
    /// Authenticated space, resolved from the API key.
    pub space_id: SpaceId,
    /// Tenant identity (reserved audit tag; currently zeroed).
    pub org_id: [u8; 16],
    /// User identity (optional human/service). Zero when not bound.
    pub user_id: [u8; 16],
    /// Schema namespace the caller may address, resolved from the key.
    pub namespace: String,
    /// Permission bitfield from [`brain_metadata::api_keys::bits`].
    pub permissions: u32,
    /// Human-readable structured space string the request selected
    /// (empty for a raw key-bound space). The server derived
    /// [`Self::space_id`] from it via `SpaceId::derive_from_string`; the
    /// registry keeps the string so `SPACE_LIST` can surface it.
    pub space_string: String,
    /// Wire-level session identifier minted at HELLO/WELCOME. Stamped
    /// onto every open transaction so the connection layer can
    /// auto-abort buffered work when the client's TCP/TLS connection
    /// drops before TXN_COMMIT. All-zero means "no session" (in-process
    /// test path or pre-handshake dispatch); the auto-abort sweep
    /// treats all-zero as a no-op.
    pub connection_id: [u8; 16],
}

impl RequestCaller {
    /// Construct a caller from a resolved API-key scope. This is the
    /// only production path — identity is always the credential.
    #[must_use]
    pub fn from_scope(
        space_id: SpaceId,
        org_id: [u8; 16],
        user_id: [u8; 16],
        namespace: String,
        permissions: u32,
    ) -> Self {
        Self {
            space_id,
            org_id,
            user_id,
            namespace,
            permissions,
            space_string: String::new(),
            connection_id: [0u8; 16],
        }
    }

    /// Test-only caller: a fully-scoped caller with FULL permissions and
    /// no namespace lock. Production code resolves callers from a key via
    /// [`RequestCaller::from_scope`]; this exists solely so in-process
    /// unit/integration tests can drive dispatch without minting a key.
    #[doc(hidden)]
    #[must_use]
    pub fn for_tests() -> Self {
        Self {
            space_id: SpaceId::default(),
            org_id: [0u8; 16],
            user_id: [0u8; 16],
            namespace: String::new(),
            permissions: perm_bits::FULL,
            space_string: String::new(),
            connection_id: [0u8; 16],
        }
    }

    /// Stamp the wire-level session id minted at HELLO/WELCOME. The
    /// connection layer calls this after `to_caller()` so the txn store
    /// can link buffered work back to the originating connection.
    #[must_use]
    pub fn with_session_id(mut self, connection_id: [u8; 16]) -> Self {
        self.connection_id = connection_id;
        self
    }

    /// Stamp the human-readable space string the request selected. The
    /// network auth layer sets this from the `act_as` selector (empty for
    /// a key-bound space) so registry writes can record the original
    /// string.
    #[must_use]
    pub fn with_space_string(mut self, space_string: String) -> Self {
        self.space_string = space_string;
        self
    }

    /// True iff every bit in `op` is set on this caller's permission
    /// bitfield.
    #[must_use]
    pub fn allows(&self, op: u32) -> bool {
        self.permissions & op == op
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

    /// Returns `Err(OpError::Unauthorized)` when the request's claimed
    /// space doesn't match the key's space.
    pub fn require_space(&self, claimed: SpaceId, what: &'static str) -> Result<(), OpError> {
        if self.space_id == claimed {
            Ok(())
        } else {
            Err(OpError::Unauthorized(format!(
                "{what}: API key is bound to a different space_id"
            )))
        }
    }

    /// Returns `Err(OpError::Unauthorized)` when the schema namespace the
    /// request targets doesn't match the key's namespace. An empty caller
    /// namespace (test-only callers) imposes no lock.
    pub fn require_namespace(&self, claimed: &str, what: &'static str) -> Result<(), OpError> {
        if self.namespace.is_empty() || self.namespace == claimed {
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
#[allow(clippy::large_enum_variant)] // Boxing Single would force a heap alloc on the dispatch hot
                                     // path for the common (non-streaming) case to save ~280 bytes of
                                     // stack. Not worth it — the outcome lives for the duration of one
                                     // request and is consumed by the framing layer immediately.
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
    // First gate: every op carries a required-permission tag. An API
    // key without the bit gets rejected before any work is done.
    enforce_permission(&caller, &req)?;
    // Second gate: handlers that act as a specific space_id must see
    // the AUTH-bound one, not whatever the client claimed. Namespace
    // checks for schema-touching ops happen inside the namespace-bound
    // handlers (SCHEMA_UPLOAD, etc.).
    enforce_namespace(&caller, &req)?;
    // Reads are structurally isolated to the caller's own space: RECALL no
    // longer carries any client-supplied space filter, so the handlers scope
    // to `caller.space_id` unconditionally. There is no cross-space read path
    // to gate here.

    // Per-request override: stamp the caller's space onto a clone
    // of the shared ctx so handlers that build writer Ops can pull
    // it via `ctx.executor.caller_space` without taking another
    // function param. The clone is cheap — every field is Arc'd.
    // Stamp the caller's wire session onto the per-request ctx so the
    // transaction handlers can enforce connection ownership (an in-txn
    // op is served only for the connection that opened the txn). This is
    // stamped even for the default-space test path — the ownership check
    // reads it directly.
    let needs_session = caller.connection_id != [0u8; 16];
    let per_request_ctx = if caller.space_id == brain_core::SpaceId::default() {
        // Test-only default-space caller — no space/namespace override
        // needed. Only clone when a session id must be stamped.
        if needs_session {
            let mut owned = ctx.clone();
            owned.caller_connection_id = caller.connection_id;
            Some(owned)
        } else {
            None
        }
    } else {
        let mut owned = ctx.clone();
        owned.caller_connection_id = caller.connection_id;
        owned.executor = owned
            .executor
            .with_caller_space(caller.space_id)
            .with_caller_space_string(caller.space_string.clone());
        // Fail-closed tenancy: an authenticated caller MUST carry a namespace
        // that resolves to a real per-shard NamespaceId. There is no SYSTEM
        // default for user data — falling back to the reserved system namespace
        // would silently cross tenant boundaries. The namespace name is the
        // cross-shard identity; NamespaceId is the shard-local interning, which
        // key-mint performs in a separate store, so the namespace may not yet
        // exist in this shard's metadata DB on first use here. Resolve via a
        // read fast-path, then intern-or-get on a miss.
        if caller.namespace.is_empty() {
            return Err(OpError::Unauthorized(
                "authenticated connection has no namespace; the API key must be bound to one"
                    .into(),
            ));
        }
        let ns = {
            let read = owned.executor.metadata.read_txn().ok().and_then(|rtxn| {
                brain_metadata::namespace::namespace_lookup_by_name(&rtxn, &caller.namespace)
                    .ok()
                    .flatten()
            });
            match read {
                Some(ns) => ns,
                None => {
                    let wtxn = owned
                        .executor
                        .metadata
                        .write_txn()
                        .map_err(|e| OpError::Internal(format!("namespace intern txn: {e}")))?;
                    let ns = brain_metadata::namespace::namespace_intern_or_get(
                        &wtxn,
                        &caller.namespace,
                        0,
                    )
                    .map_err(|e| OpError::Internal(format!("namespace intern: {e}")))?;
                    wtxn.commit()
                        .map_err(|e| OpError::Internal(format!("namespace intern commit: {e}")))?;
                    ns
                }
            }
        };
        if ns == brain_core::NamespaceId::SYSTEM {
            return Err(OpError::Unauthorized(
                "namespace resolved to the reserved system namespace; user data requires a real namespace".into(),
            ));
        }
        owned.executor = owned.executor.with_caller_namespace(ns);
        Some(owned)
    };
    let ctx = per_request_ctx.as_ref().unwrap_or(ctx);
    // Shorthand: one frame, wrap into DispatchOutcome::Single.
    let single = DispatchOutcome::Single;
    match req {
        // -----------------------------------------------------------
        // Cognitive primitives.
        // Handlers read `ctx.executor.caller_space` to populate
        // `space_id` on the writer Ops they build; the per-request
        // clone above ensures they see the auth-time value, not the
        // shared per-shard default.
        // -----------------------------------------------------------
        RequestBody::Encode(r) => crate::encode::handle_encode(r, ctx)
            .await
            .map(|b| single(ResponseBody::Encode(b))),

        RequestBody::EncodeVectorDirect(r) => {
            crate::encode_vector_direct::handle_encode_vector_direct(r, ctx)
                .await
                .map(|b| single(ResponseBody::EncodeVectorDirect(b)))
        }

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
        // LINK / UNLINK.
        // -----------------------------------------------------------
        RequestBody::Link(r) => crate::link::handle_link(r, ctx)
            .await
            .map(|b| single(ResponseBody::Link(b))),

        RequestBody::Unlink(r) => crate::link::handle_unlink(r, ctx)
            .await
            .map(|b| single(ResponseBody::Unlink(b))),

        RequestBody::MemoryList(r) => crate::handlers::memory_list::handle_memory_list(r, ctx)
            .await
            .map(|b| single(ResponseBody::MemoryList(b))),

        RequestBody::MemoryInspect(r) => {
            crate::handlers::memory_inspect::handle_memory_inspect(r, ctx)
                .await
                .map(|b| single(ResponseBody::MemoryInspect(b)))
        }

        RequestBody::GraphFetch(r) => crate::handlers::graph_fetch::handle_graph_fetch(r, ctx)
            .map(|b| single(ResponseBody::GraphFetch(b))),

        // -----------------------------------------------------------
        // Streaming. First-event shape only; subsequent
        // events ride a broadcast channel.
        // -----------------------------------------------------------
        RequestBody::Subscribe(r) => crate::subscribe::handle_subscribe(r, ctx)
            .await
            .map(|b| single(ResponseBody::SubscribeEvent(b))),

        RequestBody::Unsubscribe(r) => crate::subscribe::handle_unsubscribe(r, ctx)
            .await
            .map(|b| single(ResponseBody::Unsubscribe(b))),

        // -----------------------------------------------------------
        // Capability introspection. Same permission model as the
        // connection-lifecycle ops above (no special caller bits) —
        // capability bits don't reveal sensitive state and clients
        // call this at session warm-up.
        // -----------------------------------------------------------
        RequestBody::GetCapabilities(r) => {
            crate::handlers::capabilities::handle_get_capabilities(r, ctx)
                .await
                .map(|b| single(ResponseBody::GetCapabilities(b)))
        }

        // -----------------------------------------------------------
        // Transactions.
        // -----------------------------------------------------------
        // TXN_BEGIN stamps the wire-level session id on the entry so
        // the connection-drop sweep (on connection drop
        // before commit, none of the operations take effect) can
        // identify which buffered work belongs to a dying connection.
        RequestBody::TxnBegin(r) => crate::txn::handle_txn_begin(r, caller.connection_id, ctx)
            .await
            .map(|b| single(ResponseBody::TxnBegin(b))),

        RequestBody::TxnCommit(r) => crate::txn::handle_txn_commit(r, ctx)
            .await
            .map(|b| single(ResponseBody::TxnCommit(b))),

        RequestBody::TxnAbort(r) => crate::txn::handle_txn_abort(r, ctx)
            .await
            .map(|b| single(ResponseBody::TxnAbort(b))),

        // -----------------------------------------------------------
        // Connection lifecycle — brain-server owns these.
        // -----------------------------------------------------------
        RequestBody::Hello(_)
        | RequestBody::Auth(_)
        | RequestBody::Bye(_)
        | RequestBody::Ping(_)
        | RequestBody::ClientPong(_)
        | RequestBody::CancelStream(_) => Err(OpError::NotYetImplemented(
            "connection-lifecycle op — owned by the network layer (server)",
        )),

        // -----------------------------------------------------------
        // Admin ops — workers / server own these.
        // -----------------------------------------------------------
        RequestBody::AdminListPendingContradictions(r) => {
            handle_list_pending_contradictions(r, ctx).map(single)
        }

        // Substrate admin ops (stats / snapshot / restore / integrity-check /
        // migrate-embeddings / context create+rename / move-memory / reclassify
        // / list-tombstoned / backfill control) are NOT wire operations. The
        // operator control plane is the HTTP admin listener (the SDK +
        // observability specs make admin HTTP-only — "Brain ships no CLI;
        // operators administer over the admin API"). The opcodes remain
        // allocated in the wire table for namespace stability, but a client
        // that sends one is on the wrong plane: reject it permanently and point
        // at HTTP, rather than implying "coming later". (The typed-graph admin
        // family and SCHEMA_REPLACE ARE genuine per-shard wire ops, dispatched
        // above.)
        //
        // Backfill control (submit / cancel) rejects here too: the resumable
        // BackfillWorker is driven from the HTTP admin listener
        // (`POST/GET/DELETE /v1/backfill`), which reaches the same per-shard
        // worker handle through the shard's message loop. It is not a wire op.
        RequestBody::AdminStats(_)
        | RequestBody::AdminSnapshot(_)
        | RequestBody::AdminRestore(_)
        | RequestBody::AdminIntegrityCheck(_)
        | RequestBody::AdminMigrateEmbeddings(_)
        | RequestBody::AdminCreateSession(_)
        | RequestBody::AdminRenameSession(_)
        | RequestBody::AdminMoveMemory(_)
        | RequestBody::AdminReclassify(_)
        | RequestBody::AdminListTombstoned(_)
        | RequestBody::AdminBackfill(_)
        | RequestBody::AdminBackfillCancel(_) => Err(OpError::InvalidRequest(
            "admin operations are served over the HTTP admin listener, not the wire protocol"
                .to_owned(),
        )),

        // -----------------------------------------------------------
        // typed-graph phases.
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

        // Statement ops.
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

        // Relation ops.
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

        // Schema ops.
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
        RequestBody::SchemaReplace(r) => {
            crate::handlers::schema_replace::handle_schema_replace(r, ctx)
                .await
                .map(|b| single(ResponseBody::SchemaReplace(b)))
        }

        // Extractor introspection (read-only).
        RequestBody::ExtractorList(r) => {
            crate::handlers::extractor_admin::handle_extractor_list(r, ctx)
                .await
                .map(|b| single(ResponseBody::ExtractorList(b)))
        }

        // Retrieval query introspection ops.
        RequestBody::QueryExplain(r) => crate::query::handle_query_explain(r, ctx)
            .await
            .map(|b| single(ResponseBody::QueryExplain(b))),
        RequestBody::QueryTrace(r) => crate::query::handle_query_trace(r, ctx)
            .await
            .map(|b| single(ResponseBody::QueryTrace(b))),

        // Procedural-memory materialization (W3.1, wire v2).
        RequestBody::MaterializeProcedural(r) => {
            crate::handlers::procedural::handle_materialize_procedural(r, ctx)
                .await
                .map(|b| single(ResponseBody::MaterializeProcedural(b)))
        }

        // Space & session registry (non-admin, scoped to caller).
        RequestBody::SpaceCreate(r) => crate::handlers::space::handle_space_create(r, ctx)
            .await
            .map(|b| single(ResponseBody::SpaceCreate(b))),
        RequestBody::SpaceList(r) => crate::handlers::space::handle_space_list(r, ctx)
            .await
            .map(|b| single(ResponseBody::SpaceList(b))),
        RequestBody::SpaceDelete(r) => crate::handlers::space::handle_space_delete(r, ctx)
            .await
            .map(|b| single(ResponseBody::SpaceDelete(b))),
        RequestBody::SessionCreate(r) => crate::handlers::session::handle_session_create(r, ctx)
            .await
            .map(|b| single(ResponseBody::SessionCreate(b))),
        RequestBody::SessionList(r) => crate::handlers::session::handle_session_list(r, ctx)
            .await
            .map(|b| single(ResponseBody::SessionList(b))),
        RequestBody::SessionDelete(r) => crate::handlers::session::handle_session_delete(r, ctx)
            .await
            .map(|b| single(ResponseBody::SessionDelete(b))),
    }
}

/// Default cap on returned open contradictions when the request passes
/// `limit == 0`.
const DEFAULT_CONTRADICTION_LIST_LIMIT: usize = 256;

/// `ADMIN_LIST_PENDING_CONTRADICTIONS` — return open Fact-vs-Fact
/// contradictions. Opens one metadata write txn: the lister prunes
/// no-longer-live ids and lazily resolves rows that no longer
/// contradict, so the audit index self-heals on each call.
fn handle_list_pending_contradictions(
    req: brain_protocol::envelope::request::AdminListPendingContradictionsRequest,
    ctx: &OpsContext,
) -> Result<ResponseBody, OpError> {
    use brain_protocol::envelope::response::{
        AdminListPendingContradictionsResponse, ContradictionAuditView,
    };

    let limit = if req.limit == 0 {
        DEFAULT_CONTRADICTION_LIST_LIMIT
    } else {
        req.limit as usize
    };
    let now = crate::clock::now_unix_nanos();

    let metadata = &ctx.executor.metadata;
    let wtxn = metadata
        .write_txn()
        .map_err(|e| OpError::Internal(format!("contradiction list wtxn: {e}")))?;
    let rows = brain_metadata::statement::contradiction_audit_list_pending(&wtxn, limit, now)
        .map_err(|e| OpError::Internal(format!("contradiction list: {e}")))?;
    wtxn.commit()
        .map_err(|e| OpError::Internal(format!("contradiction list commit: {e}")))?;

    let contradictions = rows
        .into_iter()
        .map(|r| ContradictionAuditView {
            audit_id: r.audit_id_bytes,
            subject_id: r.subject_bytes,
            predicate_id: r.predicate_id,
            contradicting_statement_ids: r.contradicting_statement_ids,
            detected_at_unix_nanos: r.detected_at_unix_nanos,
            outcome: r.outcome,
        })
        .collect();
    Ok(ResponseBody::AdminListPendingContradictions(
        AdminListPendingContradictionsResponse { contradictions },
    ))
}

/// Map each `RequestBody` variant to the permission bit it needs and
/// fail with `Unauthorized` when the caller's bitfield lacks it.
fn enforce_permission(caller: &RequestCaller, req: &RequestBody) -> Result<(), OpError> {
    let (op_bit, what): (u32, &'static str) = match req {
        // Cognitive primitives.
        RequestBody::Encode(_) => (perm_bits::ENCODE, "ENCODE"),
        RequestBody::EncodeVectorDirect(_) => (perm_bits::ENCODE, "ENCODE_VECTOR_DIRECT"),
        RequestBody::Recall(_) | RequestBody::Plan(_) | RequestBody::Reason(_) => {
            (perm_bits::RECALL, "RECALL")
        }
        RequestBody::Forget(_) => (perm_bits::FORGET, "FORGET"),

        // Edge mutation.
        RequestBody::Link(_) | RequestBody::Unlink(_) => (perm_bits::LINK, "LINK"),

        // Enumeration read — same capability as RECALL.
        RequestBody::MemoryList(_) => (perm_bits::RECALL, "MEMORY_LIST"),
        // Per-memory inspection — same read capability.
        RequestBody::MemoryInspect(_) => (perm_bits::RECALL, "MEMORY_INSPECT"),

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

        // Schema ops. SCHEMA_UPLOAD merges additively (any schema-writer
        // key); SCHEMA_REPLACE is the destructive namespace wipe, so it
        // requires the ADMIN capability — a routine upload key must not be
        // able to drop or narrow an existing namespace.
        RequestBody::SchemaUpload(_) => (perm_bits::SCHEMA_UPLOAD, "SCHEMA_UPLOAD"),
        RequestBody::SchemaReplace(_) => (perm_bits::ADMIN, "SCHEMA_REPLACE"),
        RequestBody::SchemaGet(_) | RequestBody::SchemaList(_) | RequestBody::SchemaValidate(_) => {
            (perm_bits::RECALL, "SCHEMA_READ")
        }

        // typed-graph writes ride under ENCODE.
        RequestBody::EntityCreate(_)
        | RequestBody::EntityUpdate(_)
        | RequestBody::EntityRename(_)
        | RequestBody::EntityMerge(_)
        | RequestBody::EntityUnmerge(_)
        | RequestBody::StatementCreate(_)
        | RequestBody::StatementSupersede(_)
        | RequestBody::StatementRetract(_)
        | RequestBody::RelationCreate(_)
        | RequestBody::RelationSupersede(_) => (perm_bits::ENCODE, "GRAPH_WRITE"),

        // typed-graph tombstones ride under FORGET.
        RequestBody::EntityTombstone(_)
        | RequestBody::StatementTombstone(_)
        | RequestBody::RelationTombstone(_) => (perm_bits::FORGET, "GRAPH_TOMBSTONE"),

        // typed-graph reads.
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
        | RequestBody::QueryExplain(_)
        | RequestBody::QueryTrace(_)
        | RequestBody::GraphFetch(_)
        | RequestBody::MaterializeProcedural(_) => (perm_bits::RECALL, "GRAPH_READ"),

        // Space & session registry (non-admin, scoped to the caller).
        // Provision rides ENCODE, listing rides RECALL, deletion rides FORGET.
        RequestBody::SpaceCreate(_) | RequestBody::SessionCreate(_) => {
            (perm_bits::ENCODE, "REGISTRY_CREATE")
        }
        RequestBody::SpaceList(_) | RequestBody::SessionList(_) => {
            (perm_bits::RECALL, "REGISTRY_LIST")
        }
        RequestBody::SpaceDelete(_) | RequestBody::SessionDelete(_) => {
            (perm_bits::FORGET, "REGISTRY_DELETE")
        }

        // Extractor introspection — admin-only.
        RequestBody::ExtractorList(_) => (perm_bits::ADMIN, "EXTRACTOR_ADMIN"),

        // Admin ops — admin-only.
        RequestBody::AdminStats(_)
        | RequestBody::AdminSnapshot(_)
        | RequestBody::AdminRestore(_)
        | RequestBody::AdminIntegrityCheck(_)
        | RequestBody::AdminMigrateEmbeddings(_)
        | RequestBody::AdminCreateSession(_)
        | RequestBody::AdminRenameSession(_)
        | RequestBody::AdminMoveMemory(_)
        | RequestBody::AdminReclassify(_)
        | RequestBody::AdminListTombstoned(_)
        | RequestBody::AdminListPendingContradictions(_)
        | RequestBody::AdminBackfill(_)
        | RequestBody::AdminBackfillCancel(_) => (perm_bits::ADMIN, "ADMIN"),

        // Connection-lifecycle ops never reach the dispatcher in
        // production (the network layer handles them inline); the
        // arms exist so the match is exhaustive.
        RequestBody::Hello(_)
        | RequestBody::Auth(_)
        | RequestBody::Bye(_)
        | RequestBody::Ping(_)
        | RequestBody::ClientPong(_) => return Ok(()),

        // Capability introspection is open to every authenticated
        // caller — same model as the keepalive / handshake ops above.
        // Capability bits don't reveal sensitive state and clients need
        // them at session warm-up.
        RequestBody::GetCapabilities(_) => return Ok(()),
    };
    caller.require(op_bit, what)
}

/// Reject namespace-touching ops whose target namespace doesn't match
/// the caller's bound namespace. A caller with no namespace (test-only)
/// imposes no lock.
fn enforce_namespace(caller: &RequestCaller, req: &RequestBody) -> Result<(), OpError> {
    if caller.namespace.is_empty() {
        return Ok(());
    }
    let target = match req {
        RequestBody::SchemaGet(r) => Some(r.namespace.as_str()),
        RequestBody::SchemaList(r) => Some(r.namespace.as_str()),
        _ => None,
    };
    // `SchemaReplace` carries the namespace inside the DSL; the
    // namespace-bound caller check runs at handler time after parse.
    if let Some(ns) = target {
        // The seeded `brain` system namespace is shared, read-only system
        // vocabulary: any authenticated caller may read it via SCHEMA_GET /
        // SCHEMA_LIST regardless of which tenant they're bound to. Only
        // user-namespace schema reads must match the caller's namespace.
        if ns != brain_metadata::system_schema::SYSTEM_SCHEMA_NAMESPACE {
            caller.require_namespace(ns, "namespace")?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (pure permission / namespace checks).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::EncodeRequest;
    use brain_protocol::{SchemaGetRequest, SchemaListRequest};

    fn space(byte: u8) -> SpaceId {
        let mut a = [0u8; 16];
        a[15] = byte;
        SpaceId(uuid::Uuid::from_bytes(a))
    }

    /// A fully-scoped caller with FULL permissions and no namespace lock.
    fn full_caller() -> RequestCaller {
        RequestCaller::from_scope(
            space(1),
            [0u8; 16],
            [0u8; 16],
            String::new(),
            perm_bits::FULL,
        )
    }

    fn strict(perms: u32, namespace: &str, space_id: SpaceId) -> RequestCaller {
        RequestCaller::from_scope(space_id, [1u8; 16], [0u8; 16], namespace.into(), perms)
    }

    fn encode_req() -> RequestBody {
        RequestBody::Encode(EncodeRequest {
            text: "hi".into(),
            session_id: 0,
            request_id: [0u8; 16],
            txn_id: None,
            occurred_at_unix_nanos: None,
            act_as: None,
            wait: brain_protocol::WaitMode::Ack,
            allow_duplicates: false,
        })
    }

    #[test]
    fn full_caller_passes_every_permission_check() {
        let caller = full_caller();
        assert!(enforce_permission(&caller, &encode_req()).is_ok());
        assert!(caller.allows(perm_bits::ADMIN));
        assert!(caller.allows(perm_bits::ENCODE | perm_bits::FORGET));
    }

    #[test]
    fn strict_caller_without_encode_rejects_encode() {
        let caller = strict(perm_bits::RECALL, "acme", space(1));
        let err = enforce_permission(&caller, &encode_req()).unwrap_err();
        assert!(matches!(err, OpError::Unauthorized(_)));
    }

    #[test]
    fn list_pending_contradictions_requires_admin() {
        let req = RequestBody::AdminListPendingContradictions(
            brain_protocol::envelope::request::AdminListPendingContradictionsRequest { limit: 0 },
        );
        // Full-permission caller holds ADMIN — passes the gate.
        assert!(enforce_permission(&full_caller(), &req).is_ok());
        // RECALL-only caller is rejected.
        let caller = strict(perm_bits::RECALL, "acme", space(1));
        assert!(matches!(
            enforce_permission(&caller, &req).unwrap_err(),
            OpError::Unauthorized(_)
        ));
    }

    #[test]
    fn strict_caller_with_encode_passes() {
        let caller = strict(perm_bits::ENCODE | perm_bits::RECALL, "acme", space(1));
        assert!(enforce_permission(&caller, &encode_req()).is_ok());
    }

    #[test]
    fn strict_mode_enforces_namespace_scope() {
        let caller = strict(
            perm_bits::RECALL | perm_bits::SCHEMA_UPLOAD,
            "brain",
            space(1),
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
        let caller = strict(perm_bits::RECALL, "", space(1));
        let req = RequestBody::SchemaList(SchemaListRequest {
            namespace: "anywhere".into(),
            limit: 0,
            cursor: Vec::new(),
        });
        assert!(enforce_namespace(&caller, &req).is_ok());
    }

    #[test]
    fn caller_is_bound_to_its_own_space() {
        let caller = strict(perm_bits::ENCODE, "ns", space(1));
        assert!(caller.require_space(space(1), "test").is_ok());
        assert!(caller.require_space(space(2), "test").is_err());

        // A caller bound to space(1) cannot act as another space.
        let p = full_caller();
        assert!(p.require_space(space(99), "test").is_err());
        assert!(p.require_space(space(1), "test").is_ok());
    }
}
