//! Apply functions for relation-shaped phases.
//!
//! Status: scaffolded. Full implementations land in P2b.

use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, Write};

pub fn apply_upsert_relation(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("UpsertRelation"))
}

pub fn apply_supersede_relation(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("Supersede(Relation)"))
}

pub fn apply_tombstone_relation(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("Tombstone(Relation)"))
}
