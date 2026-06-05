//! Shared sweeper-side primitives — common discipline (dry-run, batch
//! cap, SweepSummary).
//!
//! Individual sweepers live in `brain-workers::workers::*` and
//! call into this module for the metadata-side scan-and-delete.

use redb::{ReadableTable, WriteTransaction};

use brain_core::{StatementKind, StatementObject};

use crate::statement::evidence::reclaim_evidence_overflow;
use crate::statement::StatementOpError;
use crate::tables::audit::EXTRACTOR_AUDIT_TABLE;
use crate::tables::statement::{
    confidence_bucket, statement_from_metadata, tombstone_reason, StatementMetadata,
    STATEMENTS_BY_EVENT_TIME_TABLE, STATEMENTS_BY_EVIDENCE_TABLE,
    STATEMENTS_BY_OBJECT_ENTITY_TABLE, STATEMENTS_BY_PREDICATE_TABLE, STATEMENTS_BY_SUBJECT_TABLE,
    STATEMENTS_TABLE, STATEMENT_CHAIN_TABLE,
};

/// Shared summary returned by every sweeper.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepSummary {
    pub scanned: u64,
    pub deleted: u64,
    pub dry_run_would_delete: u64,
    pub skipped: u64,
}

// ---------------------------------------------------------------------------
// Supersession sweeper.
// ---------------------------------------------------------------------------

/// Hard-delete superseded statements past `retention_seconds`.
///
/// `retention_seconds == 0` means "disabled" — the caller is
/// expected to short-circuit before invoking, but we double-check.
pub fn sweep_superseded_statements(
    wtxn: &WriteTransaction,
    retention_seconds: u64,
    now_unix_nanos: u64,
    batch_cap: usize,
    dry_run: bool,
) -> Result<SweepSummary, StatementOpError> {
    let mut summary = SweepSummary::default();
    if retention_seconds == 0 {
        return Ok(summary);
    }
    let cutoff_ns = now_unix_nanos.saturating_sub(retention_seconds * 1_000_000_000);

    // Collect victim ids first (statements with `superseded_by` set
    // AND retired earlier than cutoff). Two-phase scan keeps the
    // scan-side immutable.
    //
    // Retired-at proxy: `valid_to_unix_nanos`. `statement_supersede`
    // sets `valid_to_unix_nanos = Some(new.extracted_at_unix_nanos)` on
    // the old row when its existing valid_to was None — which is
    // the universal case for supersession-driven retirement. Events
    // cannot be superseded, so their None valid_to here is correct:
    // the loop skips them via the `superseded_by_bytes.is_none()`
    // guard before this check.
    //
    // Operators who explicitly set a Statement's `valid_to` in the
    // future and THEN supersede it will see this preserved
    // valid_to drive the sweeper's cutoff — i.e. the sweeper waits
    // until the operator-declared end-of-validity. That matches
    // the user's intent (the row is "valid until X"; sweep when
    // retention past X expires).
    let victims: Vec<[u8; 16]> = {
        let table = wtxn.open_table(STATEMENTS_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v) = entry?;
            let row = v.value();
            summary.scanned += 1;
            if row.superseded_by_bytes.is_none() {
                continue;
            }
            let Some(retired_at) = row.valid_to_unix_nanos else {
                continue;
            };
            if retired_at > cutoff_ns {
                continue;
            }
            out.push(row.statement_id_bytes);
            if out.len() == batch_cap {
                break;
            }
        }
        out
    };

    if dry_run {
        summary.dry_run_would_delete = victims.len() as u64;
    } else {
        let mut t = wtxn.open_table(STATEMENTS_TABLE)?;
        for key in &victims {
            t.remove(key)?;
            summary.deleted += 1;
        }
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Retract reclamation sweeper.
// ---------------------------------------------------------------------------

/// Physically reclaim retracted statement rows past the grace period.
///
/// Eligibility for a row is three conjuncts:
/// 1. it is tombstoned, and
/// 2. its `tombstone_reason` is [`tombstone_reason::RETRACT`] — the
///    durable marker the retract path stamps, distinguishing a hard
///    retract from a soft tombstone (kept for audit) and from a
///    superseded row (kept forever for chain history), and
/// 3. `now - tombstoned_at >= grace_nanos`.
///
/// On reclaim the row is removed from every table it touches:
/// `STATEMENTS_TABLE`, both `STATEMENTS_BY_SUBJECT_TABLE` current-bit
/// keys, `STATEMENTS_BY_PREDICATE_TABLE`, `STATEMENTS_BY_OBJECT_ENTITY_TABLE`
/// (object-Entity only), `STATEMENTS_BY_EVENT_TIME_TABLE` (Event only),
/// `STATEMENTS_BY_EVIDENCE_TABLE` (every evidence memory), and the
/// `EVIDENCE_OVERFLOW_TABLE` row when evidence overflowed inline.
///
/// **Chain invariant.** `STATEMENT_CHAIN_TABLE` requires a dense
/// `1..=N` version range with no gaps. Removing a *mid-chain* row
/// (one with `superseded_by` set, i.e. not the chain tail) would punch
/// a hole, so its chain entry is **kept** as a tombstone and only the
/// non-chain rows are reclaimed. A chain-tail or standalone row
/// (`superseded_by.is_none()`) is the highest version, so removing its
/// chain entry keeps `1..=N-1` dense — that entry is removed.
///
/// Two-phase to stay TOCTOU-safe: collect candidate ids under an
/// immutable scan (bounded by `batch_cap`), then re-check each id under
/// the same write txn before deleting — a row that stopped qualifying
/// between scan and delete (e.g. grace clock skew) is skipped.
///
/// `grace_nanos == 0` is treated as "disabled" and returns an empty
/// summary; the caller is expected to short-circuit but this
/// double-checks. `dry_run` computes `dry_run_would_delete` without any
/// mutation.
///
/// **No audit row at reclamation.** The retract event is already durably
/// recorded at retract time (the WAL `StatementTombstone` record carries
/// the id, the `Retract` reason, and the timestamp, and a
/// `StatementTombstoned` graph event is emitted). Reclamation is the
/// idempotent physical cleanup of an already-audited decision, so it
/// writes no separate row — do not re-add one against
/// `entity_resolution_audit` (that table is the entity-resolution log and
/// has no statement-lifecycle discriminator).
pub fn reclaim_retracted_statements(
    wtxn: &WriteTransaction,
    grace_nanos: u64,
    now_unix_nanos: u64,
    batch_cap: usize,
    dry_run: bool,
) -> Result<SweepSummary, StatementOpError> {
    let mut summary = SweepSummary::default();
    if grace_nanos == 0 {
        return Ok(summary);
    }
    let cutoff_ns = now_unix_nanos.saturating_sub(grace_nanos);

    // Phase 1: collect victim ids under an immutable scan.
    let victims: Vec<[u8; 16]> = {
        let table = wtxn.open_table(STATEMENTS_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v) = entry?;
            let row = v.value();
            summary.scanned += 1;
            if !is_reclaimable(&row, cutoff_ns) {
                continue;
            }
            out.push(row.statement_id_bytes);
            if out.len() == batch_cap {
                break;
            }
        }
        out
    };

    if dry_run {
        summary.dry_run_would_delete = victims.len() as u64;
        return Ok(summary);
    }

    // Phase 2: re-check + delete each id in the same write txn.
    for key in &victims {
        let row = {
            let t = wtxn.open_table(STATEMENTS_TABLE)?;
            let guard = t.get(key)?;
            guard.map(|g| g.value())
        };
        let Some(row) = row else {
            // Vanished between scan and now (another writer / replay).
            summary.skipped += 1;
            continue;
        };
        if !is_reclaimable(&row, cutoff_ns) {
            summary.skipped += 1;
            continue;
        }
        reclaim_one(wtxn, &row)?;
        summary.deleted += 1;
    }
    Ok(summary)
}

/// True iff a row is a retract past its grace cutoff.
fn is_reclaimable(row: &StatementMetadata, cutoff_ns: u64) -> bool {
    if !row.is_tombstoned() {
        return false;
    }
    if row.tombstone_reason != tombstone_reason::RETRACT {
        return false;
    }
    match row.tombstoned_at_unix_nanos {
        Some(at) => at <= cutoff_ns,
        // A retract row with no tombstoned_at is malformed; leave it
        // for inspection rather than reclaiming on a zero timestamp.
        None => false,
    }
}

/// Remove one retracted row from every table it touches, honouring the
/// dense-chain invariant for mid-chain rows.
fn reclaim_one(wtxn: &WriteTransaction, row: &StatementMetadata) -> Result<(), StatementOpError> {
    let id_bytes = row.statement_id_bytes;

    // 1. Primary row.
    {
        let mut t = wtxn.open_table(STATEMENTS_TABLE)?;
        t.remove(&id_bytes)?;
    }

    // 2. by_subject — only for resolved-entity subjects. Tombstoning
    // flips the row to is_current=0, but defensively remove both
    // current-bit keys so a row reclaimed before the flip committed
    // still leaves no orphan.
    if row.subject_is_pending == 0 {
        let mut t = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
        t.remove(&(row.subject_entity_bytes, row.kind, row.predicate_id, 0u8))?;
        t.remove(&(row.subject_entity_bytes, row.kind, row.predicate_id, 1u8))?;
    }

    // 3. by_predicate.
    {
        let mut t = wtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE)?;
        t.remove(&(
            row.predicate_id,
            row.kind,
            confidence_bucket(row.confidence),
        ))?;
    }

    // 4. by_object_entity — only when the object is an Entity.
    // 6. by_evidence — one row per evidence memory (inline + overflow).
    // 8. evidence_overflow row.
    // All three need the decoded brain-core view of the row.
    if let Some(stmt) = statement_from_metadata(row) {
        if let StatementObject::Entity(eid) = &stmt.object {
            let mut t = wtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE)?;
            t.remove(&(eid.to_bytes(), row.kind))?;
        }

        // Collect every evidence memory id (inline entries directly,
        // overflow entries resolved from the overflow row) so the
        // reverse-evidence index is fully stripped.
        let evidence_memory_ids: Vec<[u8; 16]> = match &stmt.evidence {
            brain_core::EvidenceRef::Inline(entries) => {
                entries.iter().map(|e| e.memory_id.to_be_bytes()).collect()
            }
            brain_core::EvidenceRef::Overflow(_) => overflow_memory_ids(wtxn, &stmt.evidence)?,
        };
        {
            let mut t = wtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE)?;
            for mid in evidence_memory_ids {
                t.remove(&(mid, id_bytes))?;
            }
        }

        reclaim_evidence_overflow(wtxn, &stmt.evidence)?;
    }

    // 5. by_event_time — only for Events.
    if row.kind == StatementKind::Event.as_u8() {
        if let Some(event_at) = row.event_at_unix_nanos {
            let mut t = wtxn.open_table(STATEMENTS_BY_EVENT_TIME_TABLE)?;
            t.remove(&(event_at, row.subject_entity_bytes))?;
        }
    }

    // 7. chain — keep the entry for a mid-chain row (its version sits
    // inside a dense 1..=N range; removing it punches a hole). A chain
    // tail / standalone row has nothing superseding it, so its version
    // is the highest — removing it keeps 1..=N-1 dense.
    let is_mid_chain = row.superseded_by_bytes.is_some();
    if !is_mid_chain {
        let mut t = wtxn.open_table(STATEMENT_CHAIN_TABLE)?;
        t.remove(&(row.chain_root_bytes, row.version))?;
    }

    Ok(())
}

/// Resolve the evidence-memory ids backing an `EvidenceRef::Overflow`.
/// Returns an empty vec for inline refs or a missing overflow row.
fn overflow_memory_ids(
    wtxn: &WriteTransaction,
    reference: &brain_core::EvidenceRef,
) -> Result<Vec<[u8; 16]>, StatementOpError> {
    use crate::tables::statement::{EvidenceOverflow, EVIDENCE_OVERFLOW_TABLE};
    let brain_core::EvidenceRef::Overflow(id) = reference else {
        return Ok(Vec::new());
    };
    let t = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
    let row: Option<EvidenceOverflow> = t.get(&id.to_bytes())?.map(|g| g.value());
    Ok(row.map(|o| o.memory_ids).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Audit log sweeper.
// ---------------------------------------------------------------------------

/// Hard-delete audit rows older than `retention_seconds`. Merge/Unmerge
/// audit rows are exempt (kept forever) — the audit table stores
/// extraction events only, so the merge-exemption is a no-op until
/// merge audits land on this table.
pub fn sweep_audit_log(
    wtxn: &WriteTransaction,
    retention_seconds: u64,
    now_unix_nanos: u64,
    batch_cap: usize,
    dry_run: bool,
) -> Result<SweepSummary, redb::Error> {
    let mut summary = SweepSummary::default();
    if retention_seconds == 0 {
        return Ok(summary);
    }
    let cutoff_ns = now_unix_nanos.saturating_sub(retention_seconds * 1_000_000_000);

    let victims: Vec<[u8; 16]> = {
        let table = wtxn.open_table(EXTRACTOR_AUDIT_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            let row = v.value();
            summary.scanned += 1;
            if row.started_at_unix_nanos > cutoff_ns {
                continue;
            }
            out.push(k.value());
            if out.len() == batch_cap {
                break;
            }
        }
        out
    };

    if dry_run {
        summary.dry_run_would_delete = victims.len() as u64;
    } else {
        let mut t = wtxn.open_table(EXTRACTOR_AUDIT_TABLE)?;
        for key in &victims {
            t.remove(key)?;
            summary.deleted += 1;
        }
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Stale extraction detector.
// ---------------------------------------------------------------------------

/// Stale-extraction flag bit on `StatementMetadata` is not yet
/// in the row layout. v1 surfaces staleness via row inspection
/// at query time (cheap: schema_version comparison). The
/// dedicated flag bit lands as a post-v1 schema bump.
///
/// This sweeper enumerates statements whose `schema_version` is
/// behind the current value and returns the count. The flag-write
/// side is deferred; admin / query layer can consult the same
/// predicate on-demand.
pub fn scan_stale_statements(
    rtxn: &redb::ReadTransaction,
    current_schema_version: u32,
    batch_cap: usize,
) -> Result<SweepSummary, StatementOpError> {
    let mut summary = SweepSummary::default();
    let table = rtxn.open_table(STATEMENTS_TABLE)?;
    for entry in table.iter()? {
        let (_, v) = entry?;
        let row = v.value();
        summary.scanned += 1;
        if row.tombstoned != 0 {
            continue;
        }
        if row.schema_version < current_schema_version {
            // Count under `dry_run_would_delete` to reuse the field.
            // A future schema bump adds STATEMENT_FLAG_STALE_EXTRACTION
            // and converts this to a real mutation.
            summary.dry_run_would_delete += 1;
        }
        if summary.scanned >= batch_cap as u64 {
            break;
        }
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Tests — retract reclamation.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod reclaim_tests {
    use super::*;
    use crate::entity::ops::{entity_put, normalize_name};
    use crate::schema::predicate::predicate_intern;
    use crate::statement::crud::{statement_create, statement_get};
    use crate::statement::tombstone::{statement_retract, statement_tombstone};
    use brain_core::{
        Entity, EntityId, EntityType, EvidenceEntry, EvidenceRef, ExtractorId, MemoryId,
        PredicateId, Statement, StatementObject, SubjectRef, TombstoneReason, INLINE_EVIDENCE_CAP,
    };
    use smallvec::SmallVec;

    const T0: u64 = 1_700_000_000_000_000_000;
    const GRACE: u64 = 30 * 24 * 60 * 60 * 1_000_000_000;

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
            T0,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_fact(db: &mut crate::MetadataDb, name: &str, stateful: bool) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Fact),
            1, // object: Entity
            1,
            "",
            stateful,
            T0,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn fresh_fact(subject: EntityId, predicate: PredicateId, object: EntityId) -> Statement {
        Statement::new_root(
            brain_core::StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Entity(object),
            0.9,
            EvidenceRef::default(),
            ExtractorId::from(0),
            T0,
            1,
        )
    }

    /// Create a Fact and retract it at `T0`, returning its id.
    fn create_and_retract(db: &mut crate::MetadataDb, pred: &str) -> brain_core::StatementId {
        let subj = make_entity(db, &format!("subj-{pred}"));
        let obj = make_entity(db, &format!("obj-{pred}"));
        let p = intern_fact(db, pred, false);
        let s = fresh_fact(subj, p, obj);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, T0).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        statement_retract(&wtxn, s.id, TombstoneReason::Retract, T0).unwrap();
        wtxn.commit().unwrap();
        s.id
    }

    #[test]
    fn grace_not_elapsed_is_skipped() {
        let (_d, mut db) = open_db();
        let id = create_and_retract(&mut db, "p_grace");
        // now is exactly at the cutoff boundary minus 1ns → not elapsed.
        let now = T0 + GRACE - 1;
        let wtxn = db.write_txn().unwrap();
        let summary = reclaim_retracted_statements(&wtxn, GRACE, now, 256, false).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(summary.deleted, 0);
        let rtxn = db.read_txn().unwrap();
        assert!(statement_get(&rtxn, id).unwrap().is_some());
    }

    #[test]
    fn grace_elapsed_is_deleted() {
        let (_d, mut db) = open_db();
        let id = create_and_retract(&mut db, "p_elapsed");
        let now = T0 + GRACE;
        let wtxn = db.write_txn().unwrap();
        let summary = reclaim_retracted_statements(&wtxn, GRACE, now, 256, false).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(summary.deleted, 1);
        let rtxn = db.read_txn().unwrap();
        assert!(statement_get(&rtxn, id).unwrap().is_none());
    }

    #[test]
    fn plain_tombstone_is_not_reclaimed() {
        let (_d, mut db) = open_db();
        let subj = make_entity(&mut db, "subj-tomb");
        let obj = make_entity(&mut db, "obj-tomb");
        let p = intern_fact(&mut db, "p_tomb", false);
        let s = fresh_fact(subj, p, obj);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, T0).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, s.id, TombstoneReason::UserRequest, T0).unwrap();
        wtxn.commit().unwrap();

        let now = T0 + GRACE * 10;
        let wtxn = db.write_txn().unwrap();
        let summary = reclaim_retracted_statements(&wtxn, GRACE, now, 256, false).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(summary.deleted, 0);
        let rtxn = db.read_txn().unwrap();
        assert!(statement_get(&rtxn, s.id).unwrap().is_some());
    }

    #[test]
    fn superseded_not_retracted_is_not_reclaimed() {
        let (_d, mut db) = open_db();
        let subj = make_entity(&mut db, "subj-sup");
        let o1 = make_entity(&mut db, "o1-sup");
        let o2 = make_entity(&mut db, "o2-sup");
        let p = intern_fact(&mut db, "p_sup", true);
        let f1 = fresh_fact(subj, p, o1);
        let f2 = fresh_fact(subj, p, o2);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &f1, T0).unwrap();
        wtxn.commit().unwrap();
        // f2 auto-supersedes f1 (stateful predicate).
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &f2, T0).unwrap();
        wtxn.commit().unwrap();

        let now = T0 + GRACE * 10;
        let wtxn = db.write_txn().unwrap();
        let summary = reclaim_retracted_statements(&wtxn, GRACE, now, 256, false).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(summary.deleted, 0);
        let rtxn = db.read_txn().unwrap();
        // f1 is superseded (not retracted) → kept forever.
        assert!(statement_get(&rtxn, f1.id).unwrap().is_some());
        assert!(statement_get(&rtxn, f2.id).unwrap().is_some());
    }

    #[test]
    fn dry_run_counts_without_mutating() {
        let (_d, mut db) = open_db();
        let id = create_and_retract(&mut db, "p_dry");
        let now = T0 + GRACE;
        let wtxn = db.write_txn().unwrap();
        let summary = reclaim_retracted_statements(&wtxn, GRACE, now, 256, true).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(summary.dry_run_would_delete, 1);
        assert_eq!(summary.deleted, 0);
        let rtxn = db.read_txn().unwrap();
        assert!(
            statement_get(&rtxn, id).unwrap().is_some(),
            "dry run must not delete"
        );
    }

    #[test]
    fn batch_cap_is_honored() {
        let (_d, mut db) = open_db();
        create_and_retract(&mut db, "p_cap_a");
        create_and_retract(&mut db, "p_cap_b");
        create_and_retract(&mut db, "p_cap_c");
        let now = T0 + GRACE;
        let wtxn = db.write_txn().unwrap();
        let summary = reclaim_retracted_statements(&wtxn, GRACE, now, 2, false).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(summary.deleted, 2, "batch cap of 2 limits the delete count");
    }

    #[test]
    fn all_applicable_tables_cleaned() {
        let (_d, mut db) = open_db();
        let subj = make_entity(&mut db, "subj-tables");
        let obj = make_entity(&mut db, "obj-tables");
        let p = intern_fact(&mut db, "p_tables", false);
        let mem = MemoryId::pack(7, brain_core::ContextId::DEFAULT.into(), 0);
        let mut s = fresh_fact(subj, p, obj);
        let mut sv = SmallVec::<[EvidenceEntry; INLINE_EVIDENCE_CAP]>::new();
        sv.push(EvidenceEntry::from_parts(
            mem,
            0.8,
            T0,
            ExtractorId::from(0),
        ));
        s.evidence = EvidenceRef::Inline(Box::new(sv));

        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, T0).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        statement_retract(&wtxn, s.id, TombstoneReason::Retract, T0).unwrap();
        wtxn.commit().unwrap();

        let now = T0 + GRACE;
        let wtxn = db.write_txn().unwrap();
        reclaim_retracted_statements(&wtxn, GRACE, now, 256, false).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        // Primary gone.
        let prim = rtxn.open_table(STATEMENTS_TABLE).unwrap();
        assert!(prim.get(&s.id.to_bytes()).unwrap().is_none());
        // by_subject (both bits) gone.
        let bys = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE).unwrap();
        assert!(bys
            .get(&(subj.to_bytes(), StatementKind::Fact.as_u8(), p.raw(), 0u8))
            .unwrap()
            .is_none());
        assert!(bys
            .get(&(subj.to_bytes(), StatementKind::Fact.as_u8(), p.raw(), 1u8))
            .unwrap()
            .is_none());
        // by_object_entity gone.
        let byo = rtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE).unwrap();
        assert!(byo
            .get(&(obj.to_bytes(), StatementKind::Fact.as_u8()))
            .unwrap()
            .is_none());
        // by_evidence gone.
        let bye = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).unwrap();
        assert!(bye
            .get(&(mem.to_be_bytes(), s.id.to_bytes()))
            .unwrap()
            .is_none());
        // chain gone (standalone → tail).
        let chain = rtxn.open_table(STATEMENT_CHAIN_TABLE).unwrap();
        assert!(chain
            .get(&(s.chain_root.to_bytes(), 1u32))
            .unwrap()
            .is_none());
    }

    #[test]
    fn mid_chain_retract_keeps_chain_entry_tail_removes_it() {
        // Build a 2-version chain (f1 → f2 via stateful supersession),
        // retract the mid-chain row f1 and the tail f2, reclaim, and
        // assert: f1's chain entry (version 1) survives as a tombstone
        // while f2's chain entry (version 2, the tail) is removed.
        let (_d, mut db) = open_db();
        let subj = make_entity(&mut db, "subj-chain");
        let o1 = make_entity(&mut db, "o1-chain");
        let o2 = make_entity(&mut db, "o2-chain");
        let p = intern_fact(&mut db, "p_chain", true);
        let f1 = fresh_fact(subj, p, o1);
        let f2 = fresh_fact(subj, p, o2);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &f1, T0).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        // f2 auto-supersedes f1 → f1 mid-chain, f2 tail.
        statement_create(&wtxn, &f2, T0).unwrap();
        wtxn.commit().unwrap();

        // Confirm chain shape before retract.
        let chain_root = {
            let rtxn = db.read_txn().unwrap();
            statement_get(&rtxn, f2.id).unwrap().unwrap().chain_root
        };

        // Retract both members.
        let wtxn = db.write_txn().unwrap();
        statement_retract(&wtxn, f1.id, TombstoneReason::Retract, T0).unwrap();
        statement_retract(&wtxn, f2.id, TombstoneReason::Retract, T0).unwrap();
        wtxn.commit().unwrap();

        let now = T0 + GRACE;
        let wtxn = db.write_txn().unwrap();
        let summary = reclaim_retracted_statements(&wtxn, GRACE, now, 256, false).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(summary.deleted, 2);

        let rtxn = db.read_txn().unwrap();
        // Both primary rows gone.
        assert!(statement_get(&rtxn, f1.id).unwrap().is_none());
        assert!(statement_get(&rtxn, f2.id).unwrap().is_none());
        let chain = rtxn.open_table(STATEMENT_CHAIN_TABLE).unwrap();
        // Mid-chain version 1 entry KEPT (tombstone) to preserve dense
        // 1..=N; tail version 2 entry REMOVED.
        assert!(
            chain.get(&(chain_root.to_bytes(), 1u32)).unwrap().is_some(),
            "mid-chain entry must survive as a tombstone"
        );
        assert!(
            chain.get(&(chain_root.to_bytes(), 2u32)).unwrap().is_none(),
            "chain-tail entry must be removed"
        );
    }

    #[test]
    fn disabled_grace_is_noop() {
        let (_d, mut db) = open_db();
        let id = create_and_retract(&mut db, "p_disabled");
        let wtxn = db.write_txn().unwrap();
        let summary = reclaim_retracted_statements(&wtxn, 0, T0 + GRACE * 100, 256, false).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(summary.deleted, 0);
        assert_eq!(summary.scanned, 0);
        let rtxn = db.read_txn().unwrap();
        assert!(statement_get(&rtxn, id).unwrap().is_some());
    }
}
