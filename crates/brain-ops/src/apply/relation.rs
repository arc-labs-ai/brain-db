//! Apply functions for relation-shaped phases.
//!
//! P2b implementation. UpsertRelation + Tombstone(Relation) port
//! straight to brain_metadata::relation_ops helpers. Supersede deferred
//! — same reason as the statement-supersede stub: the helper takes a
//! full Relation value (not just id), so resolving needs a Phase shape
//! change carrying the new Relation inline.

use brain_core::knowledge::Relation;
use brain_metadata::relation_ops::{relation_create, relation_tombstone};
use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, TombstoneTarget, Write};

pub fn apply_upsert_relation(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpsertRelation {
        id,
        ty,
        from,
        to,
        confidence,
        evidence_memories,
        is_symmetric,
        extractor,
        extracted_at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UpsertRelation"));
    };
    let r = Relation::new_root(
        *id,
        *ty,
        *from,
        *to,
        *confidence,
        evidence_memories.clone(),
        *extractor,
        *extracted_at_unix_nanos,
        *is_symmetric,
    );
    relation_create(wtxn, &r, *extracted_at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("relation_create: {e}")))?;
    Ok(PhaseAck::UpsertedRelation(*id, 1))
}

pub fn apply_supersede_relation(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("Supersede(Relation)"))
}

pub fn apply_tombstone_relation(
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
    let TombstoneTarget::Relation(id) = target else {
        return Err(ApplyError::PhaseMisShape("expected Tombstone(Relation)"));
    };
    relation_tombstone(wtxn, *id, *at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("relation_tombstone: {e}")))?;
    Ok(PhaseAck::Tombstoned(*target))
}
