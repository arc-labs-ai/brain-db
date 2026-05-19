//! Unit tests for the tantivy rebuild worker (phase 22.6).

use std::fs;
use std::path::Path;

use brain_core::knowledge::{
    EvidenceRef, Statement, StatementKind, StatementObject, StatementValue, SubjectRef,
};
use brain_core::{Entity, EntityId, EntityTypeId, ExtractorId, PredicateId, StatementId};
use brain_index::{IndexStatus, LexicalScope, TantivyShard};
use brain_metadata::entity_ops::entity_put;
use brain_metadata::entity_type_ops::entity_type_intern;
use brain_metadata::predicate_ops::predicate_intern;
use brain_metadata::statement_ops::{statement_create, statement_tombstone};
use brain_metadata::MetadataDb;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::Index;
use tempfile::TempDir;

use super::{rebuild_memory_text, rebuild_statements};

const PERSON_TYPE: &str = "brain:Person";

/// Open a fresh `MetadataDb` under `dir/metadata.redb`.
fn fresh_metadata(dir: &Path) -> MetadataDb {
    MetadataDb::open(dir.join("metadata.redb")).expect("open metadata")
}

/// Intern a Person entity type if absent; return its id.
fn ensure_person_type(metadata: &mut MetadataDb) -> EntityTypeId {
    let wtxn = metadata.write_txn().expect("wtxn");
    let id = entity_type_intern(&wtxn, PERSON_TYPE, Vec::new(), 0).expect("type intern");
    wtxn.commit().expect("commit");
    id
}

fn put_entity(metadata: &mut MetadataDb, name: &str, type_id: EntityTypeId) -> EntityId {
    let id = EntityId::new();
    let entity = Entity::new_active(id, type_id, name.into(), name.to_lowercase(), 0);
    let wtxn = metadata.write_txn().expect("wtxn");
    entity_put(&wtxn, &entity).expect("entity_put");
    wtxn.commit().expect("commit");
    id
}

fn intern_predicate(metadata: &mut MetadataDb, namespace: &str, name: &str) -> PredicateId {
    let wtxn = metadata.write_txn().expect("wtxn");
    let id = predicate_intern(&wtxn, namespace, name, None, 0, 1, "", 0).expect("predicate");
    wtxn.commit().expect("commit");
    id
}

fn create_statement(
    metadata: &mut MetadataDb,
    subject: EntityId,
    predicate: PredicateId,
    object: StatementObject,
    kind: StatementKind,
    confidence: f32,
) -> StatementId {
    let id = StatementId::new();
    let stmt = Statement::new_root(
        id,
        kind,
        SubjectRef::Entity(subject),
        predicate,
        object,
        confidence,
        EvidenceRef::default(),
        ExtractorId::from(0),
        0,
        1,
    );
    let wtxn = metadata.write_txn().expect("wtxn");
    let created = statement_create(&wtxn, &stmt, 0).expect("create");
    wtxn.commit().expect("commit");
    created
}

fn count_text_hits(index: &Index, field_name: &str, query_text: &str) -> usize {
    let schema = index.schema();
    let field = schema.get_field(field_name).expect("field");
    let reader = index.reader().expect("reader");
    reader.reload().expect("reload");
    let searcher = reader.searcher();
    let qp = QueryParser::for_index(index, vec![field]);
    let q = qp.parse_query(query_text).expect("parse");
    let top = searcher
        .search(&q, &TopDocs::with_limit(100).order_by_score())
        .expect("search");
    top.len()
}

// ---------------------------------------------------------------------------
// Memory text rebuild — v1 emits an empty valid index.
// ---------------------------------------------------------------------------

#[test]
fn rebuild_memory_text_produces_empty_valid_index() {
    let dir = TempDir::new().expect("tempdir");
    let mut metadata = fresh_metadata(dir.path());

    let report = rebuild_memory_text(dir.path(), &metadata).expect("rebuild");
    assert_eq!(report.scope, LexicalScope::MemoryText);
    assert_eq!(report.rows_processed, 0);

    // Re-open via TantivyShard to confirm Ready status.
    let startup = TantivyShard::open(dir.path()).expect("open after rebuild");
    assert!(matches!(startup.memory_status, IndexStatus::Ready));
    assert!(matches!(startup.statements_status, IndexStatus::Ready));

    let _ = &mut metadata;
}

// ---------------------------------------------------------------------------
// Statement rebuild — full content round-trip.
// ---------------------------------------------------------------------------

#[test]
fn rebuild_statements_full_round_trip() {
    let dir = TempDir::new().expect("tempdir");
    let mut metadata = fresh_metadata(dir.path());

    let type_id = ensure_person_type(&mut metadata);
    let alice = put_entity(&mut metadata, "Alice Wong", type_id);
    let predicate = intern_predicate(&mut metadata, "brain", "lives_in");
    let _stmt_id = create_statement(
        &mut metadata,
        alice,
        predicate,
        StatementObject::Value(StatementValue::Text("Paris".into())),
        StatementKind::Fact,
        0.85,
    );

    let report = rebuild_statements(dir.path(), &metadata).expect("rebuild");
    assert_eq!(report.scope, LexicalScope::StatementText);
    assert_eq!(report.rows_processed, 1);

    // Read the rebuilt index back via TantivyShard::open.
    let startup = TantivyShard::open(dir.path()).expect("open");
    assert!(matches!(startup.statements_status, IndexStatus::Ready));
    let index = &startup.shard.statements.index;
    assert_eq!(count_text_hits(index, "subject_name", "alice"), 1);
    assert_eq!(count_text_hits(index, "object_text", "paris"), 1);
}

// ---------------------------------------------------------------------------
// Tombstone skipping.
// ---------------------------------------------------------------------------

#[test]
fn rebuild_statements_skips_tombstoned() {
    let dir = TempDir::new().expect("tempdir");
    let mut metadata = fresh_metadata(dir.path());

    let type_id = ensure_person_type(&mut metadata);
    let bob = put_entity(&mut metadata, "Bob", type_id);
    let predicate = intern_predicate(&mut metadata, "brain", "owns");
    let live = create_statement(
        &mut metadata,
        bob,
        predicate,
        StatementObject::Value(StatementValue::Text("Bike".into())),
        StatementKind::Fact,
        0.6,
    );
    let dead = create_statement(
        &mut metadata,
        bob,
        predicate,
        StatementObject::Value(StatementValue::Text("Skateboard".into())),
        StatementKind::Fact,
        0.6,
    );

    // Tombstone the dead one.
    {
        let wtxn = metadata.write_txn().expect("wtxn");
        statement_tombstone(
            &wtxn,
            dead,
            brain_core::knowledge::TombstoneReason::UserRequest,
            0,
        )
        .expect("tombstone");
        wtxn.commit().expect("commit");
    }
    let _ = live;

    let report = rebuild_statements(dir.path(), &metadata).expect("rebuild");
    assert_eq!(report.rows_processed, 1);

    let startup = TantivyShard::open(dir.path()).expect("open");
    let index = &startup.shard.statements.index;
    assert_eq!(count_text_hits(index, "object_text", "bike"), 1);
    assert_eq!(count_text_hits(index, "object_text", "skateboard"), 0);
}

// ---------------------------------------------------------------------------
// Idempotency + atomic swap hygiene.
// ---------------------------------------------------------------------------

#[test]
fn rebuild_idempotent() {
    let dir = TempDir::new().expect("tempdir");
    let metadata = fresh_metadata(dir.path());

    rebuild_memory_text(dir.path(), &metadata).expect("first");
    rebuild_memory_text(dir.path(), &metadata).expect("second");
}

#[test]
fn atomic_swap_leaves_no_stale_dirs() {
    let dir = TempDir::new().expect("tempdir");
    let metadata = fresh_metadata(dir.path());

    rebuild_memory_text(dir.path(), &metadata).expect("rebuild");
    assert!(dir.path().join("memory_text.tantivy").is_dir());
    assert!(!dir.path().join("memory_text.tantivy.old").exists());
    assert!(!dir.path().join("memory_text.tantivy.rebuild").exists());
}

#[test]
fn rebuild_after_corrupt_live_replaces_it() {
    let dir = TempDir::new().expect("tempdir");
    let metadata = fresh_metadata(dir.path());

    // First-time rebuild creates a valid live dir.
    rebuild_memory_text(dir.path(), &metadata).expect("first");
    let meta_path = dir.path().join("memory_text.tantivy").join("meta.json");
    assert!(meta_path.exists());

    // Corrupt the meta.json.
    fs::write(&meta_path, b"not-json").expect("corrupt");

    // Pre-check: TantivyShard::open would now report
    // NeedsRebuild — but we can rebuild directly without going
    // through 22.1.
    rebuild_memory_text(dir.path(), &metadata).expect("second");

    // Re-open via 22.1; status must be Ready.
    let startup = TantivyShard::open(dir.path()).expect("open");
    assert!(matches!(startup.memory_status, IndexStatus::Ready));
}

#[test]
fn rebuild_creates_payload_for_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let metadata = fresh_metadata(dir.path());
    rebuild_memory_text(dir.path(), &metadata).expect("rebuild");
    rebuild_statements(dir.path(), &metadata).expect("rebuild stmts");

    // The whole point: 22.1 sees Ready after a rebuild.
    let startup = TantivyShard::open(dir.path()).expect("open");
    assert!(matches!(startup.memory_status, IndexStatus::Ready));
    assert!(matches!(startup.statements_status, IndexStatus::Ready));
}
