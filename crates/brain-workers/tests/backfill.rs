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

/// Count how many of the known `(memory, extractor)` pairs have a
/// `Completed` checkpoint. This is the test's **independent** view of
/// progress — read straight from the durable `worker_checkpoints` table,
/// never from the worker's in-memory counters — so it can cross-check the
/// worker's own `completed` accounting for the exactly-once property.
fn count_completed(metadata: &SharedMetadataDb, ids: &[MemoryId], ext_ids: &[ExtractorId]) -> u64 {
    let mut n = 0u64;
    for id in ids {
        for e in ext_ids {
            if checkpoint_completed(metadata, *id, e.raw()) {
                n += 1;
            }
        }
    }
    n
}

/// Assert every known `(memory, extractor)` pair ended `Completed`.
fn assert_all_pairs_completed(
    metadata: &SharedMetadataDb,
    ids: &[MemoryId],
    ext_ids: &[ExtractorId],
    ctx: &str,
) {
    for id in ids {
        for e in ext_ids {
            assert!(
                checkpoint_completed(metadata, *id, e.raw()),
                "pair ({id:?}, ext={}) not Completed [{ctx}]",
                e.raw()
            );
        }
    }
}

/// Deterministic splitmix64 PRNG. Seeded so a chaos scenario replays
/// byte-for-byte — a green run stays green; a failure reproduces from the
/// printed seed. No external `rand` dependency, no wall-clock, no
/// thread-scheduling non-determinism.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `[lo, hi]` (inclusive). `lo <= hi`.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next_u64() as usize % (hi - lo + 1))
    }
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

// ===========================================================================
// Chaos: exactly-once under repeated interruption (§19.01 durability bar,
// §19.06 "backfill resumable on interrupt").
//
// Crash-injection method: a "crash" is `drop(worker); drop(ctx);
// drop(metadata)` — the whole in-memory run state and every open handle go
// away — followed by re-opening the SAME redb path and re-submitting the
// SAME `BackfillRequest`. Only the durable `worker_checkpoints` rows survive
// the drop, exactly as they would across a real process restart.
//
// Exactly-once instrumentation: two mutually-checking observers.
//   1. `worker_completed_sum` — the sum, across every restart segment, of
//      that segment's final `progress().completed`. A segment's `completed`
//      counts only pairs *freshly* driven to `Completed` in that segment
//      (resume-skips land in `skipped_already_completed`, not `completed`),
//      so the sum over all segments is the number of first-time completions.
//   2. `count_completed(...)` — an independent scan of the durable
//      checkpoint table.
// Exactly-once holds iff `worker_completed_sum == final_table_completed ==
// total`. A duplicate process inflates the worker sum above the table count
// (and above `total`); a dropped/skipped pair leaves the table count below
// `total`. Either divergence fails the assert rather than being smoothed
// over.
// ===========================================================================

/// One randomized crash-resume scenario. Seeds `n_mem` memories over
/// `n_ext` extractors with `batch_size` chosen so a run spans several
/// cycles, then drives the worker in short bursts punctuated by crashes
/// (drop + reopen the same redb) until the run completes. Asserts the
/// exactly-once invariant across all restarts.
async fn run_chaos_scenario(n_mem: u64, n_ext: u32, batch: usize, seed: u64) {
    let label = format!("(n_mem={n_mem}, n_ext={n_ext}, batch={batch}, seed={seed:#x})");
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("meta.redb");
    let ext_ids: Vec<ExtractorId> = (1..=n_ext).map(ExtractorId).collect();
    let total = n_mem * u64::from(n_ext);

    // Seed the memories once into the persistent redb, then drop the handle.
    let ids: Vec<MemoryId> = {
        let (_ctx, metadata) = build_ctx(&db_path);
        (0..n_mem).map(|s| seed_memory(&metadata, s)).collect()
    };

    // The request is built once and re-submitted verbatim (same BackfillId)
    // on every resume — a restart re-issues the identical admin request.
    let req = BackfillRequest::new(BackfillRange::All, ext_ids.clone()).dry_run();

    let mut rng = Rng::new(seed);
    let mut worker_completed_sum = 0u64;
    let mut prev_table_completed = 0u64;

    // A single uninterrupted segment walks every memory exactly once, so
    // `total.div_ceil(batch) + 2` cycles always finishes from any resume
    // state. Early segments crash after 1..=3 cycles (many interruption
    // points); once the soft budget of crash-happy segments is spent, the
    // remaining segments run `full_cycles` so the scenario deterministically
    // terminates for every seed rather than relying on a lucky draw.
    let full_cycles = (total as usize).div_ceil(batch) + 2;
    let soft_segments = (total as usize) + 6;
    let max_segments = soft_segments + 3;

    let mut finished = false;
    for seg in 0..max_segments {
        let cycles = if seg < soft_segments {
            rng.range(1, 3)
        } else {
            full_cycles
        };

        let (ctx, metadata) = build_ctx(&db_path);
        let worker = BackfillWorker::new().with_config(small_batch_config(batch));
        worker.submit(req.clone());

        let mut seg_completed = 0u64;
        let mut seg_finished = false;
        for _ in 0..cycles {
            run_cycle(&worker, ctx.clone()).await;
            let p = worker.progress();
            // `completed` accumulates across cycles *within* this segment;
            // the final read is this segment's fresh-completion total.
            seg_completed = p.completed;
            if !p.running {
                seg_finished = true;
                break;
            }
        }
        worker_completed_sum += seg_completed;

        // Independent, between-crash check on the durable table: the count of
        // Completed pairs never regresses and never exceeds `total`.
        let table_completed = count_completed(&metadata, &ids, &ext_ids);
        assert!(
            table_completed >= prev_table_completed,
            "Completed checkpoints regressed {prev_table_completed} -> {table_completed} {label}"
        );
        assert!(
            table_completed <= total,
            "more Completed checkpoints ({table_completed}) than pairs ({total}) {label}"
        );
        prev_table_completed = table_completed;

        // Crash: every handle to the redb goes away; only the on-disk
        // checkpoints survive into the next segment.
        drop(worker);
        drop(ctx);
        drop(metadata);

        if seg_finished {
            finished = true;
            break;
        }
    }

    assert!(
        finished,
        "backfill never completed within {max_segments} restart segments {label}"
    );

    // Exactly-once: the worker freshly completed each pair once (sum), and
    // the durable table holds exactly one Completed row per pair. The two
    // independent observers must agree, and both must equal `total`.
    let final_metadata: SharedMetadataDb =
        Arc::new(brain_metadata::MetadataDb::open(&db_path).unwrap());
    let final_table_completed = count_completed(&final_metadata, &ids, &ext_ids);
    assert_eq!(
        worker_completed_sum, total,
        "worker completed {worker_completed_sum} fresh pairs, expected exactly {total} \
         (>{total} = a pair processed twice, <{total} = a pair skipped) {label}"
    );
    assert_eq!(
        final_table_completed, total,
        "durable table holds {final_table_completed} Completed pairs, expected {total} {label}"
    );
    assert_eq!(
        worker_completed_sum, final_table_completed,
        "worker fresh-completion count and durable Completed count diverged {label}"
    );
    assert_all_pairs_completed(&final_metadata, &ids, &ext_ids, &label);
}

#[test]
fn chaos_exactly_once_under_repeated_interruption() {
    // A spread of grid shapes and batch bounds, each with a fixed seed so the
    // crash schedule is deterministic and reproducible. Every scenario spans
    // multiple cycles (batch < total pairs) and is interrupted many times.
    glommio_run(|| async {
        let scenarios: [(u64, u32, usize, u64); 16] = [
            (4, 1, 2, 0x0000_0001),
            (5, 2, 3, 0x0000_0002),
            (6, 3, 4, 0x0000_0003),
            (3, 2, 2, 0x0000_0004),
            (8, 2, 3, 0x0000_0005),
            (4, 3, 5, 0x0000_0006),
            (7, 1, 2, 0x0000_0007),
            (5, 3, 4, 0x0000_0008),
            (6, 2, 5, 0x1234_5678),
            (3, 4, 2, 0x9ABC_DEF0),
            (9, 1, 3, 0xDEAD_BEEF),
            (4, 2, 7, 0xCAFE_F00D),
            (10, 2, 4, 0x0BAD_C0DE),
            (5, 4, 3, 0xFEED_FACE),
            (2, 3, 2, 0xA5A5_5A5A),
            (7, 3, 6, 0x1357_9BDF),
        ];
        for (n_mem, n_ext, batch, seed) in scenarios {
            run_chaos_scenario(n_mem, n_ext, batch, seed).await;
        }
    });
}

#[test]
fn chaos_crash_mid_memory_completes_all_extractors_on_resume() {
    // A crash *inside* a memory's extractor loop (some of its per-pair
    // checkpoints committed, the rest not, cursor un-advanced) must, on
    // resume, drive every remaining extractor of that memory to Completed —
    // none dropped at the batch boundary (bug #6), now under a real crash.
    //
    // We reconstruct the exact durable state such a crash leaves: memory 0's
    // first `pre` extractors are Completed, the rest of the grid is untouched.
    glommio_run(|| async {
        let cases: [(u64, u32, usize, u32); 4] = [
            (1, 3, 2, 1), // one memory, 3 extractors, batch 2, crashed after ext 1
            (2, 3, 2, 2), // crashed after 2 of 3 extractors of memory 0
            (3, 4, 3, 1),
            (2, 4, 2, 3),
        ];
        for (n_mem, n_ext, batch, pre) in cases {
            let label = format!("(n_mem={n_mem}, n_ext={n_ext}, batch={batch}, pre={pre})");
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("meta.redb");
            let ext_ids: Vec<ExtractorId> = (1..=n_ext).map(ExtractorId).collect();
            let total = n_mem * u64::from(n_ext);

            let ids: Vec<MemoryId> = {
                let (_ctx, metadata) = build_ctx(&db_path);
                let ids: Vec<MemoryId> = (0..n_mem).map(|s| seed_memory(&metadata, s)).collect();
                // Durable state a mid-memory crash leaves: memory 0's first
                // `pre` extractors committed Completed; cursor never advanced.
                let wtxn = metadata.write_txn().unwrap();
                for e in 1..=pre {
                    worker_checkpoints::mark_completed(
                        &wtxn,
                        WORKER_ID,
                        &item_key(ids[0], e),
                        now_unix_nanos(),
                    )
                    .unwrap();
                }
                wtxn.commit().unwrap();
                ids
            };

            // Resume with a fresh worker over the same redb.
            let (ctx, metadata) = build_ctx(&db_path);
            let worker = BackfillWorker::new().with_config(small_batch_config(batch));
            let req = BackfillRequest::new(BackfillRange::All, ext_ids.clone()).dry_run();
            worker.submit(req);

            drive_to_completion(&worker, &ctx, 128).await;

            let p = worker.progress();
            assert!(!p.running, "run should finish after resume {label}");
            assert_all_pairs_completed(&metadata, &ids, &ext_ids, &label);
            assert_eq!(
                p.skipped_already_completed,
                u64::from(pre),
                "exactly the pre-crash extractors of memory 0 are skipped {label}"
            );
            assert_eq!(
                p.completed,
                total - u64::from(pre),
                "every remaining pair (incl. the rest of memory 0) is freshly completed {label}"
            );
        }
    });
}

#[test]
fn chaos_cancel_then_resubmit_resumes_remaining_pairs() {
    // Cancel is not a durable veto: it stops the in-memory run within a
    // cycle, but the per-pair checkpoints already committed persist. A
    // resubmit of the SAME request resumes from those checkpoints — the
    // already-Completed pairs are skipped, only the remainder is processed.
    // This documents the actual cancel-then-resubmit contract (resume, not
    // restart-from-zero, and not a permanent block).
    glommio_run(|| async {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, metadata) = build_ctx(&dir.path().join("meta.redb"));
        let ids: Vec<MemoryId> = (0..5).map(|s| seed_memory(&metadata, s)).collect();
        let ext_ids = vec![ExtractorId(1), ExtractorId(2)];
        let total = (ids.len() * ext_ids.len()) as u64;

        // batch 2 = one memory (two extractors) per cycle.
        let worker = BackfillWorker::new().with_config(small_batch_config(2));
        let req = BackfillRequest::new(BackfillRange::All, ext_ids.clone()).dry_run();
        let id = worker.submit(req.clone());

        // Process two memories, then cancel.
        run_cycle(&worker, ctx.clone()).await;
        run_cycle(&worker, ctx.clone()).await;
        assert!(worker.progress().running);
        let done_before = worker.progress().completed;
        assert_eq!(
            done_before, 4,
            "two memories × two extractors done pre-cancel"
        );

        assert!(worker.cancel(id), "cancel flags the in-flight run");
        run_cycle(&worker, ctx.clone()).await; // observes cancel, finalises
        let pc = worker.progress();
        assert!(!pc.running, "cancelled run is finalised");
        assert_eq!(pc.completed, done_before, "no pair processed after cancel");
        assert_eq!(
            count_completed(&metadata, &ids, &ext_ids),
            done_before,
            "only the pre-cancel pairs are checkpointed Completed"
        );

        // Resubmit the same request: resumes from checkpoints.
        worker.submit(req.clone());
        drive_to_completion(&worker, &ctx, 64).await;
        let pr = worker.progress();
        assert!(!pr.running);
        assert_eq!(
            pr.completed,
            total - done_before,
            "resubmit re-runs only the not-yet-completed pairs"
        );
        assert_eq!(
            pr.skipped_already_completed, done_before,
            "the pre-cancel pairs are skipped on resume, not re-run"
        );
        assert_all_pairs_completed(&metadata, &ids, &ext_ids, "cancel-then-resubmit");
    });
}
