//! Apply functions for statement-shaped phases.
//!
//! P2b implementation. UpsertStatement + Tombstone(Statement) port
//! straight to brain_metadata::statement_ops helpers.
//!
//! Supersede(Statement) is deferred — the brain-metadata helper takes
//! the full new Statement value (not just its id), but Phase::Supersede
//! today carries only the replacement id. Resolving this needs a Phase
//! shape change: SupersedeReplacement should carry Statement / Relation
//! values inline. Tracked as a follow-up slice; for now the stub
//! returns NotYetImplemented.

use brain_core::knowledge::{EvidenceRef, Statement, TombstoneReason};
use brain_metadata::statement_ops::{statement_create, statement_tombstone};
use redb::WriteTransaction;
use smallvec::SmallVec;

use super::ApplyError;
use crate::write::{EvidenceRefPhase, Phase, PhaseAck, TombstoneTarget, Write};

pub fn apply_upsert_statement(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpsertStatement {
        id,
        kind,
        subject,
        predicate,
        object,
        confidence,
        evidence,
        valid_from_unix_nanos,
        extractor,
        extracted_at_unix_nanos,
        schema_version,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UpsertStatement"));
    };

    let evidence_ref = build_evidence_ref(evidence);
    let mut s = Statement::new_root(
        *id,
        *kind,
        *subject,
        *predicate,
        object.clone(),
        *confidence,
        evidence_ref,
        *extractor,
        *extracted_at_unix_nanos,
        *schema_version,
    );
    s.valid_from_unix_nanos = *valid_from_unix_nanos;
    statement_create(wtxn, &s, *extracted_at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("statement_create: {e}")))?;
    Ok(PhaseAck::UpsertedStatement(*id, 1))
}

pub fn apply_supersede_statement(
    _wtxn: &WriteTransaction,
    _phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    Err(ApplyError::NotYetImplemented("Supersede(Statement)"))
}

pub fn apply_tombstone_statement(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::Tombstone {
        target,
        reason,
        at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected Tombstone"));
    };
    let TombstoneTarget::Statement(id) = target else {
        return Err(ApplyError::PhaseMisShape("expected Tombstone(Statement)"));
    };
    let reason = TombstoneReason::from_u8(*reason).unwrap_or(TombstoneReason::UserRequest);
    statement_tombstone(wtxn, *id, reason, *at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("statement_tombstone: {e}")))?;
    Ok(PhaseAck::Tombstoned(*target))
}

fn build_evidence_ref(phase_ref: &EvidenceRefPhase) -> EvidenceRef {
    match phase_ref {
        EvidenceRefPhase::Inline(entries) => {
            let mut sv: SmallVec<
                [brain_core::knowledge::EvidenceEntry; brain_core::knowledge::INLINE_EVIDENCE_CAP],
            > = SmallVec::new();
            for &e in entries {
                if sv.len() == brain_core::knowledge::INLINE_EVIDENCE_CAP {
                    break;
                }
                sv.push(e);
            }
            EvidenceRef::inline(sv)
        }
        EvidenceRefPhase::Overflow(id) => EvidenceRef::Overflow(*id),
    }
}
