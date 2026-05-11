//! `next_lsn` table: singleton holding the next WAL LSN to allocate.
//!
//! See `spec/07_metadata_graph/02_table_layout.md` §1 row 12 (catalog
//! entry) and §7 (singleton convention: `()` key with `t.get(&())` /
//! `t.insert(&(), &value)`).
//!
//! ## What lives here
//!
//! - [`NEXT_LSN_TABLE`] — singleton `() → u64`, the next LSN to hand
//!   out for a WAL record.
//!
//! ## What does NOT live here
//!
//! - **LSN allocation logic** (read, hand out, advance, persist) —
//!   `MetadataSink` impl (3.11) composes this table with the WAL.
//! - **Initial value on missing** (fresh shard vs replayed-from-WAL) —
//!   spec doesn't pin; the recovery code seeds this row from a WAL
//!   scan during the open-or-recover handshake. Storage stays
//!   decision-free; callers pick their default via
//!   `.get(&()).unwrap_or_default()` or by inserting an explicit
//!   initial value.

use redb::TableDefinition;

/// The `next_lsn` table. Singleton: `()` key, `u64` value.
pub const NEXT_LSN_TABLE: TableDefinition<'static, (), u64> = TableDefinition::new("next_lsn");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase, ReadableTable};

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    #[test]
    fn singleton_insert_and_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(NEXT_LSN_TABLE).unwrap();
            t.insert(&(), &42u64).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(NEXT_LSN_TABLE).unwrap();
        assert_eq!(t.get(&()).unwrap().unwrap().value(), 42);
    }

    #[test]
    fn singleton_update_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(NEXT_LSN_TABLE).unwrap();
            t.insert(&(), &1u64).unwrap();
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(NEXT_LSN_TABLE).unwrap();
            t.insert(&(), &999u64).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(NEXT_LSN_TABLE).unwrap();
        assert_eq!(t.get(&()).unwrap().unwrap().value(), 999);
    }

    #[test]
    fn singleton_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        {
            let _t = wtxn.open_table(NEXT_LSN_TABLE).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(NEXT_LSN_TABLE).unwrap();
        assert!(t.get(&()).unwrap().is_none());
    }

    #[test]
    fn unit_key_round_trips() {
        // Guards spec §07/02 §7's prescription that redb supports `()`
        // as the Key + Value type for singletons. If a future redb
        // version dropped this, the test would fail on Insert/Get
        // rather than silently mis-encoding.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(NEXT_LSN_TABLE).unwrap();
            t.insert(&(), &u64::MAX).unwrap();
            // Only one key exists (singleton).
            assert!(t.get(&()).unwrap().is_some());
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(NEXT_LSN_TABLE).unwrap();
        assert_eq!(t.get(&()).unwrap().unwrap().value(), u64::MAX);
    }
}
