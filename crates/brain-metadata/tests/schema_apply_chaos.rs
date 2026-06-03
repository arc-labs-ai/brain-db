//! Atomicity / chaos coverage for the `SCHEMA_UPLOAD` write path.
//!
//! `schema_upload` performs three storage actions inside one redb
//! `WriteTransaction`:
//!
//! 1. Insert the version row into `SCHEMA_VERSIONS_TABLE`.
//! 2. Update the `(namespace → version)` pointer in
//!    `SCHEMA_ACTIVE_VERSIONS_TABLE`.
//! 3. Fan-out via `apply_schema_definitions` — predicate / relation
//!    type / entity type / extractor interns.
//!
//! The `OUTSIDE_ACTIVE_SCHEMA` flag-sweep is **not** part of this
//! wtxn — it runs post-commit in the SchemaMigrationWorker — so the
//! atomicity contract here covers just the three definition writes.
//!
//! If the caller's process were killed before commit, redb's WAL
//! semantics guarantee none of the three are observable. These tests
//! simulate that crash by dropping the `WriteTransaction` without
//! calling `commit()` after each phase. The DB is a raw `redb::Database`
//! (no `MetadataDb::open` seeding) so id allocation observations stay
//! deterministic.

use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::schema::store::{schema_active, schema_get, schema_upload};
use brain_metadata::tables::materialize_all_tables;
use brain_metadata::tables::predicate::{PredicateDefinition, PREDICATES_TABLE};
use brain_metadata::tables::schema_version::{SCHEMA_ACTIVE_VERSIONS_TABLE, SCHEMA_VERSIONS_TABLE};
use brain_protocol::schema::{parse_schema, validate, ValidatedSchema};
use redb::{Database, ReadableDatabase, ReadableTable};

const T0: u64 = 1_700_000_000_000_000_000;

/// A raw `redb::Database` with every table materialized but **no**
/// `MetadataDb::open` seeding — so schema/predicate id allocation stays
/// deterministic while the read paths see the same "tables always exist"
/// contract production guarantees (`storage_version::open_or_init_schema`
/// → `materialize_all_tables`). Without materialization, a read after a
/// dropped (never-committed) upload would hit `TableDoesNotExist`, which
/// the read helpers deliberately don't paper over.
fn fresh_db(dir: &tempfile::TempDir) -> Database {
    let db = Database::create(dir.path().join("test.redb")).expect("create redb");
    let wtxn = db.begin_write().expect("begin_write");
    materialize_all_tables(&wtxn).expect("materialize tables");
    wtxn.commit().expect("commit materialize");
    db
}

fn validated_schema(src: &str) -> ValidatedSchema {
    let s = parse_schema(src).expect("parse");
    validate(&s).expect("validate")
}

fn acme_v1() -> ValidatedSchema {
    validated_schema(
        "
        namespace acme
        define entity_type Person { attributes {} }
        define predicate prefers { kind: Preference object: Value<text> }
        ",
    )
}

fn acme_v2_extends_v1() -> ValidatedSchema {
    // v2 keeps every v1 definition unchanged AND adds a new one.
    // Re-applying an unchanged predicate at the new schema_version
    // fails on constraint mismatch (schema_version field differs),
    // so this schema is structured the same way as the existing
    // `schema_store::tests::acme_schema_v2` — additive only.
    validated_schema(
        "
        namespace acme
        define entity_type Person { attributes {} }
        define predicate prefers { kind: Preference object: Value<text> }
        define predicate dislikes { kind: Preference object: Value<text> }
        ",
    )
}

// ---------------------------------------------------------------------------
// All-or-nothing: dropping the txn before commit leaves the DB clean.
// ---------------------------------------------------------------------------

#[test]
fn dropped_txn_leaves_no_version_row_and_no_active_pointer() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);

    // Step 1: run schema_upload inside a wtxn, then drop without commit.
    {
        let wtxn = db.begin_write().unwrap();
        let v = schema_upload(&wtxn, &acme_v1(), T0).expect("upload runs");
        assert_eq!(v, 1, "first upload allocates v1 in-txn");
        // Intentional: no commit. `wtxn` drops here.
    }

    // Step 2: a fresh read transaction sees no version row and no
    // active pointer.
    let rtxn = db.begin_read().unwrap();
    assert_eq!(
        schema_active(&rtxn, "acme").unwrap(),
        None,
        "active pointer must not be observable post-drop",
    );
    assert!(
        schema_get(&rtxn, "acme", 1).unwrap().is_none(),
        "version row must not be observable post-drop",
    );

    // The underlying tables are either empty or never opened. Open
    // them read-only and confirm zero entries.
    if let Ok(t) = rtxn.open_table(SCHEMA_VERSIONS_TABLE) {
        assert_eq!(t.iter().unwrap().count(), 0, "no schema versions persisted");
    }
    if let Ok(t) = rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE) {
        assert_eq!(t.iter().unwrap().count(), 0, "no active pointer persisted");
    }
}

#[test]
fn dropped_txn_leaves_no_interned_predicates() {
    // `apply_schema_definitions` is the third of four phases inside
    // schema_upload — it interns each predicate / relation-type the
    // schema declares. If we abort post-fan-out (before commit), the
    // interns must vanish too.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);

    {
        let wtxn = db.begin_write().unwrap();
        let v = schema_upload(&wtxn, &acme_v1(), T0).unwrap();
        assert_eq!(v, 1);
        // Drop without commit.
    }

    // After drop, the predicate registry must NOT contain the
    // schema-declared `prefers` row. Round-trip-intern with an
    // open-vocabulary call on a fresh raw DB: id allocation starts
    // at 1.
    let wtxn = db.begin_write().unwrap();
    let id = predicate_intern_or_get(&wtxn, "acme", "prefers", 0, 0).unwrap();
    wtxn.commit().unwrap();
    assert_eq!(
        id.raw(),
        1,
        "post-drop registry must be empty — first intern must allocate id=1",
    );

    // And the freshly interned row must have ImplicitFromWrite origin
    // (not SchemaDeclared) — proof the aborted upload didn't leak.
    let rtxn = db.begin_read().unwrap();
    let t = rtxn.open_table(PREDICATES_TABLE).unwrap();
    let row: PredicateDefinition = t.get(&id.raw()).unwrap().unwrap().value();
    assert!(
        !row.origin().is_schema_declared(),
        "aborted upload must not have left a SchemaDeclared row",
    );
}

// ---------------------------------------------------------------------------
// Replay safety: abort, retry — versions don't ghost-increment.
// ---------------------------------------------------------------------------

#[test]
fn replay_after_abort_lands_at_v1_not_v2() {
    // Simulates: operator runs SCHEMA_UPLOAD, server crashes pre-commit,
    // operator retries. Expected: the retry is the *first* committed
    // upload, lands as v1, not v2 (no ghost increment from the aborted
    // attempt).
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);

    // Aborted attempt 1.
    {
        let wtxn = db.begin_write().unwrap();
        let _ = schema_upload(&wtxn, &acme_v1(), T0).unwrap();
        // Drop.
    }
    // Aborted attempt 2.
    {
        let wtxn = db.begin_write().unwrap();
        let _ = schema_upload(&wtxn, &acme_v1(), T0 + 1).unwrap();
        // Drop.
    }

    // Successful third attempt.
    {
        let wtxn = db.begin_write().unwrap();
        let v = schema_upload(&wtxn, &acme_v1(), T0 + 2).unwrap();
        assert_eq!(v, 1, "first *committed* upload must be v1");
        wtxn.commit().unwrap();
    }

    let rtxn = db.begin_read().unwrap();
    assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(1));
    assert!(schema_get(&rtxn, "acme", 1).unwrap().is_some());
    assert!(
        schema_get(&rtxn, "acme", 2).unwrap().is_none(),
        "no ghost v2 row from aborted attempts",
    );
}

#[test]
fn committed_v1_then_aborted_v2_leaves_active_at_v1() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);

    {
        let wtxn = db.begin_write().unwrap();
        schema_upload(&wtxn, &acme_v1(), T0).unwrap();
        wtxn.commit().unwrap();
    }

    // Aborted v2 upload (additive: keeps v1's definitions, adds
    // `dislikes` — same shape as the existing `acme_schema_v2`
    // fixture in `schema_store::tests`). The in-txn call succeeds
    // (re-declaring `prefers` at a newer version with unchanged
    // constraints bumps the row in-place), but the txn is dropped
    // before commit — atomicity must ensure nothing persists.
    {
        let wtxn = db.begin_write().unwrap();
        let _ = schema_upload(&wtxn, &acme_v2_extends_v1(), T0 + 1);
        // Drop without commit.
    }

    // Active pointer must still be v1; v2 row must not exist.
    let rtxn = db.begin_read().unwrap();
    assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(1));
    assert!(schema_get(&rtxn, "acme", 2).unwrap().is_none());

    // The new predicate (`dislikes`) declared only in the aborted v2
    // must not be visible in the predicate registry.
    let t = rtxn.open_table(PREDICATES_TABLE).unwrap();
    let mut seen_dislikes = false;
    for entry in t.iter().unwrap() {
        let (_, v) = entry.unwrap();
        let row = v.value();
        if row.namespace == "acme" && row.name == "dislikes" {
            seen_dislikes = true;
        }
    }
    assert!(
        !seen_dislikes,
        "aborted v2 upload must not have left a `dislikes` row",
    );
}

// ---------------------------------------------------------------------------
// Multi-write inside one wtxn: all writes inside `schema_upload` plus
// caller's extra writes share one fate.
// ---------------------------------------------------------------------------

#[test]
fn caller_writes_share_atomicity_with_schema_upload() {
    // The atomicity guarantee extends to the caller's wtxn: any extra
    // writes the caller performs inside the same `WriteTransaction`
    // commit or roll back together with the schema upload.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);

    {
        let wtxn = db.begin_write().unwrap();
        schema_upload(&wtxn, &acme_v1(), T0).unwrap();
        // Caller writes an unrelated predicate in the same txn.
        predicate_intern_or_get(&wtxn, "other", "noted", 0, T0).unwrap();
        // Drop without commit.
    }

    // Neither side-effect persists.
    let rtxn = db.begin_read().unwrap();
    assert_eq!(schema_active(&rtxn, "acme").unwrap(), None);
    if let Ok(t) = rtxn.open_table(PREDICATES_TABLE) {
        for entry in t.iter().unwrap() {
            let (_, v) = entry.unwrap();
            let row = v.value();
            assert_ne!(
                row.namespace, "acme",
                "no `acme` predicates may remain post-drop",
            );
            assert_ne!(
                row.namespace, "other",
                "no `other` predicates may remain post-drop",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Committed upload IS observable — sanity bookend so a regression that
// drops every write (even committed ones) doesn't slip past the
// abort-only assertions above.
// ---------------------------------------------------------------------------

#[test]
fn committed_upload_is_fully_observable() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    {
        let wtxn = db.begin_write().unwrap();
        let v = schema_upload(&wtxn, &acme_v1(), T0).unwrap();
        assert_eq!(v, 1);
        wtxn.commit().unwrap();
    }
    let rtxn = db.begin_read().unwrap();
    assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(1));
    let row = schema_get(&rtxn, "acme", 1).unwrap().expect("row exists");
    assert_eq!(row.namespace, "acme");
    assert_eq!(row.version, 1);

    // The predicate registry now contains a SchemaDeclared `prefers`.
    let t = rtxn.open_table(PREDICATES_TABLE).unwrap();
    let mut found = false;
    for entry in t.iter().unwrap() {
        let (_, v) = entry.unwrap();
        let row = v.value();
        if row.namespace == "acme" && row.name == "prefers" {
            assert!(
                row.origin().is_schema_declared(),
                "committed upload must produce SchemaDeclared origin",
            );
            found = true;
        }
    }
    assert!(found, "committed `prefers` predicate must be observable");
}
