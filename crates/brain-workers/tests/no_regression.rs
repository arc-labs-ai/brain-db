//! Phase 8 no-regression smoke gate (sub-task 8.14).
//!
//! Goal: catch a worker implementation that catastrophically starves
//! the foreground request path. Not the spec §16/02 acceptance
//! bench — that runs against 16-core x86_64 hardware on 1M memories
//! for 10 minutes, and lives in Phase 9. Here we just compare a
//! workers-off baseline to a workers-on run and assert the
//! workers-on path is within a generous 5× multiplier.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::{dispatch, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::request::{EncodeRequest, MemoryKindWire, RecallRequest, RequestBody};
use brain_protocol::response::ResponseBody;
use brain_workers::{
    AccessBoostWorker, CacheEvictionWorker, ConsolidationWorker, CounterReconcileWorker,
    DecayWorker, DisabledCacheEvictionSource, DisabledRebuildSource, DisabledSnapshotSource,
    DisabledSummarizer, DisabledWalRetentionSource, EdgeScrubWorker, HnswMaintenanceWorker,
    IdempotencyCleanupWorker, SlotReclamationWorker, SnapshotWorker, StatisticsUpdateWorker,
    WalRetentionWorker, WorkerConfig, WorkerScheduler,
};
use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Mock dispatcher.
// ---------------------------------------------------------------------------

struct MockDispatcher;
impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0f32; VECTOR_DIM];
        for (i, b) in text.as_bytes().iter().enumerate() {
            v[i % VECTOR_DIM] += f32::from(*b) / 255.0;
        }
        Ok(v)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0xCD; 16]
    }
}

struct Fixture {
    ctx: Arc<OpsContext>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx: Arc::new(OpsContext::new(executor)),
        _tempdir: tempdir,
    }
}

// ---------------------------------------------------------------------------
// Workload helpers.
// ---------------------------------------------------------------------------

async fn encode_one(ctx: &OpsContext, rid: u32, text: &str) {
    let mut request_id = [0u8; 16];
    request_id[..4].copy_from_slice(&rid.to_be_bytes());
    let req = EncodeRequest {
        text: text.into(),
        context_id: 1,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: vec![],
        request_id,
        txn_id: None,
        deduplicate: false,
    };
    let _ = dispatch(RequestBody::Encode(req), ctx).await.unwrap();
}

async fn recall_one(ctx: &OpsContext, cue: &str) -> usize {
    let req = RecallRequest {
        cue_text: cue.into(),
        cue_vector_offset: 0,
        cue_vector_dim: 0,
        top_k: 5,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        strategy_hint: None,
        include_vectors: false,
        include_edges: false,
        request_id: None,
        txn_id: None,
    };
    match dispatch(RequestBody::Recall(req), ctx).await.unwrap() {
        ResponseBody::Recall(r) => r.results.len(),
        _ => 0,
    }
}

/// Run `n` (encode + recall) pairs and return per-op latencies.
async fn measure_latencies(fix: &Fixture, n: usize, rid_base: u32) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(n * 2);
    for i in 0..n {
        let rid = rid_base + i as u32;
        let text = format!("workload-{rid}");
        let start = Instant::now();
        encode_one(&fix.ctx, rid, &text).await;
        samples.push(start.elapsed());
        let start = Instant::now();
        let _ = recall_one(&fix.ctx, &text).await;
        samples.push(start.elapsed());
    }
    samples
}

fn median(samples: &[Duration]) -> Duration {
    let mut s = samples.to_vec();
    s.sort();
    s[s.len() / 2]
}

fn fast_worker_cfg() -> WorkerConfig {
    WorkerConfig {
        enabled: true,
        interval: Duration::from_millis(20),
        batch_size: 100,
        max_runtime: Duration::from_millis(50),
    }
}

/// Register every Phase-8 worker on `sched`. Pluggable workers get
/// `Disabled*Source` so they tick (stressing the scheduler) but do
/// no real work.
fn register_all_workers(sched: &mut WorkerScheduler, ctx: Arc<OpsContext>) {
    sched
        .register(
            Arc::new(DecayWorker::new().with_config(fast_worker_cfg())),
            ctx.clone(),
        )
        .unwrap();
    sched
        .register(
            Arc::new(AccessBoostWorker::new().with_config(fast_worker_cfg())),
            ctx.clone(),
        )
        .unwrap();
    sched
        .register(
            Arc::new(
                ConsolidationWorker::new(Arc::new(DisabledSummarizer))
                    .with_config(fast_worker_cfg()),
            ),
            ctx.clone(),
        )
        .unwrap();
    sched
        .register(
            Arc::new(
                HnswMaintenanceWorker::new(Arc::new(DisabledRebuildSource))
                    .with_config(fast_worker_cfg()),
            ),
            ctx.clone(),
        )
        .unwrap();
    sched
        .register(
            Arc::new(IdempotencyCleanupWorker::new().with_config(fast_worker_cfg())),
            ctx.clone(),
        )
        .unwrap();
    sched
        .register(
            Arc::new(SlotReclamationWorker::new().with_config(fast_worker_cfg())),
            ctx.clone(),
        )
        .unwrap();
    sched
        .register(
            Arc::new(
                WalRetentionWorker::new(Arc::new(DisabledWalRetentionSource))
                    .with_config(fast_worker_cfg()),
            ),
            ctx.clone(),
        )
        .unwrap();
    sched
        .register(
            Arc::new(EdgeScrubWorker::new().with_config(fast_worker_cfg())),
            ctx.clone(),
        )
        .unwrap();
    sched
        .register(
            Arc::new(CounterReconcileWorker::new().with_config(fast_worker_cfg())),
            ctx.clone(),
        )
        .unwrap();
    sched
        .register(
            Arc::new(StatisticsUpdateWorker::new().with_config(fast_worker_cfg())),
            ctx.clone(),
        )
        .unwrap();
    sched
        .register(
            Arc::new(
                CacheEvictionWorker::new(Arc::new(DisabledCacheEvictionSource))
                    .with_config(fast_worker_cfg()),
            ),
            ctx.clone(),
        )
        .unwrap();
    // Snapshot defaults disabled (spec §6.2); flip it on for this
    // stress test.
    sched
        .register(
            Arc::new(
                SnapshotWorker::new(Arc::new(DisabledSnapshotSource))
                    .with_config(fast_worker_cfg()),
            ),
            ctx,
        )
        .unwrap();
}

// ===========================================================================
// Main regression gate (1).
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workers_active_do_not_catastrophically_degrade_foreground_latency() {
    let fix = build_fixture();

    // Seed 30 memories so RECALL has something to search.
    for slot in 1..=30u32 {
        encode_one(&fix.ctx, slot, &format!("seed-{slot}")).await;
    }

    // Baseline: 100 (encode + recall) pairs without any worker.
    let baseline = measure_latencies(&fix, 100, 10_000).await;
    let baseline_median = median(&baseline);

    // Register every worker. Each ticks every 20ms with batch_size=100
    // so they overlap the measurement window.
    let mut sched = WorkerScheduler::new();
    register_all_workers(&mut sched, fix.ctx.clone());
    // Give the scheduler a moment to kick off at least one cycle of each.
    tokio::time::sleep(Duration::from_millis(40)).await;

    // Workers-on: 100 more pairs.
    let with_workers = measure_latencies(&fix, 100, 20_000).await;
    let with_workers_median = median(&with_workers);

    // Sanity: at least one worker actually ran during the window.
    let names = sched.names();
    let mut any_cycle = false;
    for name in &names {
        if let Some(m) = sched.metrics(name) {
            if m.cycles_total.load(Ordering::Relaxed) >= 1 {
                any_cycle = true;
                break;
            }
        }
    }

    sched.shutdown().await.unwrap();

    assert!(
        any_cycle,
        "expected at least one worker cycle to complete during the measurement window"
    );

    // Generous bound: workers_median ≤ 5× baseline_median, with a
    // floor at 5ms so very-fast baselines (microsecond-scale) don't
    // produce nonsense thresholds.
    let max_allowed = (baseline_median * 5).max(Duration::from_millis(5));
    assert!(
        with_workers_median <= max_allowed,
        "regression: baseline median {:?}, with workers {:?} (allowed up to {:?})",
        baseline_median,
        with_workers_median,
        max_allowed,
    );
}

// ===========================================================================
// Sanity (2).
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn measure_latencies_returns_sample_count() {
    let fix = build_fixture();
    let samples = measure_latencies(&fix, 5, 1).await;
    assert_eq!(samples.len(), 10, "5 (encode+recall) pairs → 10 samples");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn baseline_runs_in_reasonable_time() {
    let fix = build_fixture();
    let start = Instant::now();
    let _ = measure_latencies(&fix, 100, 1).await;
    let elapsed = start.elapsed();
    // 100 encode+recall pairs. 30s is wildly generous; we just want
    // to catch infinite-loop regressions.
    assert!(
        elapsed < Duration::from_secs(30),
        "baseline 100-op workload took {elapsed:?}, exceeded 30s sanity bound"
    );
}
