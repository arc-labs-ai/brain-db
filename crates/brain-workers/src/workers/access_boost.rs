//! Access-boost worker.
//!
//! Drains the per-shard `AccessBuffer` (filled by RECALL responses)
//! and applies a `salience × (1 + boost_factor)` bump, capped at 1.0.
//! Default cadence 10 s; default boost 10 %. Memories already at the
//! cap are skipped. Missing rows (FORGET-then-RECALL race) are
//! silently skipped.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use brain_metadata::tables::memory::MEMORIES_TABLE;
use redb::ReadableTable;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// — default 10 % boost per access cycle.
pub const DEFAULT_BOOST_FACTOR: f32 = 0.10;

/// — salience caps at 1.0.
pub const MAX_SALIENCE: f32 = 1.0;

/// boost formula. Pure; unit-testable without a runtime.
#[must_use]
pub fn boosted_salience(current: f32, boost_factor: f32) -> f32 {
    let raw = current * (1.0 + boost_factor);
    raw.clamp(0.0, MAX_SALIENCE)
}

pub struct AccessBoostWorker {
    config: WorkerConfig,
    boost_factor: f32,
}

impl AccessBoostWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::AccessBoost),
            boost_factor: DEFAULT_BOOST_FACTOR,
        }
    }

    /// Override the default config. Tests use this to tighten the
    /// interval; operators can use it to enlarge batch_size for
    /// access-heavy workloads.
    #[must_use]
    pub fn with_config(mut self, config: WorkerConfig) -> Self {
        self.config = config;
        self
    }

    /// Override the default 10 % boost factor. `0.0` effectively
    /// disables the worker without flipping `enabled=false`.
    #[must_use]
    pub fn with_boost_factor(mut self, f: f32) -> Self {
        self.boost_factor = f;
        self
    }

    /// Current boost factor (for tests / introspection).
    #[must_use]
    pub fn boost_factor(&self) -> f32 {
        self.boost_factor
    }
}

impl Default for AccessBoostWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for AccessBoostWorker {
    fn name(&self) -> &'static str {
        WorkerKind::AccessBoost.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::AccessBoost
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_boost_cycle(self, ctx))
    }
}

async fn do_boost_cycle(
    worker: &AccessBoostWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    // Production runs with no artificial visit budget — the only stop
    // conditions are `max_runtime` and shutdown.
    run_boost_cycle(worker, ctx, usize::MAX).await
}

/// Core boost cycle. `visit_budget` caps how many drained ids the loop
/// may *visit* before it stops early (as if `max_runtime` or shutdown
/// fired). Production passes `usize::MAX`; tests pass a small value to
/// deterministically exercise the early-exit / re-queue boundary
/// without depending on wall-clock timing.
async fn run_boost_cycle(
    worker: &AccessBoostWorker,
    ctx: &WorkerContext,
    visit_budget: usize,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 || worker.boost_factor == 0.0 {
        return Ok(0);
    }

    // Drain the buffer up front; whatever exceeds batch_size we
    // re-queue at the end so a future cycle catches them.
    let ids = ctx.ops.access_buffer.drain();
    if ids.is_empty() {
        return Ok(0);
    }
    let take_n = ids.len().min(cfg.batch_size);

    let metadata = ctx.ops.executor.metadata.clone();
    let started = Instant::now();
    let mut applied = 0usize;
    // Index into `ids` marking where the unprocessed remainder begins.
    // On a normal run this ends at `take_n`; on an early exit it stops
    // at the id we broke on (which we did NOT process). Everything at
    // an index < `processed_upto` has already been visited this cycle —
    // either boosted, or intentionally skipped (missing / at-cap) —
    // and must never be re-queued as if it were pending. Re-queuing a
    // boosted id would double-boost it next cycle (salience *= 1.10
    // again), which is exactly the non-idempotency this guards against.
    let mut processed_upto = 0usize;

    {
        let wtxn = metadata
            .write_txn()
            .map_err(|e| WorkerError::Ops(format!("boost write_txn: {e:?}")))?;
        {
            let mut table = wtxn
                .open_table(MEMORIES_TABLE)
                .map_err(|e| WorkerError::Ops(format!("boost open MEMORIES: {e:?}")))?;

            for (i, id) in ids.iter().take(take_n).enumerate() {
                if i >= visit_budget || started.elapsed() >= cfg.max_runtime || ctx.is_shutdown() {
                    // Break BEFORE touching this id: it belongs to the
                    // unprocessed remainder and is re-queued below.
                    break;
                }
                // We are now committing to visiting `ids[i]`; advance the
                // watermark so the remainder re-queue starts strictly
                // after it, regardless of whether it boosts or skips.
                processed_upto = i + 1;
                let key = id.to_be_bytes();
                let prior = table
                    .get(key)
                    .map_err(|e| WorkerError::Ops(format!("boost get: {e:?}")))?
                    .map(|access| access.value());
                let Some(mut meta) = prior else {
                    continue; // tombstoned / deleted, skip silently
                };
                let new_salience = boosted_salience(meta.salience, worker.boost_factor);
                if (new_salience - meta.salience).abs() < f32::EPSILON {
                    continue; // already at cap or no change
                }
                meta.salience = new_salience;
                meta.access_count = meta.access_count.saturating_add(1);
                table
                    .insert(key, meta)
                    .map_err(|e| WorkerError::Ops(format!("boost insert: {e:?}")))?;
                applied += 1;
            }
        }
        wtxn.commit()
            .map_err(|e| WorkerError::Ops(format!("boost commit: {e:?}")))?;
    }

    // Re-queue only the unprocessed remainder: ids we never visited this
    // cycle (the current-and-later ids on early exit, plus everything
    // past `take_n` that we always defer). This range is disjoint from
    // every boosted id, so a row boosted once per drain can never be
    // boosted twice via re-queue — the cycle is idempotent.
    // `processed_upto <= take_n <= ids.len()`, so `ids[processed_upto..]`
    // is exactly the unvisited window rows on early exit plus the
    // always-deferred overflow past `take_n`.
    let requeue_from = processed_upto.min(ids.len());
    for id in &ids[requeue_from..] {
        ctx.ops.access_buffer.record(*id);
    }

    trace!(
        drained = ids.len(),
        applied,
        cycle_ms = started.elapsed().as_millis() as u64,
        "access-boost cycle"
    );

    Ok(applied)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
#[allow(clippy::arc_with_non_send_sync)]
mod tests {
    use super::*;
    use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
    use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
    use brain_metadata::tables::memory::MemoryMetadata;
    use brain_metadata::MetadataDb;
    use brain_ops::RealWriterHandle;
    use brain_planner::{ExecutorContext, WriterHandle};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    const NOW: u64 = 1_700_000_000_000_000_000;

    struct NoopDispatcher;
    impl Dispatcher for NoopDispatcher {
        fn embed(&self, _text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            Ok([0.0_f32; VECTOR_DIM])
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            Ok(vec![[0.0_f32; VECTOR_DIM]; texts.len()])
        }
        fn fingerprint(&self) -> [u8; 16] {
            [0u8; 16]
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        metadata: Arc<MetadataDb>,
        ctx: WorkerContext,
    }

    fn fixture() -> Fixture {
        use brain_index::{IndexParams, SharedHnsw};
        let dir = tempfile::tempdir().unwrap();
        let metadata = Arc::new(MetadataDb::open(dir.path().join("test.redb")).unwrap());
        let dispatcher: Arc<dyn Dispatcher> = Arc::new(NoopDispatcher);
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer: Arc<dyn WriterHandle> =
            Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let executor = ExecutorContext::new(dispatcher, shared, metadata.clone(), writer);
        let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
        let ctx = WorkerContext {
            ops,
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        Fixture {
            _dir: dir,
            metadata,
            ctx,
        }
    }

    /// Seed a memory row at the given salience and return its id.
    fn seed_memory(metadata: &Arc<MetadataDb>, n: u16, salience: f32) -> MemoryId {
        let mid = MemoryId::pack(n, SessionId::DEFAULT.into(), 0);
        let row = MemoryMetadata::new_active(
            mid,
            NamespaceId::SYSTEM,
            SpaceId::default(),
            SessionId::DEFAULT,
            mid.slot(),
            mid.version(),
            MemoryKind::Episodic,
            [0u8; 16],
            salience,
            16,
            NOW,
        );
        let wtxn = metadata.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(mid.to_be_bytes(), row).unwrap();
        }
        wtxn.commit().unwrap();
        mid
    }

    fn read_salience(metadata: &Arc<MetadataDb>, id: MemoryId) -> f32 {
        let rtxn = metadata.read_txn().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        t.get(id.to_be_bytes()).unwrap().unwrap().value().salience
    }

    fn read_access_count(metadata: &Arc<MetadataDb>, id: MemoryId) -> u32 {
        let rtxn = metadata.read_txn().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        t.get(id.to_be_bytes())
            .unwrap()
            .unwrap()
            .value()
            .access_count
    }

    fn worker() -> AccessBoostWorker {
        // Large batch_size + max_runtime so the only early-exit lever in
        // tests is the explicit visit budget.
        let mut cfg = WorkerConfig::defaults_for(WorkerKind::AccessBoost);
        cfg.batch_size = 4096;
        cfg.max_runtime = std::time::Duration::from_secs(3600);
        AccessBoostWorker::new().with_config(cfg)
    }

    #[test]
    fn boosted_salience_caps_at_one() {
        assert!((boosted_salience(0.5, 0.10) - 0.55).abs() < 1e-6);
        assert_eq!(boosted_salience(0.95, 0.10), MAX_SALIENCE);
        assert_eq!(boosted_salience(1.0, 0.10), MAX_SALIENCE);
    }

    /// An early-exit cycle must re-queue only the ids it never visited,
    /// and must never re-queue a boosted id. The proof: across the
    /// exit→resume boundary every boostable row ends up boosted exactly
    /// once (salience == 0.55, access_count == 1), never twice.
    #[test]
    fn early_exit_boosts_each_row_exactly_once() {
        let fx = fixture();
        let n_rows: u16 = 12;
        let ids: Vec<MemoryId> = (0..n_rows)
            .map(|i| seed_memory(&fx.metadata, i, 0.5))
            .collect();
        for id in &ids {
            fx.ctx.ops.access_buffer.record(*id);
        }

        // Cycle 1: visit only 5 of the 12 drained ids, then early-exit.
        // The 7 unvisited ids are re-queued; the (≤5) boosted ids are not.
        let w = worker();
        let applied1 = futures_lite::future::block_on(run_boost_cycle(&w, &fx.ctx, 5)).unwrap();
        assert_eq!(applied1, 5, "budget-limited cycle boosts exactly 5 rows");
        assert_eq!(
            fx.ctx.ops.access_buffer.len(),
            7,
            "only the 7 unvisited ids are re-queued",
        );

        // Cycle 2: drain the re-queued remainder and finish. If the fix
        // regressed and a boosted id were re-queued, that row would be
        // boosted twice here (0.55 → 0.605) and its access_count would
        // reach 2 — the assertions below catch exactly that.
        let applied2 =
            futures_lite::future::block_on(run_boost_cycle(&w, &fx.ctx, usize::MAX)).unwrap();
        assert_eq!(applied2, 7, "resume cycle boosts the 7 remaining rows");
        assert_eq!(fx.ctx.ops.access_buffer.len(), 0, "buffer drained");

        for id in &ids {
            let sal = read_salience(&fx.metadata, *id);
            assert!(
                (sal - 0.55).abs() < 1e-6,
                "row boosted exactly once expected 0.55, got {sal}",
            );
            assert_eq!(
                read_access_count(&fx.metadata, *id),
                1,
                "row boosted exactly once across the exit/resume boundary",
            );
        }
    }

    /// Skipped rows (at-cap) interleaved with boostable rows must not
    /// corrupt the re-queue watermark. This is the precise shape of the
    /// original bug: `applied` (a count of boosted rows) was used as an
    /// index, so an at-cap row before a boosted row shifted the split and
    /// re-queued an already-boosted id.
    #[test]
    fn early_exit_with_skipped_rows_never_double_boosts() {
        let fx = fixture();
        // Half at cap (salience 1.0 → skip), half boostable (0.5).
        let n_rows: u16 = 16;
        let mut boostable: Vec<MemoryId> = Vec::new();
        for i in 0..n_rows {
            let salience = if i % 2 == 0 { 1.0 } else { 0.5 };
            let id = seed_memory(&fx.metadata, i, salience);
            if salience < 1.0 {
                boostable.push(id);
            }
            fx.ctx.ops.access_buffer.record(id);
        }

        // Run in small budgeted slices until the buffer empties, so the
        // early-exit / re-queue path is exercised repeatedly regardless
        // of the arbitrary drain order.
        let w = worker();
        let mut guard = 0;
        while !fx.ctx.ops.access_buffer.is_empty() {
            let _ = futures_lite::future::block_on(run_boost_cycle(&w, &fx.ctx, 3)).unwrap();
            guard += 1;
            assert!(guard < 100, "cycles should terminate");
        }

        // Every boostable row boosted exactly once; at-cap rows untouched.
        for id in &boostable {
            let sal = read_salience(&fx.metadata, *id);
            assert!(
                (sal - 0.55).abs() < 1e-6,
                "boostable row must be boosted exactly once, got {sal}",
            );
            assert_eq!(read_access_count(&fx.metadata, *id), 1);
        }
    }

    /// A full cycle followed by a re-run over a freshly-recorded set must
    /// not double-boost: `drain` clears the buffer, so a row present in
    /// only the first drain is boosted once.
    #[test]
    fn overflow_requeue_boosts_each_row_once() {
        let fx = fixture();
        let ids: Vec<MemoryId> = (0..5).map(|i| seed_memory(&fx.metadata, i, 0.5)).collect();
        for id in &ids {
            fx.ctx.ops.access_buffer.record(*id);
        }

        // batch_size = 2 → overflow re-queue across cycles.
        let mut cfg = WorkerConfig::defaults_for(WorkerKind::AccessBoost);
        cfg.batch_size = 2;
        cfg.max_runtime = std::time::Duration::from_secs(3600);
        let w = AccessBoostWorker::new().with_config(cfg);

        let mut guard = 0;
        while !fx.ctx.ops.access_buffer.is_empty() {
            let _ =
                futures_lite::future::block_on(run_boost_cycle(&w, &fx.ctx, usize::MAX)).unwrap();
            guard += 1;
            assert!(guard < 100, "cycles should terminate");
        }
        for id in &ids {
            assert_eq!(read_access_count(&fx.metadata, *id), 1);
        }
    }
}
