//! Tests for the memory text indexer worker (phase 22.3).
//!
//! These run on the host's tokio runtime, not Glommio — tantivy's
//! `IndexWriter` is `Send + Sync` and the drain loop uses only
//! `flume` + `tokio::time`, no Glommio primitives, so `tokio::spawn`
//! is enough to exercise the full pipeline.

use std::path::Path;
use std::time::Duration;

use brain_core::{AgentId, MemoryId, MemoryKind};
use brain_index::{IndexStatus, TantivyShard};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::Value;
use tantivy::TantivyDocument;
use tempfile::TempDir;
use tokio::time::sleep;

use crate::ops::text_indexer::{
    memory::{run_memory_text_indexer, MemoryTextDispatcher, MemoryTextOp},
    CommitPolicy,
};

/// Spin up a fresh `TantivyShard`, harvest its `memory_text` handle,
/// and return the shard directory tempdir alongside.
fn fresh_shard() -> (TempDir, brain_index::IndexHandle) {
    let dir = TempDir::new().expect("tempdir");
    let startup = TantivyShard::open(dir.path()).expect("open");
    assert!(matches!(startup.memory_status, IndexStatus::Ready));
    let handle = startup.shard.memory_text.clone();
    (dir, handle)
}

/// Drive the drain loop on tokio. Returns a join handle the caller
/// can `.await` after dropping the dispatcher to flush.
async fn spawn_drain(
    handle: brain_index::IndexHandle,
    policy: CommitPolicy,
) -> (MemoryTextDispatcher, tokio::task::JoinHandle<()>) {
    let (dispatcher, rx) = MemoryTextDispatcher::default_channel();
    let join = tokio::spawn(async move {
        run_memory_text_indexer(handle, rx, policy).await;
    });
    (dispatcher, join)
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

#[tokio::test(flavor = "current_thread")]
async fn dispatch_upsert_then_query_returns_hit() {
    let (_dir, handle) = fresh_shard();
    let policy = CommitPolicy::new(1, Duration::from_secs(60));
    let (dispatcher, join) = spawn_drain(handle.clone(), policy).await;

    dispatcher
        .dispatch(MemoryTextOp::Upsert {
            id: MemoryId::pack(0, 7, 0),
            text: "ticket ACME-1247 broke production".into(),
            agent: AgentId::new(),
            kind: MemoryKind::Episodic,
            created_at_unix_ms: 0,
        })
        .await;

    // Close the channel + wait for the loop to drain + commit.
    drop(dispatcher);
    join.await.expect("drain task");

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
}

#[tokio::test(flavor = "current_thread")]
async fn forget_removes_doc() {
    let (_dir, handle) = fresh_shard();
    let policy = CommitPolicy::new(1, Duration::from_secs(60));
    let (dispatcher, join) = spawn_drain(handle.clone(), policy).await;

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
    join.await.expect("drain task");

    assert_eq!(count_hits(&handle.index, "hello"), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn commit_by_time_flushes_below_n() {
    let (_dir, handle) = fresh_shard();
    // n_writes high, interval short — the only way the doc lands
    // is via the time-based flush.
    let policy = CommitPolicy::new(1_000, Duration::from_millis(80));
    let (dispatcher, join) = spawn_drain(handle.clone(), policy).await;

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
    join.await.expect("drain task");
}

#[tokio::test(flavor = "current_thread")]
async fn commit_by_count_flushes_at_n() {
    let (_dir, handle) = fresh_shard();
    let policy = CommitPolicy::new(3, Duration::from_secs(60));
    let (dispatcher, join) = spawn_drain(handle.clone(), policy).await;

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
    join.await.expect("drain task");
}

#[tokio::test(flavor = "current_thread")]
async fn payload_stamped_on_commit_survives_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let policy = CommitPolicy::new(1, Duration::from_secs(60));

    // Run a scope so the drain task fully exits before we
    // re-open via TantivyShard.
    {
        let startup = TantivyShard::open(dir.path()).expect("first open");
        let handle = startup.shard.memory_text.clone();
        let (dispatcher, join) = spawn_drain(handle, policy).await;
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
        join.await.expect("drain");
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
}

#[tokio::test(flavor = "current_thread")]
async fn dispatching_without_drain_eventually_blocks() {
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

    // Third send must block — race a 50 ms timeout against the
    // dispatch future and assert the dispatch did NOT complete.
    let dispatch_future = dispatcher.dispatch(op());
    let timeout = sleep(Duration::from_millis(50));
    tokio::pin!(dispatch_future);
    tokio::pin!(timeout);
    tokio::select! {
        () = &mut dispatch_future => panic!("dispatch resolved despite full queue"),
        () = &mut timeout => {} // expected
    }
}

#[tokio::test(flavor = "current_thread")]
async fn upsert_round_trips_metadata_fields() {
    let (_dir, handle) = fresh_shard();
    let policy = CommitPolicy::new(1, Duration::from_secs(60));
    let (dispatcher, join) = spawn_drain(handle.clone(), policy).await;

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
    join.await.expect("drain");

    // Pull the doc back, decode the stored memory_id, assert
    // round-trip.
    let schema = handle.index.schema();
    let mem_id_field = schema.get_field("memory_id").expect("memory_id");
    let agent_field = schema.get_field("agent_id").expect("agent_id");
    let reader = handle.index.reader().expect("reader");
    let searcher = reader.searcher();
    let qp = QueryParser::for_index(&handle.index, vec![schema.get_field("text").expect("text")]);
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
}

#[tokio::test(flavor = "current_thread")]
async fn end_to_end_indexer_to_retriever() {
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
    let (dispatcher, join) = spawn_drain(handle, policy).await;

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
    join.await.expect("drain task");

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
}
