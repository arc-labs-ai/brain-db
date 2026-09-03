//! Entity wire-op handlers — `ENTITY_CREATE` / `_GET` / `_UPDATE` /
//! `_RENAME`.
//!
//! Each handler:
//!
//! 1. Validates the request.
//! 2. Acquires the per-shard `MetadataDb` lock.
//! 3. Opens a redb write (or read) transaction.
//! 4. Calls into `brain_metadata::entity::ops::*`.
//! 5. Commits the transaction.
//! 6. Maps `EntityOpError` to `OpError`'s error codes
//!    (see the `OpError` `From<EntityOpError>` impl).
//!
//! These handlers do **not** touch the entity HNSW or emit
//! subscription events. Both wire in later.

use std::sync::Arc;

use brain_core::{Entity, EntityAttributes, EntityId, EntityTypeId, RequestId};
use brain_index::EntityVectorIndex;
use brain_metadata::entity::merge::MergeActor;
use brain_metadata::entity::ops::{
    entity_get, entity_get_resolved_with_chain, entity_list_by_type_page, entity_lookup_by_alias,
    entity_lookup_by_canonical_name, EntityListFilter,
};
use brain_metadata::entity::trigram::{
    candidates_for_query, extract_trigrams, jaccard, trigrams_of_components,
};
use brain_planner::WriterError;
use brain_protocol::envelope::response::EventType;
use brain_protocol::{
    EntityCreateRequest, EntityCreateResponse, EntityCreatedEvent, EntityGetRequest,
    EntityGetResponse, EntityListItem, EntityListRequest, EntityListResponseFrame,
    EntityMergeRequest, EntityMergeResponse, EntityMergedEvent, EntityRenameRequest,
    EntityRenameResponse, EntityRenamedEvent, EntityResolveRequest, EntityResolveResponse,
    EntityTombstoneRequest, EntityTombstoneResponse, EntityTombstonedEvent, EntityUnmergeRequest,
    EntityUnmergeResponse, EntityUnmergedEvent, EntityUpdateRequest, EntityUpdateResponse,
    EntityUpdatedEvent, EntityView, GraphEventPayload, ResolutionOutcomeWire,
};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::link::downcast_writer_pub;
use crate::write::{Phase, PhaseAck, TombstoneTarget, Write, WriteId};

// Default grace window for ENTITY_MERGE — 7 days.
const DEFAULT_MERGE_GRACE_SECS: u64 = 7 * 24 * 60 * 60;

/// Resolver tier-3 (embedding) tunables. Spec defaults
/// (`spec/11_extractors/03_resolver.md`): search the entity HNSW for the top-5
/// nearest and resolve to a unique candidate whose cosine clears 0.78.
const EMBEDDING_TOP_K: usize = 5;
const EMBEDDING_THRESHOLD: f32 = 0.78;

/// Whether `id`'s primary row belongs to the caller's `(namespace,
/// space)` scope. The brain-core [`Entity`] returned by `entity_get`
/// drops the scope (brain-core has no slot for it), so the tenant wall
/// is enforced here by re-reading the row's `namespace_id` /
/// `space_id_bytes` from [`ENTITIES_TABLE`]. Returns `false` (deny) on
/// a missing row or any read error — fail-closed.
fn entity_id_in_caller_scope(ctx: &OpsContext, id: EntityId) -> bool {
    use brain_metadata::tables::entity::{EntityMetadata, ENTITIES_TABLE};
    let Ok(rtxn) = ctx.executor.metadata.read_txn() else {
        return false;
    };
    let Ok(t) = rtxn.open_table(ENTITIES_TABLE) else {
        return false;
    };
    let row: Option<EntityMetadata> = t.get(&id.to_bytes()).ok().flatten().map(|g| g.value());
    match row {
        Some(m) => {
            m.namespace_id == ctx.executor.caller_namespace.raw()
                && m.space_id_bytes == <[u8; 16]>::from(ctx.executor.caller_space)
        }
        None => false,
    }
}

/// Read an entity's `(namespace_id, space_id_bytes, entity_type_id)` in a
/// single lookup against a caller-provided read txn. Used by the resolver's
/// tier-3 scope+type filter so it can screen many HNSW hits under one txn
/// (rather than opening one per hit). Returns `None` on a missing row or read
/// error — the caller treats that as fail-closed and drops the candidate.
fn entity_scope_and_type(
    rtxn: &redb::ReadTransaction,
    id: EntityId,
) -> Option<(u32, [u8; 16], u32)> {
    use brain_metadata::tables::entity::{EntityMetadata, ENTITIES_TABLE};
    let t = rtxn.open_table(ENTITIES_TABLE).ok()?;
    let m: EntityMetadata = t.get(&id.to_bytes()).ok().flatten().map(|g| g.value())?;
    Some((m.namespace_id, m.space_id_bytes, m.entity_type_id))
}

/// Upper bound on the alias count of a single entity. Otherwise bounded
/// only by the 16 MiB payload cap; an explicit cap rejects a crafted
/// oversized alias list with a clear `InvalidRequest` instead of
/// persisting it. The bound is generous — far above any real entity's
/// alias set.
pub const MAX_ENTITY_ALIASES: usize = 256;

// ---------------------------------------------------------------------------
// ENTITY_CREATE
// ---------------------------------------------------------------------------

pub async fn handle_entity_create(
    req: EntityCreateRequest,
    ctx: &OpsContext,
) -> Result<EntityCreateResponse, OpError> {
    if req.canonical_name.trim().is_empty() {
        return Err(OpError::InvalidRequest(
            "canonical_name must be non-empty".into(),
        ));
    }
    if req.aliases.len() > MAX_ENTITY_ALIASES {
        return Err(OpError::InvalidRequest(format!(
            "aliases must have <= {MAX_ENTITY_ALIASES} entries"
        )));
    }

    let entity_type = EntityTypeId(req.entity_type_id);
    let now = crate::txn::now_unix_nanos_pub();
    // Pre-allocate the id here so the request hash + cached replay both
    // see the same value across retries. (Replays still hit the writer's
    // idempotency cache and recover the *original* id via the ack — see
    // `match ack.single_phase()` below.)
    let id = EntityId::new();
    let normalized = normalize_name(&req.canonical_name);
    let attributes = EntityAttributes::from(req.attributes_blob.clone());

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_entity_create_request(&req);

    let phase = Phase::UpsertEntity {
        id,
        ty: entity_type,
        session: brain_core::SessionId::from(req.session_id),
        canonical: req.canonical_name.clone(),
        normalized,
        aliases: req.aliases.clone(),
        attributes,
        created_at_unix_nanos: now,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    let created_id = match ack.single_phase() {
        PhaseAck::UpsertedEntity(eid) => *eid,
        other => {
            return Err(OpError::Internal(format!(
                "unexpected phase ack for ENTITY_CREATE: {other:?}"
            )));
        }
    };

    // Embed + index the new entity so tier-3 resolution can find it without
    // waiting for a rebuild (best-effort; the entity is already durable).
    index_entity_embedding(ctx, created_id, &req.canonical_name);

    // Emit ENTITY_CREATED event post-commit.
    emit_graph_event(
        ctx,
        EventType::EntityCreated,
        GraphEventPayload::EntityCreated(EntityCreatedEvent {
            entity_id: created_id.to_bytes(),
            entity_type_id: req.entity_type_id,
            canonical_name: req.canonical_name,
        }),
        now,
    )
    .await;

    Ok(EntityCreateResponse {
        entity_id: created_id.to_bytes(),
    })
}

// ---------------------------------------------------------------------------
// ENTITY_GET
// ---------------------------------------------------------------------------

pub async fn handle_entity_get(
    req: EntityGetRequest,
    ctx: &OpsContext,
) -> Result<EntityGetResponse, OpError> {
    let id = EntityId::from(req.entity_id);
    let resolved = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        // Follow the `merged_into` redirect: a GET on a merged entity's id
        // returns the surviving entity (multi-hop chains collapsed), plus the
        // audit trail of redirect hops walked to reach it.
        entity_get_resolved_with_chain(&rtxn, id).map_err(OpError::from)?
    };
    let (entity, chain) = resolved.ok_or_else(|| OpError::NotFound {
        what: "entity",
        detail: format!("{id:?}"),
    })?;
    // Tenant wall (unconditional). `entity_get` is id-keyed and does not
    // scope-check, so a caller naming a foreign `(namespace, space)`'s
    // EntityId would otherwise read across the boundary. Reject it as a
    // plain NotFound — the caller must not be able to distinguish "no such
    // entity" from "exists but belongs to another tenant".
    if !entity_id_in_caller_scope(ctx, id) {
        return Err(OpError::NotFound {
            what: "entity",
            detail: format!("{id:?}"),
        });
    }
    Ok(EntityGetResponse {
        entity: entity_to_view(&entity),
        resolved_from: chain.into_iter().map(|e| e.to_bytes()).collect(),
    })
}

// ---------------------------------------------------------------------------
// ENTITY_UPDATE
// ---------------------------------------------------------------------------

pub async fn handle_entity_update(
    req: EntityUpdateRequest,
    ctx: &OpsContext,
) -> Result<EntityUpdateResponse, OpError> {
    if req.canonical_name.trim().is_empty() {
        return Err(OpError::InvalidRequest(
            "canonical_name must be non-empty".into(),
        ));
    }
    if req.aliases.len() > MAX_ENTITY_ALIASES {
        return Err(OpError::InvalidRequest(format!(
            "aliases must have <= {MAX_ENTITY_ALIASES} entries"
        )));
    }
    let id = EntityId::from(req.entity_id);
    let now = crate::txn::now_unix_nanos_pub();

    // Tenant wall (early): a foreign / absent id reads as NotFound before
    // the write is built. The apply-layer wall re-checks atomically.
    if !entity_id_in_caller_scope(ctx, id) {
        return Err(OpError::NotFound {
            what: "entity",
            detail: format!("{id:?}"),
        });
    }

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_entity_update_request(&req, id);

    let phase = Phase::UpdateEntity {
        id,
        canonical_name: req.canonical_name.clone(),
        aliases: req.aliases.clone(),
        attributes_blob: req.attributes_blob.clone(),
        at_unix_nanos: now,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    let after = match ack.single_phase() {
        PhaseAck::EntityUpdated { snapshot, .. } => (**snapshot).clone(),
        other => {
            return Err(OpError::Internal(format!(
                "unexpected phase ack for ENTITY_UPDATE: {other:?}"
            )));
        }
    };

    emit_graph_event(
        ctx,
        EventType::EntityUpdated,
        GraphEventPayload::EntityUpdated(EntityUpdatedEvent {
            entity_id: id.to_bytes(),
            entity_type_id: after.entity_type.raw(),
            canonical_name: after.canonical_name.clone(),
            embedding_version_changed: after.embedding_version > 0,
        }),
        now,
    )
    .await;

    Ok(EntityUpdateResponse {
        entity: entity_to_view(&after),
    })
}

// ---------------------------------------------------------------------------
// ENTITY_RENAME
// ---------------------------------------------------------------------------

pub async fn handle_entity_rename(
    req: EntityRenameRequest,
    ctx: &OpsContext,
) -> Result<EntityRenameResponse, OpError> {
    if req.new_canonical_name.trim().is_empty() {
        return Err(OpError::InvalidRequest(
            "new_canonical_name must be non-empty".into(),
        ));
    }
    // `move_to_alias = false` would mean "discard old name, no alias
    // trail". The metadata rename helper always appends to aliases;
    // reject the no-trail mode until the policy is explicit.
    if !req.move_to_alias {
        return Err(OpError::InvalidRequest(
            "rename with move_to_alias=false is not yet supported".into(),
        ));
    }
    let id = EntityId::from(req.entity_id);
    let now = crate::txn::now_unix_nanos_pub();

    // Tenant wall (early): a foreign / absent id reads as NotFound before
    // the write is built. The apply-layer wall re-checks atomically.
    if !entity_id_in_caller_scope(ctx, id) {
        return Err(OpError::NotFound {
            what: "entity",
            detail: format!("{id:?}"),
        });
    }

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_entity_rename_request(&req, id);

    let phase = Phase::RenameEntity {
        id,
        new_canonical_name: req.new_canonical_name.clone(),
        at_unix_nanos: now,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    let (old_canonical_name, after) = match ack.single_phase() {
        PhaseAck::EntityRenamed {
            old_canonical_name,
            snapshot,
            ..
        } => (old_canonical_name.clone(), (**snapshot).clone()),
        other => {
            return Err(OpError::Internal(format!(
                "unexpected phase ack for ENTITY_RENAME: {other:?}"
            )));
        }
    };

    emit_graph_event(
        ctx,
        EventType::EntityRenamed,
        GraphEventPayload::EntityRenamed(EntityRenamedEvent {
            entity_id: id.to_bytes(),
            old_canonical_name,
            new_canonical_name: req.new_canonical_name,
            old_moved_to_alias: req.move_to_alias,
        }),
        now,
    )
    .await;

    Ok(EntityRenameResponse {
        entity: entity_to_view(&after),
    })
}

// ---------------------------------------------------------------------------
// ENTITY_MERGE
// ---------------------------------------------------------------------------

pub async fn handle_entity_merge(
    req: EntityMergeRequest,
    ctx: &OpsContext,
) -> Result<EntityMergeResponse, OpError> {
    if req.reason.len() > 4096 {
        return Err(OpError::InvalidRequest("reason exceeds 4 KiB".into()));
    }
    let survivor = EntityId::from(req.survivor);
    let merged = EntityId::from(req.merged);
    let now = crate::txn::now_unix_nanos_pub();

    // Tenant wall (early): the caller must own both endpoints. A foreign /
    // absent endpoint reads as NotFound before the write is built. The
    // apply-layer wall re-checks both atomically.
    if !entity_id_in_caller_scope(ctx, survivor) || !entity_id_in_caller_scope(ctx, merged) {
        return Err(OpError::NotFound {
            what: "entity",
            detail: format!("survivor={survivor:?} merged={merged:?}"),
        });
    }

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_entity_merge_request(&req);
    // Wire-initiated merges always carry the caller's space_id (operator
    // merge). The `System` actor is reserved for resolver / background
    // workers.
    let actor = MergeActor::Space(ctx.executor.caller_space.into());

    let phase = Phase::MergeEntities {
        source: merged,
        target: survivor,
        retain_aliases: true,
        retain_attributes: true,
        at_unix_nanos: now,
        confidence: req.confidence,
        reason: req.reason.clone(),
        actor,
        grace_seconds: DEFAULT_MERGE_GRACE_SECS,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    let (audit_id, statements_rerouted, relations_rerouted) = match ack.single_phase() {
        PhaseAck::EntityMerged {
            audit_id,
            statements_rerouted,
            relations_rerouted,
            ..
        } => (*audit_id, *statements_rerouted, *relations_rerouted),
        other => {
            return Err(OpError::Internal(format!(
                "unexpected phase ack for ENTITY_MERGE: {other:?}"
            )));
        }
    };

    // Emit ENTITY_MERGED event.
    emit_graph_event(
        ctx,
        EventType::EntityMerged,
        GraphEventPayload::EntityMerged(EntityMergedEvent {
            survivor: req.survivor,
            merged: req.merged,
            audit_id: audit_id.to_bytes(),
            confidence: req.confidence,
            statements_rerouted,
            relations_rerouted,
        }),
        now,
    )
    .await;

    Ok(EntityMergeResponse {
        audit_id: audit_id.to_bytes(),
        grace_period_seconds: DEFAULT_MERGE_GRACE_SECS,
    })
}

// ---------------------------------------------------------------------------
// ENTITY_UNMERGE
// ---------------------------------------------------------------------------

pub async fn handle_entity_unmerge(
    req: EntityUnmergeRequest,
    ctx: &OpsContext,
) -> Result<EntityUnmergeResponse, OpError> {
    let merged = EntityId::from(req.merged_entity);
    let now = crate::txn::now_unix_nanos_pub();

    // Tenant wall (early): a foreign / absent id reads as NotFound before
    // the write is built. The apply-layer wall re-checks atomically.
    if !entity_id_in_caller_scope(ctx, merged) {
        return Err(OpError::NotFound {
            what: "entity",
            detail: format!("{merged:?}"),
        });
    }

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_entity_unmerge_request(&req, merged);

    // Operator-initiated unmerges attribute to the caller's space —
    // mirrors handle_entity_merge. `System` is reserved for resolver /
    // background workers that auto-unmerge after a heuristic.
    let phase = Phase::UnmergeEntities {
        merged,
        actor: MergeActor::Space(ctx.executor.caller_space.into()),
        at_unix_nanos: now,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    let survivor = match ack.single_phase() {
        PhaseAck::EntitiesUnmerged { survivor, .. } => *survivor,
        other => {
            return Err(OpError::Internal(format!(
                "unexpected phase ack for ENTITY_UNMERGE: {other:?}"
            )));
        }
    };

    // audit_id is not returned by the metadata unmerge helper today;
    // emit [0;16] as the sentinel until that API surfaces it.
    emit_graph_event(
        ctx,
        EventType::EntityUnmerged,
        GraphEventPayload::EntityUnmerged(EntityUnmergedEvent {
            restored_entity_id: merged.to_bytes(),
            from_survivor: survivor.to_bytes(),
            audit_id: [0; 16],
        }),
        now,
    )
    .await;

    Ok(EntityUnmergeResponse {
        restored_entity_id: merged.to_bytes(),
    })
}

// ---------------------------------------------------------------------------
// ENTITY_TOMBSTONE
// ---------------------------------------------------------------------------

pub async fn handle_entity_tombstone(
    req: EntityTombstoneRequest,
    ctx: &OpsContext,
) -> Result<EntityTombstoneResponse, OpError> {
    if req.reason.len() > 4096 {
        return Err(OpError::InvalidRequest("reason exceeds 4 KiB".into()));
    }
    let id = EntityId::from(req.entity_id);
    let now = crate::txn::now_unix_nanos_pub();

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id =
        WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let request_hash = hash_entity_tombstone_request(&req, id);

    // Pre-check existence AND ownership so we return NotFound at the
    // handler edge before the writer accepts a phase whose apply would
    // surface the same error. Scope-aware: an entity owned by another
    // tenant is indistinguishable from a missing one (no existence leak).
    // The apply-layer wall re-checks atomically.
    if !entity_id_in_caller_scope(ctx, id) {
        return Err(OpError::NotFound {
            what: "entity",
            detail: format!("{id:?}"),
        });
    }

    let phase = Phase::Tombstone {
        target: TombstoneTarget::Entity(id),
        // Reason byte mirrors the substrate FORGET / Tombstone-Memory
        // path: 1 = ClientRequest. The full reason string lives only
        // on the subscribe event payload below.
        reason: 1,
        at_unix_nanos: now,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    // Pull the tombstone timestamp from the ack so idempotency replays
    // surface the originally-stored value rather than today's clock.
    let tombstoned_at_unix_nanos = match ack.single_phase() {
        PhaseAck::Tombstoned {
            tombstoned_at_unix_nanos,
            ..
        } => *tombstoned_at_unix_nanos,
        other => {
            return Err(OpError::Internal(format!(
                "unexpected phase ack for ENTITY_TOMBSTONE: {other:?}"
            )));
        }
    };

    emit_graph_event(
        ctx,
        EventType::EntityTombstoned,
        GraphEventPayload::EntityTombstoned(EntityTombstonedEvent {
            entity_id: id.to_bytes(),
            reason: req.reason,
        }),
        now,
    )
    .await;

    Ok(EntityTombstoneResponse {
        tombstoned_at_unix_nanos,
    })
}

/// BLAKE3 over the canonical entity-tombstone request fields. Excludes
/// `request_id` (it's the cache key). The reason string is folded in
/// so two tombstones with different reasons on the same entity_id
/// reuse-attempt counts as a conflict, matching the substrate
/// idempotency model in.
fn hash_entity_tombstone_request(req: &EntityTombstoneRequest, id: EntityId) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"entity_tombstone:");
    h.update(&id.to_bytes());
    h.update(b"\0");
    h.update(req.reason.as_bytes());
    *h.finalize().as_bytes()
}

/// BLAKE3 over the canonical ENTITY_CREATE request fields. Excludes
/// `request_id` (cache key). Aliases hash in order — a reordering
/// counts as a different request, matching the substrate idempotency
/// model in.
fn hash_entity_create_request(req: &EntityCreateRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"entity_create:");
    h.update(&req.entity_type_id.to_le_bytes());
    h.update(b"\0");
    h.update(req.canonical_name.as_bytes());
    h.update(b"\0");
    for alias in &req.aliases {
        h.update(alias.as_bytes());
        h.update(b"\0");
    }
    h.update(b"\0");
    h.update(&req.attributes_blob);
    h.update(b"\0");
    h.update(&req.session_id.to_le_bytes());
    *h.finalize().as_bytes()
}

/// BLAKE3 over the canonical ENTITY_MERGE request fields. Excludes
/// `request_id` (cache key). The reason string is folded in so a
/// retry with a changed reason on the same survivor/merged pair
/// counts as a Conflict rather than a silent replay.
fn hash_entity_merge_request(req: &EntityMergeRequest) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"entity_merge:");
    h.update(&req.survivor);
    h.update(b"\0");
    h.update(&req.merged);
    h.update(b"\0");
    h.update(&req.confidence.to_le_bytes());
    h.update(b"\0");
    h.update(req.reason.as_bytes());
    *h.finalize().as_bytes()
}

/// BLAKE3 over the canonical ENTITY_UPDATE request fields. Excludes
/// `request_id` (cache key). Aliases hash in order — reordering is a
/// distinct request.
fn hash_entity_update_request(req: &EntityUpdateRequest, id: EntityId) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"entity_update:");
    h.update(&id.to_bytes());
    h.update(b"\0");
    h.update(req.canonical_name.as_bytes());
    h.update(b"\0");
    for alias in &req.aliases {
        h.update(alias.as_bytes());
        h.update(b"\0");
    }
    h.update(b"\0");
    h.update(&req.attributes_blob);
    *h.finalize().as_bytes()
}

/// BLAKE3 over the canonical ENTITY_RENAME request fields. Excludes
/// `request_id` (cache key). `move_to_alias` folds in so toggling it
/// across retries is a Conflict.
fn hash_entity_rename_request(req: &EntityRenameRequest, id: EntityId) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"entity_rename:");
    h.update(&id.to_bytes());
    h.update(b"\0");
    h.update(req.new_canonical_name.as_bytes());
    h.update(b"\0");
    h.update(&[u8::from(req.move_to_alias)]);
    *h.finalize().as_bytes()
}

/// BLAKE3 over the canonical ENTITY_UNMERGE request fields. Excludes
/// `request_id` (cache key).
fn hash_entity_unmerge_request(_req: &EntityUnmergeRequest, merged: EntityId) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"entity_unmerge:");
    h.update(&merged.to_bytes());
    *h.finalize().as_bytes()
}

/// Map [`WriterError`] onto [`OpError`] for the entity handlers. Mirrors
/// the substrate link / forget mappers — Conflict surfaces as ExecError,
/// internal failures pass through, and "not found" tagged internals
/// project back to NotFound for the wire.
fn map_writer_err(err: WriterError) -> OpError {
    match err {
        WriterError::Internal(msg) if msg.contains("not found") => OpError::NotFound {
            what: "entity",
            detail: msg,
        },
        other => OpError::ExecError(brain_planner::ExecError::WriterFailed(other)),
    }
}

// ---------------------------------------------------------------------------
// ENTITY_LIST (single-frame snapshot — streaming refinement lands later)
// ---------------------------------------------------------------------------

pub async fn handle_entity_list(
    req: EntityListRequest,
    ctx: &OpsContext,
) -> Result<EntityListResponseFrame, OpError> {
    if req.limit == 0 || req.limit > 1000 {
        return Err(OpError::InvalidRequest("limit must be in 1..=1000".into()));
    }
    if req.entity_type_id == 0 {
        return Err(OpError::InvalidRequest(
            "entity_type_id filter is required in v1.0 ENTITY_LIST".into(),
        ));
    }
    let scope =
        brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
    let resume_after = crate::handlers::cursor::decode_opt(scope, &req.cursor)?;
    let type_id = EntityTypeId(req.entity_type_id);
    let filter = EntityListFilter {
        include_tombstoned: req.include_tombstoned,
        include_merged: req.include_merged,
        mention_count_min: req.mention_count_min,
        name_prefix_norm: if req.name_prefix.is_empty() {
            None
        } else {
            Some(brain_metadata::entity::ops::normalize_name(
                &req.name_prefix,
            ))
        },
    };

    // Keyset page directly off the (scope, type) index: the walk applies
    // every wire filter and resumes strictly past the cursor id, so a page
    // costs one page — not the whole (scope, type) bucket re-scanned per
    // request. Ascending EntityId order matches the cursor's resume order.
    let page = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        entity_list_by_type_page(
            &rtxn,
            scope,
            type_id,
            &filter,
            resume_after,
            req.limit as usize,
        )
        .map_err(OpError::from)?
    };

    let next_cursor = if page.has_more {
        page.last_id
            .map(|id| crate::handlers::cursor::encode(scope, id))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let items: Vec<EntityListItem> = page
        .rows
        .iter()
        .map(|e| EntityListItem {
            entity: entity_to_view(e),
        })
        .collect();
    let cumulative_count = items.len() as u32;
    let frame = EntityListResponseFrame {
        items,
        next_cursor,
        cumulative_count,
        is_final: true,
    };
    Ok(frame)
}

// ---------------------------------------------------------------------------
// ENTITY_RESOLVE — tiers 1 (exact/alias), 2 (trigram fuzzy), 3 (embedding
// HNSW tie-break), and 5 (create fallback under allow_create). Tier 4 (LLM
// disambiguation) is the extraction resolver's, not this wire path's.
// ---------------------------------------------------------------------------
//
// Wire ENTITY_RESOLVE returns the wire outcome surface (Resolved | Ambiguous |
// Created | NotFound). It is read-only except tier 5, which mints a new entity
// via a Phase::UpsertEntity write when allow_create is set and no tier matched.
// The extractor pipeline runs its own resolve-or-create primitive
// (resolve_or_create_with_deps) separately.

pub async fn handle_entity_resolve(
    req: EntityResolveRequest,
    ctx: &OpsContext,
) -> Result<EntityResolveResponse, OpError> {
    if req.candidate_name.trim().is_empty() {
        return Err(OpError::InvalidRequest(
            "candidate_name must be non-empty".into(),
        ));
    }
    if req.candidate_name.len() > 256 {
        return Err(OpError::InvalidRequest(
            "candidate_name exceeds 256 bytes".into(),
        ));
    }
    // No type hint → cross-type exact resolve across every declared
    // entity type. This is the grounded-read default: a caller asking
    // about "Alice" shouldn't have to know her entity type. One exact
    // canonical-name hit → resolved; several distinct hits → ambiguous;
    // none → not found. Typed fuzzy tiers (trigram/alias) need a specific
    // type, so the no-hint path stays exact-only — precise and fast.
    let scope =
        brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
    if req.entity_type_hint == 0 {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let ids =
            brain_metadata::entity_resolve_canonical_all_types(&rtxn, scope, &req.candidate_name)
                .map_err(OpError::from)?;
        drop(rtxn);
        return Ok(match ids.as_slice() {
            [id] => EntityResolveResponse {
                outcome: ResolutionOutcomeWire::Resolved,
                tier: 1,
                confidence: 1.0,
                resolved_entity: id.to_bytes(),
                candidate_ids: Vec::new(),
                audit_id: [0; 16],
            },
            [] => EntityResolveResponse {
                outcome: ResolutionOutcomeWire::NotFound,
                tier: 0,
                confidence: 0.0,
                resolved_entity: [0; 16],
                candidate_ids: Vec::new(),
                audit_id: [0; 16],
            },
            many => EntityResolveResponse {
                outcome: ResolutionOutcomeWire::Ambiguous,
                tier: 1,
                confidence: 1.0,
                resolved_entity: [0; 16],
                candidate_ids: many.iter().map(|id| id.to_bytes()).collect(),
                audit_id: [0; 16],
            },
        });
    }
    let type_id = EntityTypeId(req.entity_type_hint);
    let candidate_norm = brain_metadata::entity::ops::normalize_name(&req.candidate_name);

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    // Tier 1: exact canonical_name match.
    if let Some(eid) = entity_lookup_by_canonical_name(&rtxn, scope, type_id, &req.candidate_name)
        .map_err(OpError::from)?
    {
        return Ok(EntityResolveResponse {
            outcome: ResolutionOutcomeWire::Resolved,
            tier: 1,
            confidence: 1.0,
            resolved_entity: eid.to_bytes(),
            candidate_ids: Vec::new(),
            audit_id: [0; 16],
        });
    }

    // Tier 1b: alias match.
    let alias_hits = entity_lookup_by_alias(&rtxn, scope, type_id, &req.candidate_name)
        .map_err(OpError::from)?;
    if alias_hits.len() == 1 {
        return Ok(EntityResolveResponse {
            outcome: ResolutionOutcomeWire::Resolved,
            tier: 1,
            confidence: 1.0,
            resolved_entity: alias_hits[0].to_bytes(),
            candidate_ids: Vec::new(),
            audit_id: [0; 16],
        });
    }

    // Tier 2: trigram fuzzy match.
    let candidate_trigrams = extract_trigrams(&candidate_norm);
    let trigram_candidates = candidates_for_query(&rtxn, scope, type_id, &candidate_norm)
        .map_err(|e| OpError::Internal(format!("trigram lookup: {e}")))?;

    let mut scored: Vec<(EntityId, f32)> = Vec::new();
    for cand_id in trigram_candidates {
        if let Some(cand_entity) = entity_get(&rtxn, cand_id).map_err(OpError::from)? {
            let cand_trigrams =
                trigrams_of_components(&cand_entity.canonical_name, &cand_entity.aliases);
            let score = jaccard(&candidate_trigrams, &cand_trigrams);
            if score >= 0.85 {
                scored.push((cand_id, score));
            }
        }
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    drop(rtxn);

    if scored.len() == 1 {
        let (id, conf) = scored[0];
        return Ok(EntityResolveResponse {
            outcome: ResolutionOutcomeWire::Resolved,
            tier: 2,
            confidence: conf,
            resolved_entity: id.to_bytes(),
            candidate_ids: Vec::new(),
            audit_id: [0; 16],
        });
    }
    if scored.len() > 1 {
        return Ok(EntityResolveResponse {
            outcome: ResolutionOutcomeWire::Ambiguous,
            tier: 2,
            confidence: scored[0].1,
            resolved_entity: [0; 16],
            candidate_ids: scored.iter().map(|(id, _)| id.to_bytes()).collect(),
            audit_id: [0; 16],
        });
    }

    // No match at tiers 1+2. Tier 3 (embedding HNSW tie-break) resolves when
    // the candidate embedding lands a unique entity above threshold; multiple
    // in-band hits are Ambiguous. Skipped when no entity vector index is wired
    // (tests) — then we fall straight through to the create fallback.
    if let Some(index) = ctx.entity_vector_index.as_ref() {
        if let Some(resp) = resolve_via_embedding(ctx, &req, index).await? {
            return Ok(resp);
        }
    }

    // Tier 5 — create fallback. Per spec the resolver auto-creates a new entity
    // on a clean no-match and returns `Created`. The wire's `allow_create=false`
    // lets a caller opt out of creation, in which case we report `NotFound`.
    if req.allow_create {
        let created = create_fallback_entity(ctx, &req).await?;
        return Ok(EntityResolveResponse {
            outcome: ResolutionOutcomeWire::Created,
            tier: 5,
            confidence: 1.0,
            resolved_entity: created.to_bytes(),
            candidate_ids: Vec::new(),
            audit_id: [0; 16],
        });
    }
    Ok(EntityResolveResponse {
        outcome: ResolutionOutcomeWire::NotFound,
        tier: 0,
        confidence: 0.0,
        resolved_entity: [0; 16],
        candidate_ids: Vec::new(),
        audit_id: [0; 16],
    })
}

/// Tier-5 create fallback: mint a new entity for the unresolved candidate and
/// return its id. Mirrors [`handle_entity_create`]'s write-submit path so the
/// resolver's auto-create is durable and idempotent by `request_id`.
async fn create_fallback_entity(
    ctx: &OpsContext,
    req: &EntityResolveRequest,
) -> Result<EntityId, OpError> {
    let now = crate::txn::now_unix_nanos_pub();
    let id = EntityId::new();
    let entity_type = EntityTypeId(req.entity_type_hint);
    let normalized = normalize_name(&req.candidate_name);

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id), ctx.executor.caller_space);
    let phase = Phase::UpsertEntity {
        id,
        ty: entity_type,
        session: brain_core::SessionId::from(0u64),
        canonical: req.candidate_name.clone(),
        normalized,
        aliases: Vec::new(),
        attributes: EntityAttributes::from(Vec::new()),
        created_at_unix_nanos: now,
    };
    let write = Write::single(write_id, ctx.executor.caller_space, phase)
        .with_namespace(ctx.executor.caller_namespace);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    match ack.single_phase() {
        PhaseAck::UpsertedEntity(eid) => {
            let eid = *eid;
            // Index the fresh entity so an immediate tier-3 re-resolve finds it.
            index_entity_embedding(ctx, eid, &req.candidate_name);
            Ok(eid)
        }
        other => Err(OpError::Internal(format!(
            "unexpected phase ack for resolver create-fallback: {other:?}"
        ))),
    }
}

/// Best-effort: embed the entity's canonical name and insert it into the
/// per-shard entity vector index, so tier-3 (embedding) resolution can reach an
/// explicitly-created entity without waiting for a rebuild. Mirrors the
/// extraction path, which stages entity vectors into the same index; the stored
/// vector is the name embedding, matching the rebuild path. A missing index
/// (tests) or an embed error is a no-op — the entity is already durable, just
/// tier-3-unreachable until a rebuild.
fn index_entity_embedding(ctx: &OpsContext, entity_id: EntityId, canonical_name: &str) {
    let Some(index) = ctx.entity_vector_index.as_ref() else {
        return;
    };
    match ctx.executor.embedder.embed(canonical_name) {
        Ok(vector) => index.insert(entity_id, &vector),
        Err(e) => tracing::warn!(
            target: "brain_ops::write_trace",
            ?entity_id,
            error = %e,
            "entity embed for tier-3 index failed; entity durable but tier-3-unreachable until rebuild",
        ),
    }
}

/// Tier-3 embedding tie-break: embed `candidate + context_snippet`, search the
/// per-shard entity vector index, and resolve to the unique candidate above
/// `EMBEDDING_THRESHOLD`. Multiple in-band hits are `Ambiguous`; zero returns
/// `None` so the caller falls through to the create fallback.
async fn resolve_via_embedding(
    ctx: &OpsContext,
    req: &EntityResolveRequest,
    index: &Arc<dyn EntityVectorIndex>,
) -> Result<Option<EntityResolveResponse>, OpError> {
    // Spec: embed "candidate + first ~100 chars of context".
    let ctx_snippet: String = req.resolution_context.chars().take(100).collect();
    let to_embed = if ctx_snippet.is_empty() {
        req.candidate_name.clone()
    } else {
        format!("{} {}", req.candidate_name, ctx_snippet)
    };
    let vector = ctx
        .executor
        .embedder
        .embed(&to_embed)
        .map_err(|e| OpError::Internal(format!("resolver embed: {e}")))?;

    // The per-shard entity HNSW mixes every tenant's entities and types, so a
    // raw hit may belong to another `(namespace, space)` or a different entity
    // type. Over-fetch, then filter every hit through the caller's scope wall
    // AND the entity-type hint under a SINGLE read txn — the tenant wall is
    // unconditional (resolution is per-(namespace, agent)), and the type hint
    // stops a Person lookup aliasing onto a same-scope Organization neighbour.
    // Mirrors the extraction resolver's embedding-tier filter.
    let hits = index.search(&vector, EMBEDDING_TOP_K * 4);
    let caller_ns = ctx.executor.caller_namespace.raw();
    let caller_space = <[u8; 16]>::from(ctx.executor.caller_space);
    let type_hint = req.entity_type_hint; // 0 == no hint (accept any type)
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("resolver read_txn: {e}")))?;
    let mut in_band: Vec<(EntityId, f32)> = Vec::with_capacity(EMBEDDING_TOP_K);
    for (id, score) in hits {
        if score < EMBEDDING_THRESHOLD {
            continue;
        }
        let Some((ns, space, ty)) = entity_scope_and_type(&rtxn, id) else {
            continue; // missing row → fail-closed (drop)
        };
        let scope_ok = ns == caller_ns && space == caller_space;
        let type_ok = type_hint == 0 || ty == type_hint;
        if scope_ok && type_ok {
            in_band.push((id, score));
            if in_band.len() == EMBEDDING_TOP_K {
                break;
            }
        }
    }
    drop(rtxn);

    match in_band.as_slice() {
        [] => Ok(None),
        [(id, score)] => Ok(Some(EntityResolveResponse {
            outcome: ResolutionOutcomeWire::Resolved,
            tier: 3,
            confidence: *score,
            resolved_entity: id.to_bytes(),
            candidate_ids: Vec::new(),
            audit_id: [0; 16],
        })),
        many => Ok(Some(EntityResolveResponse {
            outcome: ResolutionOutcomeWire::Ambiguous,
            tier: 3,
            confidence: many[0].1,
            resolved_entity: [0; 16],
            candidate_ids: many.iter().map(|(id, _)| id.to_bytes()).collect(),
            audit_id: [0; 16],
        })),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Normalize per `brain-metadata::entity::ops::normalize_name`. Inlined
/// here to avoid threading the metadata crate into the wire types.
fn normalize_name(s: &str) -> String {
    brain_metadata::entity::ops::normalize_name(s)
}

fn entity_to_view(e: &Entity) -> EntityView {
    let merged_into = e.merged_into.map(|id| id.to_bytes()).unwrap_or([0u8; 16]);
    EntityView {
        entity_id: e.id.to_bytes(),
        entity_type_id: e.entity_type.raw(),
        canonical_name: e.canonical_name.clone(),
        normalized_name: e.normalized_name.clone(),
        aliases: e.aliases.clone(),
        attributes_blob: e.attributes.as_bytes().to_vec(),
        mention_count: e.mention_count,
        created_at_unix_nanos: e.created_at_unix_nanos,
        updated_at_unix_nanos: e.updated_at_unix_nanos,
        merged_into,
        embedding_version: e.embedding_version,
        flags: e.flags,
    }
}

// Entity error classification lives in `OpError`'s `From<EntityOpError>`
// impl (crate::error) — handlers use `OpError::from`.

// `emit_graph_event` + `wal_kind_for_event` moved to
// `crate::handlers::events`. Re-exported so other typed-graph handler
// modules that still import via `crate::handlers::entity::emit_graph_event`
// keep compiling; once those imports flip to the new path the
// re-export can drop too.
pub(crate) use crate::handlers::events::emit_graph_event;
