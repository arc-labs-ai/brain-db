//! LexicalRetriever criterion benches against §16/02 §2.9
//! (phase 22.8).
//!
//! Three benches:
//!
//! 1. Memory scope, single-term query (`§2.9` target p50 10 ms).
//! 2. Memory scope, multi-term + filter (`§2.9` target p50 15 ms).
//! 3. Statement scope, single-term query (`§2.9` target p50 10 ms).
//!
//! Corpus scale: 10K docs (regression detector). Spec-stated
//! 100K / 1M scales are validated by phase 14's acceptance suite;
//! this bench catches per-query regressions in CI.
//!
//! Run:
//!
//! ```bash
//! cargo bench -p brain-index --bench lexical_retrieve
//! cargo bench -p brain-index --bench lexical_retrieve -- --quick
//! ```

use std::sync::Arc;

use brain_core::knowledge::StatementKind;
use brain_core::{AgentId, MemoryId, MemoryKind, StatementId};
use brain_index::{
    LexicalFilters, LexicalQuery, LexicalRetriever, LexicalRetrieverConfig, LexicalScope,
    TantivyLexicalRetriever, TantivyShard,
};
use criterion::{black_box, criterion_group, Criterion};
use tantivy::TantivyDocument;
use tempfile::TempDir;

const CORPUS: usize = 10_000;

// ---------------------------------------------------------------------------
// Corpus builders. Reused across bench groups so setup is amortised.
// ---------------------------------------------------------------------------

fn build_memory_corpus() -> (TempDir, Arc<TantivyShard>) {
    let dir = TempDir::new().expect("tempdir");
    let startup = TantivyShard::open(dir.path()).expect("open");
    let shard = startup.shard;
    let schema = shard.memory_text.index.schema();
    let mem_id = schema.get_field("memory_id").unwrap();
    let text = schema.get_field("text").unwrap();
    let agent = schema.get_field("agent_id").unwrap();
    let kind = schema.get_field("kind").unwrap();
    let created = schema.get_field("created_at").unwrap();

    let mut writer = shard
        .memory_text
        .index
        .writer_with_num_threads(1, 200_000_000)
        .expect("writer");
    let agent_bytes: [u8; 16] = AgentId::new().into();

    for i in 0..CORPUS {
        let mut doc = TantivyDocument::default();
        let id = MemoryId::pack(0, (i + 1) as u64, 0);
        doc.add_bytes(mem_id, &id.raw().to_be_bytes());
        // Synthetic corpus: each doc gets a mix of common words +
        // a unique slot-derived token.
        let body = format!(
            "the quick brown fox jumps over slot{} alpha beta gamma delta",
            i
        );
        doc.add_text(text, &body);
        doc.add_bytes(agent, &agent_bytes);
        doc.add_u64(kind, 0);
        doc.add_u64(created, (i as u64) * 1000);
        writer.add_document(doc).expect("add");
    }
    writer.commit().expect("commit");
    (dir, shard)
}

fn build_statement_corpus() -> (TempDir, Arc<TantivyShard>) {
    let dir = TempDir::new().expect("tempdir");
    let startup = TantivyShard::open(dir.path()).expect("open");
    let shard = startup.shard;
    let schema = shard.statements.index.schema();
    let stmt_id = schema.get_field("statement_id").unwrap();
    let subj = schema.get_field("subject_name").unwrap();
    let pred_name = schema.get_field("predicate_name").unwrap();
    let pred_id = schema.get_field("predicate_id").unwrap();
    let object = schema.get_field("object_text").unwrap();
    let kind = schema.get_field("kind").unwrap();
    let bucket = schema.get_field("confidence_bucket").unwrap();
    let extracted = schema.get_field("extracted_at").unwrap();

    let mut writer = shard
        .statements
        .index
        .writer_with_num_threads(1, 200_000_000)
        .expect("writer");
    for i in 0..CORPUS {
        let mut doc = TantivyDocument::default();
        let id = StatementId::new();
        doc.add_bytes(stmt_id, &id.to_bytes());
        doc.add_text(subj, &format!("Subject {}", i));
        doc.add_text(pred_name, "lives_in");
        doc.add_u64(pred_id, 1);
        let object_text = if i % 100 == 0 {
            "Paris".to_string()
        } else {
            format!("CityName{}", i)
        };
        doc.add_text(object, &object_text);
        doc.add_u64(kind, u64::from(StatementKind::Fact.as_u8()));
        doc.add_u64(bucket, 5);
        doc.add_u64(extracted, (i as u64) * 1000);
        writer.add_document(doc).expect("add");
    }
    writer.commit().expect("commit");
    (dir, shard)
}

// ---------------------------------------------------------------------------
// Benches.
// ---------------------------------------------------------------------------

fn bench_memory_single_term(c: &mut Criterion) {
    let (_dir, shard) = build_memory_corpus();
    let retriever = TantivyLexicalRetriever::new(shard).expect("retriever");
    let query = LexicalQuery {
        terms: vec!["quick".into()],
        ..Default::default()
    };
    let config = LexicalRetrieverConfig::default();

    c.bench_function("lexical memory single-term @ 10K", |b| {
        b.iter(|| {
            let r = retriever
                .retrieve(black_box(&query), LexicalScope::MemoryText, &config)
                .expect("retrieve");
            black_box(r);
        });
    });
}

fn bench_memory_multi_term_filter(c: &mut Criterion) {
    let (_dir, shard) = build_memory_corpus();
    let retriever = TantivyLexicalRetriever::new(shard).expect("retriever");
    let query = LexicalQuery {
        terms: vec!["quick".into(), "brown".into()],
        filters: LexicalFilters {
            memory_kind: Some(MemoryKind::Episodic),
            created_at_ms: Some(0..=u64::MAX),
            ..Default::default()
        },
        ..Default::default()
    };
    let config = LexicalRetrieverConfig::default();

    c.bench_function("lexical memory multi-term + filter @ 10K", |b| {
        b.iter(|| {
            let r = retriever
                .retrieve(black_box(&query), LexicalScope::MemoryText, &config)
                .expect("retrieve");
            black_box(r);
        });
    });
}

fn bench_statement_single_term(c: &mut Criterion) {
    let (_dir, shard) = build_statement_corpus();
    let retriever = TantivyLexicalRetriever::new(shard).expect("retriever");
    let query = LexicalQuery {
        terms: vec!["paris".into()],
        ..Default::default()
    };
    let config = LexicalRetrieverConfig::default();

    c.bench_function("lexical statement single-term @ 10K", |b| {
        b.iter(|| {
            let r = retriever
                .retrieve(black_box(&query), LexicalScope::StatementText, &config)
                .expect("retrieve");
            black_box(r);
        });
    });
}

criterion_group!(
    name = lexical_retrieve;
    config = Criterion::default();
    targets = bench_memory_single_term, bench_memory_multi_term_filter, bench_statement_single_term
);

fn main() {
    lexical_retrieve();
    Criterion::default().configure_from_args().final_summary();
}
