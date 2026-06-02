//! Unit tests for the per-shard tantivy handle.

use std::fs;

use tantivy::schema::FieldType;
use tempfile::TempDir;

use super::{
    memory_text_schema, schema_payload_json, statements_schema, BrainSchemaPayload, IndexStatus,
    LexicalScope, RebuildReason, TantivyShard, BRAIN_SCHEMA_VERSION,
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
        ("agent_id", "bytes"),
        ("kind", "u64"),
        ("created_at", "u64"),
        ("context", "u64"),
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
