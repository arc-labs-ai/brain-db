//! Tests for the memory text indexer worker.
//!
//! Production runs the drain loop on a per-shard Glommio executor
//! (`spawn_memory_text_indexer_local` uses `glommio::spawn_local`).
//! These tests use the same runtime — `run_in_glommio` — so the
//! `wait_next` race against `glommio::timer::sleep` is exercised
//! end-to-end. A Tokio-based test cannot prove the production path.

use std::path::Path;
use std::time::Duration;

use brain_core::{MemoryId, MemoryKind, SpaceId};
use brain_index::{IndexStatus, TantivyShard};
use futures_lite::FutureExt;
use glommio::timer::sleep;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::Value;
use tantivy::TantivyDocument;
use tempfile::TempDir;

use crate::index::text_indexer::{
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
    let (stop_tx, stop_rx) = flume::bounded::<()>(1);
    let (_control_tx, control_rx) = flume::bounded::<crate::index::text_indexer::IndexerControl>(1);
    let task = glommio::spawn_local(async move {
        // Held for the task's lifetime so the loop never observes a
        // shutdown signal: these tests drive the drop-of-Sender
        // (`Disconnected`) path, which must keep working unchanged.
        let _stop_tx = stop_tx;
        // Held so the control channel never closes and the loop keeps
        // serving ops; the rebuild-control path has its own tests.
        let _control_tx = _control_tx;
        run_memory_text_indexer(handle, rx, policy, stop_rx, control_rx).await;
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
                space: SpaceId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
                session: 0,
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
                space: SpaceId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
                session: 0,
            })
            .await;
        dispatcher
            .dispatch(MemoryTextOp::Forget { id, hard: false })
            .await;
        drop(dispatcher);
        task.await;

        assert_eq!(count_hits(&handle.index, "hello"), 0);
    })
}

/// Sum of tombstoned-but-still-resident docs across all searchable
/// segments. `delete_term` only marks a doc deleted; its bytes stay on
/// disk until a merge compacts them out. A hard forget must drive this to
/// zero (bytes evicted), whereas a soft forget leaves it positive.
fn total_deleted_docs(index: &tantivy::Index) -> u32 {
    index
        .searchable_segment_metas()
        .expect("segment metas")
        .iter()
        .map(tantivy::index::SegmentMeta::num_deleted_docs)
        .sum()
}

#[test]
fn hard_forget_purges_text_from_segments() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        // n_writes=2 lands both upserts in a single committed segment, so the
        // forgotten doc shares a segment with a live one. Tantivy auto-drops
        // *fully*-deleted segments on commit, so co-residency is what forces
        // the code path under test — the segment survives the delete and only
        // the hard-forget force-merge can evict the secret's bytes.
        let policy = CommitPolicy::new(2, Duration::from_secs(60));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        let secret = MemoryId::pack(0, 100, 0);
        let keep = MemoryId::pack(0, 101, 0);
        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id: secret,
                text: "confidential plaintext alphaword".into(),
                space: SpaceId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
                session: 0,
            })
            .await;
        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id: keep,
                text: "public record betaword".into(),
                space: SpaceId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
                session: 0,
            })
            .await;

        // Hard forget the secret doc — triggers commit + force-merge inline.
        dispatcher
            .dispatch(MemoryTextOp::Forget {
                id: secret,
                hard: true,
            })
            .await;
        drop(dispatcher);
        task.await;

        // The secret term is unqueryable, the kept doc survives, and the
        // segment carries no residual deleted docs — the plaintext bytes were
        // physically evicted, not merely tombstoned.
        assert_eq!(count_hits(&handle.index, "alphaword"), 0, "secret purged");
        assert_eq!(count_hits(&handle.index, "betaword"), 1, "public retained");
        assert_eq!(
            total_deleted_docs(&handle.index),
            0,
            "hard forget must compact the deleted doc out of the segment",
        );
    })
}

#[test]
fn soft_forget_tombstones_but_retains_bytes() {
    run_in_glommio(|| async {
        let (_dir, handle) = fresh_shard();
        // Both docs share one segment (see the hard-forget note); the soft
        // forget must NOT force-merge, so the tombstoned doc stays resident.
        let policy = CommitPolicy::new(2, Duration::from_secs(60));
        let (dispatcher, task) = spawn_drain(handle.clone(), policy);

        let drop_me = MemoryId::pack(0, 200, 0);
        let keep = MemoryId::pack(0, 201, 0);
        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id: drop_me,
                text: "recoverable gammaword".into(),
                space: SpaceId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
                session: 0,
            })
            .await;
        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id: keep,
                text: "retained deltaword".into(),
                space: SpaceId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
                session: 0,
            })
            .await;
        dispatcher
            .dispatch(MemoryTextOp::Forget {
                id: drop_me,
                hard: false,
            })
            .await;
        drop(dispatcher);
        task.await;

        // Soft forget removes the doc from queries but leaves the deleted
        // doc resident (grace-window recoverable, no force-merge).
        assert_eq!(count_hits(&handle.index, "gammaword"), 0);
        assert_eq!(count_hits(&handle.index, "deltaword"), 1);
        assert_eq!(
            total_deleted_docs(&handle.index),
            1,
            "soft forget must not force-merge; the tombstoned doc stays resident",
        );
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
                space: SpaceId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
                session: 0,
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
                    space: SpaceId::new(),
                    kind: MemoryKind::Episodic,
                    created_at_unix_ms: 0,
                    session: 0,
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
                    space: SpaceId::new(),
                    kind: MemoryKind::Episodic,
                    created_at_unix_ms: 0,
                    session: 0,
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
            space: SpaceId::new(),
            kind: MemoryKind::Episodic,
            created_at_unix_ms: 0,
            session: 0,
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
        let space = SpaceId::new();
        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id,
                text: "round trip the stored fields".into(),
                space,
                kind: MemoryKind::Semantic,
                created_at_unix_ms: 1_700_000_000_000,
                session: 0,
            })
            .await;
        drop(dispatcher);
        task.await;

        // Pull the doc back, decode the stored memory_id, assert
        // round-trip.
        let schema = handle.index.schema();
        let mem_id_field = schema.get_field("memory_id").expect("memory_id");
        let space_field = schema.get_field("space_id").expect("space_id");
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

        let stored_space_bytes = doc
            .get_first(space_field)
            .and_then(|v| v.as_bytes())
            .expect("space_id stored");
        let stored_space_arr: [u8; 16] = stored_space_bytes.try_into().expect("16 bytes");
        let stored_space: SpaceId = stored_space_arr.into();
        assert_eq!(stored_space, space);

        // Suppress unused-path warning on macOS-non-linux builds
        let _ = Path::new(".");
    })
}

#[test]
fn end_to_end_indexer_to_retriever() {
    run_in_glommio(|| async {
        // Smoke: an Upsert via the dispatcher must surface
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
                space: SpaceId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
                session: 0,
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

/// The live-rebuild control plane: `Quiesce` drops the writer (releasing
/// tantivy's exclusive per-directory lock so the shard can rebuild + swap
/// the index), and `Resume` rebuilds the writer on the reopened index and
/// resumes draining. This exercises the indexer half of the hot tantivy
/// rebuild dance end-to-end on the production Glommio runtime.
#[test]
fn quiesce_releases_lock_and_resume_rebuilds_writer() {
    use crate::index::text_indexer::IndexerControl;

    /// Poll until the index reports `want` hits for `query` or give up.
    async fn await_hits(index: &tantivy::Index, query: &str, want: usize) {
        for _ in 0..400 {
            if count_hits(index, query) == want {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {want} hit(s) for {query:?}");
    }

    run_in_glommio(|| async {
        let (dir, handle) = fresh_shard();
        // N=1 so every op commits immediately — no interval races.
        let policy = CommitPolicy::new(1, Duration::from_secs(60));

        let (dispatcher, rx) = MemoryTextDispatcher::default_channel();
        let (_stop_tx, stop_rx) = flume::bounded::<()>(1);
        let (control_tx, control_rx) = flume::bounded::<IndexerControl>(2);
        let task = glommio::spawn_local(async move {
            let _stop_tx = _stop_tx;
            run_memory_text_indexer(handle, rx, policy, stop_rx, control_rx).await;
        });

        // 1. Normal operation: alpha lands.
        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id: MemoryId::pack(0, 1, 0),
                text: "alpha".into(),
                space: SpaceId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
                session: 0,
            })
            .await;
        {
            let idx = TantivyShard::open(dir.path()).expect("open").shard;
            await_hits(&idx.memory_text.index, "alpha", 1).await;
        }

        // 2. Quiesce: the indexer drops its writer and acks.
        let (ack_tx, ack_rx) = flume::bounded::<()>(1);
        control_tx
            .send_async(IndexerControl::Quiesce { ack: ack_tx })
            .await
            .expect("send quiesce");
        ack_rx.recv_async().await.expect("quiesce ack");

        // 3. Lock released: an external writer opens on the SAME dir (this
        //    would fail with a LockFailure if the indexer still held it).
        //    Write beta through it, mimicking the rebuild populating the dir.
        {
            let shard = TantivyShard::open(dir.path())
                .expect("reopen for rebuild")
                .shard;
            let idx = &shard.memory_text.index;
            let mut w = idx
                .writer_with_num_threads(1, 50_000_000)
                .expect("writer lock must be free after quiesce");
            let schema = idx.schema();
            let mut doc = TantivyDocument::default();
            doc.add_bytes(schema.get_field("memory_id").unwrap(), &2u128.to_be_bytes());
            doc.add_text(schema.get_field("text").unwrap(), "beta");
            let a: [u8; 16] = SpaceId::new().into();
            doc.add_bytes(schema.get_field("space_id").unwrap(), &a);
            doc.add_u64(schema.get_field("kind").unwrap(), 0);
            doc.add_u64(schema.get_field("created_at").unwrap(), 0);
            doc.add_u64(schema.get_field("session").unwrap(), 0);
            w.add_document(doc).expect("add beta");
            w.commit().expect("commit beta");
        }

        // 4. Resume against a freshly reopened handle.
        let resumed = TantivyShard::open(dir.path())
            .expect("reopen for resume")
            .shard;
        let (ack_tx, ack_rx) = flume::bounded::<()>(1);
        control_tx
            .send_async(IndexerControl::Resume {
                handle: resumed.memory_text.clone(),
                ack: ack_tx,
            })
            .await
            .expect("send resume");
        ack_rx.recv_async().await.expect("resume ack");

        // 5. Post-resume ops index against the new writer.
        dispatcher
            .dispatch(MemoryTextOp::Upsert {
                id: MemoryId::pack(0, 3, 0),
                text: "gamma".into(),
                space: SpaceId::new(),
                kind: MemoryKind::Episodic,
                created_at_unix_ms: 0,
                session: 0,
            })
            .await;
        await_hits(&resumed.memory_text.index, "gamma", 1).await;

        // beta (external rebuild write) survived and gamma (resumed
        // indexer) is present — the writer genuinely rebuilt on the new
        // index, not the old one.
        assert_eq!(count_hits(&resumed.memory_text.index, "beta"), 1);
        assert_eq!(count_hits(&resumed.memory_text.index, "gamma"), 1);

        drop(dispatcher);
        drop(control_tx);
        task.await;
    })
}
