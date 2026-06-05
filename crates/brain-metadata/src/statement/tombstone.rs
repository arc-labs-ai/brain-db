//! Soft delete (`statement_tombstone`) and v1 retract intent.
//!
//! Physical reclamation lives in [`crate::extractor::sweep`].

use brain_core::StatementId;
use brain_core::TombstoneReason;
use redb::{ReadableTable, WriteTransaction};

use crate::tables::statement::{
    StatementMetadata, STATEMENTS_TABLE, STATEMENT_EMBED_QUEUE_TABLE,
};

use super::StatementOpError;

/// Soft delete. Sets `tombstoned / tombstoned_at / tombstone_reason`
/// and flips the by-subject `is_current` bit. Re-tombstoning an
/// already-tombstoned row is a no-op (returns `Ok`).
pub fn statement_tombstone(
    wtxn: &WriteTransaction,
    id: StatementId,
    reason: TombstoneReason,
    now_unix_nanos: u64,
) -> Result<(), StatementOpError> {
    let mut row = {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        let r: Option<StatementMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
        r.ok_or(StatementOpError::NotFound(id))?
    };
    if row.is_tombstoned() {
        return Ok(());
    }
    let was_current = row.is_current != 0;
    let subject_bytes = row.subject_entity_bytes;
    let kind_byte = row.kind;
    let pred = row.predicate_id;

    row.tombstoned = 1;
    row.tombstoned_at_unix_nanos = Some(now_unix_nanos);
    row.tombstone_reason = reason.as_u8();
    // Record-time invalidation: tombstoning is the moment the substrate
    // stops recording the row as truth. Always stamp — callers that
    // pass `0` get a zero-stamped row (reads as epoch-time at as-of
    // queries, but a tombstoned row is still tombstoned at any later
    // as_of).
    row.record_invalidated_at_unix_nanos = Some(now_unix_nanos);
    row.is_current = 0;

    {
        let mut t = wtxn.open_table(STATEMENTS_TABLE)?;
        t.insert(&row.statement_id_bytes, &row)?;
    }
    if was_current {
        super::crud::flip_by_subject_to_noncurrent(
            wtxn,
            subject_bytes,
            kind_byte,
            pred,
            &row.statement_id_bytes,
        )?;
        // A tombstoned row is no longer current — drop its
        // predicate-bucket entry so predicate-anchored queries stop
        // returning it. Ownership-guarded (a superseded row's entry is
        // already gone; this is then a no-op).
        super::remove_from_predicate_index(
            wtxn,
            pred,
            kind_byte,
            row.confidence,
            &row.statement_id_bytes,
        )?;
    }
    // Drop any pending embed-queue row — a tombstoned statement does
    // not belong in the Statement HNSW. The worker filters tombstoned
    // rows defensively too, but removing here keeps the queue small
    // and avoids paying an embedding cost on a doomed row.
    {
        let mut q = wtxn.open_table(STATEMENT_EMBED_QUEUE_TABLE)?;
        q.remove(&row.statement_id_bytes)?;
    }
    Ok(())
}

/// Hard-delete intent. Tombstones the row and stamps the durable
/// [`TombstoneReason::Retract`] marker so the periodic reclamation GC
/// worker ([`crate::extractor::sweep::reclaim_retracted_statements`])
/// physically removes it from every table after the grace period. The
/// caller's `reason` is ignored for the stored byte — retract is its
/// own reason — but kept in the signature for call-site symmetry with
/// [`statement_tombstone`]; pass the audit reason for any out-of-band
/// logging the caller does.
pub fn statement_retract(
    wtxn: &WriteTransaction,
    id: StatementId,
    _reason: TombstoneReason,
    now_unix_nanos: u64,
) -> Result<(), StatementOpError> {
    statement_tombstone(wtxn, id, TombstoneReason::Retract, now_unix_nanos)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::entity::ops::{entity_put, normalize_name};
    use crate::schema::predicate::predicate_intern;
    use crate::statement::crud::{statement_create, statement_get};
    use brain_core::{
        Entity, EntityType, EvidenceRef, Statement, StatementKind, StatementObject, StatementValue,
        SubjectRef,
    };
    use brain_core::{EntityId, ExtractorId, PredicateId};

    fn open_db() -> (tempfile::TempDir, crate::MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    fn make_entity(db: &mut crate::MetadataDb, name: &str) -> EntityId {
        let id = EntityId::new();
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.to_string(),
            normalize_name(name),
            1_700_000_000_000_000_000,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_fact(db: &mut crate::MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Fact),
            2,
            1,
            "",
            false,
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn fresh_fact(subject: EntityId, predicate: PredicateId, value: &str) -> Statement {
        Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Value(StatementValue::Text(value.into())),
            0.9,
            EvidenceRef::default(),
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        )
    }

    #[test]
    fn tombstone_stamps_record_invalidated_at() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "ada");
        let pred = intern_fact(&mut db, "knows");
        let s = fresh_fact(subj, pred, "lovelace");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, 1_700_000_000_000_000_000).unwrap();
        wtxn.commit().unwrap();

        let tomb_now: u64 = 1_700_000_000_000_000_750;
        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, s.id, TombstoneReason::UserRequest, tomb_now).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = statement_get(&rtxn, s.id).unwrap().unwrap();
        assert!(got.tombstoned);
        assert_eq!(got.tombstoned_at_unix_nanos, Some(tomb_now));
        assert_eq!(got.record_invalidated_at_unix_nanos, Some(tomb_now));
    }

    #[test]
    fn tombstone_removes_predicate_bucket_entry() {
        use crate::tables::statement::{confidence_bucket, STATEMENTS_BY_PREDICATE_TABLE};
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "ada-byp");
        let pred = intern_fact(&mut db, "byp");
        let s = fresh_fact(subj, pred, "v"); // confidence 0.9 -> bucket 9
        let bucket = confidence_bucket(0.9);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, 1_700_000_000_000_000_000).unwrap();
        wtxn.commit().unwrap();

        // Live row has a predicate-bucket entry pointing at it.
        {
            let rtxn = db.read_txn().unwrap();
            let t = rtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE).unwrap();
            let got = t.get(&(pred.raw(), StatementKind::Fact.as_u8(), bucket)).unwrap();
            assert_eq!(got.map(|g| g.value()), Some(s.id.to_bytes()));
        }

        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, s.id, TombstoneReason::UserRequest, 1_700_000_000_000_000_500)
            .unwrap();
        wtxn.commit().unwrap();

        // Tombstoned row is gone from the predicate-bucket index.
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE).unwrap();
        let got = t.get(&(pred.raw(), StatementKind::Fact.as_u8(), bucket)).unwrap();
        assert!(got.is_none(), "tombstone must remove the predicate-bucket entry");
    }

    #[test]
    fn double_tombstone_keeps_first_invalidation_timestamp() {
        // Re-tombstoning is a no-op (early return), so the
        // `record_invalidated_at` stays at the first call's wall-clock.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "ada-double");
        let pred = intern_fact(&mut db, "knows_double");
        let s = fresh_fact(subj, pred, "lovelace");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, 1_700_000_000_000_000_000).unwrap();
        wtxn.commit().unwrap();

        let first: u64 = 1_700_000_000_000_000_500;
        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, s.id, TombstoneReason::UserRequest, first).unwrap();
        wtxn.commit().unwrap();

        let later: u64 = 1_700_000_000_000_001_500;
        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, s.id, TombstoneReason::UserRequest, later).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = statement_get(&rtxn, s.id).unwrap().unwrap();
        assert_eq!(got.record_invalidated_at_unix_nanos, Some(first));
    }
}
