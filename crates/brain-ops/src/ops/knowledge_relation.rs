//! Relation wire-op handlers — `RELATION_CREATE / _GET / _SUPERSEDE /
//! _TOMBSTONE / _LIST_FROM / _LIST_TO / _TRAVERSE` (spec §28/07,
//! phase 18.7).
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
//!    TOMBSTONE) per spec §28/02 §3.3.
//! 8. Projects brain-core `Relation` → wire `RelationView`.
//!
//! Phase 18.7 handlers do NOT yet handle cross-shard relation reads
//! or the relation embedding worker — both deferred per the §20
//! open questions.

use brain_core::knowledge::Relation;
use brain_core::{EntityId, RelationId, RelationTypeId};
use brain_metadata::relation_ops::{
    relation_create, relation_get, relation_history, relation_list_from, relation_list_to,
    relation_supersede, relation_tombstone, RelationListFilter, RelationOpError,
};
use brain_metadata::relation_traversal::{
    traverse, TraversalConfig, TraversalDirection, MAX_DEPTH,
};
use brain_metadata::relation_type_ops::{
    relation_type_get, relation_type_lookup_by_qname, RelationTypeOpError,
};
use brain_protocol::knowledge::{
    KnowledgeEventPayload, RelationCreateRequest, RelationCreateResponse, RelationCreatedEvent,
    RelationGetRequest, RelationGetResponse, RelationListFromRequest,
    RelationListFromResponseFrame, RelationListToRequest, RelationListToResponseFrame,
    RelationSupersedeRequest, RelationSupersedeResponse, RelationSupersededEvent,
    RelationTombstoneRequest, RelationTombstoneResponse, RelationTombstonedEvent,
    RelationTraverseRequest, RelationTraverseResponseFrame, RelationView, TraversalPathWire,
    TraversalStepWire,
};
use brain_protocol::response::EventType;
use redb::ReadableTable;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::ops::knowledge_entity::emit_knowledge_event;

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

    let (created_id, qname) = {
        let mut db_guard = ctx.executor.metadata.lock();
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;

        let rt = relation_type_lookup_by_qname_wtxn(&wtxn, namespace, name)?.ok_or_else(|| {
            OpError::InvalidRequest(format!(
                "unknown relation type {namespace:?}:{name:?}; declare via SCHEMA_UPLOAD first"
            ))
        })?;

        let relation = build_relation_from_create(&req, &rt, now)?;
        let created = relation_create(&wtxn, &relation, now).map_err(map_relation_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
        (created, rt.canonical())
    };

    emit_knowledge_event(
        ctx,
        EventType::RelationCreated,
        KnowledgeEventPayload::RelationCreated(RelationCreatedEvent {
            relation_id: created_id.to_bytes(),
            relation_type: qname,
            from: req.from_entity,
            to: req.to_entity,
        }),
        now,
    );

    Ok(RelationCreateResponse {
        relation_id: created_id.to_bytes(),
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

    let db_guard = ctx.executor.metadata.lock();
    let rtxn = db_guard
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

    let (new_id, version) = {
        let mut db_guard = ctx.executor.metadata.lock();
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;

        let rt = relation_type_lookup_by_qname_wtxn(&wtxn, namespace, name)?.ok_or_else(|| {
            OpError::InvalidRequest(format!("unknown relation type {namespace:?}:{name:?}"))
        })?;

        let new_relation = build_relation_from_create(&req.new_relation, &rt, now)?;
        let new_id =
            relation_supersede(&wtxn, old_id, &new_relation, now).map_err(map_relation_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;

        let rtxn = db_guard
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let new = relation_get(&rtxn, new_id)
            .map_err(map_relation_op_error)?
            .ok_or_else(|| OpError::Internal("new relation missing post-supersede".into()))?;
        (new_id, new.version)
    };

    emit_knowledge_event(
        ctx,
        EventType::RelationSuperseded,
        KnowledgeEventPayload::RelationSuperseded(RelationSupersededEvent {
            old_relation_id: old_id.to_bytes(),
            new_relation_id: new_id.to_bytes(),
        }),
        now,
    );

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

    {
        let mut db_guard = ctx.executor.metadata.lock();
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        relation_tombstone(&wtxn, id, now).map_err(map_relation_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
    }

    emit_knowledge_event(
        ctx,
        EventType::RelationTombstoned,
        KnowledgeEventPayload::RelationTombstoned(RelationTombstonedEvent {
            relation_id: id.to_bytes(),
            reason: req.reason,
        }),
        now,
    );

    Ok(RelationTombstoneResponse {
        tombstoned_at_unix_nanos: now,
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

    let db_guard = ctx.executor.metadata.lock();
    let rtxn = db_guard
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    let relation_type = if type_filter.is_empty() {
        None
    } else {
        validate_qname(type_filter)?;
        let (ns, name) = split_qname(type_filter)?;
        let rt = relation_type_lookup_by_qname(&rtxn, ns, name)
            .map_err(map_relation_type_op_error)?
            .ok_or_else(|| {
                OpError::InvalidRequest(format!("unknown relation type {ns:?}:{name:?}"))
            })?;
        Some(rt.id)
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

    let db_guard = ctx.executor.metadata.lock();
    let rtxn = db_guard
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    // Resolve type qnames → ids.
    let mut type_ids: Vec<RelationTypeId> = Vec::with_capacity(req.relation_types.len());
    for qname in &req.relation_types {
        validate_qname(qname)?;
        let (ns, name) = split_qname(qname)?;
        let rt = relation_type_lookup_by_qname(&rtxn, ns, name)
            .map_err(map_relation_type_op_error)?
            .ok_or_else(|| OpError::InvalidRequest(format!("unknown relation type {qname}")))?;
        type_ids.push(rt.id);
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

fn build_relation_from_create(
    req: &RelationCreateRequest,
    rt: &brain_core::knowledge::RelationType,
    now: u64,
) -> Result<Relation, OpError> {
    use brain_protocol::knowledge::evidence_ref_from_wire;

    let evidence = evidence_ref_from_wire(&req.evidence)
        .map_err(|e| OpError::InvalidRequest(format!("evidence decode: {e}")))?;
    let memories: Vec<brain_core::MemoryId> = match evidence {
        brain_core::knowledge::EvidenceRef::Inline(entries) => {
            entries.iter().map(|e| e.memory_id).collect()
        }
        brain_core::knowledge::EvidenceRef::Overflow(_) => {
            return Err(OpError::InvalidRequest(
                "RELATION evidence overflow not supported in v1 (spec §20/05 §3)".into(),
            ));
        }
    };

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
/// variant — mirrors `predicate_lookup_by_qname_wtxn` (17.7).
fn relation_type_lookup_by_qname_wtxn(
    wtxn: &redb::WriteTransaction,
    namespace: &str,
    name: &str,
) -> Result<Option<brain_core::knowledge::RelationType>, OpError> {
    use brain_metadata::tables::knowledge::relation_type::{
        RelationTypeDefinition, RELATION_TYPES_BY_QNAME_TABLE, RELATION_TYPES_TABLE,
    };
    let q = format!("{namespace}:{name}");
    let idx = wtxn
        .open_table(RELATION_TYPES_BY_QNAME_TABLE)
        .map_err(|e| OpError::Internal(format!("open by_qname: {e}")))?;
    let id_raw: Option<u32> = idx
        .get(q.as_str())
        .map_err(|e| OpError::Internal(format!("by_qname lookup: {e}")))?
        .map(|g| g.value());
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
        } => OpError::Conflict(format!(
            "cardinality {variant:?} violated; {conflicting} conflicting current relation(s)"
        )),
        RelationOpError::DecodeFailed => {
            OpError::Internal("relation row decode failed — possible corruption".into())
        }
        RelationOpError::Storage(e) => OpError::Internal(format!("redb storage: {e}")),
        RelationOpError::Table(e) => OpError::Internal(format!("redb table: {e}")),
        RelationOpError::RelationTypeOp(e) => map_relation_type_op_error(e),
        RelationOpError::EntityOp(e) => {
            OpError::Internal(format!("entity op forwarded from relation_ops: {e}"))
        }
    }
}

// Reduce dead-code warnings on transitively-needed re-exports.
#[allow(dead_code)]
fn _imports_keepalive() {
    let _ = relation_history;
}
