//! Apply StampAudit — append an audit row.
//!
//! The audit body is opaque to the apply function; the kind byte tells
//! the writer which audit table to write into. Today only the extractor
//! pipeline audit table is populated this way; schema-migration and
//! admin-op audits land in P2b.

use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, Write};

pub fn apply_stamp_audit(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("StampAudit"))
}
