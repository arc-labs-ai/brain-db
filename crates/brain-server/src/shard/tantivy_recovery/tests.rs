//! Unit tests for tantivy recovery.

use std::fs;

use brain_core::{Entity, EntityId, EntityTypeId};
use brain_core::{
    EvidenceRef, Statement, StatementKind, StatementObject, StatementValue, SubjectRef,
};
use brain_core::{ExtractorId, PredicateId, StatementId};
use brain_index::{IndexStatus, TantivyShard};
use brain_metadata::entity::ops::entity_put;
use brain_metadata::entity::types::entity_type_intern;
use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::statement::statement_create;
use brain_metadata::MetadataDb;
use tempfile::TempDir;

use super::recover_tantivy_on_open;

/// Open a fresh shard dir + metadata.
fn fresh() -> (TempDir, MetadataDb) {
    let dir = TempDir::new().expect("tempdir");
    let metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open metadata");
    (dir, metadata)
}

#[test]
fn recover_with_ready_indexes_is_noop() {
    let (dir, metadata) = fresh();
    // First open creates the tantivy dirs.
    let startup_1 = TantivyShard::open(dir.path()).expect("first open");
    assert!(matches!(startup_1.memory_status, IndexStatus::Ready));
    assert!(matches!(startup_1.statements_status, IndexStatus::Ready));
    let shard_before = startup_1.shard.clone();
    drop(startup_1);
    drop(shard_before);

    // Second open + recovery.
    let startup_2 = TantivyShard::open(dir.path()).expect("re-open");
    let shard = recover_tantivy_on_open(dir.path(), &metadata, startup_2).expect("recovery");

    // The handle is healthy; no rebuild ran.
    assert!(!dir.path().join("memory_text.tantivy.rebuild").exists());
    assert!(!dir.path().join("memory_text.tantivy.old").exists());
    let _ = shard;
}

#[test]
fn recover_with_corrupt_memory_index_rebuilds() {
    let (dir, metadata) = fresh();
    let _ = TantivyShard::open(dir.path()).expect("first open"); // create dirs

    // Corrupt memory_text/meta.json.
    let meta = dir.path().join("memory_text.tantivy").join("meta.json");
    fs::write(&meta, b"not-json").expect("corrupt meta");

    let startup = TantivyShard::open(dir.path()).expect("re-open");
    assert!(matches!(
        startup.memory_status,
        IndexStatus::NeedsRebuild { .. }
    ));
    // Statements scope is still Ready.
    assert!(matches!(startup.statements_status, IndexStatus::Ready));

    let shard = recover_tantivy_on_open(dir.path(), &metadata, startup).expect("recovery");

    // Re-open again — both scopes Ready.
    drop(shard);
    let after = TantivyShard::open(dir.path()).expect("after open");
    assert!(matches!(after.memory_status, IndexStatus::Ready));
    assert!(matches!(after.statements_status, IndexStatus::Ready));
}

#[test]
fn recover_with_version_mismatch_rebuilds() {
    let (dir, metadata) = fresh();
    let _ = TantivyShard::open(dir.path()).expect("first open");

    // Stamp a stale payload directly into meta.json (matches the
    // version-mismatch test pattern).
    let meta_path = dir.path().join("memory_text.tantivy").join("meta.json");
    let raw = fs::read_to_string(&meta_path).expect("read meta");
    let mut json: serde_json::Value = serde_json::from_str(&raw).expect("parse");
    json["payload"] = serde_json::Value::String(
        serde_json::to_string(&serde_json::json!({ "brain_schema_version": 99 })).expect("ser"),
    );
    fs::write(&meta_path, serde_json::to_vec_pretty(&json).expect("ser")).expect("write");

    let startup = TantivyShard::open(dir.path()).expect("re-open");
    assert!(matches!(
        startup.memory_status,
        IndexStatus::NeedsRebuild { .. }
    ));

    let _ = recover_tantivy_on_open(dir.path(), &metadata, startup).expect("recovery");
    let after = TantivyShard::open(dir.path()).expect("after open");
    assert!(matches!(after.memory_status, IndexStatus::Ready));
}

#[test]
fn recover_rebuilds_statements_with_join() {
    let (dir, metadata) = fresh();

    // Set up entity + predicate + statement.
    let type_id: EntityTypeId = {
        let wtxn = metadata.write_txn().expect("wtxn");
        let id = entity_type_intern(&wtxn, "brain:Person", Vec::new(), 0).expect("type intern");
        wtxn.commit().expect("commit");
        id
    };

    let alice = EntityId::new();
    {
        let entity = Entity::new_active(alice, type_id, "Alice".into(), "alice".into(), 0);
        let wtxn = metadata.write_txn().expect("wtxn");
        entity_put(&wtxn, &entity).expect("entity_put");
        wtxn.commit().expect("commit");
    }
    let pred: PredicateId = {
        let wtxn = metadata.write_txn().expect("wtxn");
        let id = predicate_intern_or_get(&wtxn, "brain", "knows", 0, 0).expect("predicate");
        wtxn.commit().expect("commit");
        id
    };
    let _stmt: StatementId = {
        let stmt = Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(alice),
            pred,
            StatementObject::Value(StatementValue::Text("Bob".into())),
            0.7,
            EvidenceRef::default(),
            ExtractorId::from(0),
            0,
            1,
        );
        let wtxn = metadata.write_txn().expect("wtxn");
        let id = statement_create(&wtxn, &stmt, 0).expect("create");
        wtxn.commit().expect("commit");
        id
    };

    // Open the tantivy dirs so they exist; then corrupt the
    // statements meta.json.
    let _ = TantivyShard::open(dir.path()).expect("first open");
    let stmt_meta = dir.path().join("statements.tantivy").join("meta.json");
    fs::write(&stmt_meta, b"not-json").expect("corrupt");

    let startup = TantivyShard::open(dir.path()).expect("re-open");
    assert!(matches!(
        startup.statements_status,
        IndexStatus::NeedsRebuild { .. }
    ));

    let shard = recover_tantivy_on_open(dir.path(), &metadata, startup).expect("recovery");

    // Verify the rebuilt statements index has the row via the
    // public LexicalRetriever surface.
    use brain_index::{
        LexicalQuery, LexicalRetriever, LexicalRetrieverConfig, LexicalScope,
        TantivyLexicalRetriever,
    };
    let retriever = TantivyLexicalRetriever::new(shard).expect("retriever");
    let result = retriever
        .retrieve(
            &LexicalQuery {
                terms: vec!["bob".into()],
                ..Default::default()
            },
            LexicalScope::StatementText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve");
    assert_eq!(
        result.len(),
        1,
        "post-recovery statements must contain the row"
    );
}
