//! Unit tests for the per-shard tantivy handle.

use std::fs;
use std::path::Path;

use tantivy::schema::FieldType;
use tantivy::{Index, TantivyDocument};
use tempfile::TempDir;

use super::{
    build_analyzer, memory_text_schema, schema_payload_json, statements_schema, BrainSchemaPayload,
    IndexStatus, LexicalScope, RebuildReason, TantivyShard, BRAIN_SCHEMA_VERSION,
    BRAIN_TOKENIZER_NAME, OLD_SUFFIX, REBUILD_SUFFIX,
};

// ---------------------------------------------------------------------------
// Schema round-trips. These field sets are pinned verbatim.
// ---------------------------------------------------------------------------

#[test]
fn memory_text_schema_matches_spec() {
    let schema = memory_text_schema();

    let expected = &[
        ("memory_id", "bytes"),
        ("text", "text"),
        ("space_id", "bytes"),
        ("kind", "u64"),
        ("created_at", "u64"),
        ("session", "u64"),
    ];

    let actual: Vec<(String, &'static str)> = schema
        .fields()
        .map(|(_, entry)| {
            let kind = match entry.field_type() {
                FieldType::Str(_) => "text",
                FieldType::U64(_) => "u64",
                FieldType::Bytes(_) => "bytes",
                other => panic!("unexpected field type: {other:?}"),
            };
            (entry.name().to_string(), kind)
        })
        .collect();

    assert_eq!(
        actual,
        expected
            .iter()
            .map(|(n, k)| ((*n).to_string(), *k))
            .collect::<Vec<_>>()
    );
}

#[test]
fn statements_schema_matches_spec() {
    let schema = statements_schema();

    let expected = &[
        ("statement_id", "bytes"),
        ("subject_name", "text"),
        ("predicate_name", "text"),
        ("predicate_id", "u64"),
        ("object_text", "text"),
        ("kind", "u64"),
        ("confidence_bucket", "u64"),
        ("extracted_at", "u64"),
    ];

    let actual: Vec<(String, &'static str)> = schema
        .fields()
        .map(|(_, entry)| {
            let kind = match entry.field_type() {
                FieldType::Str(_) => "text",
                FieldType::U64(_) => "u64",
                FieldType::Bytes(_) => "bytes",
                other => panic!("unexpected field type: {other:?}"),
            };
            (entry.name().to_string(), kind)
        })
        .collect();

    assert_eq!(
        actual,
        expected
            .iter()
            .map(|(n, k)| ((*n).to_string(), *k))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// open(): fresh, reopen, version mismatch, corrupt payload.
// ---------------------------------------------------------------------------

#[test]
fn open_creates_indexes_on_fresh_dir() {
    let dir = TempDir::new().expect("tempdir");
    let startup = TantivyShard::open(dir.path()).expect("open");

    assert!(matches!(startup.memory_status, IndexStatus::Ready));
    assert!(matches!(startup.statements_status, IndexStatus::Ready));
    assert!(dir.path().join("memory_text.tantivy").is_dir());
    assert!(dir.path().join("statements.tantivy").is_dir());
    assert_eq!(startup.shard.memory_text.scope, LexicalScope::MemoryText);
    assert_eq!(startup.shard.statements.scope, LexicalScope::StatementText);
}

#[test]
fn open_reopens_existing_indexes_as_ready() {
    let dir = TempDir::new().expect("tempdir");
    let _ = TantivyShard::open(dir.path()).expect("first open");
    let again = TantivyShard::open(dir.path()).expect("re-open");
    assert!(matches!(again.memory_status, IndexStatus::Ready));
    assert!(matches!(again.statements_status, IndexStatus::Ready));
}

#[test]
fn open_returns_needs_rebuild_on_version_mismatch() {
    let dir = TempDir::new().expect("tempdir");

    // First open creates the index dir. Then stamp a stale
    // payload into meta.json directly — the same field
    // `inspect_payload` reads (writers use a Prepared-
    // commit-with-payload flow, but for this test the
    // file-level edit is the smallest reproducer).
    let _ = TantivyShard::open(dir.path()).expect("first open");

    let meta_path = dir.path().join("memory_text.tantivy").join("meta.json");
    let raw = fs::read_to_string(&meta_path).expect("read meta.json");
    let mut meta: serde_json::Value = serde_json::from_str(&raw).expect("parse meta.json");
    let stale_payload = serde_json::to_string(&BrainSchemaPayload {
        brain_schema_version: 99,
    })
    .expect("serialize stale payload");
    meta["payload"] = serde_json::Value::String(stale_payload);
    fs::write(
        &meta_path,
        serde_json::to_vec_pretty(&meta).expect("serialize meta"),
    )
    .expect("write meta.json");

    let again = TantivyShard::open(dir.path()).expect("re-open");
    match again.memory_status {
        IndexStatus::NeedsRebuild {
            reason: RebuildReason::SchemaVersionMismatch { found, expected },
        } => {
            assert_eq!(found, 99);
            assert_eq!(expected, BRAIN_SCHEMA_VERSION);
        }
        other => panic!("expected SchemaVersionMismatch, got {other:?}"),
    }
}

#[test]
fn open_returns_needs_rebuild_on_corrupt_meta() {
    let dir = TempDir::new().expect("tempdir");
    let _ = TantivyShard::open(dir.path()).expect("first open");

    // Corrupt meta.json. tantivy's directory layout puts it at the
    // top of the index dir.
    let meta = dir.path().join("memory_text.tantivy").join("meta.json");
    fs::write(&meta, b"not-json").expect("corrupt meta");

    let again = TantivyShard::open(dir.path()).expect("re-open");
    assert!(matches!(
        again.memory_status,
        IndexStatus::NeedsRebuild {
            reason: RebuildReason::OpenFailed(_)
        }
    ));
    // The other scope must still be Ready — failures don't cascade.
    assert!(matches!(again.statements_status, IndexStatus::Ready));
}

// ---------------------------------------------------------------------------
// schema_payload_json round-trips. Writers consume this.
// ---------------------------------------------------------------------------

#[test]
fn schema_payload_json_round_trips() {
    let s = schema_payload_json();
    let parsed: BrainSchemaPayload = serde_json::from_str(&s).expect("parse");
    assert_eq!(parsed.brain_schema_version, BRAIN_SCHEMA_VERSION);
}

// ---------------------------------------------------------------------------
// Crash-safe rebuild swap. The rebuild worker replaces the live index with
// two non-atomic renames (live→`.old`, then `.rebuild`→live). A crash in
// that window leaves the live dir absent with the completed replacement in
// `.rebuild`. `open()` must finish the swap, not create a fresh empty index.
// ---------------------------------------------------------------------------

/// Build a fully-committed memory-text index at `dir` holding one doc whose
/// `text` field contains `marker`. Stamps the brain schema payload so it
/// reads as a *completed* index (the swap-recovery completeness check).
fn build_completed_memory_index(dir: &Path, marker: &str) {
    fs::create_dir_all(dir).expect("mkdir index dir");
    let index = Index::create_in_dir(dir, memory_text_schema()).expect("create index");
    index
        .tokenizers()
        .register(BRAIN_TOKENIZER_NAME, build_analyzer());
    let mut writer = index
        .writer_with_num_threads(1, 15_000_000)
        .expect("writer");
    let text_field = index.schema().get_field("text").expect("text field");
    let mut doc = TantivyDocument::default();
    doc.add_text(text_field, marker);
    writer.add_document(doc).expect("add doc");
    let mut prepared = writer.prepare_commit().expect("prepare");
    prepared.set_payload(&schema_payload_json());
    prepared.commit().expect("commit");
    drop(writer);
    drop(index);
}

fn num_docs(index: &Index) -> u64 {
    let reader = index.reader().expect("reader");
    reader.searcher().num_docs()
}

#[test]
fn open_completes_interrupted_swap_from_rebuild_dir() {
    let dir = TempDir::new().expect("tempdir");
    let live = dir.path().join("memory_text.tantivy");
    let rebuild = dir
        .path()
        .join(format!("memory_text.tantivy{REBUILD_SUFFIX}"));

    // Simulate a crash after live→`.old` succeeded but before
    // `.rebuild`→live: live is absent, the completed new index sits in
    // `.rebuild`. (No `.old` here — the prior data is irrelevant once the
    // newer complete rebuild exists.)
    build_completed_memory_index(&rebuild, "recovered payments doc");
    assert!(
        !live.exists(),
        "live must be absent to model the crash window"
    );

    let startup = TantivyShard::open(dir.path()).expect("open");

    // The promoted index must be Ready AND carry the rebuilt doc — never a
    // fresh-empty one (invariant #7).
    assert!(matches!(startup.memory_status, IndexStatus::Ready));
    assert_eq!(
        num_docs(&startup.shard.memory_text.index),
        1,
        "the completed rebuild must be promoted, not replaced by an empty index",
    );
    assert!(live.exists(), "live dir must exist after promotion");
    assert!(
        !rebuild.exists(),
        "the .rebuild scratch dir must be cleaned up"
    );
}

#[test]
fn open_restores_from_old_dir_when_rebuild_absent() {
    let dir = TempDir::new().expect("tempdir");
    let live = dir.path().join("memory_text.tantivy");
    let old = dir.path().join(format!("memory_text.tantivy{OLD_SUFFIX}"));

    // Simulate a crash after live→`.old` but before a complete `.rebuild`
    // existed (the new index never finished committing). The only complete
    // index is the prior data in `.old`; open must restore it.
    build_completed_memory_index(&old, "prior data doc");
    assert!(!live.exists());

    let startup = TantivyShard::open(dir.path()).expect("open");

    assert!(matches!(startup.memory_status, IndexStatus::Ready));
    assert_eq!(
        num_docs(&startup.shard.memory_text.index),
        1,
        "the prior .old index must be restored, not dropped for an empty one",
    );
    assert!(live.exists());
    assert!(
        !old.exists(),
        "the .old dir must be cleaned up after restore"
    );
}

#[test]
fn open_prefers_rebuild_over_old_and_cleans_both() {
    let dir = TempDir::new().expect("tempdir");
    let live = dir.path().join("memory_text.tantivy");
    let rebuild = dir
        .path()
        .join(format!("memory_text.tantivy{REBUILD_SUFFIX}"));
    let old = dir.path().join(format!("memory_text.tantivy{OLD_SUFFIX}"));

    // Both siblings complete: the crash landed after live→`.old` and after
    // `.rebuild` finished committing but before `.rebuild`→live. The newest
    // complete index (`.rebuild`) wins; both scratch dirs are cleaned up.
    build_completed_memory_index(&old, "stale doc one two three");
    build_completed_memory_index(&rebuild, "fresh doc alpha beta gamma");
    assert!(!live.exists());

    let startup = TantivyShard::open(dir.path()).expect("open");
    assert!(matches!(startup.memory_status, IndexStatus::Ready));
    assert_eq!(num_docs(&startup.shard.memory_text.index), 1);
    assert!(live.exists());
    assert!(!rebuild.exists());
    assert!(!old.exists());

    // Confirm it's the rebuild's content, not the old's.
    let index = &startup.shard.memory_text.index;
    index
        .tokenizers()
        .register(BRAIN_TOKENIZER_NAME, build_analyzer());
    let text = index.schema().get_field("text").expect("text");
    let reader = index.reader().expect("reader");
    let searcher = reader.searcher();
    let qp = tantivy::query::QueryParser::for_index(index, vec![text]);
    let q = qp.parse_query("alpha").expect("parse");
    let hits = searcher
        .search(
            &q,
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .expect("search");
    assert_eq!(hits.len(), 1, "promoted index must be the .rebuild content");
}

#[test]
fn open_ignores_incomplete_rebuild_and_creates_fresh() {
    let dir = TempDir::new().expect("tempdir");
    let live = dir.path().join("memory_text.tantivy");
    let rebuild = dir
        .path()
        .join(format!("memory_text.tantivy{REBUILD_SUFFIX}"));

    // A `.rebuild` created but never finally committed: tantivy wrote a
    // meta.json at creation but no brain payload. This is an in-progress
    // rebuild, not a completed one — it must NOT be promoted.
    fs::create_dir_all(&rebuild).expect("mkdir rebuild");
    let index = Index::create_in_dir(&rebuild, memory_text_schema()).expect("create");
    drop(index);
    assert!(rebuild.join("meta.json").exists());
    assert!(!live.exists());

    let startup = TantivyShard::open(dir.path()).expect("open");
    // Fresh empty live index (Ready), and the half-built rebuild is gone.
    assert!(matches!(startup.memory_status, IndexStatus::Ready));
    assert_eq!(num_docs(&startup.shard.memory_text.index), 0);
    assert!(live.exists());
    assert!(!rebuild.exists());
}

// ---------------------------------------------------------------------------
// Commit-generation counter — the signal the retriever uses to skip redundant
// reloads. The indexer bumps it after each commit; a clone (the indexer holds
// one, the retriever reads the shard's) must share the same value.
// ---------------------------------------------------------------------------

#[test]
fn commit_generation_starts_at_zero_and_is_shared_across_clones() {
    let dir = TempDir::new().expect("tempdir");
    let shard = TantivyShard::open(dir.path()).expect("open").shard;

    assert_eq!(shard.memory_text.commit_generation(), 0, "starts at 0");
    assert_eq!(shard.statements.commit_generation(), 0, "starts at 0");

    // The indexer works through a clone of the handle; the retriever reads the
    // shard's own handle. A bump on the clone must be visible on the original.
    let indexer_handle = shard.memory_text.clone();
    indexer_handle.bump_commit_generation();
    assert_eq!(indexer_handle.commit_generation(), 1);
    assert_eq!(
        shard.memory_text.commit_generation(),
        1,
        "clones share one counter",
    );

    // Each index scope carries its own counter — a memory commit must not
    // make the statements reader think it needs to reload.
    assert_eq!(
        shard.statements.commit_generation(),
        0,
        "per-scope counters are independent",
    );

    // The bare counter handle the drain loop keeps bumps the same value.
    let counter = indexer_handle.commit_generation_counter();
    counter.fetch_add(1, std::sync::atomic::Ordering::Release);
    assert_eq!(shard.memory_text.commit_generation(), 2);
}
