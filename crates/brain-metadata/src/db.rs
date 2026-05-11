//! `MetadataDb` — the public composition layer over the 13 tables in
//! `crate::tables`.
//!
//! Spec references:
//! - `spec/07_metadata_graph/08_transactions.md` — full transaction semantics.
//! - `spec/07_metadata_graph/02_table_layout.md` — table catalog.
//!
//! ## Surface
//!
//! - [`MetadataDb::open`] — opens or creates the redb file at the given
//!   path, then runs [`crate::schema::open_or_init_schema`] to either
//!   initialise a fresh DB at the current schema version or verify an
//!   existing file's version is compatible.
//! - [`MetadataDb::read_txn`] — `&self`; many can coexist (redb MVCC).
//! - [`MetadataDb::write_txn`] — `&mut self`; the borrow checker
//!   enforces single-writer-per-shard (CLAUDE.md §5 invariant 2)
//!   at compile time.
//! - [`MetadataDb::schema_version`] — cached at open; cheap.
//! - [`MetadataDb::path`], [`MetadataDb::db`] — diagnostics and an
//!   escape hatch for operations the wrapper doesn't surface.
//!
//! ## What does NOT live here
//!
//! - **`impl MetadataSink for MetadataDb`** — sub-task 3.11.
//! - **Typed convenience methods** (`db.get_memory(&id)` etc.) —
//!   deliberately not. Spec §07/08 §5 shows callers opening multiple
//!   tables inside one write transaction; wrapping each row type in a
//!   dedicated method would (a) duplicate redb's API, (b) break
//!   batching, (c) hide transaction granularity from the caller.
//!   Callers `use brain_metadata::tables::memory::MEMORIES_TABLE;` and
//!   open whatever they need.
//! - **Cached table handles** (spec §07/08 §14) — profile-driven; not
//!   v1.
//! - **Write-transaction timeout** (spec §07/08 §16) — writer-task
//!   concern; `MetadataDb` doesn't auto-abort.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use redb::{Database, ReadTransaction, ReadableDatabase, TransactionError, WriteTransaction};

use crate::schema::{open_or_init_schema, SchemaError};
use crate::tables::checkpoint::{latest as latest_checkpoint, CHECKPOINTS_TABLE};

/// Public type wrapping the redb metadata file. Single ownership per
/// shard; the borrow checker enforces single-writer via `&mut self` on
/// [`MetadataDb::write_txn`].
#[derive(Debug)]
pub struct MetadataDb {
    pub(crate) db: Database,
    schema_version: u32,
    path: PathBuf,

    /// Cached recovery target. Loaded at [`MetadataDb::open`] from the
    /// `checkpoints` table's most-recent row; advanced by
    /// [`crate::sink::MetadataSink::apply`] on `CheckpointEnd`.
    pub(crate) durable_lsn: u64,

    /// In-flight checkpoints seen but not yet `CheckpointEnd`-paired.
    /// Maps `checkpoint_id → started_at_unix_nanos`. Transient: any
    /// entry surviving across a restart is implicitly discarded
    /// (spec §05/09 §12.1: incomplete checkpoint is ignored).
    pub(crate) pending_checkpoints: HashMap<u64, u64>,
}

/// Errors returned by [`MetadataDb::open`].
///
/// After open, read/write transaction errors propagate as their native
/// [`redb::TransactionError`] — wrapping every transaction begin in a
/// custom enum would force a useless `?` indirection at every call
/// site.
#[derive(thiserror::Error, Debug)]
pub enum MetadataDbError {
    #[error("redb database error: {0}")]
    Database(#[from] redb::DatabaseError),

    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("schema: {0}")]
    Schema(#[from] SchemaError),
}

impl MetadataDb {
    /// Open or create the metadata file at `path`.
    ///
    /// On a fresh file, writes the current schema version. On an
    /// existing file, verifies the schema version is compatible (≤
    /// [`crate::schema::CURRENT_SCHEMA_VERSION`]) and reads it back.
    /// Refuses to open files written by a newer binary.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MetadataDbError> {
        let path = path.as_ref().to_path_buf();
        let db = Database::create(&path)?;
        let schema_version = open_or_init_schema(&db)?;

        // Seed `durable_lsn` from the latest checkpoint, if any. Missing
        // or empty checkpoints table → 0 (fresh shard or no checkpoint
        // has completed yet). Spec §05/09 §2: "the substrate keeps the
        // most recent one as the recovery target."
        let durable_lsn = {
            let rtxn = db.begin_read()?;
            match rtxn.open_table(CHECKPOINTS_TABLE) {
                Ok(t) => latest_checkpoint(&t)
                    .map_err(|e| MetadataDbError::Schema(SchemaError::Storage(e)))?
                    .map_or(0, |c| c.durable_lsn),
                Err(redb::TableError::TableDoesNotExist(_)) => 0,
                Err(e) => return Err(MetadataDbError::Schema(SchemaError::from(e))),
            }
        };

        Ok(Self {
            db,
            schema_version,
            path,
            durable_lsn,
            pending_checkpoints: HashMap::new(),
        })
    }

    /// Begin a read transaction. `&self` — many can coexist (redb
    /// uses MVCC). Reads never block writes; writes never block reads.
    pub fn read_txn(&self) -> Result<ReadTransaction, TransactionError> {
        self.db.begin_read()
    }

    /// Begin a write transaction. `&mut self` enforces
    /// single-writer-per-shard at compile time: a shard can't
    /// accidentally host two writer tasks because both would need
    /// `&mut MetadataDb`, which the borrow checker forbids.
    ///
    /// Spec §07/08 §3: "The single-writer-per-shard discipline means
    /// there's only one writer per shard, naturally serializing
    /// redb's write transactions." We encode the discipline in the
    /// type signature.
    pub fn write_txn(&mut self) -> Result<WriteTransaction, TransactionError> {
        self.db.begin_write()
    }

    /// The schema version this file was opened at. Cached; reading
    /// is O(1).
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// The path the DB was opened from. Useful for diagnostics
    /// (e.g. `tracing::error!(path = %db.path().display(), ...)`).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Escape hatch for operations the wrapper doesn't surface
    /// (e.g. backup, compact, internal statistics).
    ///
    /// **Do not** use this to call `begin_write` — that would
    /// circumvent the single-writer-per-shard discipline encoded in
    /// [`write_txn`]'s `&mut self` signature. The borrow checker
    /// can't seal this hatch perfectly; the discipline is on the
    /// caller from here.
    #[doc(hidden)]
    #[must_use]
    pub fn db(&self) -> &Database {
        &self.db
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::schema::{CURRENT_SCHEMA_VERSION, SCHEMA_META_TABLE, SCHEMA_VERSION_KEY};
    use crate::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
    use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};

    fn db_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("metadata.redb")
    }

    fn aid(byte: u8) -> AgentId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn mid(byte: u8) -> MemoryId {
        let mut b = [0u8; 16];
        b[15] = byte;
        MemoryId::from_be_bytes(b)
    }

    fn sample_memory() -> ([u8; 16], MemoryMetadata) {
        let id = mid(1);
        let agent = aid(7);
        let m = MemoryMetadata::new_active(
            id,
            agent,
            ContextId(42),
            /* slot_id */ 0,
            /* slot_version */ 1,
            MemoryKind::Episodic,
            /* embedding_model_fp */ [0u8; 16],
            /* salience_initial */ 0.5,
            /* text_size */ 32,
            /* created_at_unix_nanos */ 1_700_000_000_000_000_000,
        );
        (id.to_be_bytes(), m)
    }

    #[test]
    fn open_fresh_creates_schema_v1() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let db = MetadataDb::open(&path).unwrap();
        assert_eq!(db.schema_version(), CURRENT_SCHEMA_VERSION);
        assert_eq!(db.path(), path.as_path());
    }

    #[test]
    fn open_existing_reads_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        {
            let _db = MetadataDb::open(&path).unwrap();
        }
        // Reopen: same file, same schema version, no error.
        let db = MetadataDb::open(&path).unwrap();
        assert_eq!(db.schema_version(), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn open_too_new_schema_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);

        // Pre-seed a redb file with a schema version newer than this
        // binary supports.
        {
            let raw = Database::create(&path).unwrap();
            let wtxn = raw.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(SCHEMA_META_TABLE).unwrap();
                t.insert(SCHEMA_VERSION_KEY, &(CURRENT_SCHEMA_VERSION + 99))
                    .unwrap();
            }
            wtxn.commit().unwrap();
        }

        let err = MetadataDb::open(&path).expect_err("should refuse newer schema");
        match err {
            MetadataDbError::Schema(SchemaError::SchemaVersionTooNew { found, supported }) => {
                assert_eq!(found, CURRENT_SCHEMA_VERSION + 99);
                assert_eq!(supported, CURRENT_SCHEMA_VERSION);
            }
            other => panic!("expected SchemaVersionTooNew, got {other:?}"),
        }
    }

    #[test]
    fn write_then_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let (key, m) = sample_memory();

        // Write via the wrapper.
        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        // Read via the wrapper.
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        assert_eq!(t.get(&key).unwrap().unwrap().value(), m);
    }

    #[test]
    fn read_txn_doesnt_see_uncommitted_write() {
        // MVCC pin (spec §07/08 §2): a read transaction sees the
        // database as-of when it began; uncommitted writes from a
        // concurrent write transaction are invisible.
        //
        // redb takes an exclusive file lock per `Database::create`, so
        // we can't open a second `MetadataDb` on the same path. Instead
        // we rely on the fact that `write_txn(&mut self)` borrows &mut
        // only briefly (the returned `WriteTransaction` is owned, with
        // no lifetime tied to `db`), so calling `read_txn(&self)`
        // afterwards is legal.
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let (key, m) = sample_memory();

        // Seed the table by writing+committing an unrelated row, so
        // the table exists when the read txn opens it.
        {
            let other_key = mid(99).to_be_bytes();
            let wtxn = db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
                t.insert(&other_key, &m).unwrap();
            }
            wtxn.commit().unwrap();
        }

        // Start an uncommitted write txn inserting `key`.
        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }

        // A read txn must not see the uncommitted insert.
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        assert!(
            t.get(&key).unwrap().is_none(),
            "read txn must not see uncommitted write"
        );

        // Cleanup: drop the uncommitted txn (rollback).
        drop(wtxn);
    }

    #[test]
    fn commit_makes_write_visible_to_new_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let (key, m) = sample_memory();

        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        assert_eq!(t.get(&key).unwrap().unwrap().value(), m);
    }

    #[test]
    fn concurrent_read_txns_coexist() {
        // Spec §07/08 §2: read transactions don't block each other.
        // Two read txns from the same MetadataDb share a snapshot.
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let (key, m) = sample_memory();

        // Seed one row so there's something to observe.
        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let r1 = db.read_txn().unwrap();
        let r2 = db.read_txn().unwrap();

        let t1 = r1.open_table(MEMORIES_TABLE).unwrap();
        let t2 = r2.open_table(MEMORIES_TABLE).unwrap();

        assert_eq!(t1.get(&key).unwrap().unwrap().value(), m);
        assert_eq!(t2.get(&key).unwrap().unwrap().value(), m);
    }

    #[test]
    fn schema_version_accessor_returns_v1() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(db_path(&dir)).unwrap();
        assert_eq!(db.schema_version(), CURRENT_SCHEMA_VERSION);
        assert_eq!(CURRENT_SCHEMA_VERSION, 1);
    }

    #[test]
    fn path_accessor_returns_open_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let db = MetadataDb::open(&path).unwrap();
        assert_eq!(db.path(), path.as_path());
    }
}
