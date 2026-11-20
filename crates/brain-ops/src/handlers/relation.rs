//! Relation wire-op handlers — `RELATION_CREATE / _GET / _SUPERSEDE /
//! _TOMBSTONE / _LIST_FROM / _LIST_TO / _TRAVERSE`.
//!
//! Each handler:
//!
//! 1. Validates the request at wire layer (cap checks, qname grammar).
//! 2. Acquires the per-shard `MetadataDb` lock.
//! 3. Resolves the `relation_type` canonical string → `RelationTypeId`
//!    via the registry.
//! 4. Projects wire → brain-core `Relation` (CREATE / SUPERSEDE).
//! 5. Opens a redb txn (read for GET / LIST / TRAVERSE, write
//!    otherwise) and calls into `brain_metadata`.
//! 6. Commits writes.
//! 7. Emits a post-commit subscription event (CREATE / SUPERSEDE /
//!    TOMBSTONE).
//! 8. Projects brain-core `Relation` → wire `RelationView`.
//!
//! These handlers do NOT yet handle cross-shard relation reads
//! or the relation embedding worker — both deferred.

use crate::clock::now_unix_nanos;
use brain_core::Relation;
use brain_core::{Cardinality, EntityId, RelationId, RelationTypeId, RequestId};
use brain_metadata::relation::ops::{
    relation_get, relation_list_from_page, relation_list_to_page, RelationListFilter,
    RelationOpError,
};
use brain_metadata::relation::qnames::RelationTypeQnames;
use brain_metadata::relation::traversal::{
    traverse, TraversalConfig, TraversalDirection, MAX_DEPTH,
};
use brain_metadata::relation::types::{
    relation_type_intern_or_get, relation_type_lookup_by_qname, relation_type_lookup_by_qname_wtxn,
    relation_types_active_for_schema, relation_types_active_for_schema_wtxn,
};
use brain_metadata::schema::store::{schema_active, schema_active_wtxn};
use brain_metadata::DeclScope;
use brain_planner::WriterError;
use brain_protocol::envelope::response::EventType;
use brain_protocol::{
    GraphEventPayload, RelationCreateRequest, RelationCreateResponse, RelationCreatedEvent,
    RelationGetRequest, RelationGetResponse, RelationListFromRequest,
    RelationListFromResponseFrame, RelationListToRequest, RelationListToResponseFrame,
    RelationSupersedeRequest, RelationSupersedeResponse, RelationSupersededEvent,
    RelationTombstoneRequest, RelationTombstoneResponse, RelationTombstonedEvent,
    RelationTraverseRequest, RelationTraverseResponseFrame, RelationView, TraversalPathWire,
    TraversalStepWire,
};

use crate::context::OpsContext;
use crate::error::{caller_tombstone_reason, OpError};
use crate::handlers::events::emit_graph_event;
use crate::handlers::limits::{
    check_byte_len, QNAME_MAX_BYTES, REASON_MESSAGE_MAX_BYTES, RELATION_PROPERTIES_MAX_BYTES,
};
use crate::handlers::writer_handle::real_writer_handle;
use crate::metrics::read_degrade::{global as read_degrade_metrics, ReadDegradeReason};
use crate::write::{
    Phase, PhaseAck, SupersedeReplacement, SupersedeReplacementId, SupersedeTarget,
    TombstoneTarget, Write, WriteId,
};

const LIST_LIMIT_MAX: u32 = 1000;
const TRAVERSE_MAX_NODES: u32 = 1000;

/// Whether relation `id`'s sidecar row belongs to the caller's
/// `(namespace, space)` scope. `relation_get` returns a brain-core
/// `Relation` with the scope dropped, so the tenant wall is enforced
/// here by re-reading the sidecar's `namespace_id` / `space_id_bytes`.
/// Fail-closed: a missing row or read error denies.
fn relation_id_in_caller_scope(ctx: &OpsContext, id: RelationId) -> bool {
    use brain_metadata::tables::relation::{RelationMetadata, RELATION_METADATA_TABLE};
    let Ok(rtxn) = ctx.executor.metadata.read_txn() else {
        return false;
    };
    let Ok(t) = rtxn.open_table(RELATION_METADATA_TABLE) else {
        return false;
    };
    let row: Option<RelationMetadata> = t.get(&id.to_bytes()).ok().flatten().map(|g| g.value());
    match row {
        Some(m) => {
            m.namespace_id == ctx.executor.caller_namespace.raw()
                && m.space_id_bytes == <[u8; 16]>::from(ctx.executor.caller_space)
        }
        None => false,
    }
}

// Upper bound on the `relation_types` filter of a traverse request. Each
// entry costs a per-qname schema-registry lookup, so cap the count to
// bound per-request I/O against a crafted payload. Mirrors RECALL's
// `MAX_PREDICATE_FILTER` rationale.
const MAX_RELATION_TYPE_FILTER: usize = 256;

// ---------------------------------------------------------------------------
// RELATION_CREATE
// ---------------------------------------------------------------------------

pub async fn handle_relation_create(
    req: RelationCreateRequest,
    ctx: &OpsContext,
) -> Result<RelationCreateResponse, OpError> {
    validate_relation_create_request(&req)?;
    let now = now_unix_nanos();
    let (namespace, name) = split_qname(&req.relation_type)?;

    // Resolve the relation-type qname (strict vs schemaless dispatch).
    //
    // Strict mode (active schema in namespace): look up the declared
    // relation_type via rtxn and raise `RelationTypeNotInSchema`, a
    // structured wire error clients can act on.
    //
    // Schemaless mode (no active schema): if the qname is already
    // interned (a prior write registered it), reuse its row; otherwise
    // pass an intern hint on the Phase so interning happens inside the
    // main submit wtxn, keeping a schemaless RELATION_CREATE to a single
    // fsync instead of a second one for a separate intern txn.
    //
    // The unified apply path calls `relation_create` which preserves
    // the single-conflict cardinality auto-supersede short-circuit:
    // when exactly one existing current relation conflicts on the
    // cardinality axis, the helper internally supersedes it with the
    // pre-minted id rather than erroring.
    // Vocabulary resolves in the scope the caller writes into: the space's
    // own declarations when it has them, otherwise the namespace's. Asking
    // the namespace base instead would reject a type the caller's own space
    // declares.
    let caller_space: [u8; 16] = ctx.executor.caller_space.into();
    let (resolved_ty, resolved_is_symmetric, intern_hint) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let active_version = schema_active(&rtxn, DeclScope::space(namespace, caller_space))
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        if let Some(version) = active_version {
            let rt = relation_type_lookup_by_qname(
                &rtxn,
                DeclScope::space(namespace, caller_space),
                name,
            )
            .map_err(OpError::from)?
            .ok_or_else(|| OpError::RelationTypeNotInSchema {
                type_name: req.relation_type.clone(),
                namespace: namespace.to_string(),
                version,
            })?;
            if !relation_types_active_for_schema(
                &rtxn,
                DeclScope::space(namespace, caller_space),
                version,
            )
            .map_err(OpError::from)?
            .contains(&rt.id)
            {
                return Err(OpError::RelationTypeNotInSchema {
                    type_name: req.relation_type.clone(),
                    namespace: namespace.to_string(),
                    version,
                });
            }
            (rt.id, rt.is_symmetric, None)
        } else {
            match relation_type_lookup_by_qname(
                &rtxn,
                DeclScope::space(namespace, caller_space),
                name,
            )
            .map_err(OpError::from)?
            {
                Some(rt) => (rt.id, rt.is_symmetric, None),
                None => (
                    // Sentinel id; apply replaces it via
                    // `relation_type_intern_or_get`.
                    RelationTypeId::from(0u32),
                    // Open-vocab default for is_symmetric. Apply re-reads
                    // the row after intern to pick up any concurrent
                    // SCHEMA_UPLOAD's declared symmetry.
                    false,
                    Some((namespace.to_string(), name.to_string())),
                ),
            }
        }
    };

    // Mint id pre-submit (mirrors encode.rs MemoryId pre-allocation).
    let new_id = RelationId::new();
    let evidence_memories = evidence_memories_from_create(&req)?;
    let valid_from_phase = if req.valid_from_unix_nanos != 0 {
        Some(req.valid_from_unix_nanos)
    } else {
        None
    };
    let valid_to_phase = if req.valid_to_unix_nanos != 0 {
        Some(req.valid_to_unix_nanos)
    } else {
        None
    };
    let extracted_at = if req.valid_from_unix_nanos != 0 {
        req.valid_from_unix_nanos
    } else {
        now
    };

    let real_writer = real_writer_handle(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_relation_create_request(&req);
    let phase = Phase::UpsertRelation {
        id: new_id,
        ty: resolved_ty,
        session: brain_core::SessionId::from(req.session_id),
        from: EntityId::from(req.from_entity),
        to: EntityId::from(req.to_entity),
        confidence: req.confidence,
        evidence_memories,
        is_symmetric: resolved_is_symmetric,
        extractor: brain_core::ExtractorId::from(req.extractor_id),
        extracted_at_unix_nanos: extracted_at,
        properties_blob: req.properties_blob.clone(),
        valid_from_unix_nanos: valid_from_phase,
        valid_to_unix_nanos: valid_to_phase,
        relation_type_intern_hint: intern_hint,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Pull the id from the ack — on an idempotency replay the writer
    // returns the cached ack whose id is the *original* (potentially
    // different from today's freshly-minted `new_id`).
    let stored_id = match ack.single_phase() {
        PhaseAck::UpsertedRelation(id, _version) => *id,
        other => {
            return Err(OpError::PermanentFault(format!(
                "unexpected phase ack for RELATION_CREATE: {other:?}"
            )));
        }
    };

    emit_graph_event(
        ctx,
        EventType::RelationCreated,
        GraphEventPayload::RelationCreated(RelationCreatedEvent {
            relation_id: stored_id.to_bytes(),
            // The wire request's `relation_type` is already the
            // canonical "namespace:name" form (validated above).
            relation_type: req.relation_type.clone(),
            from: req.from_entity,
            to: req.to_entity,
        }),
        now,
    )
    .await;

    Ok(RelationCreateResponse {
        relation_id: stored_id.to_bytes(),
    })
}

// ---------------------------------------------------------------------------
// RELATION_GET
// ---------------------------------------------------------------------------

pub async fn handle_relation_get(
    req: RelationGetRequest,
    ctx: &OpsContext,
) -> Result<RelationGetResponse, OpError> {
    let id = RelationId::from(req.relation_id);

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    let current = relation_get(&rtxn, id)
        .map_err(map_relation_op_error)?
        .ok_or_else(|| OpError::NotFound {
            what: "relation",
            detail: format!("{id:?}"),
        })?;

    // Tenant wall (unconditional): a relation named by a foreign
    // `(namespace, space)`'s id reads as NotFound.
    if !relation_id_in_caller_scope(ctx, id) {
        return Err(OpError::NotFound {
            what: "relation",
            detail: format!("{id:?}"),
        });
    }

    let (current, returned_via_supersession) = if req.follow_supersession {
        resolve_supersession_head(&rtxn, id, current)?
    } else {
        (current, false)
    };

    let view = project_view(&rtxn, &mut RelationTypeQnames::new(), &current);
    Ok(RelationGetResponse {
        relation: view,
        returned_via_supersession,
    })
}

// ---------------------------------------------------------------------------
// RELATION_SUPERSEDE
// ---------------------------------------------------------------------------

pub async fn handle_relation_supersede(
    req: RelationSupersedeRequest,
    ctx: &OpsContext,
) -> Result<RelationSupersedeResponse, OpError> {
    validate_relation_create_request(&req.new_relation)?;

    let old_id = RelationId::from(req.old_relation_id);
    let now = now_unix_nanos();
    let (namespace, name) = split_qname(&req.new_relation.relation_type)?;
    // Vocabulary resolves in the scope the caller writes into — see
    // `handle_relation_create`.
    let caller_space: [u8; 16] = ctx.executor.caller_space.into();

    // Resolve the relation-type qname (strict vs schemaless dispatch)
    // and intern-on-demand for the schemaless case BEFORE submit, so
    // the `RelationTypeNotInSchema` and intern errors keep their
    // structured wire shape. The lookup runs in a short-lived wtxn that
    // commits the intern; the supersede itself then submits via the
    // unified writer.
    let rt = {
        let wtxn = ctx
            .executor
            .metadata
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        let active_version = schema_active_wtxn(&wtxn, DeclScope::space(namespace, caller_space))
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        let rt = if let Some(version) = active_version {
            let rt = relation_type_lookup_by_qname_wtxn(
                &wtxn,
                DeclScope::space(namespace, caller_space),
                name,
            )
            .map_err(OpError::from)?
            .ok_or_else(|| OpError::RelationTypeNotInSchema {
                type_name: req.new_relation.relation_type.clone(),
                namespace: namespace.to_string(),
                version,
            })?;
            if !relation_types_active_for_schema_wtxn(
                &wtxn,
                DeclScope::space(namespace, caller_space),
                version,
            )
            .map_err(OpError::from)?
            .contains(&rt.id)
            {
                return Err(OpError::RelationTypeNotInSchema {
                    type_name: req.new_relation.relation_type.clone(),
                    namespace: namespace.to_string(),
                    version,
                });
            }
            rt
        } else {
            match relation_type_lookup_by_qname_wtxn(
                &wtxn,
                DeclScope::space(namespace, caller_space),
                name,
            )
            .map_err(OpError::from)?
            {
                Some(rt) => rt,
                None => {
                    let _ = relation_type_intern_or_get(
                        &wtxn,
                        DeclScope::base(namespace),
                        name,
                        0,
                        now,
                    )
                    .map_err(OpError::from)?;
                    relation_type_lookup_by_qname_wtxn(
                        &wtxn,
                        DeclScope::space(namespace, caller_space),
                        name,
                    )
                    .map_err(OpError::from)?
                    .expect("just-interned relation type vanished")
                }
            }
        };
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
        rt
    };

    // Pre-submit existence + ownership check so a missing OR foreign
    // `old_relation_id` keeps its `NotFound { what: "relation", .. }`
    // wire shape (submit-path failures collapse to
    // `WriterError::Internal`). Scope-aware: another tenant's relation is
    // indistinguishable from a missing one. The apply-layer wall
    // re-checks atomically.
    if !relation_id_in_caller_scope(ctx, old_id) {
        return Err(OpError::NotFound {
            what: "relation",
            detail: format!("{old_id:?}"),
        });
    }

    let new_relation = build_relation_from_create(&req.new_relation, &rt, now)?;

    let real_writer = real_writer_handle(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_relation_supersede_request(&req);
    let phase = Phase::Supersede {
        target: SupersedeTarget::Relation(old_id),
        replacement: SupersedeReplacement::Relation(Box::new(new_relation)),
        at_unix_nanos: now,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Pull the replacement id from the ack — on idempotency replay the
    // cached ack carries the originally-stored id, which won't match
    // today's freshly-minted `new_relation.id`.
    let new_id = match ack.single_phase() {
        PhaseAck::Superseded(_, SupersedeReplacementId::Relation(id)) => *id,
        other => {
            return Err(OpError::PermanentFault(format!(
                "unexpected phase ack for RELATION_SUPERSEDE: {other:?}"
            )));
        }
    };

    // Read back the new relation to surface the bumped version. The
    // PhaseAck::Superseded marker carries the replacement id but not
    // the version stamped inside the wtxn; reading post-commit avoids
    // duplicating the supersession bookkeeping in the handler.
    let version = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let new = relation_get(&rtxn, new_id)
            .map_err(map_relation_op_error)?
            .ok_or_else(|| OpError::PermanentFault("new relation missing post-supersede".into()))?;
        new.version
    };

    emit_graph_event(
        ctx,
        EventType::RelationSuperseded,
        GraphEventPayload::RelationSuperseded(RelationSupersededEvent {
            old_relation_id: old_id.to_bytes(),
            new_relation_id: new_id.to_bytes(),
        }),
        now,
    )
    .await;

    Ok(RelationSupersedeResponse {
        new_relation_id: new_id.to_bytes(),
        version,
    })
}

// ---------------------------------------------------------------------------
// RELATION_TOMBSTONE
// ---------------------------------------------------------------------------

pub async fn handle_relation_tombstone(
    req: RelationTombstoneRequest,
    ctx: &OpsContext,
) -> Result<RelationTombstoneResponse, OpError> {
    check_byte_len(
        "reason_message",
        req.reason_message.len(),
        REASON_MESSAGE_MAX_BYTES,
    )?;
    let reason = caller_tombstone_reason(req.reason)?;
    let id = RelationId::from(req.relation_id);
    let now = now_unix_nanos();

    // Pre-submit existence + ownership check — submit-path failures
    // collapse into WriterError::Internal, so check first to keep the
    // missing-id case structured as OpError::NotFound. Scope-aware:
    // another tenant's relation is indistinguishable from a missing one.
    // The apply-layer wall re-checks atomically.
    if !relation_id_in_caller_scope(ctx, id) {
        return Err(OpError::NotFound {
            what: "relation",
            detail: format!("{id:?}"),
        });
    }

    let real_writer = real_writer_handle(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_relation_tombstone_request(&req);
    let phase = Phase::Tombstone {
        target: TombstoneTarget::Relation(id),
        // brain-metadata's relation tombstone is binary: it flips bits and
        // does not read this byte today. It is carried anyway so the phase
        // records why the row was retired, the same way the statement path
        // does, and so the reclamation pass has the classification available
        // when it learns to read it.
        reason: reason.as_u8(),
        at_unix_nanos: now,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Pull the tombstone timestamp from the ack so idempotency replays
    // return the originally-stored value rather than today's clock.
    let tombstoned_at_unix_nanos = match ack.single_phase() {
        PhaseAck::Tombstoned {
            tombstoned_at_unix_nanos,
            ..
        } => *tombstoned_at_unix_nanos,
        other => {
            return Err(OpError::PermanentFault(format!(
                "unexpected phase ack for RELATION_TOMBSTONE: {other:?}"
            )));
        }
    };

    emit_graph_event(
        ctx,
        EventType::RelationTombstoned,
        GraphEventPayload::RelationTombstoned(RelationTombstonedEvent {
            relation_id: id.to_bytes(),
            reason: req.reason_message,
        }),
        now,
    )
    .await;

    Ok(RelationTombstoneResponse {
        tombstoned_at_unix_nanos,
    })
}

// ---------------------------------------------------------------------------
// RELATION_LIST_FROM / _TO
// ---------------------------------------------------------------------------

pub async fn handle_relation_list_from(
    req: RelationListFromRequest,
    ctx: &OpsContext,
) -> Result<RelationListFromResponseFrame, OpError> {
    let (items, count, next_cursor) = run_list(
        ctx,
        EntityId::from(req.from_entity),
        &req.relation_type_filter,
        req.include_superseded,
        req.include_tombstoned,
        req.limit,
        &req.cursor,
        /* from_side */ true,
    )?;
    Ok(RelationListFromResponseFrame {
        items,
        next_cursor,
        cumulative_count: count,
        is_final: true,
    })
}

pub async fn handle_relation_list_to(
    req: RelationListToRequest,
    ctx: &OpsContext,
) -> Result<RelationListToResponseFrame, OpError> {
    let (items, count, next_cursor) = run_list(
        ctx,
        EntityId::from(req.to_entity),
        &req.relation_type_filter,
        req.include_superseded,
        req.include_tombstoned,
        req.limit,
        &req.cursor,
        /* from_side */ false,
    )?;
    Ok(RelationListToResponseFrame {
        items,
        next_cursor,
        cumulative_count: count,
        is_final: true,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_list(
    ctx: &OpsContext,
    entity: EntityId,
    type_filter: &str,
    include_superseded: bool,
    include_tombstoned: bool,
    limit: u32,
    cursor: &[u8],
    from_side: bool,
) -> Result<(Vec<RelationView>, u32, Vec<u8>), OpError> {
    if limit == 0 || limit > LIST_LIMIT_MAX {
        return Err(OpError::InvalidRequest("limit must be in 1..=1000".into()));
    }
    let scope =
        brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
    let filter_sig = relation_list_filter_signature(
        type_filter,
        include_superseded,
        include_tombstoned,
        from_side,
    );
    let resume_key = decode_relation_cursor(cursor, scope, &filter_sig)?;
    let caller_space: [u8; 16] = ctx.executor.caller_space.into();

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    let relation_type = if type_filter.is_empty() {
        None
    } else {
        validate_qname(type_filter)?;
        let (ns, name) = split_qname(type_filter)?;
        // Schema-strict: unknown qname → PredicateNotInSchema-style
        // error. Schemaless: short-circuit with empty result set (no
        // matching rows are possible).
        let active_version = schema_active(&rtxn, DeclScope::space(ns, caller_space))
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        match relation_type_lookup_by_qname(&rtxn, DeclScope::space(ns, caller_space), name)
            .map_err(OpError::from)?
        {
            Some(rt) => Some(rt.id),
            None => {
                if let Some(version) = active_version {
                    return Err(OpError::RelationTypeNotInSchema {
                        type_name: type_filter.to_string(),
                        namespace: ns.to_string(),
                        version,
                    });
                }
                return Ok((Vec::new(), 0, Vec::new()));
            }
        }
    };

    let filter = RelationListFilter {
        relation_type,
        current_only: !include_superseded && !include_tombstoned,
        // The page fn takes its page size as an explicit argument; the
        // struct field is unused on this path.
        limit: 0,
    };

    // Page directly from the edge index: seek strictly past the cursor,
    // apply tombstone / current filters in the walk, and return one page
    // plus whether more remain. No 1000-row window, so relations past
    // 1000 are reachable and each page costs one page's worth of scan.
    let page = if from_side {
        relation_list_from_page(
            &rtxn,
            scope,
            entity,
            &filter,
            include_tombstoned,
            resume_key.as_deref(),
            limit as usize,
        )
    } else {
        relation_list_to_page(
            &rtxn,
            scope,
            entity,
            &filter,
            include_tombstoned,
            resume_key.as_deref(),
            limit as usize,
        )
    }
    .map_err(map_relation_op_error)?;

    let mut qnames = RelationTypeQnames::new();
    let mut out = Vec::with_capacity(page.rows.len());
    for r in &page.rows {
        out.push(project_view(&rtxn, &mut qnames, r));
    }
    let count = out.len() as u32;
    let next_cursor = match (page.has_more, page.last_key) {
        (true, Some(k)) => encode_relation_cursor(scope, &filter_sig, &k),
        _ => Vec::new(),
    };
    Ok((out, count, next_cursor))
}

// ---------------------------------------------------------------------------
// RELATION_LIST_{FROM,TO} keyset-pagination cursor.
//
// Opaque bytes on the wire (a `bytes` field — the manifest is unchanged
// and no SDK parses it). Carries the owning scope (tenant reject), a
// signature of the query filters + direction (a mid-pagination change or
// a from/to mixup fails closed), and the raw edge-table key of the last
// row so the next page seeks strictly past it in-store.
// ---------------------------------------------------------------------------

const RELATION_CURSOR_VERSION: u8 = 2;
/// `version(1) + namespace_id(4) + space_id(16) + filter_sig(8)` then the
/// variable-length raw edge key.
const RELATION_CURSOR_HEADER: usize = 1 + 4 + 16 + 8;

fn relation_list_filter_signature(
    type_filter: &str,
    include_superseded: bool,
    include_tombstoned: bool,
    from_side: bool,
) -> [u8; 8] {
    let mut h = blake3::Hasher::new();
    h.update(&(type_filter.len() as u32).to_le_bytes());
    h.update(type_filter.as_bytes());
    h.update(&[
        u8::from(include_superseded),
        u8::from(include_tombstoned),
        u8::from(from_side),
    ]);
    let full = h.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&full.as_bytes()[..8]);
    out
}

fn encode_relation_cursor(scope: brain_metadata::RowScope, sig: &[u8; 8], key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RELATION_CURSOR_HEADER + key.len());
    out.push(RELATION_CURSOR_VERSION);
    out.extend_from_slice(&scope.namespace_id.to_le_bytes());
    out.extend_from_slice(&scope.space_id_bytes);
    out.extend_from_slice(sig);
    out.extend_from_slice(key);
    out
}

fn decode_relation_cursor(
    cursor: &[u8],
    scope: brain_metadata::RowScope,
    sig: &[u8; 8],
) -> Result<Option<Vec<u8>>, OpError> {
    if cursor.is_empty() {
        return Ok(None);
    }
    // The raw edge key is always non-empty, so a valid cursor is strictly
    // longer than the header.
    if cursor.len() <= RELATION_CURSOR_HEADER || cursor[0] != RELATION_CURSOR_VERSION {
        return Err(OpError::InvalidRequest("malformed cursor".into()));
    }
    let mut ns = [0u8; 4];
    ns.copy_from_slice(&cursor[1..5]);
    if u32::from_le_bytes(ns) != scope.namespace_id || cursor[5..21] != scope.space_id_bytes {
        return Err(OpError::InvalidRequest(
            "cursor does not belong to the caller's tenant".into(),
        ));
    }
    if cursor[21..29] != *sig {
        return Err(OpError::InvalidRequest(
            "stale_cursor: filters changed between pages".into(),
        ));
    }
    Ok(Some(cursor[RELATION_CURSOR_HEADER..].to_vec()))
}

// ---------------------------------------------------------------------------
// RELATION_TRAVERSE
// ---------------------------------------------------------------------------

pub async fn handle_relation_traverse(
    req: RelationTraverseRequest,
    ctx: &OpsContext,
) -> Result<RelationTraverseResponseFrame, OpError> {
    if req.max_depth == 0 || req.max_depth > MAX_DEPTH as u32 {
        return Err(OpError::InvalidRequest(format!(
            "max_depth must be in 1..={MAX_DEPTH}"
        )));
    }
    if req.max_nodes == 0 || req.max_nodes > TRAVERSE_MAX_NODES {
        return Err(OpError::InvalidRequest(format!(
            "max_nodes must be in 1..={TRAVERSE_MAX_NODES}"
        )));
    }
    if req.relation_types.len() > MAX_RELATION_TYPE_FILTER {
        return Err(OpError::InvalidRequest(format!(
            "relation_types must have <= {MAX_RELATION_TYPE_FILTER} entries"
        )));
    }
    let direction = match req.direction {
        0 => TraversalDirection::Outgoing,
        1 => TraversalDirection::Incoming,
        2 => TraversalDirection::Both,
        other => {
            return Err(OpError::InvalidRequest(format!(
                "direction must be 0..=2; got {other}"
            )))
        }
    };

    let caller_space: [u8; 16] = ctx.executor.caller_space.into();
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    // Resolve type qnames → ids. Same dispatch as LIST: schemaless
    // unknown qnames just contribute zero matches; schema-strict
    // mode surfaces the typo to the client.
    let mut type_ids: Vec<RelationTypeId> = Vec::with_capacity(req.relation_types.len());
    for qname in &req.relation_types {
        validate_qname(qname)?;
        let (ns, name) = split_qname(qname)?;
        let active_version = schema_active(&rtxn, DeclScope::space(ns, caller_space))
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        match relation_type_lookup_by_qname(&rtxn, DeclScope::space(ns, caller_space), name)
            .map_err(OpError::from)?
        {
            Some(rt) => type_ids.push(rt.id),
            None => {
                if let Some(version) = active_version {
                    return Err(OpError::RelationTypeNotInSchema {
                        type_name: qname.clone(),
                        namespace: ns.to_string(),
                        version,
                    });
                }
                // Schemaless: skip — no rows can match this qname.
            }
        }
    }

    let config = TraversalConfig {
        max_depth: req.max_depth as u8,
        max_branching_factor: req.max_nodes,
        current_only: !req.include_superseded,
    };
    let paths = traverse(
        &rtxn,
        brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space),
        EntityId::from(req.start_entity),
        &type_ids,
        direction,
        &config,
    )
    .map_err(map_relation_op_error)?;

    // Resolve type ids to canonical strings for the wire shape. One
    // resolver for the whole walk: a traversal crosses many edges over
    // few distinct types, and a step whose type row has gone must not
    // sink the paths around it.
    let mut qnames = RelationTypeQnames::new();
    let mut path_wires = Vec::with_capacity(paths.len());
    let total_paths = paths.len();
    let mut total_steps = 0;
    let mut truncated = false;
    for p in paths {
        if path_wires.len() >= req.max_nodes as usize {
            truncated = true;
            break;
        }
        let mut steps = Vec::with_capacity(p.steps.len());
        for s in p.steps {
            let qname = qnames.resolve(&rtxn, s.relation_type);
            steps.push(TraversalStepWire {
                relation_id: s.relation_id.to_bytes(),
                from: s.from.to_bytes(),
                to: s.to.to_bytes(),
                relation_type: qname,
                depth: s.depth as u32,
            });
            total_steps += 1;
        }
        path_wires.push(TraversalPathWire { steps });
    }
    let _ = total_steps;

    Ok(RelationTraverseResponseFrame {
        paths: path_wires,
        total_paths: total_paths as u32,
        truncated,
        is_final: true,
    })
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Hop cap for the `RELATION_GET` supersession walk.
///
/// A relation chain gains one row for every correction to the same edge.
/// Relation history is retained without a substrate-level chain length
/// limit, so the cap is deliberately well above any plausible correcting
/// workload while converting corrupted state into bounded point reads in
/// one transaction instead of an endless non-yielding loop.
const RELATION_SUPERSESSION_MAX_HOPS: usize = 1024;

/// Walk `superseded_by` from `start` to the head of its supersession
/// chain, returning the head row and whether any hop was taken.
///
/// Returns [`OpError::SupersessionChainCycle`] if the chain is longer
/// than [`RELATION_SUPERSESSION_MAX_HOPS`]. A write path cannot create
/// such a cycle, but a bounded failure prevents malformed state from
/// spinning forever inside a read that never yields to the executor.
fn resolve_supersession_head(
    rtxn: &redb::ReadTransaction,
    anchor: RelationId,
    start: Relation,
) -> Result<(Relation, bool), OpError> {
    let mut current = start;
    let mut followed = false;
    for _ in 0..=RELATION_SUPERSESSION_MAX_HOPS {
        let Some(succ) = current.superseded_by else {
            return Ok((current, followed));
        };
        followed = true;
        current = relation_get(rtxn, succ)
            .map_err(map_relation_op_error)?
            .ok_or_else(|| {
                OpError::PermanentFault(format!("chain dangling at {succ:?} from {anchor:?}"))
            })?;
    }
    Err(OpError::SupersessionChainCycle {
        what: "relation",
        detail: format!("{anchor:?}"),
    })
}

fn validate_qname(q: &str) -> Result<(), OpError> {
    if q.is_empty() {
        return Err(OpError::InvalidRequest(
            "relation_type must be non-empty".into(),
        ));
    }
    check_byte_len("relation_type qname", q.len(), QNAME_MAX_BYTES)?;
    if !q.contains(':') {
        return Err(OpError::InvalidRequest(
            "relation_type must use \"namespace:name\" form".into(),
        ));
    }
    Ok(())
}

fn validate_relation_create_request(req: &RelationCreateRequest) -> Result<(), OpError> {
    validate_qname(&req.relation_type)?;
    if req.confidence.is_nan() || !(0.0..=1.0).contains(&req.confidence) {
        return Err(OpError::InvalidRequest(
            "confidence must be in [0, 1] and not NaN".into(),
        ));
    }
    check_byte_len(
        "properties_blob",
        req.properties_blob.len(),
        RELATION_PROPERTIES_MAX_BYTES,
    )
}

fn split_qname(q: &str) -> Result<(&str, &str), OpError> {
    q.split_once(':')
        .ok_or_else(|| OpError::InvalidRequest("relation_type missing ':' separator".into()))
}

/// Decode the wire `EvidenceRefWire` into the inline `Vec<MemoryId>`
/// shape the unified `Phase::UpsertRelation` carries. Overflow form is
/// not supported for relations in v1.
fn evidence_memories_from_create(
    req: &RelationCreateRequest,
) -> Result<Vec<brain_core::MemoryId>, OpError> {
    use brain_protocol::evidence_ref_from_wire;

    let evidence = evidence_ref_from_wire(&req.evidence)
        .map_err(|e| OpError::InvalidRequest(format!("evidence decode: {e}")))?;
    match evidence {
        brain_core::EvidenceRef::Inline(entries) => {
            Ok(entries.iter().map(|e| e.memory_id).collect())
        }
        brain_core::EvidenceRef::Overflow(_) => Err(OpError::InvalidRequest(
            "RELATION evidence overflow not supported in v1".into(),
        )),
    }
}

fn build_relation_from_create(
    req: &RelationCreateRequest,
    rt: &brain_core::RelationType,
    now: u64,
) -> Result<Relation, OpError> {
    let memories = evidence_memories_from_create(req)?;

    let id = RelationId::new();
    let mut r = Relation::new_root(
        id,
        rt.id,
        EntityId::from(req.from_entity),
        EntityId::from(req.to_entity),
        req.confidence,
        memories,
        brain_core::ExtractorId::from(req.extractor_id),
        if req.valid_from_unix_nanos != 0 {
            req.valid_from_unix_nanos
        } else {
            now
        },
        rt.is_symmetric,
    );
    r.properties_blob = req.properties_blob.clone();
    if req.valid_from_unix_nanos != 0 {
        r.valid_from_unix_nanos = Some(req.valid_from_unix_nanos);
    }
    if req.valid_to_unix_nanos != 0 {
        r.valid_to_unix_nanos = Some(req.valid_to_unix_nanos);
    }
    Ok(r)
}

/// Project a brain-core `Relation` to a wire `RelationView` by
/// resolving the `RelationTypeId` to its canonical qname string.
///
/// `qnames` is the caller's resolver, shared across a page. It memoises
/// the registry, so a projection loop costs one lookup per DISTINCT
/// relation type rather than one per row, and it names an unresolvable
/// type with the empty sentinel instead of failing. One edge whose type
/// row has gone would otherwise deny the caller every healthy edge
/// beside it.
///
/// The empty qname is unambiguous on this surface: `validate_qname`
/// rejects an empty `relation_type` on every write, so no healthy edge
/// can carry one, and a client reading it knows the vocabulary behind
/// the edge is gone rather than absent by design. The degraded edge is
/// counted into the degraded-row family, so an unnameable type is a rate an
/// operator can alert on and not only a log line.
fn project_view(
    rtxn: &redb::ReadTransaction,
    qnames: &mut RelationTypeQnames,
    r: &Relation,
) -> RelationView {
    let qname = qnames.resolve(rtxn, r.relation_type);
    // The resolver blanks a type it cannot name and memoises the answer, so
    // the row-level count lives here: an operator needs the rate of edges
    // reaching clients untyped, not the number of registry entries that went
    // missing.
    if qname.is_empty() {
        read_degrade_metrics().inc(ReadDegradeReason::RelationTypeQnameMissing);
    }
    RelationView::from_relation(r, qname)
}

/// BLAKE3 over the canonical RELATION_CREATE request fields. Excludes
/// `request_id` (cache key). Lets the writer detect a hash mismatch on
/// retried request_ids and surface `Conflict` instead of silently
/// returning a stale cached ack with different params.
fn hash_relation_create_request(req: &RelationCreateRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"relation_create:");
    h.update(req.relation_type.as_bytes());
    h.update(b"\0");
    h.update(&req.from_entity);
    h.update(&req.to_entity);
    h.update(&req.properties_blob);
    h.update(b"\0");
    hash_evidence_ref(&mut h, &req.evidence);
    h.update(&req.extractor_id.to_le_bytes());
    h.update(&req.confidence.to_le_bytes());
    h.update(&req.valid_from_unix_nanos.to_le_bytes());
    h.update(&req.valid_to_unix_nanos.to_le_bytes());
    h.update(&req.session_id.to_le_bytes());
    *h.finalize().as_bytes()
}

fn hash_relation_supersede_request(req: &RelationSupersedeRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"relation_supersede:");
    h.update(&req.old_relation_id);
    h.update(b"\0");
    let inner = hash_relation_create_request(&req.new_relation);
    h.update(&inner);
    *h.finalize().as_bytes()
}

fn hash_relation_tombstone_request(req: &RelationTombstoneRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"relation_tombstone:");
    h.update(&req.relation_id);
    h.update(b"\0");
    h.update(&[req.reason]);
    h.update(b"\0");
    h.update(req.reason_message.as_bytes());
    *h.finalize().as_bytes()
}

/// Fold the wire `EvidenceRefWire` discriminant + inline entries into the
/// running BLAKE3 state. Mirrors the inline-vs-overflow split decoded by
/// `evidence_ref_from_wire`.
fn hash_evidence_ref(h: &mut blake3::Hasher, ev: &brain_protocol::EvidenceRefWire) {
    use brain_protocol::EvidenceRefWire;
    match ev {
        EvidenceRefWire::Inline(entries) => {
            h.update(b"inline:");
            for e in entries {
                h.update(e);
            }
        }
        EvidenceRefWire::Overflow(id) => {
            h.update(b"overflow:");
            h.update(id);
        }
    }
}

/// Translate a `WriterError` into the wire error taxonomy. The unified
/// writer collapses all apply failures into `Internal`, so a structured
/// match isn't useful here — preserve the conflict variant cleanly and
/// forward everything else through `ExecError::WriterFailed`.
fn map_writer_err(err: WriterError) -> OpError {
    OpError::ExecError(brain_planner::ExecError::WriterFailed(err))
}

// relation_type error classification lives in `OpError`'s `From` impl
// (crate::error). `map_relation_op_error` stays local — its
// CardinalityViolation arm needs the handler's cardinality helpers.

/// Human-readable cardinality label for the wire `CardinalityViolation`
/// error variant. Stable strings — clients key off them.
fn cardinality_kind_str(c: Cardinality) -> &'static str {
    match c {
        Cardinality::OneToOne => "OneToOne",
        Cardinality::OneToMany => "OneToMany",
        Cardinality::ManyToOne => "ManyToOne",
        Cardinality::ManyToMany => "ManyToMany",
    }
}

/// The maximum number of current rows the cardinality permits per
/// `from` endpoint (or unbounded — surfaced as `u32::MAX`). Surfaced
/// to the client so they can build a sensible error message.
fn cardinality_limit(c: Cardinality) -> u32 {
    match c {
        Cardinality::OneToOne | Cardinality::ManyToOne => 1,
        Cardinality::OneToMany | Cardinality::ManyToMany => u32::MAX,
    }
}

fn map_relation_op_error(err: RelationOpError) -> OpError {
    match err {
        RelationOpError::NotFound(id) => OpError::NotFound {
            what: "relation",
            detail: format!("{id:?}"),
        },
        RelationOpError::AlreadyExists(id) => {
            OpError::Conflict(format!("relation {id:?} already exists"))
        }
        RelationOpError::UnknownRelationType(id) => {
            OpError::InvalidRequest(format!("unknown relation_type {id:?}"))
        }
        RelationOpError::UnknownEntity(id) => OpError::NotFound {
            what: "entity",
            detail: format!("{id:?}"),
        },
        RelationOpError::InvalidArgument(s) => OpError::InvalidRequest(s.to_string()),
        RelationOpError::AlreadySuperseded(id, by) => {
            OpError::Conflict(format!("relation {id:?} already superseded by {by:?}"))
        }
        RelationOpError::AlreadyTombstoned(id) => {
            OpError::Conflict(format!("relation {id:?} is tombstoned"))
        }
        RelationOpError::TypeMismatch { old, new } => OpError::InvalidRequest(format!(
            "relation_type mismatch on supersede: old={old:?} new={new:?}"
        )),
        RelationOpError::EndpointMismatch => {
            OpError::InvalidRequest("endpoints must match on supersede".into())
        }
        RelationOpError::CardinalityViolation {
            variant,
            conflicting,
        } => OpError::CardinalityViolation {
            relation_type: String::new(),
            kind: cardinality_kind_str(variant),
            existing: conflicting as u32,
            limit: cardinality_limit(variant),
        },
        // Declared `from` / `to` entity type violated. Same layer as
        // the cardinality arm above; surfaced as an invalid request
        // because the client fixes it by picking a conforming endpoint.
        e @ RelationOpError::EndpointTypeViolation { .. } => OpError::InvalidRequest(e.to_string()),
        RelationOpError::Storage(e) => OpError::Internal(format!("redb storage: {e}")),
        RelationOpError::Table(e) => OpError::Internal(format!("redb table: {e}")),
        RelationOpError::EdgeOp(e) => OpError::Internal(format!("edge op: {e}")),
        RelationOpError::EdgeKey(e) => {
            OpError::PermanentFault(format!("edge key decode (corruption?): {e}"))
        }
        RelationOpError::RelationTypeOp(e) => OpError::from(e),
        RelationOpError::EntityOp(e) => {
            OpError::Internal(format!("entity op forwarded from relation_ops: {e}"))
        }
    }
}

#[cfg(test)]
mod validate_relation_tests {
    use super::{validate_relation_create_request, RELATION_PROPERTIES_MAX_BYTES};
    use crate::error::OpError;
    use brain_protocol::{EvidenceRefWire, RelationCreateRequest};

    fn req(properties_blob: Vec<u8>) -> RelationCreateRequest {
        RelationCreateRequest {
            relation_type: "test:works_at".into(),
            from_entity: [0u8; 16],
            to_entity: [1u8; 16],
            properties_blob,
            evidence: EvidenceRefWire::Inline(Vec::new()),
            extractor_id: 0,
            confidence: 0.9,
            valid_from_unix_nanos: 0,
            valid_to_unix_nanos: 0,
            session_id: 0,
            request_id: [0u8; 16],
            act_as: None,
        }
    }

    /// A blob at the cap is accepted: the WAL decoder reads lengths up to and
    /// including `RESPONSE_BLOB_MAX`, so the boundary byte is legal.
    #[test]
    fn properties_blob_at_cap_is_accepted() {
        let r = req(vec![0u8; RELATION_PROPERTIES_MAX_BYTES]);
        assert!(validate_relation_create_request(&r).is_ok());
    }

    /// One byte past the cap is refused as a client-actionable error, so the
    /// caller can shrink and retry rather than storing a relation whose WAL
    /// record cannot be decoded on replay.
    #[test]
    fn properties_blob_over_cap_is_invalid_request() {
        let r = req(vec![0u8; RELATION_PROPERTIES_MAX_BYTES + 1]);
        let err = validate_relation_create_request(&r).expect_err("over-cap blob must be refused");
        let OpError::InvalidField { field, .. } = err else {
            panic!("an over-cap blob must name the offending field");
        };
        assert_eq!(field, "properties_blob");
    }
}

#[cfg(all(test, not(miri)))]
mod project_view_tests {
    use super::project_view;
    use crate::metrics::read_degrade::{global as read_degrade_metrics, ReadDegradeReason};
    use brain_core::{Cardinality, EntityId, ExtractorId, Relation, RelationId, RelationTypeId};
    use brain_metadata::relation::qnames::RelationTypeQnames;
    use brain_metadata::relation::types::relation_type_intern;
    use brain_metadata::{DeclScope, MetadataDb};

    /// The degraded-row counters are process-wide, so a test reading them as a
    /// delta has to be the only one projecting while it does.
    static DEGRADE_COUNTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const NOW: u64 = 1_700_000_000_000_000_000;

    /// `MetadataDb::open` seeds the system schema, so every catalog table
    /// the projection reads already exists.
    fn fresh_db(dir: &tempfile::TempDir) -> MetadataDb {
        MetadataDb::open(dir.path().join("meta.redb")).expect("open metadata")
    }

    fn intern(db: &MetadataDb, name: &str) -> RelationTypeId {
        let wtxn = db.write_txn().expect("write txn");
        let id = relation_type_intern(
            &wtxn,
            DeclScope::base("acme"),
            name,
            None,
            None,
            Cardinality::ManyToMany,
            false,
            1,
            "",
            NOW,
        )
        .expect("intern relation type");
        wtxn.commit().expect("commit");
        id
    }

    fn relation(relation_type: RelationTypeId) -> Relation {
        Relation::new_root(
            RelationId::new(),
            relation_type,
            EntityId::new(),
            EntityId::new(),
            1.0,
            Vec::new(),
            ExtractorId::from(0u32),
            NOW,
            false,
        )
    }

    /// The healthy case, which the degraded one must leave alone.
    #[test]
    fn a_live_relation_type_reaches_the_view_as_its_qname() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let rt = intern(&db, "works_at");
        let rtxn = db.read_txn().expect("read txn");
        let view = project_view(&rtxn, &mut RelationTypeQnames::new(), &relation(rt));
        assert_eq!(view.relation_type, "acme:works_at");
    }

    /// A degraded row has to leave a trace an operator can alert on, because
    /// the projected row itself looks ordinary.
    #[test]
    fn a_relation_whose_type_row_is_gone_counts_a_degraded_row() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let rtxn = db.read_txn().expect("read txn");
        let before = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::RelationTypeQnameMissing);
        let view = project_view(
            &rtxn,
            &mut RelationTypeQnames::new(),
            &relation(RelationTypeId::from(4242)),
        );
        assert_eq!(view.relation_type, "");
        let after = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::RelationTypeQnameMissing);
        assert!(
            after > before,
            "an unnameable relation type must raise the degraded-row count"
        );
    }

    /// A healthy row must not raise the alert, or the signal is worthless.
    #[test]
    fn a_live_relation_type_counts_no_degraded_row() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let rt = intern(&db, "works_at");
        let rtxn = db.read_txn().expect("read txn");
        let before = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::RelationTypeQnameMissing);
        let view = project_view(&rtxn, &mut RelationTypeQnames::new(), &relation(rt));
        assert_eq!(view.relation_type, "acme:works_at");
        let after = read_degrade_metrics()
            .snapshot()
            .rows(ReadDegradeReason::RelationTypeQnameMissing);
        assert_eq!(
            after, before,
            "a nameable relation type must not raise the degraded-row count"
        );
    }

    /// An edge pointing at a relation type the registry has no row for is a
    /// broken row, not a broken request. It projects with the empty qname —
    /// a value no healthy edge can carry, since `validate_qname` refuses an
    /// empty `relation_type` on every write.
    #[test]
    fn a_relation_whose_type_row_is_gone_still_projects() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let rtxn = db.read_txn().expect("read txn");
        let orphan = relation(RelationTypeId::from(4242));
        let view = project_view(&rtxn, &mut RelationTypeQnames::new(), &orphan);
        assert_eq!(view.relation_type, "");
        assert_eq!(view.relation_id, orphan.id.to_bytes());
    }

    /// What RELATION_LIST_FROM / _TO do: one resolver over a page. An edge
    /// whose relation-type row is missing projects with an empty qname, so
    /// a single orphan does not deny the caller the healthy edges beside it.
    #[test]
    fn one_orphan_does_not_deny_the_rest_of_the_page() {
        let _counts = DEGRADE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let rt = intern(&db, "works_at");
        let page = [
            relation(rt),
            relation(RelationTypeId::from(4242)),
            relation(rt),
        ];
        let rtxn = db.read_txn().expect("read txn");
        let mut qnames = RelationTypeQnames::new();
        let names: Vec<String> = page
            .iter()
            .map(|r| project_view(&rtxn, &mut qnames, r).relation_type)
            .collect();
        assert_eq!(names, vec!["acme:works_at", "", "acme:works_at"]);
    }
}

#[cfg(all(test, not(miri)))]
mod supersession_walk_tests {
    use super::{resolve_supersession_head, RELATION_SUPERSESSION_MAX_HOPS};
    use crate::error::OpError;
    use brain_core::{EntityId, ExtractorId, Relation, RelationId, RelationTypeId};
    use brain_metadata::relation::ops::relation_get;
    use brain_metadata::tables::relation::{metadata_from_relation, RELATION_METADATA_TABLE};
    use brain_metadata::tables::scope::RowScope;
    use brain_metadata::MetadataDb;

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn relation() -> Relation {
        Relation::new_root(
            RelationId::new(),
            RelationTypeId::from(1),
            EntityId::new(),
            EntityId::new(),
            1.0,
            Vec::new(),
            ExtractorId::from(0u32),
            NOW,
            false,
        )
    }

    fn insert_rows(db: &MetadataDb, rows: &[(Relation, Option<RelationId>)]) {
        let scope = RowScope::from_bytes(0, [0u8; 16]);
        let wtxn = db.write_txn().expect("write txn");
        {
            let mut table = wtxn
                .open_table(RELATION_METADATA_TABLE)
                .expect("relation metadata table");
            for (r, next) in rows {
                let mut metadata = metadata_from_relation(r, scope);
                metadata.superseded_by_bytes = next.map(|id| id.to_bytes());
                table
                    .insert(&r.id.to_bytes(), &metadata)
                    .expect("insert relation row");
            }
        }
        wtxn.commit().expect("commit relation rows");
    }

    /// A malformed cycle terminates with a typed substrate error instead of
    /// monopolising the shard in a read loop that never yields.
    #[test]
    fn a_cyclic_chain_stops_instead_of_spinning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = MetadataDb::open(dir.path().join("meta.redb")).expect("open metadata");
        let first = relation();
        let second = relation();
        insert_rows(
            &db,
            &[
                (first.clone(), Some(second.id)),
                (second.clone(), Some(first.id)),
            ],
        );

        let rtxn = db.read_txn().expect("read txn");
        let start = relation_get(&rtxn, first.id)
            .expect("relation lookup")
            .expect("starting row");
        let err = resolve_supersession_head(&rtxn, first.id, start)
            .expect_err("a cyclic chain must stop with an error");
        assert!(matches!(
            err,
            OpError::SupersessionChainCycle {
                what: "relation",
                ..
            }
        ));
    }

    /// A long valid correction history reaches its terminal row at the hop
    /// cap, while an independent current row remains a zero-hop resolution.
    #[test]
    fn a_long_but_legal_chain_still_resolves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = MetadataDb::open(dir.path().join("meta.redb")).expect("open metadata");
        let rows: Vec<Relation> = (0..=RELATION_SUPERSESSION_MAX_HOPS)
            .map(|_| relation())
            .collect();
        let links: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.clone(), rows.get(i + 1).map(|next| next.id)))
            .collect();
        insert_rows(&db, &links);
        let standalone = relation();
        insert_rows(&db, &[(standalone.clone(), None)]);

        let rtxn = db.read_txn().expect("read txn");
        let start = relation_get(&rtxn, rows[0].id)
            .expect("relation lookup")
            .expect("starting row");
        let (head, followed) =
            resolve_supersession_head(&rtxn, rows[0].id, start).expect("legal chain resolves");
        assert_eq!(head.id, rows[RELATION_SUPERSESSION_MAX_HOPS].id);
        assert!(followed);

        let current = relation_get(&rtxn, standalone.id)
            .expect("relation lookup")
            .expect("standalone row");
        let (same, followed) =
            resolve_supersession_head(&rtxn, standalone.id, current).expect("current row resolves");
        assert_eq!(same.id, standalone.id);
        assert!(!followed);
    }
}
