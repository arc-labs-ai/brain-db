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

use brain_core::Relation;
use brain_core::{Cardinality, EntityId, RelationId, RelationTypeId, RequestId};
use brain_metadata::relation::ops::{
    relation_get, relation_list_from, relation_list_to, RelationListFilter, RelationOpError,
};
use brain_metadata::relation::traversal::{
    traverse, TraversalConfig, TraversalDirection, MAX_DEPTH,
};
use brain_metadata::relation::types::{
    relation_type_get, relation_type_intern_or_get, relation_type_lookup_by_qname,
    RelationTypeOpError,
};
use brain_metadata::schema::store::schema_active;
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
use redb::ReadableTable;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::entity::emit_graph_event;
use crate::handlers::link::downcast_writer_pub;
use crate::write::{
    Phase, PhaseAck, SupersedeReplacement, SupersedeReplacementId, SupersedeTarget,
    TombstoneTarget, Write, WriteId,
};

const REASON_MAX: usize = 4096;
const QNAME_MAX: usize = 96;
const LIST_LIMIT_MAX: u32 = 1000;
const TRAVERSE_MAX_NODES: u32 = 1000;

// ---------------------------------------------------------------------------
// RELATION_CREATE
// ---------------------------------------------------------------------------

pub async fn handle_relation_create(
    req: RelationCreateRequest,
    ctx: &OpsContext,
) -> Result<RelationCreateResponse, OpError> {
    validate_qname(&req.relation_type)?;
    if req.confidence.is_nan() || !(0.0..=1.0).contains(&req.confidence) {
        return Err(OpError::InvalidRequest(
            "confidence must be in [0, 1] and not NaN".into(),
        ));
    }
    let now = crate::txn::now_unix_nanos_pub();
    let (namespace, name) = split_qname(&req.relation_type)?;

    // Resolve the relation-type qname (strict vs schemaless dispatch).
    //
    // Strict mode (active schema in namespace): look up the declared
    // relation_type via rtxn — same shape as the pre-fold path, the
    // wire error (`RelationTypeNotInSchema`) keeps its structured
    // surface for clients.
    //
    // Schemaless mode (no active schema): if the qname is already
    // interned (a prior write registered it), reuse its row; otherwise
    // pass an intern hint on the Phase and let apply do the work
    // inside the main submit wtxn. The pre-refactor code ran a
    // separate intern wtxn here, paying an extra fsync per schemaless
    // RELATION_CREATE — folding it into apply halves the cost.
    //
    // The unified apply path calls `relation_create` which preserves
    // the single-conflict cardinality auto-supersede short-circuit:
    // when exactly one existing current relation conflicts on the
    // cardinality axis, the helper internally supersedes it with the
    // pre-minted id rather than erroring.
    let (resolved_ty, resolved_is_symmetric, intern_hint) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let active_version = schema_active(&rtxn, namespace)
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        if let Some(version) = active_version {
            let rt = relation_type_lookup_by_qname(&rtxn, namespace, name)
                .map_err(map_relation_type_op_error)?
                .ok_or_else(|| OpError::RelationTypeNotInSchema {
                    type_name: req.relation_type.clone(),
                    namespace: namespace.to_string(),
                    version,
                })?;
            if !rt_active_for_schema_rtxn(&rtxn, namespace, version)?.contains(&rt.id) {
                return Err(OpError::RelationTypeNotInSchema {
                    type_name: req.relation_type.clone(),
                    namespace: namespace.to_string(),
                    version,
                });
            }
            (rt.id, rt.is_symmetric, None)
        } else {
            match relation_type_lookup_by_qname(&rtxn, namespace, name)
                .map_err(map_relation_type_op_error)?
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

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_relation_create_request(&req);
    let phase = Phase::UpsertRelation {
        id: new_id,
        ty: resolved_ty,
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
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Pull the id from the ack — on an idempotency replay the writer
    // returns the cached ack whose id is the *original* (potentially
    // different from today's freshly-minted `new_id`).
    let stored_id = match ack.single_phase() {
        PhaseAck::UpsertedRelation(id, _version) => *id,
        other => {
            return Err(OpError::Internal(format!(
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

    let mut current = relation_get(&rtxn, id)
        .map_err(map_relation_op_error)?
        .ok_or_else(|| OpError::NotFound {
            what: "relation",
            detail: format!("{id:?}"),
        })?;

    let mut returned_via_supersession = false;
    if req.follow_supersession {
        while let Some(succ) = current.superseded_by {
            returned_via_supersession = true;
            current = relation_get(&rtxn, succ)
                .map_err(map_relation_op_error)?
                .ok_or_else(|| {
                    OpError::Internal(format!("chain dangling at {succ:?} from {id:?}"))
                })?;
        }
    }

    let view = project_view(&rtxn, &current)?;
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
    validate_qname(&req.new_relation.relation_type)?;
    if req.new_relation.confidence.is_nan() || !(0.0..=1.0).contains(&req.new_relation.confidence) {
        return Err(OpError::InvalidRequest(
            "confidence must be in [0, 1] and not NaN".into(),
        ));
    }

    let old_id = RelationId::from(req.old_relation_id);
    let now = crate::txn::now_unix_nanos_pub();
    let (namespace, name) = split_qname(&req.new_relation.relation_type)?;

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
        let active_version = schema_active_in_wtxn_rel(&wtxn, namespace)?;
        let rt = if let Some(version) = active_version {
            let rt =
                relation_type_lookup_by_qname_wtxn(&wtxn, namespace, name)?.ok_or_else(|| {
                    OpError::RelationTypeNotInSchema {
                        type_name: req.new_relation.relation_type.clone(),
                        namespace: namespace.to_string(),
                        version,
                    }
                })?;
            if !rt_active_for_schema_wtxn(&wtxn, namespace, version)?.contains(&rt.id) {
                return Err(OpError::RelationTypeNotInSchema {
                    type_name: req.new_relation.relation_type.clone(),
                    namespace: namespace.to_string(),
                    version,
                });
            }
            rt
        } else {
            match relation_type_lookup_by_qname_wtxn(&wtxn, namespace, name)? {
                Some(rt) => rt,
                None => {
                    let _ = relation_type_intern_or_get(&wtxn, namespace, name, 0, now)
                        .map_err(map_relation_type_op_error)?;
                    relation_type_lookup_by_qname_wtxn(&wtxn, namespace, name)?
                        .expect("just-interned relation type vanished")
                }
            }
        };
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
        rt
    };

    // Pre-submit existence check so a missing `old_relation_id` keeps
    // its `NotFound { what: "relation", .. }` wire shape — submit-path
    // failures collapse to `WriterError::Internal`.
    peek_relation_exists(ctx, old_id)?;

    let new_relation = build_relation_from_create(&req.new_relation, &rt, now)?;

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_relation_supersede_request(&req);
    let phase = Phase::Supersede {
        target: SupersedeTarget::Relation(old_id),
        replacement: SupersedeReplacement::Relation(Box::new(new_relation)),
        at_unix_nanos: now,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Pull the replacement id from the ack — on idempotency replay the
    // cached ack carries the originally-stored id, which won't match
    // today's freshly-minted `new_relation.id`.
    let new_id = match ack.single_phase() {
        PhaseAck::Superseded(_, SupersedeReplacementId::Relation(id)) => *id,
        other => {
            return Err(OpError::Internal(format!(
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
            .ok_or_else(|| OpError::Internal("new relation missing post-supersede".into()))?;
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
    if req.reason.len() > REASON_MAX {
        return Err(OpError::InvalidRequest("reason exceeds 4 KiB".into()));
    }
    let id = RelationId::from(req.relation_id);
    let now = crate::txn::now_unix_nanos_pub();

    // Pre-submit existence check — submit-path failures collapse into
    // WriterError::Internal, so peek first to keep the missing-id
    // case structured as OpError::NotFound.
    peek_relation_exists(ctx, id)?;

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_relation_tombstone_request(&req);
    let phase = Phase::Tombstone {
        target: TombstoneTarget::Relation(id),
        // brain-metadata's relation tombstone is binary: it flips bits
        // and doesn't read this reason. The wire reason still rides on
        // the post-commit event below.
        reason: 0,
        at_unix_nanos: now,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Pull the tombstone timestamp from the ack so idempotency replays
    // return the originally-stored value rather than today's clock.
    let tombstoned_at_unix_nanos = match ack.single_phase() {
        PhaseAck::Tombstoned {
            tombstoned_at_unix_nanos,
            ..
        } => *tombstoned_at_unix_nanos,
        other => {
            return Err(OpError::Internal(format!(
                "unexpected phase ack for RELATION_TOMBSTONE: {other:?}"
            )));
        }
    };

    emit_graph_event(
        ctx,
        EventType::RelationTombstoned,
        GraphEventPayload::RelationTombstoned(RelationTombstonedEvent {
            relation_id: id.to_bytes(),
            reason: req.reason,
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
    let (items, count) = run_list(
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
        next_cursor: Vec::new(),
        cumulative_count: count,
        is_final: true,
    })
}

pub async fn handle_relation_list_to(
    req: RelationListToRequest,
    ctx: &OpsContext,
) -> Result<RelationListToResponseFrame, OpError> {
    let (items, count) = run_list(
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
        next_cursor: Vec::new(),
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
) -> Result<(Vec<RelationView>, u32), OpError> {
    if limit == 0 || limit > LIST_LIMIT_MAX {
        return Err(OpError::InvalidRequest("limit must be in 1..=1000".into()));
    }
    if !cursor.is_empty() {
        return Err(OpError::InvalidRequest(
            "RELATION_LIST cursor pagination lands in phase 23".into(),
        ));
    }

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
        let active_version = schema_active(&rtxn, ns)
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        match relation_type_lookup_by_qname(&rtxn, ns, name).map_err(map_relation_type_op_error)? {
            Some(rt) => Some(rt.id),
            None => {
                if let Some(version) = active_version {
                    return Err(OpError::RelationTypeNotInSchema {
                        type_name: type_filter.to_string(),
                        namespace: ns.to_string(),
                        version,
                    });
                }
                return Ok((Vec::new(), 0));
            }
        }
    };

    let filter = RelationListFilter {
        relation_type,
        current_only: !include_superseded && !include_tombstoned,
        limit: limit as usize,
    };
    let mut rows = if from_side {
        relation_list_from(&rtxn, entity, &filter).map_err(map_relation_op_error)?
    } else {
        relation_list_to(&rtxn, entity, &filter).map_err(map_relation_op_error)?
    };

    // Wire-level filters not pushed into list_*.
    if !include_tombstoned {
        rows.retain(|r| !r.tombstoned);
    }

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(project_view(&rtxn, r)?);
    }
    let count = out.len() as u32;
    Ok((out, count))
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
        let active_version = schema_active(&rtxn, ns)
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        match relation_type_lookup_by_qname(&rtxn, ns, name).map_err(map_relation_type_op_error)? {
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
        EntityId::from(req.start_entity),
        &type_ids,
        direction,
        &config,
    )
    .map_err(map_relation_op_error)?;

    // Resolve type ids to canonical strings for the wire shape.
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
            let qname = type_qname(&rtxn, s.relation_type)?;
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

fn validate_qname(q: &str) -> Result<(), OpError> {
    if q.is_empty() {
        return Err(OpError::InvalidRequest(
            "relation_type must be non-empty".into(),
        ));
    }
    if q.len() > QNAME_MAX {
        return Err(OpError::InvalidRequest(format!(
            "relation_type qname exceeds {QNAME_MAX} chars"
        )));
    }
    if !q.contains(':') {
        return Err(OpError::InvalidRequest(
            "relation_type must use \"namespace:name\" form".into(),
        ));
    }
    Ok(())
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
fn project_view(rtxn: &redb::ReadTransaction, r: &Relation) -> Result<RelationView, OpError> {
    let qname = type_qname(rtxn, r.relation_type)?;
    Ok(RelationView::from_relation(r, qname))
}

fn type_qname(rtxn: &redb::ReadTransaction, id: RelationTypeId) -> Result<String, OpError> {
    let rt = relation_type_get(rtxn, id)
        .map_err(map_relation_type_op_error)?
        .ok_or_else(|| {
            OpError::Internal(format!("relation references missing relation_type {id:?}"))
        })?;
    Ok(rt.canonical())
}

/// `relation_type_lookup_by_qname` takes `&ReadTransaction`, but our
/// validation runs inside a `WriteTransaction`. Inline a wtxn-friendly
/// variant — mirrors `predicate_lookup_by_qname_wtxn`.
fn relation_type_lookup_by_qname_wtxn(
    wtxn: &redb::WriteTransaction,
    namespace: &str,
    name: &str,
) -> Result<Option<brain_core::RelationType>, OpError> {
    use brain_metadata::tables::relation_type::{
        RelationTypeDefinition, RELATION_TYPES_BY_QNAME_TABLE, RELATION_TYPES_TABLE,
    };
    let q = format!("{namespace}:{name}");
    let id_raw: Option<u32> = {
        let idx = wtxn
            .open_table(RELATION_TYPES_BY_QNAME_TABLE)
            .map_err(|e| OpError::Internal(format!("open by_qname: {e}")))?;
        let g = idx
            .get(q.as_str())
            .map_err(|e| OpError::Internal(format!("by_qname lookup: {e}")))?;
        g.map(|guard| guard.value())
    };
    let Some(id_raw) = id_raw else {
        return Ok(None);
    };
    let t = wtxn
        .open_table(RELATION_TYPES_TABLE)
        .map_err(|e| OpError::Internal(format!("open relation_types: {e}")))?;
    let row: Option<RelationTypeDefinition> = t
        .get(&id_raw)
        .map_err(|e| OpError::Internal(format!("relation_types lookup: {e}")))?
        .map(|g| g.value());
    Ok(row.as_ref().map(RelationTypeDefinition::to_relation_type))
}

/// Active schema version for `namespace` inside a write txn. Mirrors
/// the same helper in `statement.rs` — relation handlers
/// run a different write path so we keep the helpers local rather
/// than re-export through a shared module.
fn schema_active_in_wtxn_rel(
    wtxn: &redb::WriteTransaction,
    namespace: &str,
) -> Result<Option<u32>, OpError> {
    use brain_metadata::tables::schema_version::SCHEMA_ACTIVE_VERSIONS_TABLE;
    let active = wtxn
        .open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)
        .map_err(|e| OpError::Internal(format!("open schema_active: {e}")))?;
    let g = active
        .get(&namespace)
        .map_err(|e| OpError::Internal(format!("schema_active lookup: {e}")))?;
    Ok(g.map(|guard| guard.value()))
}

/// Set of `RelationTypeId`s the active schema version declares — used
/// to enforce strict-mode RELATION_CREATE / SUPERSEDE.
fn rt_active_for_schema_wtxn(
    wtxn: &redb::WriteTransaction,
    namespace: &str,
    version: u32,
) -> Result<std::collections::HashSet<RelationTypeId>, OpError> {
    use brain_metadata::tables::relation_type::{
        RelationTypeDefinition, RelationTypeOrigin, RELATION_TYPES_TABLE,
    };
    let t = wtxn
        .open_table(RELATION_TYPES_TABLE)
        .map_err(|e| OpError::Internal(format!("open relation_types: {e}")))?;
    let mut out = std::collections::HashSet::new();
    for entry in t
        .iter()
        .map_err(|e| OpError::Internal(format!("relation_types iter: {e}")))?
    {
        let (k, v) = entry.map_err(|e| OpError::Internal(format!("relation_types entry: {e}")))?;
        let row: RelationTypeDefinition = v.value();
        if row.namespace != namespace {
            continue;
        }
        if let RelationTypeOrigin::SchemaDeclared { version: v_decl } = row.origin() {
            if v_decl == version {
                out.insert(RelationTypeId::from(k.value()));
            }
        }
    }
    Ok(out)
}

/// rtxn variant of `rt_active_for_schema_wtxn`. The fold of the
/// intern micro-wtxn moved relation-type vocabulary resolution off
/// the write-txn path; the strict-mode check still needs the active
/// set, just now via a read txn.
fn rt_active_for_schema_rtxn(
    rtxn: &redb::ReadTransaction,
    namespace: &str,
    version: u32,
) -> Result<std::collections::HashSet<RelationTypeId>, OpError> {
    use brain_metadata::tables::relation_type::{
        RelationTypeDefinition, RelationTypeOrigin, RELATION_TYPES_TABLE,
    };
    let t = rtxn
        .open_table(RELATION_TYPES_TABLE)
        .map_err(|e| OpError::Internal(format!("open relation_types: {e}")))?;
    let mut out = std::collections::HashSet::new();
    for entry in t
        .iter()
        .map_err(|e| OpError::Internal(format!("relation_types iter: {e}")))?
    {
        let (k, v) = entry.map_err(|e| OpError::Internal(format!("relation_types entry: {e}")))?;
        let row: RelationTypeDefinition = v.value();
        if row.namespace != namespace {
            continue;
        }
        if let RelationTypeOrigin::SchemaDeclared { version: v_decl } = row.origin() {
            if v_decl == version {
                out.insert(RelationTypeId::from(k.value()));
            }
        }
    }
    Ok(out)
}

/// Confirm the relation row exists. Returns `OpError::NotFound` with
/// the stable `what: "relation"` discriminant when the id has never
/// been written. Used pre-submit so a missing relation keeps its
/// wire-level NotFound shape instead of collapsing into Internal via
/// WriterError.
fn peek_relation_exists(ctx: &OpsContext, id: RelationId) -> Result<(), OpError> {
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
    if relation_get(&rtxn, id)
        .map_err(map_relation_op_error)?
        .is_none()
    {
        return Err(OpError::NotFound {
            what: "relation",
            detail: format!("{id:?}"),
        });
    }
    Ok(())
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
    h.update(req.reason.as_bytes());
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

fn map_relation_type_op_error(err: RelationTypeOpError) -> OpError {
    match err {
        RelationTypeOpError::InvalidIdentifier { reason } => {
            OpError::InvalidRequest(format!("relation_type identifier: {reason}"))
        }
        RelationTypeOpError::AlreadyExists { qname, existing_id } => OpError::Conflict(format!(
            "relation_type {qname:?} already exists with id {existing_id:?}"
        )),
        RelationTypeOpError::Storage(e) => OpError::Internal(format!("redb storage: {e}")),
        RelationTypeOpError::Table(e) => OpError::Internal(format!("redb table: {e}")),
    }
}

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
        RelationOpError::Storage(e) => OpError::Internal(format!("redb storage: {e}")),
        RelationOpError::Table(e) => OpError::Internal(format!("redb table: {e}")),
        RelationOpError::EdgeOp(e) => OpError::Internal(format!("edge op: {e}")),
        RelationOpError::EdgeKey(e) => {
            OpError::Internal(format!("edge key decode (corruption?): {e}"))
        }
        RelationOpError::RelationTypeOp(e) => map_relation_type_op_error(e),
        RelationOpError::EntityOp(e) => {
            OpError::Internal(format!("entity op forwarded from relation_ops: {e}"))
        }
    }
}
