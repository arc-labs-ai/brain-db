//! Unit tests for the LexicalRetriever (phase 22.5).

use std::sync::Arc;

use brain_core::knowledge::StatementKind;
use brain_core::{AgentId, MemoryId, MemoryKind, StatementId};
use tantivy::TantivyDocument;
use tempfile::TempDir;

use super::{
    LexicalError, LexicalFilters, LexicalQuery, LexicalRetriever, LexicalRetrieverConfig,
    RankedItemId, TantivyLexicalRetriever,
};
use crate::tantivy_shard::{LexicalScope, TantivyShard};

/// Build a fresh TantivyShard + retriever pair backed by a tempdir.
fn fresh() -> (TempDir, Arc<TantivyShard>, TantivyLexicalRetriever) {
    let dir = TempDir::new().expect("tempdir");
    let startup = TantivyShard::open(dir.path()).expect("open");
    let shard = startup.shard;
    let retriever = TantivyLexicalRetriever::new(shard.clone()).expect("retriever");
    (dir, shard, retriever)
}

fn write_memory(
    shard: &TantivyShard,
    id: MemoryId,
    text: &str,
    agent: AgentId,
    kind: MemoryKind,
    created_at_ms: u64,
) {
    let schema = shard.memory_text.index.schema();
    let id_field = schema.get_field("memory_id").unwrap();
    let text_field = schema.get_field("text").unwrap();
    let agent_field = schema.get_field("agent_id").unwrap();
    let kind_field = schema.get_field("kind").unwrap();
    let created_field = schema.get_field("created_at").unwrap();

    let mut writer = shard
        .memory_text
        .index
        .writer_with_num_threads(1, 50_000_000)
        .expect("writer");
    let mut doc = TantivyDocument::default();
    doc.add_bytes(id_field, &id.raw().to_be_bytes());
    doc.add_text(text_field, text);
    let a: [u8; 16] = agent.into();
    doc.add_bytes(agent_field, &a);
    doc.add_u64(
        kind_field,
        match kind {
            MemoryKind::Episodic => 0,
            MemoryKind::Semantic => 1,
            MemoryKind::Consolidated => 2,
        },
    );
    doc.add_u64(created_field, created_at_ms);
    writer.add_document(doc).expect("add doc");
    writer.commit().expect("commit");
}

fn write_statement(
    shard: &TantivyShard,
    id: StatementId,
    subject_name: &str,
    predicate_name: &str,
    predicate_id: u32,
    object_text: &str,
    kind: StatementKind,
    confidence: f32,
    extracted_at_ms: u64,
) {
    let schema = shard.statements.index.schema();
    let id_field = schema.get_field("statement_id").unwrap();
    let subj_field = schema.get_field("subject_name").unwrap();
    let pred_name_field = schema.get_field("predicate_name").unwrap();
    let pred_id_field = schema.get_field("predicate_id").unwrap();
    let obj_field = schema.get_field("object_text").unwrap();
    let kind_field = schema.get_field("kind").unwrap();
    let bucket_field = schema.get_field("confidence_bucket").unwrap();
    let extracted_field = schema.get_field("extracted_at").unwrap();

    let bucket = ((confidence.clamp(0.0, 1.0) * 10.0).floor() as u64).min(9);

    let mut writer = shard
        .statements
        .index
        .writer_with_num_threads(1, 50_000_000)
        .expect("writer");
    let mut doc = TantivyDocument::default();
    doc.add_bytes(id_field, &id.to_bytes());
    doc.add_text(subj_field, subject_name);
    doc.add_text(pred_name_field, predicate_name);
    doc.add_u64(pred_id_field, u64::from(predicate_id));
    doc.add_text(obj_field, object_text);
    doc.add_u64(kind_field, u64::from(kind.as_u8()));
    doc.add_u64(bucket_field, bucket);
    doc.add_u64(extracted_field, extracted_at_ms);
    writer.add_document(doc).expect("add doc");
    writer.commit().expect("commit");
}

fn term_query(term: &str) -> LexicalQuery {
    LexicalQuery {
        terms: vec![term.into()],
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Memory scope.
// ---------------------------------------------------------------------------

#[test]
fn terms_query_returns_hits_in_memory_scope() {
    let (_dir, shard, retriever) = fresh();
    write_memory(
        &shard,
        MemoryId::pack(0, 1, 0),
        "the quick brown fox",
        AgentId::new(),
        MemoryKind::Episodic,
        0,
    );

    let result = retriever
        .retrieve(
            &term_query("quick"),
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve");

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].rank, 1);
    assert!(result[0].score > 0.0);
    assert!(matches!(result[0].id, RankedItemId::Memory(_)));
    assert!(result[0].snippet.is_none());
}

#[test]
fn empty_result_is_ok_not_error() {
    let (_dir, _shard, retriever) = fresh();
    let result = retriever
        .retrieve(
            &term_query("nonexistent"),
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve");
    assert!(result.is_empty());
}

#[test]
fn ranks_are_dense_and_one_based() {
    let (_dir, shard, retriever) = fresh();
    let agent = AgentId::new();
    for (slot, text) in [
        (1u64, "alpha alpha alpha alpha"),
        (2u64, "alpha beta gamma"),
        (3u64, "alpha"),
    ] {
        write_memory(
            &shard,
            MemoryId::pack(0, slot, 0),
            text,
            agent,
            MemoryKind::Episodic,
            slot * 1000,
        );
    }

    let result = retriever
        .retrieve(
            &term_query("alpha"),
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig {
                top_k: 10,
                ..Default::default()
            },
        )
        .expect("retrieve");

    assert_eq!(result.len(), 3);
    assert_eq!(result[0].rank, 1);
    assert_eq!(result[1].rank, 2);
    assert_eq!(result[2].rank, 3);
    // BM25 ranks by repetition (TF) — doc 1 should outrank doc 3.
    assert!(result[0].score >= result[1].score);
    assert!(result[1].score >= result[2].score);
}

#[test]
fn agent_id_filter_includes_matches() {
    let (_dir, shard, retriever) = fresh();
    let a = AgentId::new();
    let b = AgentId::new();
    write_memory(
        &shard,
        MemoryId::pack(0, 1, 0),
        "common term in a",
        a,
        MemoryKind::Episodic,
        0,
    );
    write_memory(
        &shard,
        MemoryId::pack(0, 2, 0),
        "common term in b",
        b,
        MemoryKind::Episodic,
        0,
    );

    let result = retriever
        .retrieve(
            &LexicalQuery {
                terms: vec!["common".into()],
                filters: LexicalFilters {
                    agent_id: Some(a),
                    ..Default::default()
                },
                ..Default::default()
            },
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve");

    assert_eq!(result.len(), 1);
    if let RankedItemId::Memory(id) = result[0].id {
        // Subject slot is the only way to disambiguate — we wrote
        // a → slot 1; assert that.
        assert_eq!(id.slot(), 1);
    } else {
        panic!("expected Memory id");
    }
}

#[test]
fn created_at_range_filter_narrows() {
    let (_dir, shard, retriever) = fresh();
    let agent = AgentId::new();
    write_memory(
        &shard,
        MemoryId::pack(0, 1, 0),
        "hello",
        agent,
        MemoryKind::Episodic,
        100,
    );
    write_memory(
        &shard,
        MemoryId::pack(0, 2, 0),
        "hello",
        agent,
        MemoryKind::Episodic,
        500,
    );
    write_memory(
        &shard,
        MemoryId::pack(0, 3, 0),
        "hello",
        agent,
        MemoryKind::Episodic,
        900,
    );

    let result = retriever
        .retrieve(
            &LexicalQuery {
                terms: vec!["hello".into()],
                filters: LexicalFilters {
                    created_at_ms: Some(200..=800),
                    ..Default::default()
                },
                ..Default::default()
            },
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve");

    assert_eq!(result.len(), 1, "exactly the middle doc should match");
}

#[test]
fn predicate_id_filter_on_memory_scope_errors() {
    let (_dir, _shard, retriever) = fresh();
    let err = retriever
        .retrieve(
            &LexicalQuery {
                terms: vec!["x".into()],
                filters: LexicalFilters {
                    predicate_id: Some(1),
                    ..Default::default()
                },
                ..Default::default()
            },
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig::default(),
        )
        .expect_err("must reject wrong-scope filter");
    assert!(matches!(err, LexicalError::QueryParseFailed(_)));
}

#[test]
fn min_score_filter_drops_low_hits() {
    let (_dir, shard, retriever) = fresh();
    write_memory(
        &shard,
        MemoryId::pack(0, 1, 0),
        "rare match here",
        AgentId::new(),
        MemoryKind::Episodic,
        0,
    );
    write_memory(
        &shard,
        MemoryId::pack(0, 2, 0),
        "rare rare rare rare",
        AgentId::new(),
        MemoryKind::Episodic,
        0,
    );

    let unfiltered = retriever
        .retrieve(
            &term_query("rare"),
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve");
    assert_eq!(unfiltered.len(), 2);
    let max_score = unfiltered[0].score;

    let filtered = retriever
        .retrieve(
            &term_query("rare"),
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig {
                min_score: Some(max_score),
                ..Default::default()
            },
        )
        .expect("retrieve");
    assert!(filtered.len() <= unfiltered.len());
    for r in &filtered {
        assert!(r.score >= max_score);
    }
}

// ---------------------------------------------------------------------------
// Statement scope.
// ---------------------------------------------------------------------------

#[test]
fn statement_terms_query_returns_hits() {
    let (_dir, shard, retriever) = fresh();
    write_statement(
        &shard,
        StatementId::from([1u8; 16]),
        "Alice Wong",
        "lives_in",
        7,
        "Paris",
        StatementKind::Fact,
        0.8,
        0,
    );

    let result = retriever
        .retrieve(
            &term_query("paris"),
            LexicalScope::StatementText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve");

    assert_eq!(result.len(), 1);
    assert!(matches!(result[0].id, RankedItemId::Statement(_)));
}

#[test]
fn confidence_bucket_range_filter() {
    let (_dir, shard, retriever) = fresh();
    write_statement(
        &shard,
        StatementId::from([1u8; 16]),
        "Bob",
        "owns",
        1,
        "Bike",
        StatementKind::Fact,
        0.2,
        0,
    );
    write_statement(
        &shard,
        StatementId::from([2u8; 16]),
        "Bob",
        "owns",
        1,
        "Bike",
        StatementKind::Fact,
        0.5,
        0,
    );
    write_statement(
        &shard,
        StatementId::from([3u8; 16]),
        "Bob",
        "owns",
        1,
        "Bike",
        StatementKind::Fact,
        0.85,
        0,
    );

    let result = retriever
        .retrieve(
            &LexicalQuery {
                terms: vec!["bike".into()],
                filters: LexicalFilters {
                    confidence_bucket: Some(4..=6),
                    ..Default::default()
                },
                ..Default::default()
            },
            LexicalScope::StatementText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve");

    assert_eq!(result.len(), 1, "only the bucket-5 statement should match");
}

#[test]
fn agent_id_filter_on_statement_scope_errors() {
    let (_dir, _shard, retriever) = fresh();
    let err = retriever
        .retrieve(
            &LexicalQuery {
                terms: vec!["x".into()],
                filters: LexicalFilters {
                    agent_id: Some(AgentId::new()),
                    ..Default::default()
                },
                ..Default::default()
            },
            LexicalScope::StatementText,
            &LexicalRetrieverConfig::default(),
        )
        .expect_err("must reject wrong-scope filter");
    assert!(matches!(err, LexicalError::QueryParseFailed(_)));
}

#[test]
fn predicate_id_filter_narrows_statement_hits() {
    let (_dir, shard, retriever) = fresh();
    write_statement(
        &shard,
        StatementId::from([1u8; 16]),
        "Dora",
        "loves",
        1,
        "trees",
        StatementKind::Preference,
        0.7,
        0,
    );
    write_statement(
        &shard,
        StatementId::from([2u8; 16]),
        "Dora",
        "hates",
        2,
        "trees",
        StatementKind::Preference,
        0.7,
        0,
    );

    let result = retriever
        .retrieve(
            &LexicalQuery {
                terms: vec!["trees".into()],
                filters: LexicalFilters {
                    predicate_id: Some(2),
                    ..Default::default()
                },
                ..Default::default()
            },
            LexicalScope::StatementText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve");

    assert_eq!(result.len(), 1);
}

#[test]
fn empty_query_returns_empty_result() {
    let (_dir, shard, retriever) = fresh();
    write_memory(
        &shard,
        MemoryId::pack(0, 1, 0),
        "anything",
        AgentId::new(),
        MemoryKind::Episodic,
        0,
    );
    let result = retriever
        .retrieve(
            &LexicalQuery::default(),
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve");
    assert!(result.is_empty());
}
