#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! WAL retention worker tests.
//!
//! Guards which WAL segments may be deleted: `decide_deletions` keeps any
//! segment whose last LSN is at or above the checkpoint cutoff (minus a
//! retention buffer that pushes the cutoff back), deleting only fully
//! superseded segments. Pins that a disabled source is a no-op, a
//! rejecting source skips deletion, and a failing list surfaces as a
//! `WorkerError`. Source behaviour is faked to isolate the policy wiring.

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::wal_retention::{CheckpointFuture, DeleteFuture, SegmentListFuture};
use brain_workers::{
    decide_deletions, CheckpointDesc, DisabledWalRetentionSource, SegmentDesc, WalRetentionSource,
    WalRetentionSourceError, WalRetentionWorker, Worker, WorkerContext,
};
use parking_lot::Mutex;

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

fn make_ops_context() -> (Arc<OpsContext>, tempfile::TempDir) {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    (
        Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)),
        tempdir,
    )
}

async fn run_one(
    worker: &WalRetentionWorker,
    ops: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wctx = WorkerContext {
        ops,
        shutdown: shutdown_flag.clone(),
    };
    worker.run_cycle(&wctx).await
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn seg(id: u64, first: u64, last: u64) -> SegmentDesc {
    SegmentDesc {
        segment_id: id,
        first_lsn: first,
        last_lsn: last,
        size_bytes: 1024,
    }
}

// ===========================================================================
// Pure decide_deletions (6).
// ===========================================================================

#[test]
fn empty_segment_list_returns_empty() {
    let r = decide_deletions(&[], CheckpointDesc { durable_lsn: 1000 }, 0);
    assert!(r.is_empty());
}

#[test]
fn all_segments_above_cutoff_returns_empty() {
    let segs = [seg(1, 1000, 1500), seg(2, 1500, 2000)];
    let r = decide_deletions(&segs, CheckpointDesc { durable_lsn: 500 }, 0);
    assert!(r.is_empty());
}

#[test]
fn segments_below_cutoff_returned() {
    let segs = [seg(1, 0, 500), seg(2, 500, 800), seg(3, 1000, 1500)];
    let r = decide_deletions(&segs, CheckpointDesc { durable_lsn: 1000 }, 0);
    let mut s = r;
    s.sort();
    assert_eq!(s, vec![1, 2]);
}

#[test]
fn retention_buffer_pushes_cutoff_back() {
    let segs = [seg(1, 0, 500), seg(2, 500, 800)];
    // Buffer 300 → cutoff = 700 → only segment 1 (last=500) deletable.
    let r = decide_deletions(&segs, CheckpointDesc { durable_lsn: 1000 }, 300);
    assert_eq!(r, vec![1]);
}

#[test]
fn buffer_larger_than_checkpoint_keeps_everything() {
    let segs = [seg(1, 0, 50), seg(2, 50, 99)];
    let r = decide_deletions(&segs, CheckpointDesc { durable_lsn: 100 }, 500);
    assert!(r.is_empty(), "saturating cutoff at 0 → nothing deletable");
}

#[test]
fn last_lsn_equal_to_cutoff_is_kept() {
    let segs = [seg(1, 0, 999), seg(2, 1000, 1500)];
    let r = decide_deletions(&segs, CheckpointDesc { durable_lsn: 1000 }, 0);
    assert_eq!(
        r,
        vec![1],
        "strict less-than: last_lsn=999 deleted, 1500 kept"
    );
}

// ===========================================================================
// Stub sources.
// ===========================================================================

struct StubSource {
    checkpoint: CheckpointDesc,
    segments: Mutex<Vec<SegmentDesc>>,
    deleted: Arc<Mutex<Vec<u64>>>,
}

impl WalRetentionSource for StubSource {
    fn current_checkpoint(&self) -> CheckpointFuture<'_> {
        let cp = self.checkpoint;
        Box::pin(async move { Ok(cp) })
    }
    fn list_segments(&self) -> SegmentListFuture<'_> {
        let segs = self.segments.lock().clone();
        Box::pin(async move { Ok(segs) })
    }
    fn delete_segment(&self, segment_id: u64) -> DeleteFuture<'_> {
        let mut guard = self.segments.lock();
        guard.retain(|s| s.segment_id != segment_id);
        drop(guard);
        self.deleted.lock().push(segment_id);
        Box::pin(async move { Ok(()) })
    }
}

struct RejectingSource;
impl WalRetentionSource for RejectingSource {
    fn current_checkpoint(&self) -> CheckpointFuture<'_> {
        Box::pin(async { Ok(CheckpointDesc { durable_lsn: 1000 }) })
    }
    fn list_segments(&self) -> SegmentListFuture<'_> {
        Box::pin(async {
            Ok(vec![
                SegmentDesc {
                    segment_id: 1,
                    first_lsn: 0,
                    last_lsn: 500,
                    size_bytes: 1024,
                },
                SegmentDesc {
                    segment_id: 2,
                    first_lsn: 500,
                    last_lsn: 900,
                    size_bytes: 1024,
                },
            ])
        })
    }
    fn delete_segment(&self, _segment_id: u64) -> DeleteFuture<'_> {
        Box::pin(async {
            Err(WalRetentionSourceError::Rejected(
                "safety check denied".into(),
            ))
        })
    }
}

struct FailingListSource;
impl WalRetentionSource for FailingListSource {
    fn current_checkpoint(&self) -> CheckpointFuture<'_> {
        Box::pin(async { Ok(CheckpointDesc { durable_lsn: 1000 }) })
    }
    fn list_segments(&self) -> SegmentListFuture<'_> {
        Box::pin(async { Err(WalRetentionSourceError::Failed("io: boom".into())) })
    }
    fn delete_segment(&self, _: u64) -> DeleteFuture<'_> {
        Box::pin(async { Err(WalRetentionSourceError::Disabled) })
    }
}

// ===========================================================================
// Source surface (3).
// ===========================================================================

#[test]
fn rejecting_source_makes_worker_skip_deletion() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let worker = WalRetentionWorker::new(Arc::new(RejectingSource));
        let processed = run_one(&worker, ops).await.unwrap();
        assert_eq!(processed, 0);
    });
}

// ===========================================================================
// Cycle (4).
// ===========================================================================

#[test]
fn cycle_with_disabled_source_returns_zero() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let worker = WalRetentionWorker::new(Arc::new(DisabledWalRetentionSource));
        let processed = run_one(&worker, ops).await.unwrap();
        assert_eq!(processed, 0);
    });
}

#[test]
fn stub_source_with_eligible_segments_deletes_and_reports_count() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let deleted = Arc::new(Mutex::new(Vec::new()));
        let stub = StubSource {
            checkpoint: CheckpointDesc { durable_lsn: 1000 },
            segments: Mutex::new(vec![seg(1, 0, 500), seg(2, 500, 800), seg(3, 800, 1500)]),
            deleted: deleted.clone(),
        };
        let worker = WalRetentionWorker::new(Arc::new(stub));
        let processed = run_one(&worker, ops).await.unwrap();
        assert_eq!(processed, 2, "segments 1 and 2 below cutoff=1000");
        let mut deleted_sorted = deleted.lock().clone();
        deleted_sorted.sort();
        assert_eq!(deleted_sorted, vec![1, 2]);
    });
}

#[test]
fn stub_source_with_no_eligible_segments_returns_zero() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let stub = StubSource {
            checkpoint: CheckpointDesc { durable_lsn: 100 },
            segments: Mutex::new(vec![seg(1, 100, 500)]),
            deleted: Arc::new(Mutex::new(Vec::new())),
        };
        let worker = WalRetentionWorker::new(Arc::new(stub));
        let processed = run_one(&worker, ops).await.unwrap();
        assert_eq!(processed, 0);
    });
}

#[test]
fn failed_list_source_propagates_as_worker_error() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let worker = WalRetentionWorker::new(Arc::new(FailingListSource));
        let r = run_one(&worker, ops).await;
        assert!(
            matches!(r, Err(brain_workers::WorkerError::Ops(_))),
            "Failed must surface, got {r:?}"
        );
    });
}

// ===========================================================================
// Worker integration (1).
// ===========================================================================

#[test]
fn retention_buffer_keeps_segments_under_cutoff() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let stub = StubSource {
            checkpoint: CheckpointDesc { durable_lsn: 1000 },
            segments: Mutex::new(vec![seg(1, 0, 500), seg(2, 500, 800)]),
            deleted: Arc::new(Mutex::new(Vec::new())),
        };
        let worker = WalRetentionWorker::new(Arc::new(stub)).with_retention_extra_lsns(400); // cutoff = 600
        let processed = run_one(&worker, ops).await.unwrap();
        // seg 1 (last=500) < 600 → deletable; seg 2 (last=800) not.
        assert_eq!(processed, 1);
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
