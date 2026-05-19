//! Tests for the statement text indexer worker (phase 22.4).
//!
//! Same runtime discipline as `super::super::memory::tests` — both
//! production and tests run under Glommio.

use std::time::Duration;

use brain_core::{StatementId, StatementKind};
use brain_index::{IndexStatus, TantivyShard};
use glommio::timer::sleep;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::Value;
use tantivy::TantivyDocument;
use tempfile::TempDir;

use crate::ops::text_indexer::{
    statement::{
        confidence_bucket, run_statement_text_indexer, StatementTextDispatcher, StatementTextOp,
    },
    CommitPolicy,
};
use crate::test_support::run_in_glommio;

fn fresh_shard() -> (TempDir, brain_index::IndexHandle) {
    let dir = TempDir::new().expect("tempdir");
    let startup = TantivyShard::open(dir.path()).expect("open");
    assert!(matches!(startup.statements_status, IndexStatus::Ready));
    let handle = startup.shard.statements.clone();
    (dir, handle)
}

fn spawn_drain(
    handle: brain_index::IndexHandle,
    policy: CommitPolicy,
) -> (StatementTextDispatcher, glommio::Task<()>) {
    let (dispatcher, rx) = StatementTextDispatcher::default_channel();
    let task = glommio::spawn_local(async move {
        run_statement_text_indexer(handle, rx, policy).await;
    });
    (dispatcher, task)
}

fn count_hits_on_field(index: &tantivy::Index, field_name: &str, query_text: &str) -> usize {
    let schema = index.schema();
    let field = schema.get_field(field_name).expect("field");
    let reader = index.reader().expect("reader");
    let searcher = reader.searcher();
    let qp = QueryParser::for_index(index, vec![field]);
    let q = qp.parse_query(query_text).expect("parse query");
    let top = searcher
        .search(&q, &TopDocs::with_limit(100).order_by_score())
        .expect("search");
    top.len()
}

#[test]
fn confidence_bucket_round_trip() {
    assert_eq!(confidence_bucket(0.0), 0);
    assert_eq!(confidence_bucket(0.05), 0);
    assert_eq!(confidence_bucket(0.27), 2);
    assert_eq!(confidence_bucket(0.5), 5);
    assert_eq!(confidence_bucket(0.99), 9);
    assert_eq!(confidence_bucket(1.0), 9, "1.0 clamps to bucket 9");
    // Defensive: out-of-range inputs clamp.
    assert_eq!(confidence_bucket(-0.5), 0);
    assert_eq!(confidence_bucket(1.5), 9);
}

#[test]
fn dispatch_upsert_then_query_returns_hit() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        let policy = CommitPolicy::new(1, Duration::from_secs(60));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        let id = StatementId::from([7u8; 16]);
        dispatcher
            .dispatch(StatementTextOp::Upsert {
                id,
                subject_canonical_name: "Alice Wong".into(),
                predicate_id: 42,
                predicate_name: "lives_in".into(),
                object_text: "Paris".into(),
                kind: StatementKind::Fact,
                confidence: 0.85,
                extracted_at_unix_ms: 1_700_000_000_000,
            })
            .await;

        drop(dispatcher);
        task.await;

        // Subject + object are TEXT fields routed through the brain
        // analyzer (lowercased + stemmed): `Alice Wong` → `alic wong`
        // (Porter stems both); `Paris` → `pari`.
        assert_eq!(
            count_hits_on_field(&handle.index, "subject_name", "alice"),
            1
        );
        assert_eq!(
            count_hits_on_field(&handle.index, "object_text", "paris"),
            1
        );
        // predicate_name uses the STRING tokenizer — exact match.
        assert_eq!(
            count_hits_on_field(&handle.index, "predicate_name", "lives_in"),
            1
        );
    })
}

#[test]
fn delete_removes_doc() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        let policy = CommitPolicy::new(1, Duration::from_secs(60));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        let id = StatementId::from([13u8; 16]);
        dispatcher
            .dispatch(StatementTextOp::Upsert {
                id,
                subject_canonical_name: "Bob".into(),
                predicate_id: 1,
                predicate_name: "works_at".into(),
                object_text: "Acme".into(),
                kind: StatementKind::Fact,
                confidence: 0.6,
                extracted_at_unix_ms: 0,
            })
            .await;
        dispatcher.dispatch(StatementTextOp::Delete { id }).await;
        drop(dispatcher);
        task.await;

        assert_eq!(count_hits_on_field(&handle.index, "subject_name", "bob"), 0);
    })
}

#[test]
fn supersede_pattern_delete_then_upsert() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        let policy = CommitPolicy::new(1, Duration::from_secs(60));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        let old_id = StatementId::from([1u8; 16]);
        let new_id = StatementId::from([2u8; 16]);

        dispatcher
            .dispatch(StatementTextOp::Upsert {
                id: old_id,
                subject_canonical_name: "Carol".into(),
                predicate_id: 1,
                predicate_name: "likes".into(),
                object_text: "salsa".into(),
                kind: StatementKind::Preference,
                confidence: 0.7,
                extracted_at_unix_ms: 0,
            })
            .await;
        dispatcher
            .dispatch(StatementTextOp::Delete { id: old_id })
            .await;
        dispatcher
            .dispatch(StatementTextOp::Upsert {
                id: new_id,
                subject_canonical_name: "Carol".into(),
                predicate_id: 1,
                predicate_name: "likes".into(),
                object_text: "tango".into(),
                kind: StatementKind::Preference,
                confidence: 0.9,
                extracted_at_unix_ms: 1_000,
            })
            .await;
        drop(dispatcher);
        task.await;

        assert_eq!(
            count_hits_on_field(&handle.index, "object_text", "salsa"),
            0
        );
        assert_eq!(
            count_hits_on_field(&handle.index, "object_text", "tango"),
            1
        );
    })
}

#[test]
fn commit_by_time_flushes_below_n() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        let policy = CommitPolicy::new(1_000, Duration::from_millis(80));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        dispatcher
            .dispatch(StatementTextOp::Upsert {
                id: StatementId::from([99u8; 16]),
                subject_canonical_name: "Dora".into(),
                predicate_id: 1,
                predicate_name: "owns".into(),
                object_text: "cabin".into(),
                kind: StatementKind::Fact,
                confidence: 0.5,
                extracted_at_unix_ms: 0,
            })
            .await;

        sleep(Duration::from_millis(200)).await;
        assert_eq!(
            count_hits_on_field(&handle.index, "object_text", "cabin"),
            1
        );

        drop(dispatcher);
        task.await;
    })
}

#[test]
fn upsert_round_trips_metadata_fields() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        let policy = CommitPolicy::new(1, Duration::from_secs(60));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        let id = StatementId::from([3u8; 16]);
        dispatcher
            .dispatch(StatementTextOp::Upsert {
                id,
                subject_canonical_name: "Eve".into(),
                predicate_id: 7,
                predicate_name: "born_in".into(),
                object_text: "Tokyo".into(),
                kind: StatementKind::Event,
                confidence: 0.65,
                extracted_at_unix_ms: 1_700_000_000_000,
            })
            .await;
        drop(dispatcher);
        task.await;

        let schema = handle.index.schema();
        let stmt_id_field = schema.get_field("statement_id").expect("statement_id");
        let predicate_id_field = schema.get_field("predicate_id").expect("predicate_id");
        let reader = handle.index.reader().expect("reader");
        let searcher = reader.searcher();
        let qp = QueryParser::for_index(
            &handle.index,
            vec![schema.get_field("subject_name").expect("subject")],
        );
        let q = qp.parse_query("eve").expect("query");
        let top = searcher
            .search(&q, &TopDocs::with_limit(10).order_by_score())
            .expect("search");
        assert_eq!(top.len(), 1);

        let doc: TantivyDocument = searcher.doc(top[0].1).expect("doc");
        let stored_id_bytes = doc
            .get_first(stmt_id_field)
            .and_then(|v| v.as_bytes())
            .expect("statement_id stored");
        let stored_id_arr: [u8; 16] = stored_id_bytes.try_into().expect("16 bytes");
        let stored_id = StatementId::from(stored_id_arr);
        assert_eq!(stored_id, id);

        // confidence_bucket is INDEXED|FAST but NOT STORED, so
        // `get_first` would return None. Verify by querying the
        // bucket field instead: floor(0.65 * 10) = 6.
        let bucket_qp = QueryParser::for_index(
            &handle.index,
            vec![schema.get_field("confidence_bucket").expect("bucket")],
        );
        let bucket_q = bucket_qp.parse_query("6").expect("query bucket");
        let bucket_hits = searcher
            .search(&bucket_q, &TopDocs::with_limit(10).order_by_score())
            .expect("search bucket");
        assert_eq!(bucket_hits.len(), 1);
        // predicate_id similarly INDEXED-only; query confirms presence.
        let pid_qp = QueryParser::for_index(&handle.index, vec![predicate_id_field]);
        let pid_q = pid_qp.parse_query("7").expect("query predicate_id");
        let pid_hits = searcher
            .search(&pid_q, &TopDocs::with_limit(10).order_by_score())
            .expect("search predicate_id");
        assert_eq!(pid_hits.len(), 1);
    })
}
