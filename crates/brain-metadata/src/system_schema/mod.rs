//! System schema bootstrap (spec §21/06, phase 19.7).
//!
//! At `MetadataDb::open`, this module:
//!
//! 1. Reads the current active version for the `brain:` namespace.
//! 2. If unset (fresh DB), parses the embedded
//!    [`SYSTEM_SCHEMA_SOURCE`], validates it under the system-only
//!    relaxed validator entry point (`validate_system_schema`), and
//!    runs `schema_upload` to persist + fan out into the
//!    entity_type / predicate / relation_type intern paths.
//! 3. Idempotent: existing DBs skip the seed (`schema_active`
//!    returns `Some(_)`).
//!
//! Parse / validate failures **panic** — the source is
//! `include_str!()` content; a failure is a build bug, not a
//! runtime condition.

use brain_protocol::schema::{parse_schema, validate_system_schema};
use redb::{Database, ReadableDatabase};

use crate::schema_store::{schema_active, schema_upload, SchemaStoreError};

/// The embedded system-schema DSL source. Single source of truth
/// for the built-in `brain:*` types.
pub const SYSTEM_SCHEMA_SOURCE: &str = include_str!("schema.brain");

/// The namespace name the system schema declares. Reserved per
/// §21/04.
pub const SYSTEM_SCHEMA_NAMESPACE: &str = "brain";

#[derive(thiserror::Error, Debug)]
pub enum SystemSchemaError {
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),

    #[error("schema_store: {0}")]
    Schema(#[from] SchemaStoreError),
}

/// Seed the system schema on first open. No-op on subsequent opens.
pub fn seed_system_schema(db: &Database) -> Result<(), SystemSchemaError> {
    // Cheap pre-check before opening a write txn.
    {
        let rtxn = db.begin_read()?;
        if schema_active(&rtxn, SYSTEM_SCHEMA_NAMESPACE)?.is_some() {
            return Ok(());
        }
    }

    let schema = parse_schema(SYSTEM_SCHEMA_SOURCE)
        .expect("system schema must parse — include_str!() content is compile-time");
    let validated = validate_system_schema(&schema).unwrap_or_else(|errs| {
        panic!(
            "system schema must validate — include_str!() content is compile-time. Errors: {errs:?}"
        )
    });

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let wtxn = db.begin_write()?;
    schema_upload(&wtxn, &validated, now)?;
    wtxn.commit()?;
    Ok(())
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::schema_store::{schema_get, schema_list};
    use brain_core::EntityType;

    #[test]
    fn system_schema_parses_and_validates() {
        let schema = parse_schema(SYSTEM_SCHEMA_SOURCE)
            .expect("system schema parses");
        let _ = validate_system_schema(&schema)
            .expect("system schema validates under system mode");
    }

    #[test]
    fn user_validate_rejects_brain_namespace() {
        let schema = parse_schema(SYSTEM_SCHEMA_SOURCE).unwrap();
        let errs = brain_protocol::schema::validate(&schema)
            .expect_err("user validate must reject `namespace brain`");
        assert!(errs.iter().any(
            |e| e.code == brain_protocol::schema::ValidationErrorCode::NamespaceInvalidIdentifier
        ));
    }

    #[test]
    fn seed_first_open_creates_brain_v1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let db = Database::create(&path).unwrap();
        seed_system_schema(&db).unwrap();

        let rtxn = db.begin_read().unwrap();
        assert_eq!(schema_active(&rtxn, "brain").unwrap(), Some(1));
        let row = schema_get(&rtxn, "brain", 1).unwrap().unwrap();
        assert_eq!(row.namespace, "brain");
        assert_eq!(row.version, 1);
        assert_eq!(row.validator_version, 1);
    }

    #[test]
    fn seed_reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        {
            let db = Database::create(&path).unwrap();
            seed_system_schema(&db).unwrap();
        }
        {
            let db = Database::open(&path).unwrap();
            seed_system_schema(&db).unwrap();

            let rtxn = db.begin_read().unwrap();
            let list = schema_list(&rtxn, "brain").unwrap();
            assert_eq!(list.len(), 1, "reopen must not create v2");
            assert_eq!(list[0].version, 1);
        }
    }

    #[test]
    fn person_resolves_to_id_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let db = Database::create(&path).unwrap();
        seed_system_schema(&db).unwrap();

        // Verify the Person entity-type row exists at PERSON_ID.
        use crate::tables::knowledge::entity_type::ENTITY_TYPES_TABLE;
        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
        let row = t
            .get(&EntityType::PERSON_ID.raw())
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(row.name, "Person");
    }

    #[test]
    fn system_schema_seeds_two_builtin_extractors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let db = Database::create(&path).unwrap();
        seed_system_schema(&db).unwrap();

        let rtxn = db.begin_read().unwrap();
        let all = crate::extractor_ops::extractor_list(&rtxn).unwrap();
        assert_eq!(all.len(), 2);
        let names: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"entity_mentions"));
        assert!(names.contains(&"basic_ner"));
        for ext in &all {
            assert_eq!(ext.namespace, "brain");
            assert!(ext.is_enabled());
        }
    }

    #[test]
    fn system_schema_extractor_ids_are_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let db = Database::create(&path).unwrap();
        seed_system_schema(&db).unwrap();

        let rtxn = db.begin_read().unwrap();
        let entity_mentions = crate::extractor_ops::extractor_lookup_by_qname(
            &rtxn,
            "brain",
            "entity_mentions",
        )
        .unwrap()
        .expect("entity_mentions registered");
        let basic_ner =
            crate::extractor_ops::extractor_lookup_by_qname(&rtxn, "brain", "basic_ner")
                .unwrap()
                .expect("basic_ner registered");
        assert_eq!(entity_mentions.id().raw(), 1);
        assert_eq!(basic_ner.id().raw(), 2);
    }

    #[test]
    fn system_schema_extractor_definitions_decode_via_serde() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let db = Database::create(&path).unwrap();
        seed_system_schema(&db).unwrap();

        let rtxn = db.begin_read().unwrap();
        let row =
            crate::extractor_ops::extractor_lookup_by_qname(&rtxn, "brain", "entity_mentions")
                .unwrap()
                .unwrap();
        let ast: brain_protocol::schema::ExtractorDef =
            serde_json::from_slice(&row.definition_blob).expect("decode AST");
        assert_eq!(ast.name, "entity_mentions");
        match ast.target {
            brain_protocol::schema::ExtractorTarget::Entity { ref entity_type } => {
                assert_eq!(entity_type, "Person");
            }
            other => panic!("expected Entity target, got {other:?}"),
        }
        let has_patterns = ast.fields.iter().any(|f| {
            matches!(f, brain_protocol::schema::ExtractorField::Patterns(p) if p.len() == 2)
        });
        assert!(has_patterns);
    }

    #[test]
    fn reopen_does_not_duplicate_builtin_extractors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        {
            let db = Database::create(&path).unwrap();
            seed_system_schema(&db).unwrap();
        }
        let db = Database::open(&path).unwrap();
        seed_system_schema(&db).unwrap();

        let rtxn = db.begin_read().unwrap();
        let all = crate::extractor_ops::extractor_list(&rtxn).unwrap();
        assert_eq!(all.len(), 2, "reopen must not duplicate built-ins");
    }
}
