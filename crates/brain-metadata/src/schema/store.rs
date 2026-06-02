//! Per-namespace schema persistence.
//!
//! Single transactional path for the four schema-management opcodes:
//!
//! - `SCHEMA_UPLOAD` → [`schema_upload`]: bumps the active version
//!   counter for the namespace and persists the parsed AST.
//! - `SCHEMA_GET` → [`schema_get`]: by `(namespace, version)`.
//! - `SCHEMA_LIST` → [`schema_list`]: newest-first.
//! - `SCHEMA_VALIDATE` → not in storage; the wire handler composes
//!   `parse_schema` + `validate` + [`schema_active`] (for the
//!   would-be-next version hint).
//!
//! Migration-time compatibility checks are out of scope.

use brain_protocol::schema::ValidatedSchema;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use super::apply::{apply_schema_definitions, SchemaApplyError};
use super::predicate::PredicateOpError;
use crate::tables::schema_version::{
    SchemaVersionRow, SCHEMA_ACTIVE_VERSIONS_TABLE, SCHEMA_VERSIONS_TABLE, VALIDATOR_VERSION,
};

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum SchemaStoreError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("schema_version overflow for namespace {namespace:?}")]
    VersionOverflow { namespace: String },

    #[error("json encode failed: {0}")]
    Encode(String),

    #[error("schema apply: {0}")]
    Apply(#[from] SchemaApplyError),

    #[error("predicate op while flagging pre-existing rows: {0}")]
    Predicate(#[from] PredicateOpError),
}

// ---------------------------------------------------------------------------
// Writes.
// ---------------------------------------------------------------------------

/// Persist a validated schema as a new version of its namespace.
///
/// - Reads the namespace's current active version, increments by 1.
/// - Writes the version row to `SCHEMA_VERSIONS_TABLE`.
/// - Updates `SCHEMA_ACTIVE_VERSIONS_TABLE` to point at the new
///   version.
///
/// Atomicity: both writes live inside the caller's `wtxn`; on
/// commit they apply together. On rollback (caller dropping the
/// txn) neither row appears.
///
/// Returns the new version number.
pub fn schema_upload(
    wtxn: &WriteTransaction,
    validated: &ValidatedSchema,
    now_unix_nanos: u64,
) -> Result<u32, SchemaStoreError> {
    let schema = validated.as_schema();
    let namespace = schema.namespace.clone();
    let new_version = next_version_in(wtxn, &namespace)?;

    let source = serde_json::to_vec(schema).map_err(|e| SchemaStoreError::Encode(e.to_string()))?;
    let row = SchemaVersionRow {
        namespace: namespace.clone(),
        version: new_version,
        uploaded_at_unix_nanos: now_unix_nanos,
        source,
        source_text: schema.source.clone(),
        validator_version: VALIDATOR_VERSION,
    };

    {
        let mut versions = wtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
        versions.insert(&(namespace.as_str(), new_version), &row)?;
    }
    {
        let mut active = wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
        active.insert(&namespace.as_str(), &new_version)?;
    }

    // Fan out new + changed definitions into the existing
    // entity_type / predicate / relation_type intern paths.
    apply_schema_definitions(wtxn, validated, new_version, now_unix_nanos)?;

    // The OUTSIDE_ACTIVE_SCHEMA flag-sweep runs **outside** this
    // wtxn — the writer's post-commit fan-out enqueues a
    // `SchemaFlagSweepJob` to the SchemaMigrationWorker, which opens
    // its own wtxn against the just-committed schema state. Doing the
    // sweep inline would couple SCHEMA_UPLOAD ack latency to a full
    // STATEMENTS_TABLE scan; moving it post-commit keeps the upload
    // commit bounded while the worker catches up within the next tick.

    // `namespace` was cloned above to construct the row; suppress the
    // unused-binding lint now that the inline sweep is gone.
    let _ = namespace;

    Ok(new_version)
}

fn next_version_in(wtxn: &WriteTransaction, namespace: &str) -> Result<u32, SchemaStoreError> {
    let active = wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let guard = active.get(&namespace)?;
    let current: Option<u32> = guard.map(|g| g.value());
    drop(active);
    match current {
        Some(v) => v
            .checked_add(1)
            .ok_or_else(|| SchemaStoreError::VersionOverflow {
                namespace: namespace.to_string(),
            }),
        None => Ok(1),
    }
}

// ---------------------------------------------------------------------------
// Reads.
// ---------------------------------------------------------------------------

/// Fetch a specific version of a namespace's schema. Returns
/// `Ok(None)` if the row doesn't exist.
pub fn schema_get(
    rtxn: &ReadTransaction,
    namespace: &str,
    version: u32,
) -> Result<Option<SchemaVersionRow>, SchemaStoreError> {
    let versions = rtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
    let guard = versions.get(&(namespace, version))?;
    Ok(guard.map(|g| g.value()))
}

/// Fetch the active version number for a namespace.
pub fn schema_active(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Option<u32>, SchemaStoreError> {
    let active = rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let guard = active.get(&namespace)?;
    Ok(guard.map(|g| g.value()))
}

/// Fetch the active version's row in a single call.
pub fn schema_active_row(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Option<SchemaVersionRow>, SchemaStoreError> {
    let Some(v) = schema_active(rtxn, namespace)? else {
        return Ok(None);
    };
    schema_get(rtxn, namespace, v)
}

/// All versions for a namespace, **newest first**.
pub fn schema_list(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Vec<SchemaVersionRow>, SchemaStoreError> {
    let versions = rtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
    let lo = (namespace, 0u32);
    let hi = (namespace, u32::MAX);
    let mut out = Vec::new();
    for entry in versions.range(lo..=hi)? {
        let (_k, v) = entry?;
        out.push(v.value());
    }
    out.reverse();
    Ok(out)
}

/// All namespaces with at least one active schema.
pub fn schema_namespaces(rtxn: &ReadTransaction) -> Result<Vec<String>, SchemaStoreError> {
    let active = rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let mut out = Vec::new();
    for entry in active.iter()? {
        let (k, _v) = entry?;
        out.push(k.value().to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_protocol::schema::{parse_schema, validate};
    use redb::{Database, ReadableDatabase};

    fn open_db(dir: &tempfile::TempDir) -> Database {
        let db = Database::create(dir.path().join("test.redb")).unwrap();
        let wtxn = db.begin_write().unwrap();
        crate::tables::materialize_all_tables(&wtxn).unwrap();
        wtxn.commit().unwrap();
        db
    }

    fn validated(src: &str) -> ValidatedSchema {
        let schema = parse_schema(src).expect("parse");
        validate(&schema).expect("validate")
    }

    fn acme_schema_v1() -> ValidatedSchema {
        validated(
            "
            namespace acme
            define entity_type Person { attributes {} }
            ",
        )
    }

    fn acme_schema_v2() -> ValidatedSchema {
        validated(
            "
            namespace acme
            define entity_type Person { attributes {} }
            define predicate prefers { kind: Preference object: Value<text> }
            ",
        )
    }

    fn crm_schema() -> ValidatedSchema {
        validated(
            "
            namespace crm
            define entity_type Lead { attributes {} }
            ",
        )
    }

    #[test]
    fn first_upload_is_version_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        let v = schema_upload(&wtxn, &acme_schema_v1(), 1_700_000_000_000_000_000).unwrap();
        assert_eq!(v, 1);
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(1));
        assert!(schema_get(&rtxn, "acme", 1).unwrap().is_some());
    }

    #[test]
    fn second_upload_bumps_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        {
            let wtxn = db.begin_write().unwrap();
            schema_upload(&wtxn, &acme_schema_v1(), 1).unwrap();
            wtxn.commit().unwrap();
        }
        let v2 = {
            let wtxn = db.begin_write().unwrap();
            let v = schema_upload(&wtxn, &acme_schema_v2(), 2).unwrap();
            wtxn.commit().unwrap();
            v
        };
        assert_eq!(v2, 2);

        let rtxn = db.begin_read().unwrap();
        assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(2));
        // v1 still readable.
        assert!(schema_get(&rtxn, "acme", 1).unwrap().is_some());
        assert!(schema_get(&rtxn, "acme", 2).unwrap().is_some());
    }

    #[test]
    fn schema_get_missing_version_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let rtxn = db.begin_read().unwrap();
        assert!(schema_get(&rtxn, "acme", 7).unwrap().is_none());
        assert_eq!(schema_active(&rtxn, "acme").unwrap(), None);
    }

    #[test]
    fn schema_list_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        for (i, s) in [acme_schema_v1(), acme_schema_v2()].iter().enumerate() {
            let wtxn = db.begin_write().unwrap();
            schema_upload(&wtxn, s, i as u64).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let list = schema_list(&rtxn, "acme").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].version, 2);
        assert_eq!(list[1].version, 1);
    }

    #[test]
    fn namespaces_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        for s in [acme_schema_v1(), acme_schema_v2(), crm_schema()] {
            let wtxn = db.begin_write().unwrap();
            schema_upload(&wtxn, &s, 1).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(2));
        assert_eq!(schema_active(&rtxn, "crm").unwrap(), Some(1));
        let nss = schema_namespaces(&rtxn).unwrap();
        assert!(nss.contains(&"acme".to_string()));
        assert!(nss.contains(&"crm".to_string()));
    }

    #[test]
    fn active_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = open_db(&dir);
            let wtxn = db.begin_write().unwrap();
            schema_upload(&wtxn, &acme_schema_v1(), 1).unwrap();
            wtxn.commit().unwrap();
        }
        let db = Database::open(dir.path().join("test.redb")).unwrap();
        let rtxn = db.begin_read().unwrap();
        assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(1));
    }

    #[test]
    fn schema_upload_does_not_run_inline_flag_sweep() {
        // The OUTSIDE_ACTIVE_SCHEMA flag-sweep is post-commit work
        // owned by the SchemaMigrationWorker — `schema_upload` must
        // never mutate flags inside its own wtxn. This test pins that
        // contract: a pre-existing statement against an
        // open-vocabulary predicate stays clean immediately after
        // `schema_upload` commits a schema that doesn't declare its
        // predicate. The worker (driven by the writer's post-commit
        // enqueue) is what eventually sets the flag.
        use crate::schema::apply::flag_statements_outside_schema;
        use crate::schema::predicate::{predicate_intern_or_get, predicates_active_for_schema};
        use crate::statement::crud::statement_create;
        use crate::tables::statement::{statement_flags, STATEMENTS_TABLE};
        use brain_core::{ContextId, EntityId, ExtractorId, MemoryId, StatementId, StatementKind};
        use brain_core::{
            EvidenceEntry, EvidenceRef, Statement, StatementObject, StatementValue, SubjectRef,
        };

        let dir = tempfile::tempdir().unwrap();
        // Use the seeded wrapper so EntityTypeId(1) (Person) exists.
        let md = crate::MetadataDb::open(dir.path().join("test.redb")).unwrap();
        let db = md.db();

        // Pre-existing schemaless world: intern two predicates and
        // write a statement for each.
        let subject = EntityId::new();
        let (sid_inside, sid_outside) = {
            let wtxn = db.begin_write().unwrap();
            // Ensure the subject exists.
            use crate::entity::ops::entity_put;
            use brain_core::Entity;
            let now = 0u64;
            entity_put(
                &wtxn,
                &Entity::new_active(
                    subject,
                    brain_core::EntityTypeId(1),
                    "anchor".into(),
                    "anchor".into(),
                    now,
                ),
            )
            .unwrap();

            let p_in = predicate_intern_or_get(&wtxn, "acme", "prefers", 0, 0).unwrap();
            let p_out = predicate_intern_or_get(&wtxn, "acme", "ghost", 0, 0).unwrap();
            let mk_stmt = |pid| {
                let id = StatementId::new();
                let evidence_entry = EvidenceEntry::from_parts(
                    MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
                    1.0,
                    0,
                    ExtractorId::default(),
                );
                Statement::new_root(
                    id,
                    StatementKind::Fact,
                    SubjectRef::Entity(subject),
                    pid,
                    StatementObject::Value(StatementValue::Text("x".into())),
                    0.9,
                    EvidenceRef::inline_from_slice(&[evidence_entry]),
                    ExtractorId::default(),
                    0,
                    1,
                )
            };
            let s_in = mk_stmt(p_in);
            let s_out = mk_stmt(p_out);
            let sid_in = statement_create(&wtxn, &s_in, 0).unwrap();
            let sid_out = statement_create(&wtxn, &s_out, 0).unwrap();
            wtxn.commit().unwrap();
            (sid_in, sid_out)
        };

        // Upload a schema that declares only `prefers`. Immediately
        // after commit, NEITHER row should be flagged — the worker
        // hasn't run yet.
        {
            let wtxn = db.begin_write().unwrap();
            schema_upload(&wtxn, &acme_schema_v2(), 0).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let rtxn = db.begin_read().unwrap();
            let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
            let inside_pre = t.get(&sid_inside.to_bytes()).unwrap().unwrap().value();
            let outside_pre = t.get(&sid_outside.to_bytes()).unwrap().unwrap().value();
            assert!(
                !inside_pre.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA),
                "in-vocab row must not be flagged inline: flags={:#b}",
                inside_pre.flags,
            );
            assert!(
                !outside_pre.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA),
                "out-of-vocab row must not be flagged inline either — the sweep is post-commit: flags={:#b}",
                outside_pre.flags,
            );
        }

        // Now drive the sweep directly (mirroring what the worker
        // does on its next tick). The contract: in-vocab stays clean,
        // out-of-vocab gains the flag.
        let active = {
            let rtxn = db.begin_read().unwrap();
            predicates_active_for_schema(&rtxn, "acme", 1).unwrap()
        };
        {
            let wtxn = db.begin_write().unwrap();
            flag_statements_outside_schema(&wtxn, "acme", &active).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
        let inside = t.get(&sid_inside.to_bytes()).unwrap().unwrap().value();
        let outside = t.get(&sid_outside.to_bytes()).unwrap().unwrap().value();
        assert!(
            !inside.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA),
            "in-vocabulary statement must not be flagged post-sweep: flags={:#b}",
            inside.flags,
        );
        assert!(
            outside.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA),
            "out-of-vocabulary statement must be flagged post-sweep: flags={:#b}",
            outside.flags,
        );
    }

    #[test]
    fn schema_active_row_returns_full_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        schema_upload(&wtxn, &acme_schema_v2(), 42).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let row = schema_active_row(&rtxn, "acme").unwrap().unwrap();
        assert_eq!(row.version, 1);
        assert_eq!(row.namespace, "acme");
        assert_eq!(row.uploaded_at_unix_nanos, 42);
        assert_eq!(row.validator_version, VALIDATOR_VERSION);
        assert!(row.source_text.is_some());
        // Source is JSON; decode round-trips.
        let decoded: brain_protocol::schema::Schema = serde_json::from_slice(&row.source).unwrap();
        assert_eq!(decoded.namespace, "acme");
    }
}
