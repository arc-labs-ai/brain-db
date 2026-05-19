//! Tests for the memory text indexer worker (phase 22.3).
//!
//! Production runs the drain loop on a per-shard Glommio executor
//! (`spawn_memory_text_indexer_local` uses `glommio::spawn_local`).
//! These tests use the same runtime — `run_in_glommio` — so the
//! `wait_next` race against `glommio::timer::sleep` is exercised
//! end-to-end. A Tokio-based test cannot prove the production path.

use std::path::Path;
use std::time::Duration;

use brain_core::{AgentId, MemoryId, MemoryKind};
use brain_index::{IndexStatus, TantivyShard};
use futures_lite::FutureExt;
use glommio::timer::sleep;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::Value;
use tantivy::TantivyDocument;
use tempfile::TempDir;

use crate::ops::text_indexer::{
    memory::{run_memory_text_indexer, MemoryTextDispatcher, MemoryTextOp},
    CommitPolicy,
};
use crate::test_support::run_in_glommio;

/// Spin up a fresh `TantivyShard`, harvest its `memory_text` handle,
/// and return the shard directory tempdir alongside.
fn fresh_shard() -> (TempDir, brain_index::IndexHandle) {
    let dir = TempDir::new().expect("tempdir");
    let startup = TantivyShard::open(dir.path()).expect("open");
    assert!(matches!(startup.memory_status, IndexStatus::Ready));
    let handle = startup.shard.memory_text.clone();
    (dir, handle)
}

/// Drive the drain loop on the current Glommio executor. Returns the
/// task handle the caller can `.await` after dropping the dispatcher
/// to flush.
fn spawn_drain(
    handle: brain_index::IndexHandle,
    policy: CommitPolicy,
) -> (MemoryTextDispatcher, glommio::Task<()>) {
    let (dispatcher, rx) = MemoryTextDispatcher::default_channel();
    let task = glommio::spawn_local(async move {
        run_memory_text_indexer(handle, rx, policy).await;
    });
    (dispatcher, task)
}

fn count_hits(index: &tantivy::Index, query_text: &str) -> usize {
    let schema = index.schema();
    let text_field = schema.get_field("text").expect("text field");
    let reader = index.reader().expect("reader");
    let searcher = reader.searcher();
    let qp = QueryParser::for_index(index, vec![text_field]);
    let q = qp.parse_query(query_text).expect("parse query");
    let top = searcher
        .search(&q, &TopDocs::with_limit(100).order_by_score())
        .expect("search");
    top.len()
}

#[test]
fn dispatch_upsert_then_query_returns_hit() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        let policy = CommitPolicy::new(1, Duration::from_secs(60));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id: MemoryId::pack(0, 7, 0),
                text: "ticket ACME-1247 broke production".into(),
                agent: AgentId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
            })
            .await;

        drop(dispatcher);
        task.await;

        assert_eq!(
            count_hits(&handle.index, "acme-1247"),
            1,
            "BM25 query for the protected code ID must return the doc",
        );
        assert_eq!(
            count_hits(&handle.index, "production"),
            1,
            "stemmed residue must also be findable",
        );
    })
}

#[test]
fn forget_removes_doc() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        let policy = CommitPolicy::new(1, Duration::from_secs(60));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        let id = MemoryId::pack(0, 42, 0);
        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id,
                text: "hello world".into(),
                agent: AgentId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
            })
            .await;
        dispatcher.dispatch(MemoryTextOp::Forget { id }).await;
        drop(dispatcher);
        task.await;

        assert_eq!(count_hits(&handle.index, "hello"), 0);
    })
}

#[test]
fn commit_by_time_flushes_below_n() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        // n_writes high, interval short — the only way the doc lands
        // is via the time-based flush.
        let policy = CommitPolicy::new(1_000, Duration::from_millis(80));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id: MemoryId::pack(0, 1, 0),
                text: "elapsed timeout flushes".into(),
                agent: AgentId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
            })
            .await;

        // Wait > interval so the worker times out and commits.
        sleep(Duration::from_millis(200)).await;
        assert_eq!(count_hits(&handle.index, "timeout"), 1);

        drop(dispatcher);
        task.await;
    })
}

#[test]
fn commit_by_count_flushes_at_n() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        let policy = CommitPolicy::new(3, Duration::from_secs(60));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        for slot in 1..=3 {
            dispatcher
                .dispatch(MemoryTextOp::Upsert {
                    id: MemoryId::pack(0, slot, 0),
                    text: format!("batchword{slot}"),
                    agent: AgentId::new(),
                    kind: MemoryKind::Episodic,
                    created_at_unix_ms: 0,
                })
                .await;
        }

        // Three writes should trigger a count-based commit. Allow a
        // few ms for the loop to run.
        for _ in 0..20 {
            if count_hits(&handle.index, "batchword2") == 1 {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(count_hits(&handle.index, "batchword2"), 1);

        drop(dispatcher);
        task.await;
    })
}

#[test]
fn payload_stamped_on_commit_survives_reopen() {
    run_in_glommio(|| async {
        let dir = TempDir::new().expect("tempdir");
        let policy = CommitPolicy::new(1, Duration::from_secs(60));

        // Run a scope so the drain task fully exits before we
        // re-open via TantivyShard.
        {
            let startup = TantivyShard::open(dir.path()).expect("first open");
            let handle = startup.shard.memory_text.clone();
            let (dispatcher, task) = spawn_drain(handle, policy);
            dispatcher
                .dispatch(MemoryTextOp::Upsert {
                    id: MemoryId::pack(0, 1, 0),
                    text: "payload survives".into(),
                    agent: AgentId::new(),
                    kind: MemoryKind::Episodic,
                    created_at_unix_ms: 0,
                })
                .await;
            drop(dispatcher);
            task.await;
            // The TantivyShard arc drops here; tantivy's directory
            // mutex on Linux requires the writer to be dropped before
            // a fresh open succeeds. The drain task dropped its
            // writer already so we're safe.
        }

        let reopen = TantivyShard::open(dir.path()).expect("reopen");
        assert!(
            matches!(reopen.memory_status, IndexStatus::Ready),
            "stamped payload must round-trip as Ready, got {:?}",
            reopen.memory_status,
        );
        // And the doc is still queryable.
        let handle = reopen.shard.memory_text.clone();
        assert_eq!(count_hits(&handle.index, "survives"), 1);
    })
}

#[test]
fn dispatching_without_drain_eventually_blocks() {
    run_in_glommio(|| async {
        // Tiny queue + no drain task. Once full, sends await.
        let (dispatcher, _rx_kept_alive) = MemoryTextDispatcher::channel(2);
        let op = || MemoryTextOp::Upsert {
            id: MemoryId::pack(0, 1, 0),
            text: "x".into(),
            agent: AgentId::new(),
            kind: MemoryKind::Episodic,
            created_at_unix_ms: 0,
        };
        dispatcher.dispatch(op()).await;
        dispatcher.dispatch(op()).await;

        // Third send must block — race a 50 ms timer against the
        // dispatch future. `true` means dispatch won (would be a
        // bug); `false` means the timer fired first (expected).
        let dispatch_done = async {
            dispatcher.dispatch(op()).await;
            true
        };
        let timer_done = async {
            sleep(Duration::from_millis(50)).await;
            false
        };
        let dispatch_won = dispatch_done.or(timer_done).await;
        assert!(!dispatch_won, "dispatch resolved despite full queue");
    })
}

#[test]
fn upsert_round_trips_metadata_fields() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        let policy = CommitPolicy::new(1, Duration::from_secs(60));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        let id = MemoryId::pack(7, 13, 4);
        let agent = AgentId::new();
        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id,
                text: "round trip the stored fields".into(),
                agent,
                kind: MemoryKind::Semantic,
                created_at_unix_ms: 1_700_000_000_000,
            })
            .await;
        drop(dispatcher);
        task.await;

        // Pull the doc back, decode the stored memory_id, assert
        // round-trip.
        let schema = handle.index.schema();
        let mem_id_field = schema.get_field("memory_id").expect("memory_id");
        let agent_field = schema.get_field("agent_id").expect("agent_id");
        let reader = handle.index.reader().expect("reader");
        let searcher = reader.searcher();
        let qp =
            QueryParser::for_index(&handle.index, vec![schema.get_field("text").expect("text")]);
        let q = qp.parse_query("round").expect("query");
        let top = searcher
            .search(&q, &TopDocs::with_limit(10).order_by_score())
            .expect("search");
        assert_eq!(top.len(), 1);

        let doc: TantivyDocument = searcher.doc(top[0].1).expect("doc");
        let stored_id_bytes = doc
            .get_first(mem_id_field)
            .and_then(|v| v.as_bytes())
            .expect("memory_id stored");
        let stored_id_arr: [u8; 16] = stored_id_bytes.try_into().expect("16 bytes");
        let stored_id = MemoryId::from_raw(u128::from_be_bytes(stored_id_arr));
        assert_eq!(stored_id, id);

        let stored_agent_bytes = doc
            .get_first(agent_field)
            .and_then(|v| v.as_bytes())
            .expect("agent_id stored");
        let stored_agent_arr: [u8; 16] = stored_agent_bytes.try_into().expect("16 bytes");
        let stored_agent: AgentId = stored_agent_arr.into();
        assert_eq!(stored_agent, agent);

        // Suppress unused-path warning on macOS-non-linux builds
        let _ = Path::new(".");
    })
}

#[test]
fn end_to_end_indexer_to_retriever() {
    run_in_glommio(|| async {
        // 22.5 smoke: an Upsert via the dispatcher must surface
        // through `TantivyLexicalRetriever::retrieve` against the
        // same shard. Exercises the full write→reload→search path
        // including the protected-token tokenizer.
        use std::sync::Arc;

        use brain_index::{
            LexicalQuery, LexicalRetriever, LexicalRetrieverConfig, LexicalScope, RankedItemId,
            TantivyLexicalRetriever, TantivyShard,
        };

        let dir = TempDir::new().expect("tempdir");
        let startup = TantivyShard::open(dir.path()).expect("open");
        let shard = startup.shard.clone();
        let handle = shard.memory_text.clone();
        let policy = CommitPolicy::new(1, Duration::from_secs(60));
        let (dispatcher, task) = spawn_drain(handle, policy);

        let id = MemoryId::pack(0, 5, 0);
        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id,
                text: "ticket ACME-1247 reproduces under load".into(),
                agent: AgentId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
            })
            .await;
        drop(dispatcher);
        task.await;

        let retriever = TantivyLexicalRetriever::new(shard).expect("retriever");
        let result = retriever
            .retrieve(
                &LexicalQuery {
                    terms: vec!["acme-1247".into()],
                    ..Default::default()
                },
                LexicalScope::MemoryText,
                &LexicalRetrieverConfig::default(),
            )
            .expect("retrieve");

        assert_eq!(result.len(), 1, "indexed protected ID must surface");
        if let RankedItemId::Memory(found) = result[0].id {
            assert_eq!(found, id);
        } else {
            panic!("expected MemoryId");
        }

        // Borrow check — Arc<dyn LexicalRetriever> works.
        let _: Arc<dyn LexicalRetriever> = Arc::new(
            TantivyLexicalRetriever::new(TantivyShard::open(dir.path()).expect("reopen").shard)
                .expect("retriever"),
        );
    })
}
