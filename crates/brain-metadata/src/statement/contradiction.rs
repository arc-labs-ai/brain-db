//! Fact-vs-Fact contradiction audit operations.
//!
//! Detection runs inside `statement_create` (read-only — the insert
//! always proceeds). When a new Fact disagrees with an active Fact on
//! the same `(subject, predicate)`, [`contradiction_audit_record`]
//! writes/updates a durable row so operators can list and reconcile
//! open contradictions.
//!
//! Liveness is re-checked at list time against the primary `statements`
//! table. The `statements_by_subject` index now enumerates coexisting
//! current Facts (the statement id is part of its key), but the audit
//! row records the specific statement ids directly, so liveness is
//! confirmed by loading each primary row rather than re-scanning the
//! index.

use brain_core::{AuditId, EntityId, PredicateId, StatementId, StatementObject};
use redb::{ReadableTable, WriteTransaction};

use crate::tables::contradiction::{
    contradiction_outcome, ContradictionAudit, STATEMENT_CONTRADICTION_AUDIT_TABLE,
};
use crate::tables::statement::{statement_from_metadata, StatementMetadata, STATEMENTS_TABLE};

use super::StatementOpError;

/// Record (or update) the open contradiction for a `(subject,
/// predicate)`. One row per pair: a re-detection unions the new ids into
/// the existing open row and refreshes the timestamp, preserving the
/// stable audit id. A previously-resolved row is reopened.
pub fn contradiction_audit_record(
    wtxn: &WriteTransaction,
    subject: EntityId,
    predicate: PredicateId,
    contradicting: &[StatementId],
    now_unix_nanos: u64,
) -> Result<(), StatementOpError> {
    let key = (subject.to_bytes(), predicate.raw());
    let mut t = wtxn.open_table(STATEMENT_CONTRADICTION_AUDIT_TABLE)?;
    let existing = t.get(&key)?.map(|g| g.value());
    let (audit_id_bytes, mut ids) = match existing {
        Some(row) if row.outcome == contradiction_outcome::PENDING => {
            (row.audit_id_bytes, row.contradicting_statement_ids)
        }
        // No open row (absent or previously resolved) — start fresh with
        // a new stable id.
        _ => (AuditId::new().to_bytes(), Vec::new()),
    };
    for s in contradicting {
        let b = s.to_bytes();
        if !ids.contains(&b) {
            ids.push(b);
        }
    }
    let row = ContradictionAudit {
        audit_id_bytes,
        subject_bytes: subject.to_bytes(),
        predicate_id: predicate.raw(),
        contradicting_statement_ids: ids,
        detected_at_unix_nanos: now_unix_nanos,
        resolved_at_unix_nanos: None,
        outcome: contradiction_outcome::PENDING,
    };
    t.insert(&key, &row)?;
    Ok(())
}

/// List open contradictions, up to `limit`. Each candidate row is
/// re-checked against the primary `statements` table: ids that are no
/// longer current/untombstoned are pruned, and a row that drops to ≤1
/// distinct object is lazily flipped to `RESOLVED` (and excluded from the
/// result). Pruned/resolved rows are written back in the same txn, so
/// the index self-heals on every list.
pub fn contradiction_audit_list_pending(
    wtxn: &WriteTransaction,
    limit: usize,
    now_unix_nanos: u64,
) -> Result<Vec<ContradictionAudit>, StatementOpError> {
    let mut live: Vec<ContradictionAudit> = Vec::new();
    let mut rewrites: Vec<(([u8; 16], u32), ContradictionAudit)> = Vec::new();
    {
        let t = wtxn.open_table(STATEMENT_CONTRADICTION_AUDIT_TABLE)?;
        let st = wtxn.open_table(STATEMENTS_TABLE)?;
        for entry in t.iter()? {
            let (k, v) = entry?;
            let key = k.value();
            let mut row = v.value();
            if row.outcome != contradiction_outcome::PENDING {
                continue;
            }
            let mut live_ids: Vec<[u8; 16]> = Vec::new();
            let mut distinct_objects: Vec<StatementObject> = Vec::new();
            for id in &row.contradicting_statement_ids {
                let Some(m): Option<StatementMetadata> = st.get(id)?.map(|g| g.value()) else {
                    continue;
                };
                if m.is_tombstoned() || m.is_current == 0 {
                    continue;
                }
                let Some(s) = statement_from_metadata(&m) else {
                    continue;
                };
                live_ids.push(*id);
                if !distinct_objects.contains(&s.object) {
                    distinct_objects.push(s.object);
                }
            }
            row.contradicting_statement_ids = live_ids;
            if distinct_objects.len() >= 2 {
                live.push(row.clone());
                rewrites.push((key, row));
                if live.len() >= limit {
                    break;
                }
            } else {
                row.outcome = contradiction_outcome::RESOLVED;
                row.resolved_at_unix_nanos = Some(now_unix_nanos);
                rewrites.push((key, row));
            }
        }
    }
    {
        let mut t = wtxn.open_table(STATEMENT_CONTRADICTION_AUDIT_TABLE)?;
        for (key, row) in rewrites {
            t.insert(&key, &row)?;
        }
    }
    Ok(live)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::entity::ops::{entity_put, normalize_name};
    use crate::schema::predicate::predicate_intern;
    use crate::statement::crud::statement_create;
    use crate::statement::tombstone::statement_tombstone;
    use brain_core::{
        Entity, EntityType, EvidenceRef, Statement, StatementKind, StatementValue, SubjectRef,
        TombstoneReason,
    };
    use brain_core::{ExtractorId, PredicateId};

    const T0: u64 = 1_700_000_000_000_000_000;

    fn open_db() -> (tempfile::TempDir, crate::MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    fn make_entity(db: &crate::MetadataDb, name: &str) -> EntityId {
        let id = EntityId::new();
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            normalize_name(name),
            T0,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    // Non-stateful Fact predicate so two Facts coexist (no auto-supersede).
    fn intern_fact(db: &crate::MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Fact),
            2,
            1,
            "",
            /* is_stateful */ false,
            T0,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn fact(subject: EntityId, predicate: PredicateId, value: &str) -> Statement {
        Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Value(StatementValue::Text(value.into())),
            0.9,
            EvidenceRef::default(),
            ExtractorId::from(0),
            T0,
            1,
        )
    }

    #[test]
    fn create_records_contradiction_list_then_resolve_on_tombstone() {
        let (_dir, db) = open_db();
        let subj = make_entity(&db, "ada");
        let pred = intern_fact(&db, "favorite_color");

        let a = fact(subj, pred, "blue");
        let b = fact(subj, pred, "green"); // disagrees with a

        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &a, T0).unwrap();
        statement_create(&wtxn, &b, T0).unwrap(); // detects + records contradiction
        wtxn.commit().unwrap();

        // Listed as one pending contradiction over both ids.
        let wtxn = db.write_txn().unwrap();
        let pending = contradiction_audit_list_pending(&wtxn, 16, T0 + 1).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(pending.len(), 1);
        let mut ids = pending[0].contradicting_statement_ids.clone();
        ids.sort_unstable();
        let mut want = vec![a.id.to_bytes(), b.id.to_bytes()];
        want.sort_unstable();
        assert_eq!(ids, want);
        assert_eq!(pending[0].subject_bytes, subj.to_bytes());

        // Tombstone one side -> contradiction resolves -> no longer listed.
        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, a.id, TombstoneReason::UserRequest, T0 + 2).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        let pending = contradiction_audit_list_pending(&wtxn, 16, T0 + 3).unwrap();
        wtxn.commit().unwrap();
        assert!(
            pending.is_empty(),
            "tombstoning one side resolves the contradiction"
        );
    }

    #[test]
    fn agreeing_facts_do_not_record() {
        let (_dir, db) = open_db();
        let subj = make_entity(&db, "bo");
        let pred = intern_fact(&db, "city");
        let a = fact(subj, pred, "paris");
        let b = fact(subj, pred, "paris"); // same object — not a contradiction
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &a, T0).unwrap();
        statement_create(&wtxn, &b, T0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        let pending = contradiction_audit_list_pending(&wtxn, 16, T0 + 1).unwrap();
        wtxn.commit().unwrap();
        assert!(pending.is_empty());
    }
}
