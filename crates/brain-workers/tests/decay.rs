#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Decay worker integration tests.
//!
//! Guards salience time-decay: salience follows an exponential half-life
//! keyed by memory kind (episodic / semantic / consolidated), so a memory
//! aged one half-life is at half salience, age zero is identity, and
//! extreme age clamps above zero without NaN. Pins the per-cycle batch
//! cap, the sub-threshold skip (no write for negligible change), and
//! cursor advance/wrap so every row is eventually visited.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{
    decayed_salience, half_life_days, DecayWorker, Worker, WorkerConfig, WorkerContext, WorkerKind,
    WorkerScheduler, CONSOLIDATED_HALF_LIFE_DAYS, EPISODIC_HALF_LIFE_DAYS, SEMANTIC_HALF_LIFE_DAYS,
};
use uuid::Uuid;

const NANOS_PER_DAY: u64 = 86_400 * 1_000_000_000;

// ---------------------------------------------------------------------------
// Fixture: OpsContext + helper to seed memory rows with custom
// created_at to control age, and read salience back.
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

struct Fixture {
    ctx: Arc<OpsContext>,
    metadata: SharedMetadataDb,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx: Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)),
        metadata,
        _tempdir: tempdir,
    }
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

fn seed_memory(
    metadata: &SharedMetadataDb,
    slot: u64,
    salience_initial: f32,
    kind: MemoryKind,
    created_at_unix_nanos: u64,
) -> MemoryId {
    let id = make_id(slot);
    let wtxn = metadata.write_txn().unwrap();
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        let meta = MemoryMetadata::new_active(
            id,
            AgentId(Uuid::nil()),
            ContextId(1),
            slot,
            1,
            kind,
            [0; 16],
            salience_initial,
            16,
            created_at_unix_nanos,
        );
        table.insert(id.to_be_bytes(), meta).unwrap();
    }
    wtxn.commit().unwrap();
    id
}

fn read_salience(metadata: &SharedMetadataDb, id: MemoryId) -> f32 {
    let rtxn = metadata.read_txn().unwrap();
    let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
    let access = table.get(id.to_be_bytes()).unwrap().unwrap();
    access.value().salience
}

async fn run_one_cycle(
    worker: &DecayWorker,
    ctx: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wctx = WorkerContext {
        ops: ctx,
        shutdown: shutdown_flag.clone(),
    };
    worker.run_cycle(&wctx).await
}

// ===========================================================================
// Pure-function correctness (5).
// ===========================================================================

#[test]
fn episodic_30_days_old_halves() {
    let s = decayed_salience(1.0, 30 * NANOS_PER_DAY, MemoryKind::Episodic);
    assert!((s - 0.5).abs() < 1e-5, "expected ~0.5, got {s}");
}

#[test]
fn semantic_365_days_old_halves() {
    let s = decayed_salience(0.8, 365 * NANOS_PER_DAY, MemoryKind::Semantic);
    assert!((s - 0.4).abs() < 1e-5, "expected ~0.4, got {s}");
}

#[test]
fn consolidated_90_days_old_halves() {
    let s = decayed_salience(1.0, 90 * NANOS_PER_DAY, MemoryKind::Consolidated);
    assert!((s - 0.5).abs() < 1e-5, "expected ~0.5, got {s}");
}

#[test]
fn age_zero_is_identity() {
    let s = decayed_salience(0.5, 0, MemoryKind::Episodic);
    assert_eq!(s, 0.5);
}

#[test]
fn extreme_age_clamps_above_zero_no_nan() {
    let s = decayed_salience(1.0, 10_000 * NANOS_PER_DAY, MemoryKind::Episodic);
    assert!(s.is_finite() && s >= 0.0, "got {s}");
    // 10000 / 30 ≈ 333 half-lives; well past f32 precision.
    assert!(s < 1e-10, "ought to be effectively zero, got {s}");
}

#[test]
fn half_life_days_matches_constants() {
    assert_eq!(
        half_life_days(MemoryKind::Episodic),
        EPISODIC_HALF_LIFE_DAYS
    );
    assert_eq!(
        half_life_days(MemoryKind::Semantic),
        SEMANTIC_HALF_LIFE_DAYS
    );
    assert_eq!(
        half_life_days(MemoryKind::Consolidated),
        CONSOLIDATED_HALF_LIFE_DAYS
    );
}

// ===========================================================================
// Cycle behaviour (6).
// ===========================================================================

#[test]
fn cycle_decays_one_memory_when_past_threshold() {
    glommio_run(|| async {
        let fix = build_fixture();
        let id = seed_memory(
            &fix.metadata,
            1,
            1.0,
            MemoryKind::Episodic,
            now_unix_nanos() - 30 * NANOS_PER_DAY,
        );
        let worker = DecayWorker::new();
        let processed = run_one_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 1);
        let new_sal = read_salience(&fix.metadata, id);
        assert!(
            (new_sal - 0.5).abs() < 1e-3,
            "expected ~0.5 after 30d Episodic, got {new_sal}"
        );
    });
}

#[test]
fn cycle_skips_minor_changes_under_threshold() {
    glommio_run(|| async {
        let fix = build_fixture();
        // 1 minute old — decay is far below 0.001 threshold.
        let one_minute_nanos: u64 = 60 * 1_000_000_000;
        let id = seed_memory(
            &fix.metadata,
            1,
            1.0,
            MemoryKind::Episodic,
            now_unix_nanos() - one_minute_nanos,
        );
        let worker = DecayWorker::new();
        let processed = run_one_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0, "minor-change cycle must not write");
        let s = read_salience(&fix.metadata, id);
        assert_eq!(s, 1.0, "salience must be untouched");
    });
}

#[test]
fn cycle_respects_batch_size() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // Seed 50 memories all 30 days old — all want a write.
        for slot in 1..=50 {
            seed_memory(
                &fix.metadata,
                slot,
                1.0,
                MemoryKind::Episodic,
                now - 30 * NANOS_PER_DAY,
            );
        }
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(1),
            batch_size: 10,
            max_runtime: Duration::from_secs(60),
        };
        let worker = DecayWorker::new().with_config(cfg);
        let processed = run_one_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 10, "batch_size=10 must bound the write count");
    });
}

#[test]
fn cycle_advances_cursor_across_invocations() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        for slot in 1..=30 {
            seed_memory(
                &fix.metadata,
                slot,
                1.0,
                MemoryKind::Episodic,
                now - 30 * NANOS_PER_DAY,
            );
        }
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(1),
            batch_size: 10,
            max_runtime: Duration::from_secs(60),
        };
        let worker = DecayWorker::new().with_config(cfg);

        let p1 = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        let p2 = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        let p3 = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(p1 + p2 + p3, 30, "three cycles cover all 30 memories");

        // Every memory must now be ~0.5 — proves no gaps, no duplicates.
        for slot in 1..=30 {
            let s = read_salience(&fix.metadata, make_id(slot));
            assert!(
                (s - 0.5).abs() < 1e-3,
                "slot {slot} salience {s} not decayed"
            );
        }
    });
}

#[test]
fn cycle_wraps_cursor_after_full_pass() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        for slot in 1..=5 {
            seed_memory(
                &fix.metadata,
                slot,
                1.0,
                MemoryKind::Episodic,
                now - 30 * NANOS_PER_DAY,
            );
        }
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(1),
            batch_size: 10, // larger than table → one cycle reaches end
            max_runtime: Duration::from_secs(60),
        };
        let worker = DecayWorker::new().with_config(cfg);

        // First cycle decays all 5 to ~0.5 and the cursor wraps to None.
        let p1 = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(p1, 5);

        // Second cycle re-scans from the start; deltas are below threshold
        // (salience already ~0.5, new compute ~0.5) → 0 writes, idempotent.
        let p2 = run_one_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(p2, 0, "re-decay of already-decayed memories must be no-op");
    });
}

#[test]
fn cycle_processed_count_feeds_scheduler_metrics() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        for slot in 1..=3 {
            seed_memory(
                &fix.metadata,
                slot,
                1.0,
                MemoryKind::Semantic,
                now - 365 * NANOS_PER_DAY,
            );
        }
        let mut sched = WorkerScheduler::new();
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_millis(20),
            batch_size: 100,
            max_runtime: Duration::from_secs(60),
        };
        sched
            .register(Arc::new(DecayWorker::new().with_config(cfg)), fix.ctx)
            .unwrap();
        let metrics = sched.metrics(WorkerKind::Decay.name()).unwrap();
        // Wait for the worker's first cycle to run.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if metrics.cycles_total.load(Ordering::Relaxed) >= 1 {
                break;
            }
            glommio::timer::sleep(Duration::from_millis(5)).await;
        }
        sched.shutdown().await.unwrap();
        let processed = metrics.processed_total.load(Ordering::Relaxed);
        assert!(processed >= 3, "expected >=3 processed, got {processed}");
    });
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("worker-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}
