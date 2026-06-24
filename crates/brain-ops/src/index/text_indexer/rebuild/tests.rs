//! Unit tests for the tantivy rebuild worker.

use std::fs;
use std::path::Path;

use brain_core::{
    AgentId, ContextId, Entity, EntityId, EntityTypeId, ExtractorId, MemoryId, MemoryKind,
    PredicateId, StatementId,
};
use brain_core::{
    EvidenceRef, Statement, StatementKind, StatementObject, StatementValue, SubjectRef,
};
use brain_index::{build_analyzer, IndexStatus, LexicalScope, TantivyShard, BRAIN_TOKENIZER_NAME};
use brain_metadata::entity::ops::entity_put;
use brain_metadata::entity::types::entity_type_intern;
use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::statement::{statement_create, statement_tombstone};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::tables::text::TEXTS_TABLE;
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

/// Seed an active memory row in `MEMORIES_TABLE` + its text in
/// `TEXTS_TABLE`, mirroring what the apply layer writes at ENCODE.
fn put_memory(metadata: &mut MetadataDb, slot: u64, text: &str, kind: MemoryKind) -> MemoryId {
    let id = MemoryId::pack(0, slot, 1);
    let meta = MemoryMetadata::new_active(
        id,
        AgentId::from([7u8; 16]),
        ContextId::from(42),
        slot,
        1,
        kind,
        [0u8; 16],
        0.5,
        text.len() as u32,
        1_700_000_000_000_000_000,
    );
    let wtxn = metadata.write_txn().expect("wtxn");
    {
        let mut m = wtxn.open_table(MEMORIES_TABLE).expect("open MEMORIES");
        m.insert(&id.to_be_bytes(), &meta).expect("insert mem");
        let mut t = wtxn.open_table(TEXTS_TABLE).expect("open TEXTS");
        t.insert(&id.to_be_bytes(), text.as_bytes())
            .expect("insert text");
    }
    wtxn.commit().expect("commit");
    id
}

fn intern_predicate(metadata: &mut MetadataDb, namespace: &str, name: &str) -> PredicateId {
    // Tolerate predicates already seeded by the default system schema —
    // the rebuild tests reuse declared predicates like `brain:speaks`
    // and `brain:current_role` which already exist post-seed.
    let wtxn = metadata.write_txn().expect("wtxn");
    let id = predicate_intern_or_get(&wtxn, namespace, name, 0, 0).expect("predicate");
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
// Memory text rebuild — content-complete from MEMORIES_TABLE + TEXTS_TABLE.
// ---------------------------------------------------------------------------

#[test]
fn rebuild_memory_text_empty_db_yields_empty_index() {
    let dir = TempDir::new().expect("tempdir");
    let metadata = fresh_metadata(dir.path());

    // No memories seeded → zero rows, but a valid index + Ready status.
    let report = rebuild_memory_text(dir.path(), &metadata).expect("rebuild");
    assert_eq!(report.scope, LexicalScope::MemoryText);
    assert_eq!(report.rows_processed, 0);

    let startup = TantivyShard::open(dir.path()).expect("open after rebuild");
    assert!(matches!(startup.memory_status, IndexStatus::Ready));
    assert!(matches!(startup.statements_status, IndexStatus::Ready));
}

#[test]
fn rebuild_memory_text_round_trip() {
    let dir = TempDir::new().expect("tempdir");
    let mut metadata = fresh_metadata(dir.path());

    put_memory(
        &mut metadata,
        1,
        "Sarah leads the payments platform team at Aurora",
        MemoryKind::Semantic,
    );
    put_memory(
        &mut metadata,
        2,
        "Cello practice happens every morning before work",
        MemoryKind::Episodic,
    );

    let report = rebuild_memory_text(dir.path(), &metadata).expect("rebuild");
    assert_eq!(report.scope, LexicalScope::MemoryText);
    assert_eq!(report.rows_processed, 2, "both active memories indexed");

    // Re-open the rebuilt index and confirm each is lexically findable.
    let index = Index::open_in_dir(dir.path().join("memory_text.tantivy")).expect("open index");
    index
        .tokenizers()
        .register(BRAIN_TOKENIZER_NAME, build_analyzer());
    assert_eq!(count_text_hits(&index, "text", "payments"), 1);
    assert_eq!(count_text_hits(&index, "text", "cello"), 1);
    assert_eq!(count_text_hits(&index, "text", "Aurora"), 1);
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
    // Use `current_role` (declared as Value<text>) so the text-indexer
    // test's Text object satisfies the predicate's object constraint.
    let predicate = intern_predicate(&mut metadata, "brain", "current_role");
    let _stmt_id = create_statement(
        &mut metadata,
        alice,
        predicate,
        StatementObject::Value(StatementValue::Text("Paris team lead".into())),
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
    // `speaks` is declared as Value<text>; matches the Text object.
    let predicate = intern_predicate(&mut metadata, "brain", "speaks");
    let live = create_statement(
        &mut metadata,
        bob,
        predicate,
        StatementObject::Value(StatementValue::Text("English".into())),
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
        statement_tombstone(&wtxn, dead, brain_core::TombstoneReason::UserRequest, 0)
            .expect("tombstone");
        wtxn.commit().expect("commit");
    }
    let _ = live;

    let report = rebuild_statements(dir.path(), &metadata).expect("rebuild");
    assert_eq!(report.rows_processed, 1);

    let startup = TantivyShard::open(dir.path()).expect("open");
    let index = &startup.shard.statements.index;
    assert_eq!(count_text_hits(index, "object_text", "english"), 1);
    assert_eq!(count_text_hits(index, "object_text", "skateboard"), 0);
}

// ---------------------------------------------------------------------------
// Idempotency + atomic swap hygiene.
// ---------------------------------------------------------------------------

#[test]
fn repeated_text_rebuild_succeeds_without_error() {
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
    // through `TantivyShard::open`.
    rebuild_memory_text(dir.path(), &metadata).expect("second");

    // Re-open via `TantivyShard::open`; status must be Ready.
    let startup = TantivyShard::open(dir.path()).expect("open");
    assert!(matches!(startup.memory_status, IndexStatus::Ready));
}

#[test]
fn rebuild_creates_payload_for_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let metadata = fresh_metadata(dir.path());
    rebuild_memory_text(dir.path(), &metadata).expect("rebuild");
    rebuild_statements(dir.path(), &metadata).expect("rebuild stmts");

    // The whole point: `TantivyShard::open` sees Ready after a rebuild.
    let startup = TantivyShard::open(dir.path()).expect("open");
    assert!(matches!(startup.memory_status, IndexStatus::Ready));
    assert!(matches!(startup.statements_status, IndexStatus::Ready));
}
