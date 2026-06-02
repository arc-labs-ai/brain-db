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
use brain_metadata::tables::merge_review_queue::proposal_status;
use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, TombstoneTarget, Write};

/// Build the [`Entity`] a [`Phase::UpsertEntity`] describes. Shared by
/// the apply path (which persists it via `entity_put`) and the
/// WAL-mapping path (which serializes its row form into the WAL body),
/// so both agree byte-for-byte on what an upsert writes. Returns `None`
/// for any other phase shape.
pub(crate) fn entity_from_upsert_phase(phase: &Phase) -> Option<Entity> {
    let Phase::UpsertEntity {
        id,
        ty,
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
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let e = entity_from_upsert_phase(phase)
        .ok_or(ApplyError::PhaseMisShape("expected UpsertEntity"))?;
    let id = e.id;
    entity_put(wtxn, &e).map_err(|err| ApplyError::Metadata(format!("entity_put: {err}")))?;
    Ok(PhaseAck::UpsertedEntity(id))
}

pub fn apply_tombstone_entity(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
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
    _write: &Write,
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
    _write: &Write,
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
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::RenameEntity {
        id,
        new_canonical_name,
        at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected RenameEntity"));
    };
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
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UnmergeEntities {
        merged,
        actor,
        at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UnmergeEntities"));
    };
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
            brain_core::AgentId::default(),
            Phase::SetExtractorEnabled {
                id: brain_core::ExtractorId::from(0),
                enabled: true,
            },
        )
    }

    #[test]
    fn upsert_entity_writes_row() {
        let (_dir, db) = open_db();
        let id = EntityId::new();
        let phase = Phase::UpsertEntity {
            id,
            ty: EntityType::PERSON_ID,
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
            entity_put(&wtxn, &e).unwrap();
            wtxn.commit().unwrap();
        }
        // Tombstone via apply.
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Entity(id),
            reason: 0,
            at_unix_nanos: 1_700_000_001_000,
        };
        let wtxn = db.write_txn().unwrap();
        let ack = apply_tombstone_entity(&wtxn, &phase, &empty_write()).unwrap();
        assert!(matches!(ack, PhaseAck::Tombstoned { .. }));
        wtxn.commit().unwrap();
    }
}
