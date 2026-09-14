//! Unit tests for the LexicalRetriever.

use std::sync::Arc;

use brain_core::StatementKind;
use brain_core::{MemoryId, MemoryKind, SpaceId, StatementId};
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
    space: SpaceId,
    kind: MemoryKind,
    created_at_ms: u64,
) {
    let schema = shard.memory_text.index.schema();
    let id_field = schema.get_field("memory_id").unwrap();
    let text_field = schema.get_field("text").unwrap();
    let space_field = schema.get_field("space_id").unwrap();
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
    let a: [u8; 16] = space.into();
    doc.add_bytes(space_field, &a);
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
    // Mirror the production indexer: a commit advances the handle's commit
    // generation so the retriever knows to reload. Without this the
    // reload-gated retriever would not observe writes made across queries.
    shard.memory_text.bump_commit_generation();
}

// Test helper that mirrors the underlying schema's field set; introducing a
// builder struct would just shadow the same nine fields without improving
// the test sites.
#[allow(clippy::too_many_arguments)]
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

    // Mirrors the canonical bucket formula (floor(c*10).clamp(0,10),
    // 0..=10) used by brain-metadata + the brain-ops tantivy writer.
    let bucket = ((confidence.clamp(0.0, 1.0) * 10.0).floor() as u64).min(10);

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
    // Mirror the production indexer: advance the commit generation so the
    // reload-gated retriever reloads and observes this write.
    shard.statements.bump_commit_generation();
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
        SpaceId::new(),
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
    let space = SpaceId::new();
    for (slot, text) in [
        (1u64, "alpha alpha alpha alpha"),
        (2u64, "alpha beta gamma"),
        (3u64, "alpha"),
    ] {
        write_memory(
            &shard,
            MemoryId::pack(0, slot, 0),
            text,
            space,
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
fn space_id_filter_includes_matches() {
    let (_dir, shard, retriever) = fresh();
    let a = SpaceId::new();
    let b = SpaceId::new();
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
                    space_ids: vec![a],
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
fn space_ids_filter_or_groups_match_any() {
    let (_dir, shard, retriever) = fresh();
    let a1 = SpaceId::new();
    let a2 = SpaceId::new();
    let a3 = SpaceId::new();
    write_memory(
        &shard,
        MemoryId::pack(0, 1, 0),
        "common term one",
        a1,
        MemoryKind::Episodic,
        0,
    );
    write_memory(
        &shard,
        MemoryId::pack(0, 2, 0),
        "common term two",
        a2,
        MemoryKind::Episodic,
        0,
    );
    write_memory(
        &shard,
        MemoryId::pack(0, 3, 0),
        "common term three",
        a3,
        MemoryKind::Episodic,
        0,
    );

    let result = retriever
        .retrieve(
            &LexicalQuery {
                terms: vec!["common".into()],
                filters: LexicalFilters {
                    space_ids: vec![a1, a2],
                    ..Default::default()
                },
                ..Default::default()
            },
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig {
                top_k: 10,
                ..Default::default()
            },
        )
        .expect("retrieve");

    assert_eq!(result.len(), 2, "OR-group must match a1 or a2 but not a3");
    let slots: std::collections::HashSet<u64> = result
        .iter()
        .map(|item| match item.id {
            RankedItemId::Memory(id) => id.slot(),
            _ => panic!("expected Memory id"),
        })
        .collect();
    assert!(slots.contains(&1));
    assert!(slots.contains(&2));
    assert!(!slots.contains(&3));
}

#[test]
fn created_at_range_filter_narrows() {
    let (_dir, shard, retriever) = fresh();
    let space = SpaceId::new();
    write_memory(
        &shard,
        MemoryId::pack(0, 1, 0),
        "hello",
        space,
        MemoryKind::Episodic,
        100,
    );
    write_memory(
        &shard,
        MemoryId::pack(0, 2, 0),
        "hello",
        space,
        MemoryKind::Episodic,
        500,
    );
    write_memory(
        &shard,
        MemoryId::pack(0, 3, 0),
        "hello",
        space,
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
        SpaceId::new(),
        MemoryKind::Episodic,
        0,
    );
    write_memory(
        &shard,
        MemoryId::pack(0, 2, 0),
        "rare rare rare rare",
        SpaceId::new(),
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
fn space_id_filter_on_statement_scope_errors() {
    let (_dir, _shard, retriever) = fresh();
    let err = retriever
        .retrieve(
            &LexicalQuery {
                terms: vec!["x".into()],
                filters: LexicalFilters {
                    space_ids: vec![SpaceId::new()],
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
        SpaceId::new(),
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

// ---------------------------------------------------------------------------
// Hot swap (swap_shard) — the read side of the live tantivy rebuild.
// ---------------------------------------------------------------------------

/// After `swap_shard`, `retrieve` serves the new index's content and no
/// longer the old — the atomic publish flips the whole bundle (shard +
/// both readers) without ever exposing a mixed view.
#[test]
fn swap_shard_flips_reads_to_the_new_index() {
    // Old index: one memory "alpha".
    let (_dir_a, shard_a, retriever) = fresh();
    let alpha = MemoryId::pack(0, 1, 0);
    write_memory(
        &shard_a,
        alpha,
        "alpha",
        SpaceId::new(),
        MemoryKind::Episodic,
        0,
    );
    let hits = retriever
        .retrieve(
            &term_query("alpha"),
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve alpha");
    assert_eq!(hits.len(), 1, "old index serves alpha before swap");
    assert_eq!(hits[0].id, RankedItemId::Memory(alpha));

    // New index in a separate directory: one memory "beta".
    let dir_b = TempDir::new().expect("tempdir b");
    let shard_b = TantivyShard::open(dir_b.path()).expect("open b").shard;
    let beta = MemoryId::pack(1, 2, 0);
    write_memory(
        &shard_b,
        beta,
        "beta",
        SpaceId::new(),
        MemoryKind::Episodic,
        0,
    );

    retriever.swap_shard(shard_b).expect("swap");

    // Post-swap: beta is visible, alpha is gone. Never an error, never a
    // stale-mixed result (invariant #7).
    let after_beta = retriever
        .retrieve(
            &term_query("beta"),
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve beta");
    assert_eq!(after_beta.len(), 1, "new index serves beta after swap");
    assert_eq!(after_beta[0].id, RankedItemId::Memory(beta));

    let after_alpha = retriever
        .retrieve(
            &term_query("alpha"),
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve alpha after swap");
    assert!(
        after_alpha.is_empty(),
        "old index content is gone after swap",
    );
}

/// A second swap composes: reads always reflect the most recently
/// published bundle.
#[test]
fn swap_shard_is_repeatable() {
    let (_dir_a, shard_a, retriever) = fresh();
    write_memory(
        &shard_a,
        MemoryId::pack(0, 1, 0),
        "first",
        SpaceId::new(),
        MemoryKind::Episodic,
        0,
    );

    for (i, term) in ["second", "third"].iter().enumerate() {
        let dir = TempDir::new().expect("tempdir");
        let shard = TantivyShard::open(dir.path()).expect("open").shard;
        write_memory(
            &shard,
            MemoryId::pack(0, i as u64 + 2, 0),
            term,
            SpaceId::new(),
            MemoryKind::Episodic,
            0,
        );
        retriever.swap_shard(shard).expect("swap");
        let hits = retriever
            .retrieve(
                &term_query(term),
                LexicalScope::MemoryText,
                &LexicalRetrieverConfig::default(),
            )
            .expect("retrieve");
        assert_eq!(hits.len(), 1, "reads reflect the latest swap for {term}");
    }
}

// ---------------------------------------------------------------------------
// Commit-generation reload gating — the retriever reloads only when the
// indexer's commit generation has advanced, but must still observe every
// committed write (read-your-commits, invariant #7). This guards the specific
// failure the optimization could introduce: a stale read that skips a reload
// after a real commit.
// ---------------------------------------------------------------------------

#[test]
fn retrieve_observes_each_commit_as_the_generation_advances() {
    let (_dir, shard, retriever) = fresh();

    // Each `write_memory` commits and bumps the generation (mirroring the
    // production indexer). The first query reloads off the sentinel; every
    // later query must reload again because the generation advanced — a gate
    // that reloaded only once would miss beta and gamma.
    let alpha = MemoryId::pack(0, 1, 0);
    write_memory(
        &shard,
        alpha,
        "alpha",
        SpaceId::new(),
        MemoryKind::Episodic,
        0,
    );
    assert_eq!(
        retriever
            .retrieve(
                &term_query("alpha"),
                LexicalScope::MemoryText,
                &LexicalRetrieverConfig::default(),
            )
            .expect("retrieve alpha")
            .len(),
        1,
        "first commit is visible",
    );

    let beta = MemoryId::pack(0, 2, 0);
    write_memory(
        &shard,
        beta,
        "beta",
        SpaceId::new(),
        MemoryKind::Episodic,
        0,
    );
    assert_eq!(
        retriever
            .retrieve(
                &term_query("beta"),
                LexicalScope::MemoryText,
                &LexicalRetrieverConfig::default(),
            )
            .expect("retrieve beta")
            .len(),
        1,
        "second commit is visible after the generation advanced",
    );

    let gamma = MemoryId::pack(0, 3, 0);
    write_memory(
        &shard,
        gamma,
        "gamma",
        SpaceId::new(),
        MemoryKind::Episodic,
        0,
    );
    assert_eq!(
        retriever
            .retrieve(
                &term_query("gamma"),
                LexicalScope::MemoryText,
                &LexicalRetrieverConfig::default(),
            )
            .expect("retrieve gamma")
            .len(),
        1,
        "third commit is visible",
    );

    // The earlier commits are still present — reloading forward never drops
    // prior segments.
    assert_eq!(
        retriever
            .retrieve(
                &term_query("alpha"),
                LexicalScope::MemoryText,
                &LexicalRetrieverConfig::default(),
            )
            .expect("retrieve alpha again")
            .len(),
        1,
        "prior commits remain visible",
    );
}

#[test]
fn retrieve_is_stable_across_repeated_queries_without_new_commits() {
    // No new commit between queries ⇒ the generation does not advance ⇒ the
    // gate skips the reload, and results stay identical (the idempotency
    // contract the reload used to guarantee by reloading unconditionally).
    let (_dir, shard, retriever) = fresh();
    let id = MemoryId::pack(0, 1, 0);
    write_memory(
        &shard,
        id,
        "stable",
        SpaceId::new(),
        MemoryKind::Episodic,
        0,
    );

    for _ in 0..3 {
        let hits = retriever
            .retrieve(
                &term_query("stable"),
                LexicalScope::MemoryText,
                &LexicalRetrieverConfig::default(),
            )
            .expect("retrieve");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, RankedItemId::Memory(id));
    }
}
