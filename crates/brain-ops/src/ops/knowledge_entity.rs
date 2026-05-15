//! Entity wire-op handlers — `ENTITY_CREATE` / `_GET` / `_UPDATE` /
//! `_RENAME` (spec §28/00, phase 16.6c).
//!
//! Each handler:
//!
//! 1. Validates the request.
//! 2. Acquires the per-shard `MetadataDb` lock.
//! 3. Opens a redb write (or read) transaction.
//! 4. Calls into `brain_metadata::entity_ops::*`.
//! 5. Commits the transaction.
//! 6. Maps `EntityOpError` to `OpError` per spec §28's error codes
//!    (mapped through the substrate ErrorCode taxonomy until §28 error
//!    codes land as first-class — see [`map_entity_op_error`]).
//!
//! Phase 16.6c handlers do **not** touch the entity HNSW (16.3) or
//! emit subscription events. Both wire in later sub-tasks.

use brain_core::{
    Entity, EntityAttributes, EntityId, EntityTypeId, MemoryId,
};
use brain_metadata::entity_merge_ops::{merge_entity, unmerge_entity, EntityMergeOpError, MergeActor};
use brain_metadata::entity_ops::{
    entity_get, entity_list_by_type, entity_lookup_by_alias, entity_lookup_by_canonical_name,
    entity_put, entity_rename, entity_tombstone, entity_update, EntityOpError,
};
use brain_metadata::trigram_ops::{
    candidates_for_query, extract_trigrams, jaccard, trigrams_of_components,
};
use brain_protocol::knowledge::{
    EntityCreateRequest, EntityCreateResponse, EntityCreatedEvent, EntityGetRequest,
    EntityGetResponse, EntityListItem, EntityListRequest, EntityListResponseFrame,
    EntityMergeRequest, EntityMergeResponse, EntityMergedEvent, EntityRenameRequest,
    EntityRenameResponse, EntityRenamedEvent, EntityResolveRequest, EntityResolveResponse,
    EntityTombstoneRequest, EntityTombstoneResponse, EntityTombstonedEvent, EntityUnmergeRequest,
    EntityUnmergeResponse, EntityUnmergedEvent, EntityUpdateRequest, EntityUpdateResponse,
    EntityUpdatedEvent, EntityView, KnowledgeEventPayload, ResolutionOutcomeWire,
};
use brain_protocol::response::EventType;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::subscribe::EventEnvelope;

// Default grace window for ENTITY_MERGE — 7 days. See spec/18/03 §7.
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
    let id = EntityId::new();
    let mut entity = Entity::new_active(
        id,
        entity_type,
        req.canonical_name.clone(),
        normalize_name(&req.canonical_name),
        now,
    );
    entity.aliases = req.aliases;
    entity.attributes = EntityAttributes::from(req.attributes_blob);

    {
        let mut db_guard = ctx.executor.metadata.lock();
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        entity_put(&wtxn, &entity).map_err(map_entity_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
    }

    // 16.7.8 — emit ENTITY_CREATED event post-commit.
    emit_knowledge_event(
        ctx,
        EventType::EntityCreated,
        KnowledgeEventPayload::EntityCreated(EntityCreatedEvent {
            entity_id: id.to_bytes(),
            entity_type_id: req.entity_type_id,
            canonical_name: req.canonical_name,
        }),
        now,
    );

    Ok(EntityCreateResponse {
        entity_id: id.to_bytes(),
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
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        entity_get(&rtxn, id).map_err(map_entity_op_error)?
    };
    let entity = entity.ok_or_else(|| OpError::NotFound { what: "entity", detail: format!("{id:?}") })?;
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
    let after = {
        let mut db_guard = ctx.executor.metadata.lock();
        // Load current entity to preserve immutable / unchanged fields.
        let current = {
            let rtxn = db_guard
                .read_txn()
                .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
            entity_get(&rtxn, id)
                .map_err(map_entity_op_error)?
                .ok_or_else(|| OpError::NotFound { what: "entity", detail: format!("{id:?}") })?
        };
        let mut next = current.clone();
        next.canonical_name = req.canonical_name.clone();
        next.normalized_name = normalize_name(&req.canonical_name);
        next.aliases = req.aliases;
        next.attributes = EntityAttributes::from(req.attributes_blob);

        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        entity_update(&wtxn, &next, now).map_err(map_entity_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;

        // Re-read post-commit so we return the persisted view (including
        // entity_update's derived fields like embedding_version).
        let rtxn = db_guard
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        entity_get(&rtxn, id)
            .map_err(map_entity_op_error)?
            .ok_or_else(|| OpError::Internal(format!("entity {id:?} missing post-update")))?
    };
    // 16.7.8 — emit ENTITY_UPDATED event.
    emit_knowledge_event(
        ctx,
        EventType::EntityUpdated,
        KnowledgeEventPayload::EntityUpdated(EntityUpdatedEvent {
            entity_id: id.to_bytes(),
            entity_type_id: after.entity_type.raw(),
            canonical_name: after.canonical_name.clone(),
            embedding_version_changed: after.embedding_version > 0,
        }),
        now,
    );

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
    // trail". `brain_metadata::entity_ops::entity_rename` always
    // appends to aliases (spec §18/02 default). Reject the no-trail
    // mode until a later phase implements it explicitly.
    if !req.move_to_alias {
        return Err(OpError::InvalidRequest(
            "rename with move_to_alias=false is not yet supported".into(),
        ));
    }
    let id = EntityId::from(req.entity_id);
    let now = crate::txn::now_unix_nanos_pub();
    let (old_canonical_name, after) = {
        let mut db_guard = ctx.executor.metadata.lock();
        // Capture old name from the read transaction before mutating.
        let old_name = {
            let rtxn = db_guard
                .read_txn()
                .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
            entity_get(&rtxn, id)
                .map_err(map_entity_op_error)?
                .ok_or_else(|| OpError::NotFound {
                    what: "entity",
                    detail: format!("{id:?}"),
                })?
                .canonical_name
        };
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        entity_rename(&wtxn, id, req.new_canonical_name.clone(), now)
            .map_err(map_entity_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;

        let rtxn = db_guard
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let row = entity_get(&rtxn, id)
            .map_err(map_entity_op_error)?
            .ok_or_else(|| OpError::Internal(format!("entity {id:?} missing post-rename")))?;
        (old_name, row)
    };

    // 16.7.8 — emit ENTITY_RENAMED event.
    emit_knowledge_event(
        ctx,
        EventType::EntityRenamed,
        KnowledgeEventPayload::EntityRenamed(EntityRenamedEvent {
            entity_id: id.to_bytes(),
            old_canonical_name,
            new_canonical_name: req.new_canonical_name,
            old_moved_to_alias: req.move_to_alias,
        }),
        now,
    );

    Ok(EntityRenameResponse {
        entity: entity_to_view(&after),
    })
}

// ---------------------------------------------------------------------------
// ENTITY_MERGE (16.7.5)
// ---------------------------------------------------------------------------

pub async fn handle_entity_merge(
    req: EntityMergeRequest,
    ctx: &OpsContext,
) -> Result<EntityMergeResponse, OpError> {
    if req.reason.len() > 4096 {
        return Err(OpError::InvalidRequest(
            "reason exceeds 4 KiB".into(),
        ));
    }
    let survivor = EntityId::from(req.survivor);
    let merged = EntityId::from(req.merged);
    let now = crate::txn::now_unix_nanos_pub();
    let merge_id = {
        let mut db_guard = ctx.executor.metadata.lock();
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        let merge_id = merge_entity(
            &wtxn,
            survivor,
            merged,
            req.confidence,
            req.reason,
            // Phase 16.7: wire-initiated merges are always operator. The
            // bound agent_id lives on the connection state; for now we
            // record System actor since the OpsContext doesn't carry
            // agent_id through to brain-ops yet. Tracked as a follow-up
            // in §18/06 — actor-agent propagation lands when statement
            // handlers need it (phase 17).
            MergeActor::System,
            DEFAULT_MERGE_GRACE_SECS,
            now,
        )
        .map_err(map_entity_merge_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
        merge_id
    };

    // 16.7.8 — emit ENTITY_MERGED event.
    emit_knowledge_event(
        ctx,
        EventType::EntityMerged,
        KnowledgeEventPayload::EntityMerged(EntityMergedEvent {
            survivor: req.survivor,
            merged: req.merged,
            audit_id: merge_id.to_bytes(),
            confidence: req.confidence,
            statements_rerouted: 0,
            relations_rerouted: 0,
        }),
        now,
    );

    Ok(EntityMergeResponse {
        audit_id: merge_id.to_bytes(),
        grace_period_seconds: DEFAULT_MERGE_GRACE_SECS,
    })
}

// ---------------------------------------------------------------------------
// ENTITY_UNMERGE (16.7.5)
// ---------------------------------------------------------------------------

pub async fn handle_entity_unmerge(
    req: EntityUnmergeRequest,
    ctx: &OpsContext,
) -> Result<EntityUnmergeResponse, OpError> {
    let merged = EntityId::from(req.merged_entity);
    let now = crate::txn::now_unix_nanos_pub();
    let survivor = {
        let mut db_guard = ctx.executor.metadata.lock();
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        let survivor =
            unmerge_entity(&wtxn, merged, MergeActor::System, now).map_err(map_entity_merge_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
        survivor
    };

    // 16.7.8 — emit ENTITY_UNMERGED event. audit_id is not returned by
    // unmerge_entity in the current API; phase 17 may surface it. For
    // 16.7 we emit [0;16] as the audit id sentinel.
    emit_knowledge_event(
        ctx,
        EventType::EntityUnmerged,
        KnowledgeEventPayload::EntityUnmerged(EntityUnmergedEvent {
            restored_entity_id: merged.to_bytes(),
            from_survivor: survivor.to_bytes(),
            audit_id: [0; 16],
        }),
        now,
    );

    Ok(EntityUnmergeResponse {
        restored_entity_id: merged.to_bytes(),
    })
}

// ---------------------------------------------------------------------------
// ENTITY_TOMBSTONE (16.7.5)
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
    {
        let mut db_guard = ctx.executor.metadata.lock();
        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        entity_tombstone(&wtxn, id, now).map_err(map_entity_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
    }

    emit_knowledge_event(
        ctx,
        EventType::EntityTombstoned,
        KnowledgeEventPayload::EntityTombstoned(EntityTombstonedEvent {
            entity_id: id.to_bytes(),
            reason: req.reason,
        }),
        now,
    );

    Ok(EntityTombstoneResponse {
        tombstoned_at_unix_nanos: now,
    })
}

// ---------------------------------------------------------------------------
// ENTITY_LIST (16.7.5; single-frame snapshot — streaming refinement
// lands in 16.7.6)
// ---------------------------------------------------------------------------

pub async fn handle_entity_list(
    req: EntityListRequest,
    ctx: &OpsContext,
) -> Result<EntityListResponseFrame, OpError> {
    if req.limit == 0 || req.limit > 1000 {
        return Err(OpError::InvalidRequest(
            "limit must be in 1..=1000".into(),
        ));
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
        Some(brain_metadata::entity_ops::normalize_name(&req.name_prefix))
    };
    let entities = {
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard
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
    // 16.7.5: single-frame snapshot; 16.7.6 splits into streamed batches.
    let frame = EntityListResponseFrame {
        items: std::mem::take(&mut items),
        next_cursor: Vec::new(),
        cumulative_count,
        is_final: true,
    };
    Ok(frame)
}

// ---------------------------------------------------------------------------
// ENTITY_RESOLVE (16.7.7; tiers 1+2 only — tier 3 / 4 deferred to phase 21
// when the entity HNSW + LLM backends are wired into the shard runtime)
// ---------------------------------------------------------------------------

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
    // Phase 16.7.7: require a type hint. Without one we'd need to scan
    // every type's index — usable but slow; defer to phase 21 alongside
    // tier-3 embedding lookup.
    if req.entity_type_hint == 0 {
        return Err(OpError::InvalidRequest(
            "entity_type_hint is required in phase 16.7.7 ENTITY_RESOLVE".into(),
        ));
    }
    let type_id = EntityTypeId(req.entity_type_hint);
    let candidate_norm =
        brain_metadata::entity_ops::normalize_name(&req.candidate_name);

    let db_guard = ctx.executor.metadata.lock();
    let rtxn = db_guard
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
    let alias_hits = entity_lookup_by_alias(&rtxn, type_id, &req.candidate_name)
        .map_err(map_entity_op_error)?;
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
        if let Some(cand_entity) =
            entity_get(&rtxn, cand_id).map_err(map_entity_op_error)?
        {
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
    drop(db_guard);

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
    // wired through ExecutorContext — phase 21. Tier 5 (create) only
    // fires if allow_create=true.
    if req.allow_create {
        // Phase 16.7.7: stub — defer create-fallback to the caller.
        // Returning NotFound here so clients explicitly call
        // ENTITY_CREATE if they want creation; auto-create lands when
        // the resolver's tier 5 wires statement extraction (phase 17+).
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

/// Normalize per `brain-metadata::entity_ops::normalize_name`. Inlined
/// here to avoid threading the metadata crate into the wire types.
fn normalize_name(s: &str) -> String {
    brain_metadata::entity_ops::normalize_name(s)
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

/// Map `EntityOpError` to `OpError`. Until §28 error codes get a
/// first-class slot in the substrate's `ErrorCode`, we surface them as
/// the closest substrate categories — NotFound for missing rows,
/// Conflict for duplicates, InvalidArgument for type-registry misses,
/// Internal for redb failures.
fn map_entity_op_error(err: EntityOpError) -> OpError {
    match err {
        EntityOpError::NotFound(id) => OpError::NotFound { what: "entity", detail: format!("{id:?}") },
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

/// Map `EntityMergeOpError` to `OpError`. Until §28 error codes get a
/// first-class slot, we map onto the closest substrate categories per
/// `spec/28_knowledge_wire_protocol/03_errors.md` Strategy B.
fn map_entity_merge_op_error(err: EntityMergeOpError) -> OpError {
    match err {
        EntityMergeOpError::EntityNotFound(id) => OpError::NotFound {
            what: "entity",
            detail: format!("{id:?}"),
        },
        EntityMergeOpError::SelfMerge => {
            OpError::Conflict("survivor and merged are the same entity".into())
        }
        EntityMergeOpError::AlreadyMerged(id, into) => {
            OpError::Conflict(format!("entity {id:?} already merged into {into:?}"))
        }
        EntityMergeOpError::TypeMismatch { survivor, merged } => OpError::InvalidRequest(format!(
            "type mismatch: survivor {survivor:?}, merged {merged:?}"
        )),
        EntityMergeOpError::Tombstoned(id) => {
            OpError::Conflict(format!("entity {id:?} is tombstoned"))
        }
        EntityMergeOpError::LowConfidence(c) => {
            OpError::InvalidRequest(format!("confidence {c} below merge threshold 0.7"))
        }
        EntityMergeOpError::OutOfGracePeriod => {
            OpError::Conflict("merge grace period expired".into())
        }
        EntityMergeOpError::NotMerged(id) => OpError::NotFound {
            what: "active merge",
            detail: format!("entity {id:?} is not currently merged"),
        },
        EntityMergeOpError::AuditMissing(id) => OpError::Internal(format!(
            "no active merge audit found for entity {id:?} (should not happen)"
        )),
        EntityMergeOpError::Storage(e) => OpError::Internal(format!("redb storage: {e}")),
        EntityMergeOpError::Table(e) => OpError::Internal(format!("redb table: {e}")),
        EntityMergeOpError::TrigramOp(e) => OpError::Internal(format!("trigram op: {e}")),
        EntityMergeOpError::EntityOp(e) => map_entity_op_error(e),
    }
}

/// Emit a knowledge-layer event onto the EventBus. Substrate fields are
/// zero-filled per spec §28/02 §2. Called post-commit by every entity
/// handler that mutates state.
fn emit_knowledge_event(
    ctx: &OpsContext,
    event_type: EventType,
    payload: KnowledgeEventPayload,
    timestamp_unix_nanos: u64,
) {
    let envelope = EventEnvelope {
        lsn: 0, // overwritten by EventBus::publish
        event_type,
        memory_id: MemoryId::NULL,
        context_id: brain_core::ContextId::default(),
        kind: brain_core::MemoryKind::Episodic,
        salience: 0.0,
        timestamp_unix_nanos,
        text: None,
        knowledge_payload: Some(payload),
    };
    let _ = ctx.events.publish(envelope);
}
