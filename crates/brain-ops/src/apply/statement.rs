//! Apply functions for statement-shaped phases.
//!
//! Covers UpsertStatement, Tombstone(Statement), and
//! Supersede(Statement). Each ports to a brain-metadata
//! helper that runs inside the wtxn.

use brain_core::{EvidenceRef, Statement, StatementId, TombstoneReason};
use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::statement::{
    statement_create, statement_supersede, statement_tombstone, StatementOpError,
};
use brain_metadata::tables::statement::{statement_flags, StatementMetadata, STATEMENTS_TABLE};
use brain_metadata::DeclScope;
use redb::{ReadableTable, WriteTransaction};
use smallvec::SmallVec;

use super::{ApplyError, RedbFault};
use crate::write::{
    EvidenceRefPhase, Phase, PhaseAck, SupersedeReplacement, SupersedeTarget, TombstoneTarget,
    Write,
};

pub fn apply_upsert_statement(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let scope = brain_metadata::RowScope::new(write.namespace, write.space_id);
    // Only the fields the apply path needs directly: the predicate
    // resolution + idempotency stamp. The rest of the row is built by
    // `statement_from_upsert_phase` (shared with the WAL-mapping path).
    let Phase::UpsertStatement {
        id,
        session,
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
            DeclScope::base(namespace),
            name,
            /* first_seen_lsn */ 0,
            *extracted_at_unix_nanos,
        )
        .map_err(|e| ApplyError::Metadata(format!("predicate_intern_or_get: {e}")))?,
    };

    let s = statement_from_upsert_phase(phase, resolved_predicate)
        .ok_or(ApplyError::PhaseMisShape("expected UpsertStatement"))?;
    let stored_id = statement_create(wtxn, scope, *session, &s, *extracted_at_unix_nanos)
        .map_err(|e| statement_op_err("statement_create", e))?;

    // Write-path trace: the structured fact this ENCODE produced. The
    // predicate name comes from the intern hint (schemaless path) so the
    // triple is human-readable; subject/object are ids (resolve via the
    // entity tables when correlating a read miss to a missing/odd write).
    tracing::debug!(
        target: "brain_ops::write_trace",
        ?id,
        subject = ?s.subject,
        predicate = %predicate_intern_hint
            .as_ref()
            .map(|(ns, n)| format!("{ns}:{n}"))
            .unwrap_or_else(|| format!("{resolved_predicate:?}")),
        kind = ?s.kind,
        object = ?s.object,
        "write: statement created"
    );

    // Stamp IMPLICIT_PREDICATE on the schemaless write's row so later
    // schema-adoption analysis can tell which rows the new schema
    // would need to adopt or evict. Folded into the same wtxn — the
    // pre-refactor code paid an extra commit for this single byte.
    //
    // Stamp the row `statement_create` actually settled on, not the
    // phase's minted id: the function has three exits — a fresh insert
    // at the minted id, an auto-supersede that mints its own successor,
    // and a content-dedup no-op that keeps a pre-existing identical row
    // and returns ITS id. Only the returned id is guaranteed to name a
    // row in this wtxn. Re-stamping an already-flagged dedup target is
    // a no-op: `set_flag` reports no change and the write is skipped.
    if predicate_intern_hint.is_some() {
        stamp_implicit_predicate_flag(wtxn, stored_id)?;
    }

    Ok(PhaseAck::UpsertedStatement(stored_id, 1))
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
            .map_err(|e| ApplyError::storage("open statements", &e))?;
        let g = t
            .get(&key)
            .map_err(|e| ApplyError::storage("statement lookup", &e))?;
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
            .map_err(|e| ApplyError::storage("open statements (write)", &e))?;
        t.insert(&key, &row)
            .map_err(|e| ApplyError::storage("statement update", &e))?;
    }
    Ok(())
}

/// Tenant wall for a statement mutation. Loads the target row inside the
/// wtxn and confirms it belongs to the caller's `(namespace, space)`
/// scope; a row owned by another tenant (or a missing one) reads as
/// NotFound — no existence leak. Mirrors the memory-layer wall in
/// `apply_tombstone_memory`: the apply layer is the authoritative,
/// atomic last line of defense, because a `StatementId` is an
/// enumerable value and the mutators look rows up by global id alone.
fn statement_scope_guard(
    wtxn: &WriteTransaction,
    id: StatementId,
    write: &Write,
) -> Result<(), ApplyError> {
    let row: Option<StatementMetadata> = {
        let t = wtxn
            .open_table(STATEMENTS_TABLE)
            .map_err(|e| ApplyError::storage("open statements", &e))?;
        let got = t
            .get(&id.to_bytes())
            .map_err(|e| ApplyError::storage("statement lookup", &e))?;
        got.map(|g| g.value())
    };
    let caller = brain_metadata::RowScope::new(write.namespace, write.space_id);
    match row {
        Some(m) if m.scope() == caller => Ok(()),
        _ => Err(ApplyError::NotFound {
            what: "statement",
            detail: format!("{id:?}"),
        }),
    }
}

pub fn apply_supersede_statement(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let scope = brain_metadata::RowScope::new(write.namespace, write.space_id);
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
    // Wall: the caller must own the row it supersedes. `statement_supersede`
    // re-checks this too (defense in depth); guarding here keeps the write
    // from touching any table when the target is foreign / absent.
    statement_scope_guard(wtxn, *old_id, write)?;
    // Explicit STATEMENT_SUPERSEDE carries no session on the phase; the
    // replacement row lands in the default session (session grouping is
    // driven by the write-path UpsertStatement, not the supersede verb).
    statement_supersede(
        wtxn,
        scope,
        brain_core::SessionId::DEFAULT,
        *old_id,
        new_statement.as_ref(),
        *at_unix_nanos,
    )
    .map_err(|e| statement_op_err("statement_supersede", e))?;
    Ok(PhaseAck::Superseded(*target, replacement.id()))
}

pub fn apply_tombstone_statement(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
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
    // Wall: STATEMENT_TOMBSTONE / STATEMENT_RETRACT share this apply path
    // and the metadata mutator looks the row up by global id, so the
    // tenant check must happen here (mirroring the memory FORGET wall).
    statement_scope_guard(wtxn, *id, write)?;
    let reason = TombstoneReason::from_u8(*reason).unwrap_or(TombstoneReason::UserRequest);
    statement_tombstone(wtxn, *id, reason, *at_unix_nanos)
        .map_err(|e| statement_op_err("statement_tombstone", e))?;
    Ok(PhaseAck::Tombstoned {
        target: *target,
        tombstoned_at_unix_nanos: *at_unix_nanos,
    })
}

/// Classify a [`StatementOpError`] for the wire. A missing predicate /
/// subject / statement is the caller pointing at something that is not
/// there, and a tombstoned, superseded or already-existing target is the
/// caller racing committed state — neither can succeed on a retry of the
/// same request. Redb failures retain their retry classification. Everything
/// else is the engine's own fault. A declared subject or object type the write
/// violated, a malformed argument, and a supersede whose kind, subject or
/// predicate does not match its target are the caller's input to fix.
fn statement_op_err(ctx: &'static str, e: StatementOpError) -> ApplyError {
    use brain_metadata::statement::StatementOpError as E;
    let detail = format!("{ctx}: {e}");
    match e {
        E::NotFound(_) => ApplyError::NotFound {
            what: "statement",
            detail,
        },
        E::UnknownPredicate(_) => ApplyError::NotFound {
            what: "predicate",
            detail,
        },
        E::UnknownSubject(_) => ApplyError::NotFound {
            what: "subject entity",
            detail,
        },
        E::UnknownObjectEntity(_) => ApplyError::NotFound {
            what: "entity",
            detail,
        },
        E::AlreadyExists(_)
        | E::AlreadySuperseded(..)
        | E::AlreadyTombstoned(_)
        | E::EventCannotSupersede => ApplyError::Conflict(detail),
        E::Storage(ref se) => ApplyError::Storage {
            fault: se.storage_fault(),
            detail,
        },
        E::ObjectEntityTypeMismatch { .. }
        | E::SubjectEntityTypeMismatch { .. }
        | E::InvalidArgument(_)
        | E::KindMismatch { .. }
        | E::SubjectMismatch
        | E::PredicateMismatch => ApplyError::InvalidRequest(detail),
        _ => ApplyError::Metadata(detail),
    }
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

#[cfg(test)]
mod classifier_tests {
    use super::{statement_op_err, ApplyError};
    use crate::apply::StorageFault;
    use brain_core::{EntityId, EntityTypeId, StatementId, StatementKind};
    use brain_metadata::statement::StatementOpError;

    #[test]
    fn statement_faults_the_caller_caused_are_classified_for_the_wire() {
        assert!(matches!(
            statement_op_err("create", StatementOpError::UnknownPredicate(7)),
            ApplyError::NotFound {
                what: "predicate",
                ..
            }
        ));
        assert!(matches!(
            statement_op_err("create", StatementOpError::UnknownSubject(EntityId::new())),
            ApplyError::NotFound {
                what: "subject entity",
                ..
            }
        ));
        assert!(matches!(
            statement_op_err(
                "tombstone",
                StatementOpError::AlreadyTombstoned(StatementId::new())
            ),
            ApplyError::Conflict(_)
        ));
        assert!(matches!(
            statement_op_err(
                "supersede",
                StatementOpError::AlreadySuperseded(StatementId::new(), StatementId::new())
            ),
            ApplyError::Conflict(_)
        ));
        assert!(matches!(
            statement_op_err("supersede", StatementOpError::EventCannotSupersede),
            ApplyError::Conflict(_)
        ));
        assert!(matches!(
            statement_op_err(
                "create",
                StatementOpError::Storage(redb::StorageError::DatabaseClosed)
            ),
            ApplyError::Storage {
                fault: StorageFault::Transient,
                ..
            }
        ));
    }

    /// Every statement refusal the caller can fix must leave the apply layer
    /// as an invalid request. Flattened into `Metadata` it becomes an engine
    /// fault on the wire, in a category clients retry — against a write that
    /// is refused identically every time.
    #[test]
    fn caller_fixable_statement_faults_are_invalid_requests() {
        let entity = EntityId::from_bytes([0; 16]);
        let cases = [
            StatementOpError::ObjectEntityTypeMismatch {
                entity,
                expected: EntityTypeId::from(1),
                actual: EntityTypeId::from(2),
            },
            StatementOpError::SubjectEntityTypeMismatch {
                entity,
                expected: EntityTypeId::from(1),
                actual: EntityTypeId::from(2),
            },
            StatementOpError::InvalidArgument("confidence out of range"),
            StatementOpError::KindMismatch {
                old: StatementKind::Fact,
                new: StatementKind::Event,
            },
            StatementOpError::SubjectMismatch,
            StatementOpError::PredicateMismatch,
        ];
        for case in cases {
            let rendered = case.to_string();
            let mapped = statement_op_err("statement_supersede", case);
            assert!(
                matches!(mapped, ApplyError::InvalidRequest(_)),
                "{rendered} must classify as an invalid request, got {mapped:?}"
            );
        }
    }
}

#[cfg(all(test, not(miri)))]
mod tenant_wall_tests {
    //! Cross-tenant write isolation for the statement apply path. A
    //! caller in tenant B must not be able to tombstone / retract /
    //! supersede a statement owned by tenant A; the mutation reads as
    //! NotFound and A's row is left untouched.
    use super::*;
    use brain_core::{
        Entity, EntityId, EntityType, EvidenceRef as CoreEvidenceRef, ExtractorId, PredicateId,
        SessionId, StatementId, StatementKind, StatementObject, StatementValue, SubjectRef,
    };
    use brain_metadata::entity::ops::{entity_put, normalize_name};
    use brain_metadata::schema::predicate::predicate_intern;
    use brain_metadata::statement::{statement_create, statement_get};
    use brain_metadata::{MetadataDb, RowScope};
    use tempfile::TempDir;

    use crate::write::{
        Phase, SupersedeReplacement, SupersedeTarget, TombstoneTarget, Write, WriteId,
    };

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn scope_a() -> RowScope {
        RowScope::from_bytes(1, [0xA1; 16])
    }
    fn scope_b() -> RowScope {
        RowScope::from_bytes(2, [0xB2; 16])
    }

    fn open_db() -> (TempDir, MetadataDb) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        (dir, db)
    }

    fn write_for(scope: RowScope, phase: Phase) -> Write {
        Write::single(WriteId::new(), scope.space(), phase).with_namespace(scope.namespace())
    }

    fn make_entity(db: &MetadataDb, scope: RowScope, name: &str) -> EntityId {
        let id = EntityId::new();
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            normalize_name(name),
            NOW,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, scope, SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_fact(db: &MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            DeclScope::base("test"),
            name,
            Some(StatementKind::Fact),
            2,
            1,
            "",
            false,
            NOW,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn fresh_fact(subject: EntityId, predicate: PredicateId, value: &str) -> brain_core::Statement {
        brain_core::Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Value(StatementValue::Text(value.into())),
            0.9,
            CoreEvidenceRef::default(),
            ExtractorId::from(0),
            NOW,
            1,
        )
    }

    /// Seed a current Fact owned by tenant A; returns its id.
    fn seed_a(db: &MetadataDb) -> StatementId {
        let subj = make_entity(db, scope_a(), "ada");
        let pred = intern_fact(db, "knows");
        let s = fresh_fact(subj, pred, "lovelace");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, scope_a(), SessionId::DEFAULT, &s, NOW).unwrap();
        wtxn.commit().unwrap();
        s.id
    }

    fn tombstone_phase(id: StatementId) -> Phase {
        Phase::Tombstone {
            target: TombstoneTarget::Statement(id),
            reason: brain_core::TombstoneReason::UserRequest.as_u8(),
            at_unix_nanos: NOW + 1_000,
        }
    }

    #[test]
    fn cross_tenant_tombstone_denied_and_row_untouched() {
        let (_dir, db) = open_db();
        let id = seed_a(&db);

        let wtxn = db.write_txn().unwrap();
        let err = apply_tombstone_statement(
            &wtxn,
            &tombstone_phase(id),
            &write_for(scope_b(), tombstone_phase(id)),
        )
        .expect_err("tenant B must not tombstone tenant A's statement");
        assert!(matches!(
            err,
            ApplyError::NotFound {
                what: "statement",
                ..
            }
        ));
        drop(wtxn);

        let rtxn = db.read_txn().unwrap();
        let got = statement_get(&rtxn, id).unwrap().expect("A's row present");
        assert!(!got.tombstoned, "A's row must not be tombstoned");
        assert!(
            got.superseded_by.is_none(),
            "A's row must not be superseded"
        );
    }

    #[test]
    fn cross_tenant_retract_denied_and_row_untouched() {
        // Retract shares the tombstone apply path (Retract reason byte).
        let (_dir, db) = open_db();
        let id = seed_a(&db);
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Statement(id),
            reason: brain_core::TombstoneReason::Retract.as_u8(),
            at_unix_nanos: NOW + 1_000,
        };

        let wtxn = db.write_txn().unwrap();
        let err = apply_tombstone_statement(&wtxn, &phase, &write_for(scope_b(), phase.clone()))
            .expect_err("tenant B must not retract tenant A's statement");
        assert!(matches!(
            err,
            ApplyError::NotFound {
                what: "statement",
                ..
            }
        ));
        drop(wtxn);

        let rtxn = db.read_txn().unwrap();
        let got = statement_get(&rtxn, id).unwrap().unwrap();
        assert!(!got.tombstoned);
    }

    #[test]
    fn cross_tenant_supersede_denied_no_row_in_victim_scope() {
        let (_dir, db) = open_db();
        // Old row owned by A; the B caller reuses A's subject/predicate so
        // the only thing standing between B and a cross-tenant write is
        // the wall.
        let subj = make_entity(&db, scope_a(), "grace");
        let pred = intern_fact(&db, "wrote");
        let old = fresh_fact(subj, pred, "cobol");
        {
            let wtxn = db.write_txn().unwrap();
            statement_create(&wtxn, scope_a(), SessionId::DEFAULT, &old, NOW).unwrap();
            wtxn.commit().unwrap();
        }
        let replacement = fresh_fact(subj, pred, "flow-matic");
        let new_id = replacement.id;
        let phase = Phase::Supersede {
            target: SupersedeTarget::Statement(old.id),
            replacement: SupersedeReplacement::Statement(Box::new(replacement)),
            at_unix_nanos: NOW + 1_000,
        };

        let wtxn = db.write_txn().unwrap();
        let err = apply_supersede_statement(&wtxn, &phase, &write_for(scope_b(), phase.clone()))
            .expect_err("tenant B must not supersede tenant A's statement");
        assert!(matches!(
            err,
            ApplyError::NotFound {
                what: "statement",
                ..
            }
        ));
        drop(wtxn);

        let rtxn = db.read_txn().unwrap();
        let old_got = statement_get(&rtxn, old.id).unwrap().unwrap();
        assert!(
            old_got.superseded_by.is_none() && !old_got.tombstoned,
            "A's row must stay current, not superseded"
        );
        assert!(
            statement_get(&rtxn, new_id).unwrap().is_none(),
            "no B-authored replacement may land in A's scope"
        );
    }

    #[test]
    fn same_tenant_tombstone_succeeds() {
        let (_dir, db) = open_db();
        let id = seed_a(&db);

        let wtxn = db.write_txn().unwrap();
        apply_tombstone_statement(
            &wtxn,
            &tombstone_phase(id),
            &write_for(scope_a(), tombstone_phase(id)),
        )
        .expect("same-tenant tombstone must succeed");
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = statement_get(&rtxn, id).unwrap().unwrap();
        assert!(got.tombstoned, "same-tenant tombstone must apply");
    }
}

#[cfg(all(test, not(miri)))]
mod content_dedup_tests {
    //! Re-asserting a byte-identical triple against a cumulative,
    //! non-stateful predicate is idempotent: `statement_create` keeps
    //! the row it already has and writes nothing. The ack has to carry
    //! that pre-existing row's id — acking the phase's own minted id
    //! names a row no transaction ever wrote, and the handler's
    //! post-commit read-back then fails the request.
    use super::*;
    use brain_core::{
        Entity, EntityId, EntityType, ExtractorId, PredicateId, SessionId, StatementId,
        StatementKind, StatementObject, StatementValue, SubjectRef,
    };
    use brain_metadata::entity::ops::{entity_put, normalize_name};
    use brain_metadata::schema::predicate::predicate_intern;
    use brain_metadata::statement::statement_get;
    use brain_metadata::{MetadataDb, RowScope};
    use tempfile::TempDir;

    use crate::write::{EvidenceRefPhase, Phase, PhaseAck, Write, WriteId};

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn scope() -> RowScope {
        RowScope::from_bytes(1, [0xC3; 16])
    }

    /// A fresh phase for the same `(subject, knows, "lovelace")` triple.
    /// Each call mints its own phase id, exactly as a repeated client
    /// request does.
    fn upsert_phase(subject: EntityId, predicate: PredicateId) -> Phase {
        Phase::UpsertStatement {
            id: StatementId::new(),
            kind: StatementKind::Fact,
            session: SessionId::DEFAULT,
            subject: SubjectRef::Entity(subject),
            predicate,
            object: StatementObject::Value(StatementValue::Text("lovelace".into())),
            confidence: 0.9,
            evidence: EvidenceRefPhase::Inline(Vec::new()),
            valid_from_unix_nanos: None,
            extractor: ExtractorId::from(0),
            extracted_at_unix_nanos: NOW,
            schema_version: 1,
            predicate_intern_hint: None,
        }
    }

    fn minted_id(phase: &Phase) -> StatementId {
        match phase {
            Phase::UpsertStatement { id, .. } => *id,
            _ => unreachable!("built as UpsertStatement"),
        }
    }

    #[test]
    fn repeating_an_identical_fact_acks_the_row_that_already_exists() {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        let scope = scope();

        let subject = EntityId::new();
        {
            let wtxn = db.write_txn().unwrap();
            let e = Entity::new_active(
                subject,
                EntityType::PERSON_ID,
                "ada".into(),
                normalize_name("ada"),
                NOW,
            );
            entity_put(&wtxn, scope, SessionId::DEFAULT, &e).unwrap();
            wtxn.commit().unwrap();
        }
        // `is_stateful: false` Fact — a cumulative kind, so nothing
        // auto-supersedes the repeat and content dedup is the only
        // thing that collapses it.
        let predicate = {
            let wtxn = db.write_txn().unwrap();
            let id = predicate_intern(
                &wtxn,
                DeclScope::base("test"),
                "knows",
                Some(StatementKind::Fact),
                2,
                1,
                "",
                false,
                NOW,
            )
            .unwrap();
            wtxn.commit().unwrap();
            id
        };

        let apply = |phase: &Phase| -> StatementId {
            let write = Write::single(WriteId::new(), scope.space(), phase.clone())
                .with_namespace(scope.namespace());
            let wtxn = db.write_txn().unwrap();
            let ack = apply_upsert_statement(&wtxn, phase, &write).expect("apply must succeed");
            wtxn.commit().unwrap();
            match ack {
                PhaseAck::UpsertedStatement(id, _) => id,
                other => panic!("expected an UpsertedStatement ack, got {other:?}"),
            }
        };

        let first_phase = upsert_phase(subject, predicate);
        let second_phase = upsert_phase(subject, predicate);
        assert_ne!(
            minted_id(&first_phase),
            minted_id(&second_phase),
            "the two attempts must mint distinct phase ids"
        );

        let first = apply(&first_phase);
        let second = apply(&second_phase);

        assert_eq!(
            first, second,
            "the repeat must ack the existing row's id, not its own minted id"
        );
        assert_ne!(
            second,
            minted_id(&second_phase),
            "the dedup no-op wrote no row at the second phase's minted id"
        );
        let rtxn = db.read_txn().unwrap();
        assert!(
            statement_get(&rtxn, second).unwrap().is_some(),
            "the acked id must resolve to a stored statement"
        );
    }
}
