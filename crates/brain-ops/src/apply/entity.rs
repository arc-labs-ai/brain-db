//! Apply functions for entity-shaped phases.
//!
//! Implemented:
//! - apply_upsert_entity         — entity_ops::entity_put
//! - apply_tombstone_entity      — entity_ops::entity_tombstone
//! - apply_update_entity         — entity_ops::entity_update
//! - apply_rename_entity         — entity_ops::entity_rename
//! - apply_unmerge_entities      — entity_merge::unmerge_entity
//! - apply_merge_entities        — entity_merge_ops::merge_entity

use brain_core::{Entity, EntityAttributes, EntityId};
use brain_metadata::entity::ops::{
    entity_get_inside_wtxn, entity_put, entity_rename, entity_tombstone, entity_update,
    normalize_name,
};
use brain_metadata::entity::review::{proposal_get_inside_wtxn, update_proposal_status};
use brain_metadata::tables::entity::{EntityMetadata, ENTITIES_TABLE};
use brain_metadata::tables::merge_review_queue::proposal_status;
use redb::{ReadableTable, WriteTransaction};

use super::ApplyError;
use crate::write::{Phase, PhaseAck, TombstoneTarget, Write};

/// Tenant wall for an entity mutation. Loads the primary row inside the
/// wtxn and confirms it belongs to the caller's `(namespace, space)`
/// scope; a row owned by another tenant (or a missing one) reads as
/// NotFound — no existence leak. Mirrors the memory-layer wall in
/// `apply_tombstone_memory`: the `entity::ops` mutators look rows up by
/// global id alone, so the apply layer is the atomic last line of
/// defense against a cross-tenant `EntityId`.
fn entity_scope_guard(
    wtxn: &WriteTransaction,
    id: EntityId,
    write: &Write,
) -> Result<(), ApplyError> {
    let row: Option<EntityMetadata> = {
        let t = wtxn
            .open_table(ENTITIES_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open entities: {e}")))?;
        let got = t
            .get(&id.to_bytes())
            .map_err(|e| ApplyError::Storage(format!("entity lookup: {e}")))?;
        got.map(|g| g.value())
    };
    let caller = brain_metadata::RowScope::new(write.namespace, write.space_id);
    match row {
        Some(m) if m.scope() == caller => Ok(()),
        _ => Err(ApplyError::NotFound {
            what: "entity",
            detail: format!("{id:?}"),
        }),
    }
}

/// Build the [`Entity`] a [`Phase::UpsertEntity`] describes. Shared by
/// the apply path (which persists it via `entity_put`) and the
/// WAL-mapping path (which serializes its row form into the WAL body),
/// so both agree byte-for-byte on what an upsert writes. Returns `None`
/// for any other phase shape.
pub(crate) fn entity_from_upsert_phase(phase: &Phase) -> Option<Entity> {
    let Phase::UpsertEntity {
        id,
        ty,
        // Session is stamped by the apply path (see `apply_upsert_entity`),
        // not carried on the brain-core `Entity` this builds.
        session: _,
        canonical,
        normalized,
        aliases,
        attributes,
        created_at_unix_nanos,
    } = phase
    else {
        return None;
    };
    let mut e = Entity::new_active(
        *id,
        *ty,
        canonical.clone(),
        normalized.clone(),
        *created_at_unix_nanos,
    );
    e.aliases = aliases.clone();
    e.attributes = attributes.clone();
    Some(e)
}

pub fn apply_upsert_entity(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let scope = brain_metadata::RowScope::new(write.namespace, write.space_id);
    let Phase::UpsertEntity { session, .. } = phase else {
        return Err(ApplyError::PhaseMisShape("expected UpsertEntity"));
    };
    let e = entity_from_upsert_phase(phase)
        .ok_or(ApplyError::PhaseMisShape("expected UpsertEntity"))?;
    let id = e.id;
    entity_put(wtxn, scope, *session, &e)
        .map_err(|err| ApplyError::Metadata(format!("entity_put: {err}")))?;
    // Write-path trace: which entity (canonical name) was minted/updated.
    // Subject resolution at read time keys on this canonical name, so a read
    // miss often traces back to a name that was never written this way.
    tracing::debug!(
        target: "brain_ops::write_trace",
        ?id,
        canonical = %e.canonical_name,
        type_id = ?e.entity_type,
        "write: entity upserted"
    );
    Ok(PhaseAck::UpsertedEntity(id))
}

pub fn apply_tombstone_entity(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::Tombstone {
        target,
        at_unix_nanos,
        ..
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected Tombstone"));
    };
    let TombstoneTarget::Entity(id) = target else {
        return Err(ApplyError::PhaseMisShape("expected Tombstone(Entity)"));
    };
    entity_scope_guard(wtxn, *id, write)?;
    entity_tombstone(wtxn, *id, *at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("entity_tombstone: {e}")))?;
    Ok(PhaseAck::Tombstoned {
        target: *target,
        tombstoned_at_unix_nanos: *at_unix_nanos,
    })
}

pub fn apply_merge_entities(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::MergeEntities {
        source,
        target,
        at_unix_nanos,
        confidence,
        reason,
        actor,
        grace_seconds,
        ..
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected MergeEntities"));
    };
    // Wall: the caller must own both endpoints. `merge_entity` already
    // requires source and target to share a scope, but that scope could
    // be a foreign tenant's — guard against the caller scope here.
    entity_scope_guard(wtxn, *target, write)?;
    entity_scope_guard(wtxn, *source, write)?;
    let audit_id = brain_metadata::entity::merge::merge_entity(
        wtxn,
        *target,
        *source,
        *confidence,
        reason.clone(),
        *actor,
        *grace_seconds,
        *at_unix_nanos,
    )
    .map_err(|e| ApplyError::Metadata(format!("merge_entity: {e}")))?;
    Ok(PhaseAck::EntityMerged {
        source: *source,
        target: *target,
        audit_id,
    })
}

pub fn apply_update_entity(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpdateEntity {
        id,
        canonical_name,
        aliases,
        attributes_blob,
        at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UpdateEntity"));
    };
    entity_scope_guard(wtxn, *id, write)?;
    let current = entity_get_inside_wtxn(wtxn, *id)
        .map_err(|e| ApplyError::Metadata(format!("entity_get: {e}")))?
        .ok_or_else(|| ApplyError::NotFound {
            what: "entity",
            detail: format!("{id:?}"),
        })?;
    let mut next = current;
    next.canonical_name = canonical_name.clone();
    next.normalized_name = normalize_name(canonical_name);
    next.aliases = aliases.clone();
    next.attributes = EntityAttributes::from(attributes_blob.clone());

    entity_update(wtxn, &next, *at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("entity_update: {e}")))?;

    let persisted = entity_get_inside_wtxn(wtxn, *id)
        .map_err(|e| ApplyError::Metadata(format!("entity_get post-update: {e}")))?
        .ok_or_else(|| ApplyError::Invariant(format!("entity {id:?} missing post-update")))?;

    Ok(PhaseAck::EntityUpdated {
        id: *id,
        snapshot: Box::new(persisted),
    })
}

pub fn apply_rename_entity(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::RenameEntity {
        id,
        new_canonical_name,
        at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected RenameEntity"));
    };
    entity_scope_guard(wtxn, *id, write)?;
    let current = entity_get_inside_wtxn(wtxn, *id)
        .map_err(|e| ApplyError::Metadata(format!("entity_get: {e}")))?
        .ok_or_else(|| ApplyError::NotFound {
            what: "entity",
            detail: format!("{id:?}"),
        })?;
    let old_canonical_name = current.canonical_name.clone();

    entity_rename(wtxn, *id, new_canonical_name.clone(), *at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("entity_rename: {e}")))?;

    let persisted = entity_get_inside_wtxn(wtxn, *id)
        .map_err(|e| ApplyError::Metadata(format!("entity_get post-rename: {e}")))?
        .ok_or_else(|| ApplyError::Invariant(format!("entity {id:?} missing post-rename")))?;

    Ok(PhaseAck::EntityRenamed {
        id: *id,
        old_canonical_name,
        snapshot: Box::new(persisted),
    })
}

/// Approve a Pending merge proposal. Looks up the proposal, executes
/// the underlying `merge_entity(source → candidate)`, and stamps the
/// proposal row `Approved` — all inside the caller's wtxn. Fails if
/// the proposal is missing, already-terminal, or the underlying
/// merge's pre-conditions don't hold.
///
/// Used by both the admin "approve by id" path (operator clicks
/// approve) and the ambiguity-resolver worker's "auto-apply"
/// path — the worker uses [`apply_approve_merge_with_status`] to
/// stamp `AutoApplied` instead of `Approved` so audit can tell them
/// apart.
pub fn apply_approve_merge(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::ApproveMerge {
        proposal_id,
        actor,
        grace_seconds,
        at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected ApproveMerge"));
    };
    apply_approve_merge_with_status(
        wtxn,
        *proposal_id,
        *actor,
        *grace_seconds,
        *at_unix_nanos,
        proposal_status::APPROVED,
        write,
    )
}

/// Lower-level approve that lets the worker stamp `AutoApplied` instead
/// of `Approved`. Re-used by [`apply_approve_merge`] and by direct
/// worker-side callers.
#[allow(clippy::too_many_arguments)]
pub fn apply_approve_merge_with_status(
    wtxn: &WriteTransaction,
    proposal_id: brain_core::MergeId,
    actor: brain_metadata::entity::merge::MergeActor,
    grace_seconds: u64,
    at_unix_nanos: u64,
    new_status: u8,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let proposal = proposal_get_inside_wtxn(wtxn, proposal_id)
        .map_err(|e| ApplyError::Metadata(format!("proposal_get: {e}")))?
        .ok_or_else(|| ApplyError::NotFound {
            what: "merge_proposal",
            detail: format!("{proposal_id:?}"),
        })?;
    if proposal.is_terminal() {
        return Err(ApplyError::Invariant(format!(
            "proposal {proposal_id:?} is already in terminal state {}",
            proposal.status
        )));
    }
    let source = EntityId::from(proposal.source_entity);
    let candidate = EntityId::from(proposal.candidate_entity);
    // The proposal points "merge source into candidate" — candidate
    // is canonical (it pre-dated the source).
    let audit_id = brain_metadata::entity::merge::merge_entity(
        wtxn,
        candidate,
        source,
        // The recheck score is more accurate than the proposal-time
        // score; fall back to proposal confidence when the worker
        // never ran (operator clicked approve before any tick visited
        // the proposal).
        if proposal.last_recheck_confidence > 0.0 {
            proposal.last_recheck_confidence
        } else {
            proposal.confidence
        },
        "merge_review_queue: proposal approved".to_string(),
        actor,
        grace_seconds,
        at_unix_nanos,
    )
    .map_err(|e| ApplyError::Metadata(format!("merge_entity: {e}")))?;
    update_proposal_status(
        wtxn,
        proposal_id,
        new_status,
        proposal.last_recheck_confidence,
        at_unix_nanos,
    )
    .map_err(|e| ApplyError::Metadata(format!("update_proposal_status: {e}")))?;
    Ok(PhaseAck::MergeProposalApproved {
        proposal_id,
        audit_id,
    })
}

/// Reject a Pending merge proposal. Stamps the proposal `Rejected`;
/// leaves the source and candidate entities untouched.
pub fn apply_reject_merge(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::RejectMerge {
        proposal_id,
        at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected RejectMerge"));
    };
    let proposal = proposal_get_inside_wtxn(wtxn, *proposal_id)
        .map_err(|e| ApplyError::Metadata(format!("proposal_get: {e}")))?
        .ok_or_else(|| ApplyError::NotFound {
            what: "merge_proposal",
            detail: format!("{proposal_id:?}"),
        })?;
    if proposal.is_terminal() {
        return Err(ApplyError::Invariant(format!(
            "proposal {proposal_id:?} is already in terminal state {}",
            proposal.status
        )));
    }
    update_proposal_status(
        wtxn,
        *proposal_id,
        proposal_status::REJECTED,
        proposal.last_recheck_confidence,
        *at_unix_nanos,
    )
    .map_err(|e| ApplyError::Metadata(format!("update_proposal_status: {e}")))?;
    Ok(PhaseAck::MergeProposalRejected {
        proposal_id: *proposal_id,
    })
}

pub fn apply_unmerge_entities(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UnmergeEntities {
        merged,
        actor,
        at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UnmergeEntities"));
    };
    // Wall: the caller must own the merged entity. Its survivor shares the
    // same scope (a merge never crosses tenants), so guarding `merged` is
    // sufficient.
    entity_scope_guard(wtxn, *merged, write)?;
    let survivor =
        brain_metadata::entity::merge::unmerge_entity(wtxn, *merged, *actor, *at_unix_nanos)
            .map_err(|e| ApplyError::Metadata(format!("unmerge_entity: {e}")))?;

    Ok(PhaseAck::EntitiesUnmerged {
        restored: *merged,
        survivor,
    })
}

#[cfg(test)]
mod tests {
    fn __ts() -> brain_metadata::RowScope {
        brain_metadata::RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xA1; 16])
    }

    use super::*;
    use brain_core::{EntityAttributes, EntityId, EntityType};
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    use crate::write::{Phase, Write, WriteId};

    fn open_db() -> (TempDir, MetadataDb) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        (dir, db)
    }

    fn empty_write() -> Write {
        Write::single(
            WriteId::new(),
            brain_core::SpaceId::default(),
            Phase::ReclaimSlots { slots: Vec::new() },
        )
    }

    #[test]
    fn upsert_entity_writes_row() {
        let (_dir, db) = open_db();
        let id = EntityId::new();
        let phase = Phase::UpsertEntity {
            id,
            ty: EntityType::PERSON_ID,
            session: brain_core::SessionId::DEFAULT,
            canonical: "Alice".into(),
            normalized: brain_metadata::entity::ops::normalize_name("Alice"),
            aliases: Vec::new(),
            attributes: EntityAttributes::empty(),
            created_at_unix_nanos: 1_700_000_000_000,
        };
        let wtxn = db.write_txn().unwrap();
        let ack = apply_upsert_entity(&wtxn, &phase, &empty_write()).unwrap();
        assert!(matches!(ack, PhaseAck::UpsertedEntity(eid) if eid == id));
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = brain_metadata::entity::ops::entity_get(&rtxn, id).unwrap();
        let e = got.expect("entity must exist after upsert");
        assert_eq!(e.canonical_name, "Alice");
    }

    #[test]
    fn tombstone_entity_marks_merged_or_inactive() {
        let (_dir, db) = open_db();
        let id = EntityId::new();
        // Seed.
        {
            let wtxn = db.write_txn().unwrap();
            let e = Entity::new_active(
                id,
                EntityType::PERSON_ID,
                "Alice".into(),
                brain_metadata::entity::ops::normalize_name("Alice"),
                1_700_000_000_000,
            );
            entity_put(&wtxn, __ts(), brain_core::SessionId::DEFAULT, &e).unwrap();
            wtxn.commit().unwrap();
        }
        // Tombstone via apply.
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Entity(id),
            reason: 0,
            at_unix_nanos: 1_700_000_001_000,
        };
        // The write must carry the SAME scope the entity was seeded under
        // (`__ts()`); the apply-layer tenant wall now rejects a mismatch.
        let ts = __ts();
        let scoped_write = Write::single(
            WriteId::new(),
            ts.space(),
            Phase::ReclaimSlots { slots: Vec::new() },
        )
        .with_namespace(ts.namespace());
        let wtxn = db.write_txn().unwrap();
        let ack = apply_tombstone_entity(&wtxn, &phase, &scoped_write).unwrap();
        assert!(matches!(ack, PhaseAck::Tombstoned { .. }));
        wtxn.commit().unwrap();
    }
}

#[cfg(all(test, not(miri)))]
mod tenant_wall_tests {
    //! Cross-tenant write isolation for the entity apply path. A caller
    //! in tenant B must not tombstone / update / rename / merge an entity
    //! owned by tenant A; the mutation reads as NotFound and A's row is
    //! left untouched.
    use super::*;
    use brain_core::EntityType;
    use brain_metadata::entity::merge::MergeActor;
    use brain_metadata::entity::ops::{entity_get, entity_put, normalize_name};
    use brain_metadata::{MetadataDb, RowScope};
    use tempfile::TempDir;

    use crate::write::{Phase, TombstoneTarget, Write, WriteId};

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn scope_a() -> RowScope {
        RowScope::from_bytes(1, [0xA1; 16])
    }
    fn scope_b() -> RowScope {
        RowScope::from_bytes(2, [0xB2; 16])
    }

    fn open_db() -> (TempDir, MetadataDb) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        (dir, db)
    }

    fn write_for(scope: RowScope, phase: Phase) -> Write {
        Write::single(WriteId::new(), scope.space(), phase).with_namespace(scope.namespace())
    }

    fn seed_entity(db: &MetadataDb, scope: RowScope, name: &str) -> EntityId {
        let id = EntityId::new();
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            normalize_name(name),
            NOW,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, scope, brain_core::SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    #[test]
    fn cross_tenant_tombstone_denied_and_row_untouched() {
        let (_dir, db) = open_db();
        let id = seed_entity(&db, scope_a(), "Alice");
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Entity(id),
            reason: 1,
            at_unix_nanos: NOW + 1_000,
        };
        let wtxn = db.write_txn().unwrap();
        let err = apply_tombstone_entity(&wtxn, &phase, &write_for(scope_b(), phase.clone()))
            .expect_err("tenant B must not tombstone tenant A's entity");
        assert!(matches!(err, ApplyError::NotFound { what: "entity", .. }));
        drop(wtxn);

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().expect("A's entity present");
        assert_eq!(
            got.flags & brain_metadata::tables::entity::flags::TOMBSTONED,
            0
        );
    }

    #[test]
    fn cross_tenant_update_denied_and_row_untouched() {
        let (_dir, db) = open_db();
        let id = seed_entity(&db, scope_a(), "Alice");
        let phase = Phase::UpdateEntity {
            id,
            canonical_name: "Mallory".into(),
            aliases: Vec::new(),
            attributes_blob: Vec::new(),
            at_unix_nanos: NOW + 1_000,
        };
        let wtxn = db.write_txn().unwrap();
        let err = apply_update_entity(&wtxn, &phase, &write_for(scope_b(), phase.clone()))
            .expect_err("tenant B must not update tenant A's entity");
        assert!(matches!(err, ApplyError::NotFound { what: "entity", .. }));
        drop(wtxn);

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(got.canonical_name, "Alice", "A's name must be unchanged");
    }

    #[test]
    fn cross_tenant_rename_denied_and_row_untouched() {
        let (_dir, db) = open_db();
        let id = seed_entity(&db, scope_a(), "Alice");
        let phase = Phase::RenameEntity {
            id,
            new_canonical_name: "Mallory".into(),
            at_unix_nanos: NOW + 1_000,
        };
        let wtxn = db.write_txn().unwrap();
        let err = apply_rename_entity(&wtxn, &phase, &write_for(scope_b(), phase.clone()))
            .expect_err("tenant B must not rename tenant A's entity");
        assert!(matches!(err, ApplyError::NotFound { what: "entity", .. }));
        drop(wtxn);

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(got.canonical_name, "Alice");
    }

    #[test]
    fn cross_tenant_merge_denied_and_rows_untouched() {
        let (_dir, db) = open_db();
        let survivor = seed_entity(&db, scope_a(), "Alice");
        let merged = seed_entity(&db, scope_a(), "Alicia");
        let phase = Phase::MergeEntities {
            source: merged,
            target: survivor,
            retain_aliases: true,
            retain_attributes: true,
            at_unix_nanos: NOW + 1_000,
            confidence: 0.95,
            reason: "b-initiated".into(),
            actor: MergeActor::Space(scope_b().space_id_bytes),
            grace_seconds: 7 * 24 * 60 * 60,
        };
        let wtxn = db.write_txn().unwrap();
        let err = apply_merge_entities(&wtxn, &phase, &write_for(scope_b(), phase.clone()))
            .expect_err("tenant B must not merge tenant A's entities");
        assert!(matches!(err, ApplyError::NotFound { what: "entity", .. }));
        drop(wtxn);

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, merged).unwrap().unwrap();
        assert!(got.merged_into.is_none(), "A's entity must not be merged");
    }

    #[test]
    fn same_tenant_tombstone_succeeds() {
        let (_dir, db) = open_db();
        let id = seed_entity(&db, scope_a(), "Alice");
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Entity(id),
            reason: 1,
            at_unix_nanos: NOW + 1_000,
        };
        let wtxn = db.write_txn().unwrap();
        apply_tombstone_entity(&wtxn, &phase, &write_for(scope_a(), phase.clone()))
            .expect("same-tenant tombstone must succeed");
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().unwrap();
        assert_ne!(
            got.flags & brain_metadata::tables::entity::flags::TOMBSTONED,
            0
        );
    }
}
