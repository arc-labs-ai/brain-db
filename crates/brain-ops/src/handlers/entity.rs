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
//!    (see the module-private `map_entity_op_error` helper).
//!
//! These handlers do **not** touch the entity HNSW or emit
//! subscription events. Both wire in later.

use brain_core::{Entity, EntityAttributes, EntityId, EntityTypeId, RequestId};
use brain_metadata::entity::merge::MergeActor;
use brain_metadata::entity::ops::{
    entity_get, entity_list_by_type, entity_lookup_by_alias, entity_lookup_by_canonical_name,
    EntityOpError,
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
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_entity_create_request(&req);

    let phase = Phase::UpsertEntity {
        id,
        ty: entity_type,
        canonical: req.canonical_name.clone(),
        normalized,
        aliases: req.aliases.clone(),
        attributes,
        created_at_unix_nanos: now,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    let created_id = match ack.single_phase() {
        PhaseAck::UpsertedEntity(eid) => *eid,
        other => {
            return Err(OpError::Internal(format!(
                "unexpected phase ack for ENTITY_CREATE: {other:?}"
            )));
        }
    };

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
    let entity = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        entity_get(&rtxn, id).map_err(map_entity_op_error)?
    };
    let entity = entity.ok_or_else(|| OpError::NotFound {
        what: "entity",
        detail: format!("{id:?}"),
    })?;
    Ok(EntityGetResponse {
        entity: entity_to_view(&entity),
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
    let id = EntityId::from(req.entity_id);
    let now = crate::txn::now_unix_nanos_pub();

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_entity_update_request(&req, id);

    let phase = Phase::UpdateEntity {
        id,
        canonical_name: req.canonical_name.clone(),
        aliases: req.aliases.clone(),
        attributes_blob: req.attributes_blob.clone(),
        at_unix_nanos: now,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
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

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_entity_rename_request(&req, id);

    let phase = Phase::RenameEntity {
        id,
        new_canonical_name: req.new_canonical_name.clone(),
        at_unix_nanos: now,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
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

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_entity_merge_request(&req);
    // Wire-initiated merges always carry the caller's agent_id (operator
    // merge). The `System` actor is reserved for resolver / background
    // workers.
    let actor = MergeActor::Agent(ctx.executor.caller_agent.into());

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
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
    let ack = real_writer.submit(write).await.map_err(map_writer_err)?;
    let audit_id = match ack.single_phase() {
        PhaseAck::EntityMerged { audit_id, .. } => *audit_id,
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
            statements_rerouted: 0,
            relations_rerouted: 0,
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

    let real_writer = downcast_writer_pub(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_entity_unmerge_request(&req, merged);

    // Operator-initiated unmerges attribute to the caller's agent —
    // mirrors handle_entity_merge. `System` is reserved for resolver /
    // background workers that auto-unmerge after a heuristic.
    let phase = Phase::UnmergeEntities {
        merged,
        actor: MergeActor::Agent(ctx.executor.caller_agent.into()),
        at_unix_nanos: now,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
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
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_entity_tombstone_request(&req, id);

    // Pre-check existence so we return NotFound at the handler edge
    // before the writer accepts a phase whose apply would surface the
    // same error from inside the wtxn.
    {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        if entity_get(&rtxn, id)
            .map_err(map_entity_op_error)?
            .is_none()
        {
            return Err(OpError::NotFound {
                what: "entity",
                detail: format!("{id:?}"),
            });
        }
    }

    let phase = Phase::Tombstone {
        target: TombstoneTarget::Entity(id),
        // Reason byte mirrors the substrate FORGET / Tombstone-Memory
        // path: 1 = ClientRequest. The full reason string lives only
        // on the subscribe event payload below.
        reason: 1,
        at_unix_nanos: now,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
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
    if !req.cursor.is_empty() {
        return Err(OpError::InvalidRequest(
            "ENTITY_LIST cursor pagination lands in phase 16.7.6".into(),
        ));
    }
    let type_id = EntityTypeId(req.entity_type_id);
    let name_prefix_norm = if req.name_prefix.is_empty() {
        None
    } else {
        Some(brain_metadata::entity::ops::normalize_name(
            &req.name_prefix,
        ))
    };
    let entities = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        entity_list_by_type(&rtxn, type_id).map_err(map_entity_op_error)?
    };

    let mut items: Vec<EntityListItem> = entities
        .into_iter()
        .filter(|e| {
            if !req.include_tombstoned && e.flags & 1 != 0 {
                return false;
            }
            if !req.include_merged && e.is_merged() {
                return false;
            }
            if e.mention_count < req.mention_count_min {
                return false;
            }
            if let Some(prefix) = &name_prefix_norm {
                if !e.normalized_name.starts_with(prefix.as_str()) {
                    return false;
                }
            }
            true
        })
        .take(req.limit as usize)
        .map(|e| EntityListItem {
            entity: entity_to_view(&e),
        })
        .collect();

    let cumulative_count = items.len() as u32;
    // Single-frame snapshot; streamed batches land later.
    let frame = EntityListResponseFrame {
        items: std::mem::take(&mut items),
        next_cursor: Vec::new(),
        cumulative_count,
        is_final: true,
    };
    Ok(frame)
}

// ---------------------------------------------------------------------------
// ENTITY_RESOLVE (tiers 1+2 only — tier 3 / 4 deferred until
// the entity HNSW + LLM backends are wired into the shard runtime)
// ---------------------------------------------------------------------------
//
// Wire ENTITY_RESOLVE is a read operation: it returns the wire's richer
// outcome surface (Resolved | Ambiguous | NotFound) without mutating state.
// The writer-side `Phase::Resolve` is intentionally separate — it serves
// the extractor pipeline's resolve-or-create primitive (always succeeds,
// auto-aliases the surface form into the matched entity).

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
    // Require a type hint. Without one we'd need to scan
    // every type's index — usable but slow; deferred alongside
    // tier-3 embedding lookup.
    if req.entity_type_hint == 0 {
        return Err(OpError::InvalidRequest(
            "entity_type_hint is required in phase 16.7.7 ENTITY_RESOLVE".into(),
        ));
    }
    let type_id = EntityTypeId(req.entity_type_hint);
    let candidate_norm = brain_metadata::entity::ops::normalize_name(&req.candidate_name);

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    // Tier 1: exact canonical_name match.
    if let Some(eid) = entity_lookup_by_canonical_name(&rtxn, type_id, &req.candidate_name)
        .map_err(map_entity_op_error)?
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
    let alias_hits =
        entity_lookup_by_alias(&rtxn, type_id, &req.candidate_name).map_err(map_entity_op_error)?;
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
    let trigram_candidates = candidates_for_query(&rtxn, type_id, &candidate_norm)
        .map_err(|e| OpError::Internal(format!("trigram lookup: {e}")))?;

    let mut scored: Vec<(EntityId, f32)> = Vec::new();
    for cand_id in trigram_candidates {
        if let Some(cand_entity) = entity_get(&rtxn, cand_id).map_err(map_entity_op_error)? {
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

    // No match at tiers 1+2. Tier 3 (embedding) requires the entity HNSW
    // wired through ExecutorContext (deferred). Tier 5 (create) only
    // fires if allow_create=true.
    if req.allow_create {
        // Stub — defer create-fallback to the caller.
        // Returning NotFound here so clients explicitly call
        // ENTITY_CREATE if they want creation; auto-create lands when
        // the resolver's tier 5 wires statement extraction.
        return Ok(EntityResolveResponse {
            outcome: ResolutionOutcomeWire::NotFound,
            tier: 0,
            confidence: 0.0,
            resolved_entity: [0; 16],
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

/// Map `EntityOpError` to `OpError`. Until entity-specific error codes
/// get a first-class slot in `ErrorCode`, we surface them as
/// the closest categories — NotFound for missing rows,
/// Conflict for duplicates, InvalidArgument for type-registry misses,
/// Internal for redb failures.
fn map_entity_op_error(err: EntityOpError) -> OpError {
    match err {
        EntityOpError::NotFound(id) => OpError::NotFound {
            what: "entity",
            detail: format!("{id:?}"),
        },
        EntityOpError::UnknownEntityType(t) => {
            OpError::InvalidRequest(format!("unknown entity_type {t:?}"))
        }
        EntityOpError::DuplicateCanonicalName {
            type_id,
            name,
            existing,
        } => OpError::Conflict(format!(
            "canonical_name {name:?} already exists for type {type_id:?}: {existing:?}"
        )),
        EntityOpError::Storage(e) => OpError::Internal(format!("redb storage: {e}")),
        EntityOpError::Table(e) => OpError::Internal(format!("redb table: {e}")),
        EntityOpError::TrigramOp(e) => OpError::Internal(format!("trigram op: {e}")),
    }
}

// `emit_graph_event` + `wal_kind_for_event` moved to
// `crate::handlers::events`. Re-exported so other typed-graph handler
// modules that still import via `crate::handlers::entity::emit_graph_event`
// keep compiling; once those imports flip to the new path the
// re-export can drop too.
pub(crate) use crate::handlers::events::emit_graph_event;
