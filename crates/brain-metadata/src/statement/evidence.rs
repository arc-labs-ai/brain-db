//! Evidence packing, reading, and overflow reclamation.
//!
//! Statements carry their evidence as an [`EvidenceRef`]: up to
//! [`INLINE_EVIDENCE_CAP`] (8) entries stored inline on the
//! `StatementMetadata` row, and the rest spilled to a row in
//! [`EVIDENCE_OVERFLOW_TABLE`] keyed by an [`EvidenceOverflowId`].
//!
//! This module exposes three operations callers compose against either
//! a `ReadTransaction` or `WriteTransaction`:
//!
//! - [`pack_evidence_ids`] / [`pack_evidence_entries`] — write helper.
//!   Hand it the full evidence list; it returns an inline `EvidenceRef`
//!   when it fits and allocates an overflow row when it doesn't.
//! - [`read_evidence_ids`] / [`read_evidence_ids_w`] — read helper.
//!   Transparently resolves inline and overflow into a flat
//!   `Vec<MemoryId>`.
//! - [`reclaim_evidence_overflow`] — cleanup hook. Drops the overflow
//!   row when an `EvidenceRef::Overflow` is no longer referenced
//!   (statement tombstoned + reclaimed, or evidence list shrunk back
//!   inside the inline cap).
//!
//! ## Inline-vs-overflow exclusivity
//!
//! When the chosen representation is overflow, the inline list on the
//! storage row is empty and the overflow row carries the full list.
//! `metadata_from_statement` / `statement_from_metadata` enforce this
//! by construction. Mixed encodings (some entries inline + some in
//! overflow) are not supported — callers either own one or the other.
//!
//! ## Re-spill behaviour
//!
//! [`pack_evidence_entries`] always allocates a fresh
//! [`EvidenceOverflowId`] when it spills. Callers that want to extend
//! an existing overflow (the future `STATEMENT_ADD_EVIDENCE` op) read
//! the prior list with [`read_evidence_ids_w`], append, reclaim the old
//! row with [`reclaim_evidence_overflow`], then call
//! [`pack_evidence_entries`] again with the merged list.

use brain_core::{
    EvidenceEntry, EvidenceOverflowId, EvidenceRef, ExtractorId, MemoryId, INLINE_EVIDENCE_CAP,
};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};
use smallvec::SmallVec;

use crate::tables::statement::{EvidenceOverflow, EVIDENCE_OVERFLOW_TABLE};

use super::StatementOpError;

// ---------------------------------------------------------------------------
// Write helpers.
// ---------------------------------------------------------------------------

/// Pack the supplied memory ids into an [`EvidenceRef`], spilling to
/// [`EVIDENCE_OVERFLOW_TABLE`] when the list exceeds
/// [`INLINE_EVIDENCE_CAP`].
///
/// `confidence`, `now_unix_nanos`, and `extractor_id` populate every
/// resulting [`EvidenceEntry`] — the call site is responsible for
/// per-entry metadata when entries truly differ (use
/// [`pack_evidence_entries`] in that case).
///
/// Returns the prepared [`EvidenceRef`]; the caller stamps it onto a
/// [`brain_core::Statement`] before passing the statement to
/// [`crate::statement::statement_create`].
pub fn pack_evidence_ids(
    wtxn: &WriteTransaction,
    ids: Vec<MemoryId>,
    confidence: f32,
    now_unix_nanos: u64,
    extractor_id: ExtractorId,
) -> Result<EvidenceRef, StatementOpError> {
    let entries: Vec<EvidenceEntry> = ids
        .into_iter()
        .map(|m| EvidenceEntry::from_parts(m, confidence, now_unix_nanos, extractor_id))
        .collect();
    pack_evidence_entries(wtxn, entries, now_unix_nanos)
}

/// Pack the supplied evidence entries into an [`EvidenceRef`], spilling
/// to [`EVIDENCE_OVERFLOW_TABLE`] when the list exceeds
/// [`INLINE_EVIDENCE_CAP`].
///
/// Empty input returns [`EvidenceRef::default`] (an empty inline list).
/// Lists at-or-below the inline cap land as
/// [`EvidenceRef::Inline`]; longer lists allocate a fresh overflow row
/// and the returned ref carries its id. Inline-vs-overflow is exclusive
/// — when overflow is chosen, the inline buffer stays empty and the
/// overflow row carries every entry.
pub fn pack_evidence_entries(
    wtxn: &WriteTransaction,
    entries: Vec<EvidenceEntry>,
    now_unix_nanos: u64,
) -> Result<EvidenceRef, StatementOpError> {
    if entries.len() <= INLINE_EVIDENCE_CAP {
        let mut sv: SmallVec<[EvidenceEntry; INLINE_EVIDENCE_CAP]> = SmallVec::new();
        for e in entries {
            sv.push(e);
        }
        return Ok(EvidenceRef::inline(sv));
    }

    let overflow_id = EvidenceOverflowId::new();
    let row = EvidenceOverflow::from_entries(overflow_id, &entries, now_unix_nanos);
    let mut t = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
    t.insert(&row.overflow_id_bytes, &row)?;
    Ok(EvidenceRef::Overflow(overflow_id))
}

// ---------------------------------------------------------------------------
// Read helpers.
// ---------------------------------------------------------------------------

/// Read the full evidence id list backing a statement.
///
/// Returns the inline ids verbatim when `reference` is
/// [`EvidenceRef::Inline`]; loads the overflow row and projects its
/// `memory_ids` when it is [`EvidenceRef::Overflow`].
///
/// Returns [`StatementOpError::DecodeFailed`] when an overflow ref
/// dangles — the overflow id has no backing row. That means a prior
/// reclamation ran before the statement row was updated; the caller
/// surfaces this as storage corruption.
pub fn read_evidence_ids(
    rtxn: &ReadTransaction,
    reference: &EvidenceRef,
) -> Result<Vec<MemoryId>, StatementOpError> {
    match reference {
        EvidenceRef::Inline(entries) => Ok(entries.iter().map(|e| e.memory_id).collect()),
        EvidenceRef::Overflow(id) => {
            let t = rtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
            let row: Option<EvidenceOverflow> = t.get(&id.to_bytes())?.map(|g| g.value());
            let over = row.ok_or(StatementOpError::DecodeFailed)?;
            Ok(over
                .memory_ids
                .iter()
                .map(|b| MemoryId::from_be_bytes(*b))
                .collect())
        }
    }
}

/// Write-txn-side variant of [`read_evidence_ids`]. Use this when the
/// caller already holds a `WriteTransaction` (cascade, supersession);
/// redb does not let one transaction nest inside another.
pub fn read_evidence_ids_w(
    wtxn: &WriteTransaction,
    reference: &EvidenceRef,
) -> Result<Vec<MemoryId>, StatementOpError> {
    match reference {
        EvidenceRef::Inline(entries) => Ok(entries.iter().map(|e| e.memory_id).collect()),
        EvidenceRef::Overflow(id) => {
            let t = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
            let row: Option<EvidenceOverflow> = t.get(&id.to_bytes())?.map(|g| g.value());
            let over = row.ok_or(StatementOpError::DecodeFailed)?;
            Ok(over
                .memory_ids
                .iter()
                .map(|b| MemoryId::from_be_bytes(*b))
                .collect())
        }
    }
}

/// Read the full [`EvidenceEntry`] list (memory id + per-entry
/// confidence / timestamp / extractor) backing a statement.
///
/// The inline path clones the inline buffer; the overflow path
/// projects the four parallel vectors back into entries. Returns
/// [`StatementOpError::DecodeFailed`] when an overflow ref dangles.
pub fn read_evidence_entries_w(
    wtxn: &WriteTransaction,
    reference: &EvidenceRef,
) -> Result<Vec<EvidenceEntry>, StatementOpError> {
    match reference {
        EvidenceRef::Inline(entries) => Ok(entries.to_vec()),
        EvidenceRef::Overflow(id) => {
            let t = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
            let row: Option<EvidenceOverflow> = t.get(&id.to_bytes())?.map(|g| g.value());
            let over = row.ok_or(StatementOpError::DecodeFailed)?;
            Ok(over.to_entries())
        }
    }
}

// ---------------------------------------------------------------------------
// Cleanup.
// ---------------------------------------------------------------------------

/// Drop the overflow row referenced by `reference` when it is
/// [`EvidenceRef::Overflow`]. Inline references are a no-op.
///
/// Returns `Ok(true)` when an overflow row was removed, `Ok(false)`
/// otherwise. Calling this on an inline ref, or on an overflow ref
/// whose row was already reclaimed, is harmless — the operation is
/// idempotent.
///
/// Hook this on every path that removes the owning statement: the
/// FORGET cascade tombstone branch, statement-supersede chain
/// reclamation, and the future statement GC worker. Forgetting to call
/// it leaks an `EvidenceOverflow` row.
pub fn reclaim_evidence_overflow(
    wtxn: &WriteTransaction,
    reference: &EvidenceRef,
) -> Result<bool, StatementOpError> {
    let id = match reference {
        EvidenceRef::Inline(_) => return Ok(false),
        EvidenceRef::Overflow(id) => *id,
    };
    let mut t = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
    let removed = t.remove(&id.to_bytes())?.is_some();
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_core::ContextId;
    use redb::ReadableDatabase;

    fn fresh_db() -> (tempfile::TempDir, redb::Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("ev.redb")).unwrap();
        (dir, db)
    }

    fn ids(n: usize) -> Vec<MemoryId> {
        (0..n)
            .map(|i| MemoryId::pack(i as u16, ContextId::DEFAULT.into(), 0))
            .collect()
    }

    #[test]
    fn pack_eight_or_fewer_stays_inline() {
        let (_dir, db) = fresh_db();
        let wtxn = db.begin_write().unwrap();
        let r = pack_evidence_ids(&wtxn, ids(8), 0.9, 1_700_000_000, ExtractorId::from(0)).unwrap();
        wtxn.commit().unwrap();
        match r {
            EvidenceRef::Inline(entries) => assert_eq!(entries.len(), 8),
            EvidenceRef::Overflow(_) => panic!("eight entries must stay inline"),
        }
    }

    #[test]
    fn pack_nine_spills_to_overflow() {
        let (_dir, db) = fresh_db();
        let wtxn = db.begin_write().unwrap();
        let r = pack_evidence_ids(&wtxn, ids(9), 0.9, 1_700_000_000, ExtractorId::from(0)).unwrap();
        wtxn.commit().unwrap();
        let overflow_id = match r {
            EvidenceRef::Overflow(id) => id,
            EvidenceRef::Inline(_) => panic!("nine entries must spill"),
        };

        // The overflow row is persisted.
        let rtxn = db.begin_read().unwrap();
        let back = read_evidence_ids(&rtxn, &EvidenceRef::Overflow(overflow_id)).unwrap();
        assert_eq!(back.len(), 9);
    }

    #[test]
    fn pack_empty_stays_inline_empty() {
        let (_dir, db) = fresh_db();
        let wtxn = db.begin_write().unwrap();
        let r =
            pack_evidence_ids(&wtxn, Vec::new(), 0.9, 1_700_000_000, ExtractorId::from(0)).unwrap();
        wtxn.commit().unwrap();
        match r {
            EvidenceRef::Inline(entries) => assert!(entries.is_empty()),
            EvidenceRef::Overflow(_) => panic!("empty must stay inline"),
        }
    }

    #[test]
    fn read_inline_round_trip() {
        let (_dir, db) = fresh_db();
        let want = ids(5);
        let wtxn = db.begin_write().unwrap();
        let r = pack_evidence_ids(
            &wtxn,
            want.clone(),
            0.9,
            1_700_000_000,
            ExtractorId::from(0),
        )
        .unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = read_evidence_ids(&rtxn, &r).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn read_overflow_round_trip_one_hundred() {
        let (_dir, db) = fresh_db();
        let want = ids(100);
        let wtxn = db.begin_write().unwrap();
        let r = pack_evidence_ids(
            &wtxn,
            want.clone(),
            0.75,
            1_700_000_000,
            ExtractorId::from(0),
        )
        .unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = read_evidence_ids(&rtxn, &r).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn reclaim_drops_overflow_row() {
        let (_dir, db) = fresh_db();
        let wtxn = db.begin_write().unwrap();
        let r = pack_evidence_ids(&wtxn, ids(20), 0.9, 1_700_000_000, ExtractorId::from(0))
            .unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let removed = reclaim_evidence_overflow(&wtxn, &r).unwrap();
        wtxn.commit().unwrap();
        assert!(removed);

        // The overflow row is gone; subsequent reads dangle.
        let rtxn = db.begin_read().unwrap();
        let err = read_evidence_ids(&rtxn, &r).unwrap_err();
        assert!(matches!(err, StatementOpError::DecodeFailed));
    }

    #[test]
    fn reclaim_inline_is_noop() {
        let (_dir, db) = fresh_db();
        let wtxn = db.begin_write().unwrap();
        let r = pack_evidence_ids(&wtxn, ids(3), 0.9, 1_700_000_000, ExtractorId::from(0)).unwrap();
        let removed = reclaim_evidence_overflow(&wtxn, &r).unwrap();
        wtxn.commit().unwrap();
        assert!(!removed);
    }

    #[test]
    fn reclaim_already_gone_is_idempotent() {
        let (_dir, db) = fresh_db();
        let wtxn = db.begin_write().unwrap();
        let r = pack_evidence_ids(&wtxn, ids(20), 0.9, 1_700_000_000, ExtractorId::from(0))
            .unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        assert!(reclaim_evidence_overflow(&wtxn, &r).unwrap());
        wtxn.commit().unwrap();

        // Second reclaim — overflow already gone; idempotent.
        let wtxn = db.begin_write().unwrap();
        assert!(!reclaim_evidence_overflow(&wtxn, &r).unwrap());
        wtxn.commit().unwrap();
    }

    #[test]
    fn read_w_matches_read() {
        let (_dir, db) = fresh_db();
        let want = ids(15);
        let wtxn = db.begin_write().unwrap();
        let r = pack_evidence_ids(
            &wtxn,
            want.clone(),
            0.9,
            1_700_000_000,
            ExtractorId::from(0),
        )
        .unwrap();
        let got = read_evidence_ids_w(&wtxn, &r).unwrap();
        assert_eq!(got, want);
        wtxn.commit().unwrap();
    }

    #[test]
    fn pack_entries_preserves_per_entry_metadata() {
        let (_dir, db) = fresh_db();
        let entries: Vec<EvidenceEntry> = (0..12)
            .map(|i| {
                EvidenceEntry::from_parts(
                    MemoryId::pack(i, ContextId::DEFAULT.into(), 0),
                    0.5 + (i as f32) * 0.01,
                    1_700_000_000 + i as u64,
                    ExtractorId::from(i as u32),
                )
            })
            .collect();
        let wtxn = db.begin_write().unwrap();
        let r = pack_evidence_entries(&wtxn, entries.clone(), 1_700_000_000).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let back = read_evidence_entries_w(&wtxn, &r).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(back, entries);
    }
}
