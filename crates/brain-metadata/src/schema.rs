//! Schema version tracking for the redb metadata store.
//!
//! Every redb file written by `brain-metadata` carries a singleton
//! `__schema_meta` table whose only row records the schema version the
//! file was written against. [`open_or_init_schema`] is the entry point:
//!
//! - **Fresh DB** (table absent): writes [`CURRENT_SCHEMA_VERSION`].
//! - **Same version**: returns the stored version.
//! - **Older version**: returns the stored version (the caller may
//!   dispatch to a migration registry — none exist at v1).
//! - **Newer version**: returns [`SchemaError::SchemaVersionTooNew`];
//!   the binary is older than the file it's trying to read.
//!
//! ## Single global version vs per-table versions
//!
//! Spec `§07/02 §6` reads "each table has a format version embedded in
//! its metadata." A literal implementation would maintain 13 separate
//! version rows. We instead carry one global row covering the whole
//! metadata file. The 13 tables ship from the same crate and co-evolve
//! — bumping one means bumping the whole file's format — and the
//! per-table machinery (13× the open-time checks and migration registry
//! entries) adds bookkeeping with no concrete benefit at v1. This is a
//! coverage-scope decision, not a behavioral deviation; if a future
//! version diverges per-table, we'll extend this module.
//!
//! Internal "private" tables get an underscore-prefixed name; the 13
//! spec'd domain tables in `§07/02 §1` never use that prefix.

use redb::{Database, ReadableDatabase, TableDefinition};

/// The schema version this crate writes. Bumped on backward-incompatible
/// changes to the redb table layout or value encoding.
///
/// Phase C unified the substrate edge tables and the typed-relation
/// tables under a single `NodeRef`-keyed layout. The on-disk shape is
/// not readable by a v1 binary, and a v1 DB is not readable by a v2
/// binary — operators must run the migration tool to copy data into a
/// fresh v2 directory.
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// Singleton key inside [`SCHEMA_META_TABLE`].
pub const SCHEMA_VERSION_KEY: &str = "schema_version";

/// Table tracking the on-disk schema version.
pub const SCHEMA_META_TABLE: TableDefinition<'static, &'static str, u32> =
    TableDefinition::new("__schema_meta");

/// Errors returned by [`open_or_init_schema`].
#[derive(thiserror::Error, Debug)]
pub enum SchemaError {
    #[error("redb error: {0}")]
    Redb(#[from] redb::Error),

    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error(
        "schema version {found} is newer than this binary supports ({supported}); \
         upgrade the substrate or restore from a compatible backup"
    )]
    SchemaVersionTooNew { found: u32, supported: u32 },

    #[error(
        "data/ contains an older schema version (v{found}); this binary requires v{current}. \
         Phase C is a hard break — delete the data/ directory and restart for a fresh DB."
    )]
    SchemaTooOld { found: u32, current: u32 },
}

/// Read the schema version from `db`, or initialize it on a fresh DB.
///
/// See the module docs for behavior across the four cases (fresh, same,
/// older, newer).
pub fn open_or_init_schema(db: &Database) -> Result<u32, SchemaError> {
    // Peek with a read transaction first.
    {
        let rtxn = db.begin_read()?;
        match rtxn.open_table(SCHEMA_META_TABLE) {
            Ok(table) => {
                if let Some(stored) = table.get(SCHEMA_VERSION_KEY)? {
                    let v = stored.value();
                    if v > CURRENT_SCHEMA_VERSION {
                        return Err(SchemaError::SchemaVersionTooNew {
                            found: v,
                            supported: CURRENT_SCHEMA_VERSION,
                        });
                    }
                    if v < CURRENT_SCHEMA_VERSION {
                        // No in-place migration and no migration tool:
                        // Brain is pre-user, fresh-start is acceptable.
                        return Err(SchemaError::SchemaTooOld {
                            found: v,
                            current: CURRENT_SCHEMA_VERSION,
                        });
                    }
                    tracing::info!(
                        schema_version = v,
                        "opened brain-metadata at existing schema"
                    );
                    return Ok(v);
                }
                // Table exists but the key is missing — treat as fresh.
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {
                // Fresh DB; fall through to init.
            }
            Err(e) => return Err(e.into()),
        }
    }

    // Initialize.
    let wtxn = db.begin_write()?;
    {
        let mut table = wtxn.open_table(SCHEMA_META_TABLE)?;
        table.insert(SCHEMA_VERSION_KEY, &CURRENT_SCHEMA_VERSION)?;
    }
    wtxn.commit()?;
    tracing::info!(
        schema_version = CURRENT_SCHEMA_VERSION,
        "initialized brain-metadata schema"
    );
    Ok(CURRENT_SCHEMA_VERSION)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

// redb internally uses mmap, which miri doesn't shim. Gate the test module
// behind `not(miri)` (consistent with brain-storage's pattern). See
// `.claude/plans/phase-02-miri.md` for context.
#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::Database;
    use std::path::PathBuf;

    fn fresh_db(dir: &tempfile::TempDir) -> (Database, PathBuf) {
        let path = dir.path().join("test.redb");
        let db = Database::create(&path).expect("create redb");
        (db, path)
    }

    #[test]
    fn fresh_db_initializes_at_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _) = fresh_db(&dir);
        let v = open_or_init_schema(&db).unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn reopen_reads_existing_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");

        // Initialize then drop.
        {
            let db = Database::create(&path).unwrap();
            assert_eq!(open_or_init_schema(&db).unwrap(), CURRENT_SCHEMA_VERSION);
        }
        // Reopen; same version observed.
        let db = Database::open(&path).unwrap();
        let v = open_or_init_schema(&db).unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn future_version_refuses_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");

        // Hand-write a future version (current + 1) into the singleton.
        let future = CURRENT_SCHEMA_VERSION + 1;
        {
            let db = Database::create(&path).unwrap();
            let wtxn = db.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(SCHEMA_META_TABLE).unwrap();
                t.insert(SCHEMA_VERSION_KEY, &future).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let db = Database::open(&path).unwrap();
        let err = open_or_init_schema(&db).unwrap_err();
        match err {
            SchemaError::SchemaVersionTooNew { found, supported } => {
                assert_eq!(found, future);
                assert_eq!(supported, CURRENT_SCHEMA_VERSION);
            }
            other => panic!("expected SchemaVersionTooNew, got {other:?}"),
        }
    }

    #[test]
    fn schema_too_old_on_v1_db() {
        // A v1 DB on disk is unreachable from the Phase C v2 layout. No
        // migration tool exists — operators delete data/ and restart.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");

        {
            let db = Database::create(&path).unwrap();
            let wtxn = db.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(SCHEMA_META_TABLE).unwrap();
                t.insert(SCHEMA_VERSION_KEY, &1_u32).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let db = Database::open(&path).unwrap();
        let err = open_or_init_schema(&db).unwrap_err();
        match err {
            SchemaError::SchemaTooOld { found, current } => {
                assert_eq!(found, 1);
                assert_eq!(current, CURRENT_SCHEMA_VERSION);
                assert_eq!(current, 2);
            }
            other => panic!("expected SchemaTooOld, got {other:?}"),
        }
    }

    #[test]
    fn idempotent_reinit_returns_same_version() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _) = fresh_db(&dir);
        let v1 = open_or_init_schema(&db).unwrap();
        let v2 = open_or_init_schema(&db).unwrap();
        let v3 = open_or_init_schema(&db).unwrap();
        assert_eq!(v1, v2);
        assert_eq!(v2, v3);
        assert_eq!(v1, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn table_present_but_row_missing_initializes_to_v1() {
        // Edge case: someone created the table but didn't write the row.
        // Our code treats this as fresh and initializes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        {
            let db = Database::create(&path).unwrap();
            let wtxn = db.begin_write().unwrap();
            {
                // Open the table but don't insert anything.
                let _t = wtxn.open_table(SCHEMA_META_TABLE).unwrap();
            }
            wtxn.commit().unwrap();
        }
        let db = Database::open(&path).unwrap();
        let v = open_or_init_schema(&db).unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION);
    }
}
