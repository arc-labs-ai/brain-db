//! `MetadataDb` — the public composition layer over the 13 tables in
//! `crate::tables`.
//!
//! ## Surface
//!
//! - [`MetadataDb::open`] — opens or creates the redb file at the given
//!   path, then runs [`crate::storage_version::open_or_init_schema`] to either
//!   initialise a fresh DB at the current schema version or verify an
//!   existing file's version is compatible.
//! - [`MetadataDb::read_txn`] — `&self`; many can coexist (redb MVCC).
//! - [`MetadataDb::write_txn`] — `&self`; redb itself serialises writes
//!   per database. The single-writer-per-shard discipline lives at the
//!   shard's writer task, not in the borrow checker — Brain wraps
//!   `MetadataDb` in `Arc` so readers and the writer task share one
//!   handle without a mutex blocking reads against reads.
//! - [`MetadataDb::schema_version`] — cached at open; cheap.
//! - [`MetadataDb::path`], [`MetadataDb::db`] — diagnostics and an
//!   escape hatch for operations the wrapper doesn't surface.
//!
//! ## What does NOT live here
//!
//! - **`impl MetadataSink for MetadataDb`** — lives in `recovery/`.
//! - **Typed convenience methods** (`db.get_memory(&id)` etc.) —
//!   deliberately omitted. Callers open multiple tables inside one
//!   write transaction; wrapping each row type in a dedicated method
//!   would (a) duplicate redb's API, (b) break batching, (c) hide
//!   transaction granularity from the caller. Callers
//!   `use brain_metadata::tables::memory::MEMORIES_TABLE;` and open
//!   whatever they need.
//! - **Cached table handles** — profile-driven; not done yet.
//! - **Write-transaction timeout** — writer-task concern; `MetadataDb`
//!   doesn't auto-abort.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

use parking_lot::Mutex;
use redb::{Database, ReadTransaction, ReadableDatabase, TransactionError, WriteTransaction};

use crate::storage_version::{open_or_init_schema, SchemaError};
use crate::system_schema::{seed_system_schema, SystemSchemaError};
use crate::tables::checkpoint::{latest as latest_checkpoint, CHECKPOINTS_TABLE};

/// Public type wrapping the redb metadata file.
///
/// Designed to be shared via `Arc<MetadataDb>` across a shard's reader
/// paths and its single writer task. Reads (`read_txn`) and writes
/// (`write_txn`) both take `&self` because redb itself coordinates
/// MVCC reads and per-database write serialisation; wrapping the
/// handle in a mutex would only block readers against readers without
/// adding any actual safety. The single-writer-per-shard invariant is
/// enforced architecturally — one dedicated writer task per shard
/// drives `write_txn` — not by the borrow checker.
#[derive(Debug)]
pub struct MetadataDb {
    pub(crate) db: Database,
    schema_version: u32,
    path: PathBuf,

    /// Cached recovery target. Loaded at [`MetadataDb::open`] from the
    /// `checkpoints` table's most-recent row; advanced by
    /// [`crate::sink::MetadataSink::apply`] on `CheckpointEnd`.
    ///
    /// Stored atomically so reader callers (snapshot durability lookups,
    /// retention workers) can observe the watermark through
    /// `Arc<MetadataDb>` without taking a mutex.
    pub(crate) durable_lsn: AtomicU64,

    /// In-flight checkpoints seen but not yet `CheckpointEnd`-paired.
    /// Maps `checkpoint_id → started_at_unix_nanos`. Transient: any
    /// entry surviving across a restart is implicitly discarded
    /// (incomplete checkpoint is ignored).
    ///
    /// Mutated only by the per-shard recovery / checkpoint apply path,
    /// which runs single-threaded inside the writer task. The mutex is
    /// here purely so the field is reachable through `&self` for the
    /// trait-required `MetadataSink::apply(&mut self)` impl while
    /// production code holds the DB through `Arc<MetadataDb>`.
    pub(crate) pending_checkpoints: Mutex<HashMap<u64, u64>>,
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

    /// System-schema seed failed at `MetadataDb::open`.
    #[error("system schema seed: {0}")]
    SystemSchemaSeed(#[from] SystemSchemaError),
}

impl MetadataDb {
    /// Open or create the metadata file at `path`.
    ///
    /// On a fresh file, writes the current schema version. On an
    /// existing file, verifies the schema version is compatible (≤
    /// [`crate::storage_version::CURRENT_SCHEMA_VERSION`]) and reads it back.
    /// Refuses to open files written by a newer binary.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MetadataDbError> {
        let path = path.as_ref().to_path_buf();
        let db = Database::create(&path)?;
        let schema_version = open_or_init_schema(&db)?;

        // Seed `durable_lsn` from the latest checkpoint, if any. Empty
        // checkpoints table → 0 (fresh shard or no checkpoint has
        // completed yet): "the substrate keeps the most recent one as
        // the recovery target."
        let durable_lsn = {
            let rtxn = db.begin_read()?;
            let t = rtxn
                .open_table(CHECKPOINTS_TABLE)
                .map_err(|e| MetadataDbError::Schema(SchemaError::from(e)))?;
            latest_checkpoint(&t)
                .map_err(|e| MetadataDbError::Schema(SchemaError::Storage(e)))?
                .map_or(0, |c| c.durable_lsn)
        };

        // Single parse-validate-apply over the embedded
        // `system_schema/schema.brain` source. Idempotent — re-opens
        // are no-ops because `schema_active("brain")` returns `Some(1)`
        // on a previously-seeded DB.
        seed_system_schema(&db)?;

        Ok(Self {
            db,
            schema_version,
            path,
            durable_lsn: AtomicU64::new(durable_lsn),
            pending_checkpoints: Mutex::new(HashMap::new()),
        })
    }

    /// Begin a read transaction. `&self` — many can coexist (redb
    /// uses MVCC). Reads never block writes; writes never block reads.
    pub fn read_txn(&self) -> Result<ReadTransaction, TransactionError> {
        self.db.begin_read()
    }

    /// Begin a write transaction. Takes `&self` so a shared
    /// `Arc<MetadataDb>` can drive both readers and the writer task
    /// without a wrapping mutex serialising readers against readers.
    ///
    /// redb itself enforces single-writer-per-database at the file
    /// level. Brain layers the single-writer-per-shard discipline on
    /// top: every shard owns one writer task, so the borrow checker's
    /// `&mut self` belt-and-suspenders constraint stops earning its
    /// keep once `MetadataDb` is reached through `Arc<...>`. Callers
    /// outside the writer task must not invoke this.
    pub fn write_txn(&self) -> Result<WriteTransaction, TransactionError> {
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
    use crate::storage_version::{CURRENT_SCHEMA_VERSION, SCHEMA_META_TABLE, SCHEMA_VERSION_KEY};
    use crate::tables::entity_type::{EntityTypeDefinition, ENTITY_TYPES_TABLE};
    use brain_core::EntityType;
    use redb::ReadableTable;

    fn db_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("metadata.redb")
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

    // -----------------------------------------------------------------
    // Person bootstrap.
    // -----------------------------------------------------------------

    /// The system schema seeds the six built-in entity types in
    /// declaration order: Person, Organization, Project, Event,
    /// Place, Concept. Person stays at id 1 (`EntityType::PERSON_ID`)
    /// for back-compat with the original single-entity seed.
    const SEEDED_ENTITY_TYPES: &[(u32, &str)] = &[
        (1, "Person"),
        (2, "Organization"),
        (3, "Project"),
        (4, "Event"),
        (5, "Place"),
        (6, "Concept"),
    ];

    #[test]
    fn builtin_entity_types_seeded_on_fresh_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(db_path(&dir)).unwrap();

        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
        let mut got: Vec<(u32, String)> = t
            .iter()
            .unwrap()
            .map(|e| {
                let (k, v) = e.unwrap();
                (k.value(), v.value().name)
            })
            .collect();
        got.sort_by_key(|(id, _)| *id);
        let expected: Vec<(u32, String)> = SEEDED_ENTITY_TYPES
            .iter()
            .map(|(id, n)| (*id, (*n).to_string()))
            .collect();
        assert_eq!(got, expected, "fresh open seeds the built-in noun set");
        assert_eq!(EntityType::PERSON_ID.raw(), 1);
        assert_eq!(EntityType::PERSON_NAME, "Person");
    }

    #[test]
    fn builtin_entity_types_seed_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);

        drop(MetadataDb::open(&path).unwrap());
        let db = MetadataDb::open(&path).unwrap();
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
        let rows: usize = t.iter().unwrap().count();
        assert_eq!(
            rows,
            SEEDED_ENTITY_TYPES.len(),
            "re-open must not duplicate the built-in entity-type seeds",
        );
    }

    #[test]
    fn system_schema_seed_skipped_when_brain_namespace_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);

        // First open seeds the system schema → schema_active("brain") = Some(1).
        drop(MetadataDb::open(&path).unwrap());

        // Inject a fresh user-namespace entity type with id=42 between
        // opens. The injected name doesn't collide with any built-in,
        // so a hypothetical re-seed would surface as either a
        // duplicate-name error or an extra Person row at the next id.
        // Because schema_active("brain") is already set, the next open
        // must NOT re-run the system schema seed.
        {
            let db = redb::Database::create(&path).unwrap();
            let wtxn = db.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
                let row = EntityTypeDefinition::new(
                    brain_core::EntityTypeId(42),
                    "UserCustomNoun".into(),
                    Vec::new(),
                    1_700_000_000_000_000_000,
                );
                t.insert(&42u32, &row).unwrap();
            }
            wtxn.commit().unwrap();
        }

        // Re-open: seed must be a no-op. The seed gates on
        // `schema_active("brain")`, not on table emptiness.
        let db = MetadataDb::open(&path).unwrap();
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
        let mut rows: Vec<u32> = t.iter().unwrap().map(|e| e.unwrap().0.value()).collect();
        rows.sort_unstable();
        // The six built-in nouns (ids 1..=6) from the first seed plus
        // the injected user row at id 42. No re-registration.
        assert_eq!(
            rows,
            vec![1, 2, 3, 4, 5, 6, 42],
            "second open must not re-seed",
        );
    }

    // -----------------------------------------------------------------
    // Built-in relation type seeding.
    // -----------------------------------------------------------------

    #[test]
    fn builtin_relation_types_seeded_on_fresh_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(db_path(&dir)).unwrap();

        let rtxn = db.read_txn().unwrap();
        let got =
            crate::relation::types::relation_type_lookup_by_qname(&rtxn, "brain", "related_to")
                .unwrap()
                .expect("brain:related_to seeded");
        assert_eq!(got.canonical(), "brain:related_to");
        assert_eq!(got.cardinality, brain_core::Cardinality::ManyToMany);
        assert!(got.is_symmetric);
        assert!(got.from_type.is_none());
        assert!(got.to_type.is_none());
    }

    #[test]
    fn builtin_relation_types_seed_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        drop(MetadataDb::open(&path).unwrap());
        let db = MetadataDb::open(&path).unwrap();

        let rtxn = db.read_txn().unwrap();
        let all = crate::relation::types::relation_type_list(&rtxn, Some("brain")).unwrap();
        // The system schema seeds 4 brain relation types
        // (`related_to`, `reports_to`, `co_authored`, `family_of`).
        // Idempotent on reopen — count stays at 4, not 8.
        assert_eq!(all.len(), 4, "re-open must not duplicate built-in seeds");
    }
}
