//! Apply functions for statement-shaped phases.
//!
//! Status: scaffolded. Full implementations land in P2b.

use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, Write};

pub fn apply_upsert_statement(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("UpsertStatement"))
}

pub fn apply_supersede_statement(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("Supersede(Statement)"))
}

pub fn apply_tombstone_statement(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("Tombstone(Statement)"))
}
