//! Apply UpdateAttribute — polymorphic over target type.
//!
//! Status: scaffolded. Each target type (Memory / Entity / Statement /
//! Relation) dispatches to its own row update. Full implementation
//! lands in P2b.

use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, Write};

pub fn apply_update_attribute(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("UpdateAttribute"))
}
