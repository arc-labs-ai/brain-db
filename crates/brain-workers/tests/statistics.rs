#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Statistics update worker tests.
//!
//! Guards the per-shard stats snapshot the worker recomputes each cycle:
//! tombstone count reflects live HNSW state, age fields track the
//! min/max `created_at` across memories, and `computed_at` advances. Pins
//! that the cached handle observes the same data as the fresh snapshot
//! and that an empty shard yields zero counts rather than erroring.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{StatisticsUpdateWorker, Worker, WorkerContext};
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

struct Fixture {
    ctx: Arc<OpsContext>,
    metadata: SharedMetadataDb,
    index: SharedHnsw,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let index = shared.clone();
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
        index,
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

fn seed_memory(metadata: &SharedMetadataDb, slot: u64, created_at: u64) -> MemoryId {
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
            MemoryKind::Episodic,
            [0; 16],
            0.5,
            16,
            created_at,
        );
        table.insert(id.to_be_bytes(), meta).unwrap();
    }
    wtxn.commit().unwrap();
    id
}

async fn run_one(
    worker: &StatisticsUpdateWorker,
    ops: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wctx = WorkerContext {
        ops,
        shutdown: shutdown_flag.clone(),
    };
    worker.run_cycle(&wctx).await
}

// ===========================================================================
// Cycle (6).
// ===========================================================================

#[test]
fn empty_fixture_returns_zero_counts() {
    glommio_run(|| async {
        let fix = build_fixture();
        let worker = StatisticsUpdateWorker::new();
        let processed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 1);
        let s = worker.snapshot();
        assert_eq!(s.memory_count, 0);
        assert_eq!(s.tombstone_count, 0);
        assert_eq!(s.tombstone_ratio, 0.0);
        assert_eq!(s.oldest_memory_age_nanos, None);
        assert_eq!(s.newest_memory_age_nanos, None);
    });
}

#[test]
fn seeded_memories_reflected_in_count() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        for slot in 1..=5u64 {
            seed_memory(&fix.metadata, slot, now);
        }
        let worker = StatisticsUpdateWorker::new();
        run_one(&worker, fix.ctx).await.unwrap();
        let s = worker.snapshot();
        assert_eq!(s.memory_count, 5);
    });
}

#[test]
fn tombstone_count_reflects_hnsw_state() {
    glommio_run(|| async {
        let fix = build_fixture();
        // Insert 4 vectors via SharedHnsw (writer is owned by
        // RealWriterHandle — reach through with a fresh writer pair
        // and swap).
        let (replacement_reader, mut replacement_writer) =
            SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let _ = replacement_reader;
        for slot in 1..=4u64 {
            let mut v = [0.0f32; VECTOR_DIM];
            v[(slot as usize) % VECTOR_DIM] = 1.0;
            replacement_writer.insert(make_id(slot), &v).unwrap();
        }
        replacement_writer.mark_tombstoned(make_id(1)).unwrap();
        replacement_writer.mark_tombstoned(make_id(2)).unwrap();
        // Build a fresh HnswIndex with the same content and swap it
        // into the fixture's live index.
        let source: Vec<_> = (1..=4u64)
            .map(|slot| {
                let mut v = [0.0f32; VECTOR_DIM];
                v[(slot as usize) % VECTOR_DIM] = 1.0;
                (make_id(slot), v)
            })
            .collect();
        let (mut new_idx, _r) =
            brain_index::rebuild::rebuild_impl(fix.index.params(), source).unwrap();
        new_idx.mark_tombstoned(make_id(1)).unwrap();
        new_idx.mark_tombstoned(make_id(2)).unwrap();
        fix.index.swap(new_idx);

        let worker = StatisticsUpdateWorker::new();
        run_one(&worker, fix.ctx).await.unwrap();
        let s = worker.snapshot();
        assert_eq!(s.tombstone_count, 2);
        assert!(
            (s.tombstone_ratio - 0.5).abs() < 1e-5,
            "expected 0.5, got {}",
            s.tombstone_ratio
        );
    });
}

#[test]
fn age_fields_track_min_max_created_at() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        let old = now.saturating_sub(1_000_000_000_000); // ~1000s ago
        let young = now.saturating_sub(1_000_000_000); // ~1s ago
        seed_memory(&fix.metadata, 1, old);
        seed_memory(&fix.metadata, 2, young);

        let worker = StatisticsUpdateWorker::new();
        run_one(&worker, fix.ctx).await.unwrap();
        let s = worker.snapshot();
        let oldest = s.oldest_memory_age_nanos.unwrap();
        let newest = s.newest_memory_age_nanos.unwrap();
        assert!(
            oldest > newest,
            "oldest age {oldest} must exceed newest {newest}"
        );
        assert!(oldest >= 999_000_000_000);
        assert!(newest >= 999_000_000);
    });
}

#[test]
fn cache_updates_across_cycles() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        seed_memory(&fix.metadata, 1, now);
        let worker = StatisticsUpdateWorker::new();
        run_one(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(worker.snapshot().memory_count, 1);

        seed_memory(&fix.metadata, 2, now);
        seed_memory(&fix.metadata, 3, now);
        run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(worker.snapshot().memory_count, 3);
    });
}

// ===========================================================================
// Worker integration (1).
// ===========================================================================

#[test]
fn cache_handle_observes_same_data_as_snapshot() {
    glommio_run(|| async {
        let fix = build_fixture();
        seed_memory(&fix.metadata, 1, now_unix_nanos());
        seed_memory(&fix.metadata, 2, now_unix_nanos());
        let worker = StatisticsUpdateWorker::new();
        let handle = worker.cache_handle();
        run_one(&worker, fix.ctx).await.unwrap();
        let via_snapshot = worker.snapshot();
        let via_handle = handle.read().clone();
        assert_eq!(via_snapshot, via_handle);
        assert_eq!(via_handle.memory_count, 2);
    });
}

// ===========================================================================
// Edge case (1).
// ===========================================================================

#[test]
fn computed_at_unix_nanos_advances() {
    glommio_run(|| async {
        let fix = build_fixture();
        let worker = StatisticsUpdateWorker::new();
        run_one(&worker, fix.ctx.clone()).await.unwrap();
        let t1 = worker.snapshot().computed_at_unix_nanos;
        glommio::timer::sleep(Duration::from_millis(5)).await;
        run_one(&worker, fix.ctx).await.unwrap();
        let t2 = worker.snapshot().computed_at_unix_nanos;
        assert!(t2 >= t1, "computed_at must be monotonic, got {t1} → {t2}");
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
