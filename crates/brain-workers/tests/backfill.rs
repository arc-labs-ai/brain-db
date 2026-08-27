#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Backfill worker integration tests.
//!
//! Guards the resumable/cancelable backfill machinery now wired to the
//! `ADMIN_BACKFILL` / `ADMIN_BACKFILL_CANCEL` wire ops: a submitted run
//! walks the `(memory × extractor)` grid, checkpointing each pair in the
//! shared `worker_checkpoints` redb table; a restart (drop + re-create the
//! worker over the same redb) resumes from the checkpoints without
//! re-running or dropping any pair; a cancel stops the run within a cycle;
//! a memory whose extractor count exceeds the batch budget still has every
//! pair processed (bug #6); and a pair past the per-item attempt cap is
//! reported `failed`, not `skipped` (bug #8).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_core::{
    BackfillRange, BackfillRequest, ExtractorId, MemoryId, MemoryKind, SessionId, SpaceId,
};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::tables::worker_checkpoints;
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::workers::backfill::{BackfillWorker, MAX_ATTEMPTS_PER_ITEM, WORKER_ID};
use brain_workers::{Worker, WorkerConfig, WorkerContext, WorkerKind};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct NopDispatcher;
impl Dispatcher for NopDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}

/// Build an `OpsContext` over a metadata DB at `db_path`. The DB is
/// re-openable, so a resume test can drop the worker + ctx and reopen the
/// same path to prove the checkpoints survived.
fn build_ctx(db_path: &std::path::Path) -> (Arc<OpsContext>, SharedMetadataDb) {
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );
    let ctx = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
    (ctx, metadata)
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn make_id(slot: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[8..16].copy_from_slice(&slot.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

fn seed_memory(metadata: &SharedMetadataDb, slot: u64) -> MemoryId {
    let id = make_id(slot);
    let wtxn = metadata.write_txn().unwrap();
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        let meta = MemoryMetadata::new_active(
            id,
            brain_core::NamespaceId::SYSTEM,
            SpaceId(Uuid::nil()),
            SessionId(1),
            slot,
            1,
            MemoryKind::Episodic,
            [0; 16],
            1.0,
            16,
            now_unix_nanos(),
        );
        table.insert(id.to_be_bytes(), meta).unwrap();
    }
    wtxn.commit().unwrap();
    id
}

/// Reconstruct the worker's per-pair checkpoint key. Matches the
/// `item_key_for` layout in the worker: `memory_id` big-endian (16 bytes)
/// followed by `extractor_id` little-endian (4 bytes).
fn item_key(memory_id: MemoryId, extractor_id_raw: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(20);
    k.extend_from_slice(&memory_id.raw().to_be_bytes());
    k.extend_from_slice(&extractor_id_raw.to_le_bytes());
    k
}

fn checkpoint_completed(metadata: &SharedMetadataDb, memory_id: MemoryId, ext: u32) -> bool {
    let rtxn = metadata.read_txn().unwrap();
    worker_checkpoints::get(&rtxn, WORKER_ID, &item_key(memory_id, ext))
        .unwrap()
        .map(|r| r.is_completed())
        .unwrap_or(false)
}

fn small_batch_config(batch_size: usize) -> WorkerConfig {
    WorkerConfig {
        enabled: true,
        interval: Duration::from_millis(1),
        batch_size,
        max_runtime: Duration::from_secs(5),
    }
}

async fn run_cycle(worker: &BackfillWorker, ctx: Arc<OpsContext>) -> usize {
    let wctx = WorkerContext {
        ops: ctx,
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    worker.run_cycle(&wctx).await.unwrap()
}

/// Drive `run_cycle` until the worker reports it is no longer running,
/// bounded by `max_cycles` so a bug can't hang the suite.
async fn drive_to_completion(worker: &BackfillWorker, ctx: &Arc<OpsContext>, max_cycles: usize) {
    for _ in 0..max_cycles {
        run_cycle(worker, ctx.clone()).await;
        if !worker.progress().running {
            return;
        }
    }
    panic!("backfill did not complete within {max_cycles} cycles");
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("backfill-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}

// ===========================================================================
// Tests.
// ===========================================================================

#[test]
fn processes_all_seeded_memories() {
    glommio_run(|| async {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, metadata) = build_ctx(&dir.path().join("meta.redb"));
        let ids: Vec<MemoryId> = (0..3).map(|s| seed_memory(&metadata, s)).collect();

        let worker = BackfillWorker::new();
        let req = BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]).dry_run();
        worker.submit(req);

        drive_to_completion(&worker, &ctx, 16).await;

        let p = worker.progress();
        assert!(!p.running, "run should be finished");
        assert_eq!(p.completed, 3, "every seeded memory processed once");
        assert_eq!(p.failed, 0);
        for id in &ids {
            assert!(
                checkpoint_completed(&metadata, *id, 1),
                "{id:?} not completed"
            );
        }
    });
}

#[test]
fn resumes_after_restart_without_dup_or_skip() {
    glommio_run(|| async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("meta.redb");
        let ids: Vec<MemoryId>;

        // First worker: batch_size = 2 so a single cycle stops after two
        // memories, leaving the run mid-flight.
        {
            let (ctx, metadata) = build_ctx(&db_path);
            ids = (0..4).map(|s| seed_memory(&metadata, s)).collect();
            let worker = BackfillWorker::new().with_config(small_batch_config(2));
            let req = BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]).dry_run();
            worker.submit(req);

            run_cycle(&worker, ctx.clone()).await;
            let p = worker.progress();
            assert!(p.running, "run should still be in flight after one cycle");
            assert_eq!(p.completed, 2, "batch cap processes exactly two memories");
            // Two memories checkpointed, two not yet.
            let done = ids
                .iter()
                .filter(|id| checkpoint_completed(&metadata, **id, 1))
                .count();
            assert_eq!(done, 2, "two of four checkpointed pre-restart");
        }

        // Second worker over the SAME redb: the checkpoints persisted, so
        // it re-walks, skips the two already-`Completed`, and finishes the
        // remaining two — no pair re-run, none dropped.
        {
            let (ctx, metadata) = build_ctx(&db_path);
            let worker = BackfillWorker::new().with_config(small_batch_config(2));
            let req = BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]).dry_run();
            worker.submit(req);

            drive_to_completion(&worker, &ctx, 16).await;

            let p = worker.progress();
            assert!(!p.running);
            assert_eq!(p.completed, 2, "only the two unfinished pairs re-run");
            assert_eq!(
                p.skipped_already_completed, 2,
                "the two pre-restart pairs are skipped, not re-run"
            );
            for id in &ids {
                assert!(
                    checkpoint_completed(&metadata, *id, 1),
                    "{id:?} not completed after resume"
                );
            }
        }
    });
}

#[test]
fn cancel_stops_run_within_a_cycle() {
    glommio_run(|| async {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, metadata) = build_ctx(&dir.path().join("meta.redb"));
        let ids: Vec<MemoryId> = (0..3).map(|s| seed_memory(&metadata, s)).collect();

        let worker = BackfillWorker::new().with_config(small_batch_config(1));
        let req = BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]).dry_run();
        let id = worker.submit(req);

        // Cycle 1 processes one memory and leaves the run in flight.
        run_cycle(&worker, ctx.clone()).await;
        assert!(worker.progress().running);
        assert_eq!(worker.progress().completed, 1);

        // Cancel the in-flight run, then run another cycle: it finalises
        // immediately without touching the remaining memories.
        assert!(worker.cancel(id), "cancel should flag the in-flight run");
        run_cycle(&worker, ctx.clone()).await;

        let p = worker.progress();
        assert!(!p.running, "cancelled run should be finalised");
        assert_eq!(p.completed, 1, "no memory processed after cancel");
        let done = ids
            .iter()
            .filter(|m| checkpoint_completed(&metadata, **m, 1))
            .count();
        assert_eq!(done, 1, "only the pre-cancel memory is checkpointed");
    });
}

#[test]
fn batch_boundary_processes_all_extractors_of_a_memory() {
    // Bug #6: a memory with more extractors than the batch budget must get
    // every extractor processed, none dropped at the batch boundary.
    glommio_run(|| async {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, metadata) = build_ctx(&dir.path().join("meta.redb"));
        let id = seed_memory(&metadata, 0);

        // batch_size = 2, but the memory has 3 extractors.
        let worker = BackfillWorker::new().with_config(small_batch_config(2));
        let req = BackfillRequest::new(
            BackfillRange::All,
            vec![ExtractorId(1), ExtractorId(2), ExtractorId(3)],
        )
        .dry_run();
        worker.submit(req);

        drive_to_completion(&worker, &ctx, 16).await;

        let p = worker.progress();
        assert!(!p.running);
        assert_eq!(p.completed, 3, "all three (memory, extractor) pairs done");
        for ext in [1u32, 2, 3] {
            assert!(
                checkpoint_completed(&metadata, id, ext),
                "extractor {ext} pair was dropped at the batch boundary"
            );
        }
    });
}

#[test]
fn permanently_failed_item_is_counted_failed_not_skipped() {
    // Bug #8: a pair past MAX_ATTEMPTS_PER_ITEM is reported `failed`, not
    // masked as `skipped_already_completed`.
    glommio_run(|| async {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, metadata) = build_ctx(&dir.path().join("meta.redb"));
        let id = seed_memory(&metadata, 0);

        // Pre-seed the (memory, extractor 1) checkpoint as Failed with
        // attempts == MAX_ATTEMPTS_PER_ITEM (drive it there via mark_failed).
        let key = item_key(id, 1);
        {
            let wtxn = metadata.write_txn().unwrap();
            worker_checkpoints::mark_started(&wtxn, WORKER_ID, &key, now_unix_nanos()).unwrap();
            for _ in 0..MAX_ATTEMPTS_PER_ITEM {
                worker_checkpoints::mark_failed(
                    &wtxn,
                    WORKER_ID,
                    &key,
                    "seeded failure".to_owned(),
                    now_unix_nanos(),
                )
                .unwrap();
            }
            wtxn.commit().unwrap();
        }
        {
            let rtxn = metadata.read_txn().unwrap();
            let row = worker_checkpoints::get(&rtxn, WORKER_ID, &key)
                .unwrap()
                .unwrap();
            assert!(row.is_failed());
            assert!(row.attempts >= MAX_ATTEMPTS_PER_ITEM);
        }

        let worker = BackfillWorker::new();
        // Not dry_run: exercise the live path so the permanent-failure
        // short-circuit (not the dry-run mark-completed) is what runs.
        let req = BackfillRequest::new(BackfillRange::All, vec![ExtractorId(1)]);
        worker.submit(req);

        drive_to_completion(&worker, &ctx, 16).await;

        let p = worker.progress();
        assert!(!p.running);
        assert_eq!(p.failed, 1, "permanently-failed pair reported as failed");
        assert_eq!(
            p.skipped_already_completed, 0,
            "must not be masked as a resume-skip"
        );
        assert_eq!(p.completed, 0);
    });
}

#[test]
fn idle_worker_run_cycle_is_a_noop() {
    // A provisioned worker with no submitted run ticks to a no-op.
    glommio_run(|| async {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _metadata) = build_ctx(&dir.path().join("meta.redb"));
        let worker = BackfillWorker::new();
        assert_eq!(worker.kind(), WorkerKind::Backfill);
        let processed = run_cycle(&worker, ctx.clone()).await;
        assert_eq!(processed, 0, "no run submitted → nothing processed");
        assert!(!worker.progress().running);
    });
}
