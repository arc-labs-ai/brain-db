#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Edge scrub worker tests.
//!
//! Guards removal of orphaned edges whose endpoints no longer live:
//! an edge to or from a dead memory is dropped from both the forward
//! and reverse tables, while live-to-live edges are kept. Pins per-cycle
//! batch caps, cursor advance, and the metric the scheduler reads.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{
    derived_by, link, origin, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{
    EdgeScrubWorker, Worker, WorkerConfig, WorkerContext, WorkerKind, WorkerScheduler,
};
use redb::ReadableTable;
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

fn seed_memory(metadata: &SharedMetadataDb, slot: u64) -> MemoryId {
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
            now_unix_nanos(),
        );
        table.insert(id.to_be_bytes(), meta).unwrap();
    }
    wtxn.commit().unwrap();
    id
}

/// Insert an edge directly into both tables — bypasses the writer's
/// alive-endpoint validation so we can craft orphans.
fn seed_edge_raw(metadata: &SharedMetadataDb, src: MemoryId, kind: EdgeKind, tgt: MemoryId) {
    let wtxn = metadata.write_txn().unwrap();
    {
        let mut out = wtxn.open_table(EDGES_TABLE).unwrap();
        let mut rev = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        let data = EdgeData::new(1.0, origin::EXPLICIT, derived_by::CLIENT, now_unix_nanos());
        link(
            &mut out,
            &mut rev,
            brain_core::NodeRef::Memory(src),
            brain_core::EdgeKindRef::Builtin(kind),
            brain_core::NodeRef::Memory(tgt),
            zero_disambiguator(),
            &data,
        )
        .unwrap();
    }
    wtxn.commit().unwrap();
}

fn count_edges_out(metadata: &SharedMetadataDb) -> usize {
    let rtxn = metadata.read_txn().unwrap();
    let t = rtxn.open_table(EDGES_TABLE).unwrap();
    t.iter().unwrap().count()
}

fn count_edges_in(metadata: &SharedMetadataDb) -> usize {
    let rtxn = metadata.read_txn().unwrap();
    let t = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
    t.iter().unwrap().count()
}

async fn run_one(
    worker: &EdgeScrubWorker,
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
// Cycle behaviour (8).
// ===========================================================================

#[test]
fn live_to_live_edge_is_kept() {
    glommio_run(|| async {
        let fix = build_fixture();
        let a = seed_memory(&fix.metadata, 1);
        let b = seed_memory(&fix.metadata, 2);
        seed_edge_raw(&fix.metadata, a, EdgeKind::FollowedBy, b);

        let worker = EdgeScrubWorker::new();
        let removed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(removed, 0);
        assert_eq!(count_edges_out(&fix.metadata), 1);
        assert_eq!(count_edges_in(&fix.metadata), 1);
    });
}

#[test]
fn edge_to_dead_target_removed_from_out() {
    glommio_run(|| async {
        let fix = build_fixture();
        let alive = seed_memory(&fix.metadata, 1);
        let dead = make_id(99); // never seeded
        seed_edge_raw(&fix.metadata, alive, EdgeKind::FollowedBy, dead);
        assert_eq!(count_edges_out(&fix.metadata), 1);

        let worker = EdgeScrubWorker::new();
        let removed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(count_edges_out(&fix.metadata), 0);
    });
}

#[test]
fn edge_to_dead_target_mirror_in_also_removed() {
    glommio_run(|| async {
        let fix = build_fixture();
        let alive = seed_memory(&fix.metadata, 1);
        let dead = make_id(99);
        seed_edge_raw(&fix.metadata, alive, EdgeKind::Caused, dead);
        assert_eq!(count_edges_in(&fix.metadata), 1);

        let worker = EdgeScrubWorker::new();
        run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(count_edges_in(&fix.metadata), 0);
    });
}

#[test]
fn edge_from_dead_source_removed_from_in() {
    glommio_run(|| async {
        let fix = build_fixture();
        let alive_tgt = seed_memory(&fix.metadata, 1);
        let dead_src = make_id(99);
        // Seed BOTH tables directly (simulating slot reclamation that
        // removed EDGES_OUT[dead_src,*,*] + EDGES_IN[dead_src,*,*]
        // but left mirror EDGES_IN[alive_tgt, K, dead_src] dangling
        // — wait, the mirror is in EDGES_OUT[dead_src, K, alive_tgt]
        // which slot reclamation removes. The surviving dangling row
        // is EDGES_IN[alive_tgt, K, dead_src] — exactly what we want).
        seed_edge_raw(&fix.metadata, dead_src, EdgeKind::FollowedBy, alive_tgt);
        // Simulate post-reclamation: remove dead_src's MEMORIES row plus
        // the EDGES_OUT[dead_src,*,*] entry.
        {
            let wtxn = fix.metadata.write_txn().unwrap();
            {
                let mut out = wtxn.open_table(EDGES_TABLE).unwrap();
                let key = brain_metadata::tables::edge::EdgeKey {
                    from: brain_core::NodeRef::Memory(dead_src),
                    kind: brain_core::EdgeKindRef::Builtin(EdgeKind::FollowedBy),
                    to: brain_core::NodeRef::Memory(alive_tgt),
                    disambiguator: zero_disambiguator(),
                }
                .encode();
                out.remove(key.as_slice()).unwrap();
            }
            wtxn.commit().unwrap();
        }
        assert_eq!(count_edges_out(&fix.metadata), 0);
        assert_eq!(count_edges_in(&fix.metadata), 1);

        let worker = EdgeScrubWorker::new();
        let removed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(
            removed, 1,
            "dangling EDGES_IN[alive, K, dead] must be scrubbed"
        );
        assert_eq!(count_edges_in(&fix.metadata), 0);
    });
}

#[test]
fn both_endpoints_dead_edge_removed() {
    glommio_run(|| async {
        let fix = build_fixture();
        let dead1 = make_id(99);
        let dead2 = make_id(100);
        seed_edge_raw(&fix.metadata, dead1, EdgeKind::FollowedBy, dead2);

        let worker = EdgeScrubWorker::new();
        let removed = run_one(&worker, fix.ctx).await.unwrap();
        assert!(
            removed >= 1,
            "both-dead edge must be scrubbed, got {removed}"
        );
        assert_eq!(count_edges_out(&fix.metadata), 0);
        assert_eq!(count_edges_in(&fix.metadata), 0);
    });
}

#[test]
fn batch_size_caps_per_cycle() {
    glommio_run(|| async {
        let fix = build_fixture();
        let alive = seed_memory(&fix.metadata, 1);
        // 30 distinct dead targets.
        for slot in 100..130u64 {
            seed_edge_raw(&fix.metadata, alive, EdgeKind::FollowedBy, make_id(slot));
        }
        assert_eq!(count_edges_out(&fix.metadata), 30);

        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(60),
            batch_size: 10,
            max_runtime: Duration::from_secs(60),
        };
        let worker = EdgeScrubWorker::new().with_config(cfg);
        let removed = run_one(&worker, fix.ctx).await.unwrap();
        // EDGES_OUT phase removes 10 (capped by batch_size); the EDGES_IN
        // phase also runs and may catch additional mirrors. Bound the
        // total at batch_size×2.
        assert!(
            (10..=20).contains(&removed),
            "expected 10..=20 removals, got {removed}"
        );
        let remaining = count_edges_out(&fix.metadata);
        assert!(
            remaining < 30,
            "some edges must remain after capped cycle (got {remaining})"
        );
    });
}

#[test]
fn cursor_advances_across_cycles() {
    glommio_run(|| async {
        let fix = build_fixture();
        let alive = seed_memory(&fix.metadata, 1);
        for slot in 100..130u64 {
            seed_edge_raw(&fix.metadata, alive, EdgeKind::FollowedBy, make_id(slot));
        }
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_secs(60),
            batch_size: 10,
            max_runtime: Duration::from_secs(60),
        };
        let worker = EdgeScrubWorker::new().with_config(cfg);

        let mut total = 0;
        for _ in 0..6 {
            total += run_one(&worker, fix.ctx.clone()).await.unwrap();
            if count_edges_out(&fix.metadata) == 0 {
                break;
            }
        }
        let _ = total;
        assert_eq!(count_edges_out(&fix.metadata), 0, "all orphans removed");
        assert_eq!(count_edges_in(&fix.metadata), 0);
    });
}

#[test]
fn mixed_live_and_orphan_only_orphans_removed() {
    glommio_run(|| async {
        let fix = build_fixture();
        let a = seed_memory(&fix.metadata, 1);
        let b = seed_memory(&fix.metadata, 2);
        let dead = make_id(99);
        seed_edge_raw(&fix.metadata, a, EdgeKind::FollowedBy, b);
        seed_edge_raw(&fix.metadata, a, EdgeKind::FollowedBy, dead);
        seed_edge_raw(&fix.metadata, b, EdgeKind::Caused, a);

        let worker = EdgeScrubWorker::new();
        run_one(&worker, fix.ctx).await.unwrap();
        // Live↔live edges keep their pair (in + out).
        assert_eq!(count_edges_out(&fix.metadata), 2);
        assert_eq!(count_edges_in(&fix.metadata), 2);
    });
}

// ===========================================================================
// Worker integration (3).
// ===========================================================================

#[test]
fn cycle_processed_count_feeds_metrics() {
    glommio_run(|| async {
        let fix = build_fixture();
        let alive = seed_memory(&fix.metadata, 1);
        for slot in 100..103u64 {
            seed_edge_raw(&fix.metadata, alive, EdgeKind::FollowedBy, make_id(slot));
        }
        let cfg = WorkerConfig {
            enabled: true,
            interval: Duration::from_millis(20),
            batch_size: 100,
            max_runtime: Duration::from_secs(1),
        };
        let mut sched = WorkerScheduler::new();
        sched
            .register(Arc::new(EdgeScrubWorker::new().with_config(cfg)), fix.ctx)
            .unwrap();
        let metrics = sched.metrics(WorkerKind::EdgeScrub.name()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if metrics.processed_total.load(Ordering::Relaxed) >= 3 {
                break;
            }
            glommio::timer::sleep(Duration::from_millis(5)).await;
        }
        sched.shutdown().await.unwrap();
        assert!(
            metrics.processed_total.load(Ordering::Relaxed) >= 3,
            "expected ≥3 processed, got {}",
            metrics.processed_total.load(Ordering::Relaxed)
        );
    });
}

// ===========================================================================
// Edge cases (1).
// ===========================================================================

#[test]
fn empty_edge_tables_cycle_is_noop() {
    glommio_run(|| async {
        let fix = build_fixture();
        let worker = EdgeScrubWorker::new();
        let removed = run_one(&worker, fix.ctx).await.unwrap();
        assert_eq!(removed, 0);
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
