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

use redb::{
    Database, ReadTransaction, ReadableDatabase, ReadableTable, TransactionError, WriteTransaction,
};

use crate::predicate_ops::{predicate_intern, PredicateOpError};
use crate::relation_type_ops::{relation_type_intern, RelationTypeOpError};
use crate::schema::{open_or_init_schema, SchemaError};
use crate::tables::checkpoint::{latest as latest_checkpoint, CHECKPOINTS_TABLE};
use crate::tables::knowledge::entity_type::{EntityTypeDefinition, ENTITY_TYPES_TABLE};
use brain_core::knowledge::StatementKind;
use brain_core::Cardinality;
use brain_core::EntityType;

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

/// Seed the built-in entity types if the registry is empty.
///
/// Sub-task 16.1. Inserts a `Person` row with `EntityTypeId(1)` when
/// no entity types exist yet. Idempotent: if any row is present (test
/// fixture, prior open, or phase-19 user upload), this is a no-op.
fn seed_builtin_entity_types(db: &Database) -> Result<(), MetadataDbError> {
    // Cheap empty-check in a read txn; only escalate to a write txn
    // if we actually need to seed.
    let registry_is_empty = {
        let rtxn = db.begin_read()?;
        match rtxn.open_table(ENTITY_TYPES_TABLE) {
            Ok(t) => t
                .first()
                .map_err(|e| MetadataDbError::Schema(SchemaError::Storage(e)))?
                .is_none(),
            // Table not yet materialized → counts as empty; will be
            // created by the write txn below.
            Err(redb::TableError::TableDoesNotExist(_)) => true,
            Err(e) => return Err(MetadataDbError::Schema(SchemaError::from(e))),
        }
    };
    if !registry_is_empty {
        return Ok(());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let person = EntityType::person(now);
    let row = EntityTypeDefinition::new(
        person.id,
        person.name,
        person.attribute_schema_blob,
        person.created_at_unix_nanos,
    );

    let wtxn = db.begin_write()?;
    {
        let mut t = wtxn
            .open_table(ENTITY_TYPES_TABLE)
            .map_err(|e| MetadataDbError::Schema(SchemaError::from(e)))?;
        t.insert(&row.entity_type_id, &row)
            .map_err(|e| MetadataDbError::Schema(SchemaError::Storage(e)))?;
    }
    wtxn.commit()
        .map_err(|e| MetadataDbError::Schema(SchemaError::Commit(e)))?;
    Ok(())
}

/// Built-in predicates seeded at `MetadataDb::open` per spec §19/00
/// §"Built-in predicates". User-declared predicates land via phase
/// 19's `SCHEMA_UPLOAD`.
///
/// `(namespace, name, kind_constraint, object_type_constraint_byte,
///  description)`.
///
/// `object_type_constraint_byte`: `0` = any / `1` = Entity / `2` =
/// Value / `3` = Memory / `4` = Statement (matches
/// `StatementObject::discriminant()` offset by 1).
const BUILTIN_PREDICATES: &[(&str, &str, Option<StatementKind>, u8, &str)] = &[
    (
        "brain",
        "is_a",
        Some(StatementKind::Fact),
        1, // Entity
        "Subject is an instance of the object entity type.",
    ),
    (
        "brain",
        "has_name",
        Some(StatementKind::Fact),
        2, // Value
        "Subject's canonical name as a text value.",
    ),
    (
        "brain",
        "mentions",
        Some(StatementKind::Fact),
        0, // any
        "Generic mention — subject mentions object.",
    ),
    (
        "brain",
        "related_to",
        Some(StatementKind::Fact),
        1, // Entity
        "Generic relation between subject entity and object entity.",
    ),
    // 17.10a — enable integration-test coverage of Preference / Event
    // kinds without a SCHEMA_UPLOAD path (phase 19). Generic enough
    // that users picking these qnames is unlikely; user schemas pick
    // their own predicates in their own namespace.
    (
        "brain",
        "prefers",
        Some(StatementKind::Preference),
        2, // Value
        "Generic Preference about the subject (any value).",
    ),
    (
        "brain",
        "scheduled",
        Some(StatementKind::Event),
        0, // any object
        "Generic Event scheduled at event_at_unix_nanos.",
    ),
];

/// Seed the built-in `brain:*` predicates idempotently. Sub-task 17.3.
///
/// Walks the [`BUILTIN_PREDICATES`] catalog; each entry is interned via
/// [`predicate_intern`], which is itself idempotent when the row
/// already matches and refuses to clobber when constraints differ.
/// `seed_builtin_predicates` therefore leaves pre-existing rows alone
/// (test fixtures, prior opens, future user schemas that import a
/// `brain:*` predicate verbatim) and never overwrites diverging shapes.
/// Built-in relation types seeded at `MetadataDb::open` per spec
/// §20/00 §"Built-in" + §29/00 phase-scope. Phase 18.3.
///
/// `(namespace, name, cardinality, is_symmetric, description)`.
/// `from_type` / `to_type` are both `None` (any entity type).
const BUILTIN_RELATION_TYPES: &[(&str, &str, Cardinality, bool, &str)] = &[
    (
        "brain",
        "related_to",
        Cardinality::ManyToMany,
        false,
        "Generic relation between two entities.",
    ),
    // 18.9a — enable integration-test coverage of cardinality and
    // symmetric paths without a SCHEMA_UPLOAD path (phase 19).
    (
        "brain",
        "reports_to",
        Cardinality::ManyToOne,
        false,
        "Generic ManyToOne relation; second create on same `from` auto-supersedes.",
    ),
    (
        "brain",
        "co_authored",
        Cardinality::ManyToMany,
        true,
        "Generic symmetric ManyToMany relation; canonicalises from/to byte-wise.",
    ),
];

/// Seed the built-in `brain:*` relation types idempotently. Mirrors
/// `seed_builtin_predicates` (17.3). Pre-existing rows with
/// diverging shapes are preserved — never overwritten.
fn seed_builtin_relation_types(db: &Database) -> Result<(), MetadataDbError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let wtxn = db.begin_write()?;
    for (ns, name, cardinality, is_symmetric, desc) in BUILTIN_RELATION_TYPES {
        match relation_type_intern(
            &wtxn,
            ns,
            name,
            None,
            None,
            *cardinality,
            *is_symmetric,
            /* schema_version */ 1,
            desc,
            now,
        ) {
            Ok(_) => {}
            Err(RelationTypeOpError::AlreadyExists { .. }) => {}
            Err(e) => return Err(MetadataDbError::BuiltinRelationTypeSeed(e)),
        }
    }
    wtxn.commit()
        .map_err(|e| MetadataDbError::Schema(SchemaError::Commit(e)))?;
    Ok(())
}

fn seed_builtin_predicates(db: &Database) -> Result<(), MetadataDbError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let wtxn = db.begin_write()?;
    for (ns, name, kind_constraint, obj_type, desc) in BUILTIN_PREDICATES {
        match predicate_intern(
            &wtxn,
            ns,
            name,
            *kind_constraint,
            *obj_type,
            /* schema_version */ 1,
            desc,
            now,
        ) {
            Ok(_) => {}
            // AlreadyExists with diverging constraints: leave the
            // pre-existing row alone — never overwrite a user/test
            // shape. Spec §19/00 leaves the precedence question open;
            // the conservative choice for v1 is to preserve.
            Err(PredicateOpError::AlreadyExists { .. }) => {}
            Err(e) => return Err(MetadataDbError::BuiltinPredicateSeed(e)),
        }
    }
    wtxn.commit()
        .map_err(|e| MetadataDbError::Schema(SchemaError::Commit(e)))?;
    Ok(())
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

    /// Seeding a built-in predicate failed at `MetadataDb::open`.
    #[error("built-in predicate seed: {0}")]
    BuiltinPredicateSeed(PredicateOpError),

    /// Seeding a built-in relation type failed at `MetadataDb::open`.
    #[error("built-in relation type seed: {0}")]
    BuiltinRelationTypeSeed(RelationTypeOpError),
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

        // Sub-task 16.1: seed a built-in `Person` `EntityTypeDefinition`
        // if the registry is empty. Phase 19's `SCHEMA_UPLOAD` owns the
        // registry once user-declared types arrive; user types start at
        // EntityTypeId(2)+ so this slot stays stable. Idempotent — any
        // pre-existing row (test fixture or prior open) skips the seed.
        seed_builtin_entity_types(&db)?;

        // Sub-task 17.3: seed `brain:is_a` / `brain:has_name` /
        // `brain:mentions` / `brain:related_to` predicates. Each
        // intern is idempotent on identical constraints; diverging
        // rows are preserved. Phase 19's `SCHEMA_UPLOAD` registers
        // user predicates against this same registry.
        seed_builtin_predicates(&db)?;

        // Sub-task 18.3: seed `brain:related_to` relation type
        // (any→any ManyToMany asymmetric). Same idempotency
        // semantics as predicate seeding.
        seed_builtin_relation_types(&db)?;

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

    // -----------------------------------------------------------------
    // Sub-task 16.1: Person bootstrap.
    // -----------------------------------------------------------------

    #[test]
    fn person_entity_type_seeded_on_fresh_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(db_path(&dir)).unwrap();

        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
        let mut rows = 0u32;
        let mut saw_person = false;
        for entry in t.iter().unwrap() {
            let (_k, v) = entry.unwrap();
            let def = v.value();
            rows += 1;
            if def.id() == EntityType::PERSON_ID && def.name == EntityType::PERSON_NAME {
                saw_person = true;
            }
        }
        assert_eq!(rows, 1, "fresh open should seed exactly one entity type");
        assert!(saw_person, "the seeded row must be the Person row");
    }

    #[test]
    fn person_seed_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);

        // First open seeds.
        drop(MetadataDb::open(&path).unwrap());
        // Second open must NOT add another row.
        let db = MetadataDb::open(&path).unwrap();
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
        let rows: usize = t.iter().unwrap().count();
        assert_eq!(rows, 1, "re-open must not duplicate the Person seed");
    }

    #[test]
    fn person_seed_skipped_when_registry_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);

        // Pre-seed a NON-Person type with id=42 before opening
        // MetadataDb.
        {
            let db = redb::Database::create(&path).unwrap();
            let wtxn = db.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
                let row = EntityTypeDefinition::new(
                    brain_core::EntityTypeId(42),
                    "Project".into(),
                    Vec::new(),
                    1_700_000_000_000_000_000,
                );
                t.insert(&42u32, &row).unwrap();
            }
            wtxn.commit().unwrap();
        }

        // Open. seed_builtin_entity_types should detect non-empty
        // registry and skip the Person insert.
        let db = MetadataDb::open(&path).unwrap();
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
        let rows: Vec<u32> = t
            .iter()
            .unwrap()
            .map(|e| e.unwrap().0.value())
            .collect();
        assert_eq!(rows, vec![42], "Person seed must skip when registry has rows");
    }

    // -----------------------------------------------------------------
    // Sub-task 18.3: built-in relation type seeding.
    // -----------------------------------------------------------------

    #[test]
    fn builtin_relation_types_seeded_on_fresh_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(db_path(&dir)).unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = crate::relation_type_ops::relation_type_lookup_by_qname(
            &rtxn,
            "brain",
            "related_to",
        )
        .unwrap()
        .expect("brain:related_to seeded");
        assert_eq!(got.canonical(), "brain:related_to");
        assert_eq!(got.cardinality, brain_core::Cardinality::ManyToMany);
        assert!(!got.is_symmetric);
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
        let all = crate::relation_type_ops::relation_type_list(&rtxn, Some("brain")).unwrap();
        assert_eq!(all.len(), 1, "re-open must not duplicate built-in seeds");
    }
}
