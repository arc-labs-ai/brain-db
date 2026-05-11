//! `slot_versions` table: per-slot version counter for lazy reclaim.
//!
//! See `spec/07_metadata_graph/02_table_layout.md` §13 (purpose +
//! shape) and `spec/05_storage_arena_wal/07_write_path.md` §2.3
//! (initial value: 1 for never-used, current+1 for reclaimed).
//!
//! ## Realignment note (sub-task 3.7)
//!
//! The phase doc originally titled this sub-task "Tombstone table" with
//! value `(memory_id, tombstoned_at, grace_until)`. That table is not
//! in the spec's 13-table catalog (§07/02 §1). Tombstone *state* lives
//! as `flags & HARD_FORGOTTEN` + `forgot_at_unix_nanos` on the existing
//! `memories` row (3.2's `MemoryMetadata`); the reclaim worker scans
//! memories for `forgot_at + grace < now` per spec §09/06 §16. The
//! actual reclaim-related table in the spec catalog is
//! `slot_versions`, which is this file.
//!
//! ## What lives here
//!
//! - [`SLOT_VERSIONS_TABLE`] — `slot_id` (u64) → current `version` (u32).
//! - [`increment`] — atomic read-modify-write inside the caller's
//!   transaction. Missing row starts at 1; existing row goes to N+1;
//!   `u32::MAX` returns [`SlotVersionError::Exhausted`] and does not
//!   write.
//!
//! ## What does NOT live here
//!
//! - Composition with FORGET / reclaim (increment + memory-row remove
//!   + arena zero in one txn) — `MetadataDb` (3.10) + Phase 8 worker.
//! - MemoryId minting (packing `slot_id + version` into 16 bytes) —
//!   `brain-core`'s identifier code.
//! - Recovery cross-check vs the arena's slot metadata (spec §05/08
//!   §6) — composition lives in the `MetadataSink` impl (3.11).
//! - Retirement strategy for u32::MAX-overflowed slots — spec doesn't
//!   address; v1 surfaces the error and lets the caller decide.

use redb::{ReadableTable, Table, TableDefinition};

/// The `slot_versions` table. Key is `slot_id` as `u64`; value is the
/// current `version` as `u32`. Uses redb's built-in scalar `Value`
/// impls — no rkyv wrapper (no struct to evolve).
///
/// `slot_id` is logically 48 bits in the MemoryId (`spec/02_data_model/03_identifiers.md`
/// §2.1) but is stored as `u64` here per spec §07/02 §1's catalog.
pub const SLOT_VERSIONS_TABLE: TableDefinition<'static, u64, u32> =
    TableDefinition::new("slot_versions");

/// Errors from [`increment`].
#[derive(thiserror::Error, Debug)]
pub enum SlotVersionError {
    /// redb storage-layer failure (I/O, corruption, etc.).
    #[error("storage error: {0}")]
    Storage(#[from] redb::StorageError),

    /// The slot's version field is already `u32::MAX`. Incrementing
    /// would wrap to zero and silently violate spec §02/03 §2.3's
    /// MemoryId-stability invariant ("A `MemoryId` that previously
    /// identified memory M never identifies a different memory"), so
    /// storage refuses the write. The caller decides how to surface
    /// (likely permanent retirement; v1 doesn't auto-retire).
    #[error("slot {slot_id} version exhausted (reached u32::MAX)")]
    Exhausted { slot_id: u64 },
}

/// Atomic read-modify-write of `slot_versions[slot_id]`. Returns the
/// new version.
///
/// - Missing row (never-used slot, spec §05/07 §2.3) → writes `1`,
///   returns `Ok(1)`.
/// - Existing row at version `N` → writes `N + 1`, returns `Ok(N + 1)`.
/// - Row at `u32::MAX` → returns [`SlotVersionError::Exhausted`] and
///   leaves the table untouched (fail-stop on overflow).
///
/// The caller owns the redb transaction; this helper composes with
/// other table operations (memory-row removal, arena zeroing) inside
/// the same write transaction.
pub fn increment(table: &mut Table<'_, u64, u32>, slot_id: u64) -> Result<u32, SlotVersionError> {
    let current = table.get(&slot_id)?.map_or(0u32, |access| access.value());

    let next = current
        .checked_add(1)
        .ok_or(SlotVersionError::Exhausted { slot_id })?;

    table.insert(&slot_id, &next)?;
    Ok(next)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase, ReadableTable};

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    #[test]
    fn increment_missing_starts_at_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        let returned = {
            let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
            increment(&mut t, 42).unwrap()
        };
        wtxn.commit().unwrap();
        assert_eq!(returned, 1);

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
        assert_eq!(t.get(&42u64).unwrap().unwrap().value(), 1);
    }

    #[test]
    fn increment_existing_returns_next() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        // Seed slot 7 at version 5.
        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
            t.insert(&7u64, &5u32).unwrap();
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let returned = {
            let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
            increment(&mut t, 7).unwrap()
        };
        wtxn.commit().unwrap();
        assert_eq!(returned, 6);

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
        assert_eq!(t.get(&7u64).unwrap().unwrap().value(), 6);
    }

    #[test]
    fn increment_is_monotonic_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let slot = 99u64;

        for expected in 1u32..=10 {
            let wtxn = db.begin_write().unwrap();
            let got = {
                let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
                increment(&mut t, slot).unwrap()
            };
            wtxn.commit().unwrap();
            assert_eq!(got, expected);
        }

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
        assert_eq!(t.get(&slot).unwrap().unwrap().value(), 10);
    }

    #[test]
    fn independent_slots_dont_interfere() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let slot_a = 1u64;
        let slot_b = 2u64;

        // Bump A three times, B five times, interleaved.
        let increments = [
            slot_a, slot_b, slot_a, slot_b, slot_a, slot_b, slot_b, slot_b,
        ];
        for slot in increments {
            let wtxn = db.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
                increment(&mut t, slot).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
        assert_eq!(t.get(&slot_a).unwrap().unwrap().value(), 3);
        assert_eq!(t.get(&slot_b).unwrap().unwrap().value(), 5);
    }

    #[test]
    fn overflow_returns_exhausted_and_does_not_write() {
        // The catastrophic-failure-mode pin: a slot at u32::MAX must
        // not wrap to 0 on increment. Silent wrap would violate spec
        // §02/03 §2.3's MemoryId-stability invariant.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let slot = 0xDEAD_BEEFu64;

        // Pre-seed at u32::MAX.
        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
            t.insert(&slot, &u32::MAX).unwrap();
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let result = {
            let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
            increment(&mut t, slot)
        };
        wtxn.commit().unwrap();

        match result {
            Err(SlotVersionError::Exhausted { slot_id }) => assert_eq!(slot_id, slot),
            other => panic!("expected Exhausted, got {other:?}"),
        }

        // The row is untouched — still u32::MAX, not 0.
        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
        assert_eq!(t.get(&slot).unwrap().unwrap().value(), u32::MAX);
    }

    #[test]
    fn direct_get_after_insert() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
            t.insert(&1234u64, &42u32).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
        assert_eq!(t.get(&1234u64).unwrap().unwrap().value(), 42);
    }

    #[test]
    fn range_scan_returns_in_order() {
        // Pins redb's u64-lexicographic-by-bytes key ordering matches
        // numerical order: inserting 100, 50, 200 must iterate as
        // 50, 100, 200. (redb sorts integer keys big-endian-encoded
        // under the hood; this test catches any change.)
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
            t.insert(&100u64, &1u32).unwrap();
            t.insert(&50u64, &2u32).unwrap();
            t.insert(&200u64, &3u32).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
        let keys: Vec<u64> = t
            .iter()
            .unwrap()
            .map(|entry| entry.unwrap().0.value())
            .collect();
        assert_eq!(keys, vec![50, 100, 200]);
    }

    #[test]
    fn missing_key_get_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        // Create the table so a read txn can open it.
        let wtxn = db.begin_write().unwrap();
        {
            let _t = wtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
        assert!(t.get(&u64::MAX).unwrap().is_none());
    }
}
