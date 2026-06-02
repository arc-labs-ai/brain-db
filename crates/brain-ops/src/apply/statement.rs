//! Apply functions for statement-shaped phases.
//!
//! Covers UpsertStatement, Tombstone(Statement), and
//! Supersede(Statement). Each ports to a brain-metadata
//! helper that runs inside the wtxn.

use brain_core::{EvidenceRef, Statement, StatementId, TombstoneReason};
use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::statement::{statement_create, statement_supersede, statement_tombstone};
use brain_metadata::tables::statement::{statement_flags, StatementMetadata, STATEMENTS_TABLE};
use redb::{ReadableTable, WriteTransaction};
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
    // Only the fields the apply path needs directly: the predicate
    // resolution + idempotency stamp. The rest of the row is built by
    // `statement_from_upsert_phase` (shared with the WAL-mapping path).
    let Phase::UpsertStatement {
        id,
        predicate,
        extracted_at_unix_nanos,
        predicate_intern_hint,
        ..
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UpsertStatement"));
    };

    // Schemaless path: intern the predicate inside this wtxn so the
    // schemaless STATEMENT_CREATE costs one fsync instead of three.
    // `predicate_intern_or_get` is idempotent — a concurrent writer that
    // interned the same qname just before us returns the existing id.
    let resolved_predicate = match predicate_intern_hint {
        None => *predicate,
        Some((namespace, name)) => predicate_intern_or_get(
            wtxn,
            namespace,
            name,
            /* first_seen_lsn */ 0,
            *extracted_at_unix_nanos,
        )
        .map_err(|e| ApplyError::Metadata(format!("predicate_intern_or_get: {e}")))?,
    };

    let s = statement_from_upsert_phase(phase, resolved_predicate)
        .ok_or(ApplyError::PhaseMisShape("expected UpsertStatement"))?;
    statement_create(wtxn, &s, *extracted_at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("statement_create: {e}")))?;

    // Stamp IMPLICIT_PREDICATE on the schemaless write's row so later
    // schema-adoption analysis can tell which rows the new schema
    // would need to adopt or evict. Folded into the same wtxn — the
    // pre-refactor code paid an extra commit for this single byte.
    //
    // `*id` is the same id `statement_create` writes (and the same one
    // the ack carries) — both the fresh path and the auto-supersede
    // shortcut reuse the phase's pre-minted id.
    if predicate_intern_hint.is_some() {
        stamp_implicit_predicate_flag(wtxn, *id)?;
    }

    Ok(PhaseAck::UpsertedStatement(*id, 1))
}

/// OR the `IMPLICIT_PREDICATE` bit into the just-written statement
/// row's flags. Runs inside the same wtxn as `statement_create`.
fn stamp_implicit_predicate_flag(
    wtxn: &WriteTransaction,
    id: StatementId,
) -> Result<(), ApplyError> {
    let key = id.to_bytes();
    let existing: Option<StatementMetadata> = {
        let t = wtxn
            .open_table(STATEMENTS_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open statements: {e}")))?;
        let g = t
            .get(&key)
            .map_err(|e| ApplyError::Storage(format!("statement lookup: {e}")))?;
        g.map(|guard| guard.value())
    };
    let Some(mut row) = existing else {
        // statement_create just wrote this — should always be present.
        return Err(ApplyError::Invariant(format!(
            "statement {id:?} missing after create in same wtxn"
        )));
    };
    if row.set_flag(statement_flags::IMPLICIT_PREDICATE) {
        let mut t = wtxn
            .open_table(STATEMENTS_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open statements (write): {e}")))?;
        t.insert(&key, &row)
            .map_err(|e| ApplyError::Storage(format!("statement update: {e}")))?;
    }
    Ok(())
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

/// Build the [`Statement`] a [`Phase::UpsertStatement`] describes, given
/// the resolved predicate. Shared by the apply path (which interns the
/// predicate inside its wtxn, then persists) and the WAL-mapping path
/// (which passes the phase's placeholder predicate and carries the intern
/// hint for recovery to re-resolve) — so both agree on every
/// non-predicate field. Returns `None` for any other phase shape.
pub(crate) fn statement_from_upsert_phase(
    phase: &Phase,
    predicate: brain_core::PredicateId,
) -> Option<Statement> {
    let Phase::UpsertStatement {
        id,
        kind,
        subject,
        object,
        confidence,
        evidence,
        valid_from_unix_nanos,
        extractor,
        extracted_at_unix_nanos,
        schema_version,
        ..
    } = phase
    else {
        return None;
    };
    let evidence_ref = build_evidence_ref(evidence);
    let mut s = Statement::new_root(
        *id,
        *kind,
        *subject,
        predicate,
        object.clone(),
        *confidence,
        evidence_ref,
        *extractor,
        *extracted_at_unix_nanos,
        *schema_version,
    );
    s.valid_from_unix_nanos = *valid_from_unix_nanos;
    Some(s)
}

fn build_evidence_ref(phase_ref: &EvidenceRefPhase) -> EvidenceRef {
    match phase_ref {
        EvidenceRefPhase::Inline(entries) => {
            let mut sv: SmallVec<[brain_core::EvidenceEntry; brain_core::INLINE_EVIDENCE_CAP]> =
                SmallVec::new();
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
