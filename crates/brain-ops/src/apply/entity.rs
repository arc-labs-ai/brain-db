//! Apply functions for entity-shaped phases.
//!
//! Status: scaffolded. Full implementations land in P2b — entity merge
//! and resolve are non-trivial (read-modify-write inside the wtxn).

use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, Write};

pub fn apply_upsert_entity(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("UpsertEntity"))
}

pub fn apply_tombstone_entity(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("Tombstone(Entity)"))
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
