//! Apply functions for statement-shaped phases.
//!
//! Full P2c coverage: UpsertStatement, Tombstone(Statement),
//! Supersede(Statement) all real. Each ports to a brain-metadata
//! helper that runs inside the wtxn.

use brain_core::{EvidenceRef, Statement, TombstoneReason};
use brain_metadata::statement::{statement_create, statement_supersede, statement_tombstone};
use redb::WriteTransaction;
use smallvec::SmallVec;

use super::ApplyError;
use crate::write::{
    EvidenceRefPhase, Phase, PhaseAck, SupersedeReplacement, SupersedeTarget, TombstoneTarget,
    Write,
};

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
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::Supersede {
        target,
        replacement,
        at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected Supersede"));
    };
    let SupersedeTarget::Statement(old_id) = target else {
        return Err(ApplyError::PhaseMisShape("expected Supersede(Statement)"));
    };
    let SupersedeReplacement::Statement(new_statement) = replacement else {
        return Err(ApplyError::PhaseMisShape(
            "expected Supersede with Statement replacement",
        ));
    };
    statement_supersede(wtxn, *old_id, new_statement.as_ref(), *at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("statement_supersede: {e}")))?;
    Ok(PhaseAck::Superseded(*target, replacement.id()))
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
    Ok(PhaseAck::Tombstoned {
        target: *target,
        tombstoned_at_unix_nanos: *at_unix_nanos,
    })
}

fn build_evidence_ref(phase_ref: &EvidenceRefPhase) -> EvidenceRef {
    match phase_ref {
        EvidenceRefPhase::Inline(entries) => {
            let mut sv: SmallVec<
                [brain_core::EvidenceEntry; brain_core::INLINE_EVIDENCE_CAP],
            > = SmallVec::new();
            for &e in entries {
                if sv.len() == brain_core::INLINE_EVIDENCE_CAP {
                    break;
                }
                sv.push(e);
            }
            EvidenceRef::inline(sv)
        }
        EvidenceRefPhase::Overflow(id) => EvidenceRef::Overflow(*id),
    }
}
