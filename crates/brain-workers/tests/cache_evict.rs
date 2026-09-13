#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Embedder cache eviction worker tests.
//!
//! Guards the worker's contract with its pluggable eviction source: an
//! enabled source is called once per cycle with the configured max-age
//! and its prune count is reported; a disabled source is a no-op; a
//! failing source surfaces as a `WorkerError`. Source behaviour is
//! faked so the test pins the worker's wiring, not the prune logic.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::cache_evict::PruneFuture;
use brain_workers::{
    CacheEvictionError, CacheEvictionSource, CacheEvictionWorker, DisabledCacheEvictionSource,
    Worker, WorkerContext, DEFAULT_CACHE_MAX_AGE,
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
    worker: &CacheEvictionWorker,
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
// Stub sources.
// ---------------------------------------------------------------------------

struct StubSource {
    returns: usize,
    last_max_age: Arc<Mutex<Option<Duration>>>,
    calls: AtomicU64,
}

impl CacheEvictionSource for StubSource {
    fn prune_older_than(&self, max_age: Duration) -> PruneFuture<'_> {
        *self.last_max_age.lock() = Some(max_age);
        self.calls.fetch_add(1, Ordering::Relaxed);
        let n = self.returns;
        Box::pin(async move { Ok(n) })
    }
}

struct FailingSource;
impl CacheEvictionSource for FailingSource {
    fn prune_older_than(&self, _max_age: Duration) -> PruneFuture<'_> {
        Box::pin(async { Err(CacheEvictionError::Failed("io error".into())) })
    }
}

// ===========================================================================
// Source surface (3).
// ===========================================================================

#[test]
fn failed_source_propagates_as_worker_error() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let worker = CacheEvictionWorker::new(Arc::new(FailingSource));
        let r = run_one(&worker, ops).await;
        assert!(
            matches!(r, Err(brain_workers::WorkerError::Ops(_))),
            "Failed must surface, got {r:?}"
        );
    });
}

// ===========================================================================
// Cycle (3).
// ===========================================================================

#[test]
fn cycle_with_disabled_source_returns_zero() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let worker = CacheEvictionWorker::new(Arc::new(DisabledCacheEvictionSource));
        let processed = run_one(&worker, ops).await.unwrap();
        assert_eq!(processed, 0);
    });
}

#[test]
fn cycle_returns_source_count() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let stub = StubSource {
            returns: 12,
            last_max_age: Arc::new(Mutex::new(None)),
            calls: AtomicU64::new(0),
        };
        let worker = CacheEvictionWorker::new(Arc::new(stub));
        let processed = run_one(&worker, ops).await.unwrap();
        assert_eq!(processed, 12);
    });
}

#[test]
fn cycle_calls_source_with_default_max_age() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let last = Arc::new(Mutex::new(None));
        let stub = StubSource {
            returns: 0,
            last_max_age: last.clone(),
            calls: AtomicU64::new(0),
        };
        let worker = CacheEvictionWorker::new(Arc::new(stub));
        run_one(&worker, ops).await.unwrap();
        assert_eq!(*last.lock(), Some(DEFAULT_CACHE_MAX_AGE));
    });
}

// ===========================================================================
// Worker integration (1).
// ===========================================================================

#[test]
fn custom_max_age_honoured() {
    glommio_run(|| async {
        let (ops, _td) = make_ops_context();
        let last = Arc::new(Mutex::new(None));
        let stub = StubSource {
            returns: 0,
            last_max_age: last.clone(),
            calls: AtomicU64::new(0),
        };
        let worker =
            CacheEvictionWorker::new(Arc::new(stub)).with_max_age(Duration::from_secs(3600));
        run_one(&worker, ops).await.unwrap();
        assert_eq!(*last.lock(), Some(Duration::from_secs(3600)));
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
