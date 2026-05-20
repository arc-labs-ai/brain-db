//! Apply functions for entity-shaped phases.
//!
//! Implemented in P2b:
//! - apply_upsert_entity         — entity_ops::entity_put
//! - apply_tombstone_entity      — entity_ops::entity_tombstone
//!
//! Deferred:
//! - apply_resolve               — the resolver gauntlet needs to run
//!   ahead of the write; the apply function would just persist what's
//!   already been decided. Lands when handler migration drives demand.
//! - apply_merge_entities        — complex aliases / attributes /
//!   edge-rewrite logic in handle_entity_merge that needs careful
//!   porting. Stays NotYetImplemented until a follow-up slice.

use brain_core::knowledge::Entity;
use brain_metadata::entity_ops::{entity_put, entity_tombstone};
use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, TombstoneTarget, Write};

pub fn apply_upsert_entity(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpsertEntity {
        id,
        ty,
        canonical,
        normalized,
        attributes,
        created_at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UpsertEntity"));
    };
    let mut e = Entity::new_active(
        *id,
        *ty,
        canonical.clone(),
        normalized.clone(),
        *created_at_unix_nanos,
    );
    e.attributes = attributes.clone();
    entity_put(wtxn, &e).map_err(|err| ApplyError::Metadata(format!("entity_put: {err}")))?;
    Ok(PhaseAck::UpsertedEntity(*id))
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
    Ok(PhaseAck::Tombstoned(*target))
}

pub fn apply_resolve(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("Resolve"))
}

pub fn apply_merge_entities(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("MergeEntities"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::knowledge::{EntityAttributes, EntityId, EntityType};
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
                id: brain_core::knowledge::ExtractorId::from(0),
                enabled: true,
            },
        )
    }

    #[test]
    fn upsert_entity_writes_row() {
        let (_dir, mut db) = open_db();
        let id = EntityId::new();
        let phase = Phase::UpsertEntity {
            id,
            ty: EntityType::PERSON_ID,
            canonical: "Alice".into(),
            normalized: brain_metadata::entity_ops::normalize_name("Alice"),
            attributes: EntityAttributes::empty(),
            created_at_unix_nanos: 1_700_000_000_000,
        };
        let wtxn = db.write_txn().unwrap();
        let ack = apply_upsert_entity(&wtxn, &phase, &empty_write()).unwrap();
        assert!(matches!(ack, PhaseAck::UpsertedEntity(eid) if eid == id));
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = brain_metadata::entity_ops::entity_get(&rtxn, id).unwrap();
        let e = got.expect("entity must exist after upsert");
        assert_eq!(e.canonical_name, "Alice");
    }

    #[test]
    fn tombstone_entity_marks_merged_or_inactive() {
        let (_dir, mut db) = open_db();
        let id = EntityId::new();
        // Seed.
        {
            let wtxn = db.write_txn().unwrap();
            let e = Entity::new_active(
                id,
                EntityType::PERSON_ID,
                "Alice".into(),
                brain_metadata::entity_ops::normalize_name("Alice"),
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
        assert!(matches!(ack, PhaseAck::Tombstoned(_)));
        wtxn.commit().unwrap();
    }
}
