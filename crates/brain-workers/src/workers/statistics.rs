//! Statistics update worker (sub-task 8.11). Spec §11/08 §3.
//!
//! Every 5 min, scans `MEMORIES_TABLE` + `SharedHnsw` and refreshes
//! a per-shard `Stats` cache. Phase 9's `ADMIN_STATS` handler will
//! hold a clone of [`StatisticsUpdateWorker::cache_handle`] and read
//! the cache without doing the scan itself.
//!
//! ## v1 deviations (documented)
//!
//! Spec §3.1 lists nine fields; v1 fills five and leaves the others
//! as `Option::None`:
//! - `arena_used_bytes` / `arena_capacity_bytes` — no arena yet.
//! - `wal_size_bytes` — no WAL hookup.
//! - `metadata_size_bytes` — `OpsContext` doesn't hold the metadata
//!   filesystem path.
//!
//! Spec §3.1's "histograms of salience / edge degree / age" is Phase
//! 9 admin tooling. v1 ships the counts + age range layer.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use brain_metadata::tables::memory::MEMORIES_TABLE;
use parking_lot::RwLock;
use redb::ReadableTable;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub memory_count: u64,
    pub tombstone_count: u64,
    /// `tombstone_count / total_entries` (HNSW len). `0.0` if empty.
    pub tombstone_ratio: f32,
    /// nanos since the oldest memory's `created_at`. `None` if empty.
    pub oldest_memory_age_nanos: Option<u64>,
    /// nanos since the newest memory's `created_at`. `None` if empty.
    pub newest_memory_age_nanos: Option<u64>,
    /// Phase 9 wires the arena.
    pub arena_used_bytes: Option<u64>,
    pub arena_capacity_bytes: Option<u64>,
    /// Phase 9 wires the WAL.
    pub wal_size_bytes: Option<u64>,
    /// Phase 9 wires the metadata file path.
    pub metadata_size_bytes: Option<u64>,
    /// Worker wall-clock when this snapshot was computed.
    pub computed_at_unix_nanos: u64,
}

pub struct StatisticsUpdateWorker {
    config: WorkerConfig,
    cache: Arc<RwLock<Stats>>,
}

impl StatisticsUpdateWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::Statistics),
            cache: Arc::new(RwLock::new(Stats::default())),
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    /// Cheap read-lock clone of the most recent snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Stats {
        self.cache.read().clone()
    }

    /// Hand the cache handle to consumers (e.g., Phase 9's
    /// `ADMIN_STATS` handler). Cloning bumps the `Arc` refcount.
    #[must_use]
    pub fn cache_handle(&self) -> Arc<RwLock<Stats>> {
        self.cache.clone()
    }
}

impl Default for StatisticsUpdateWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for StatisticsUpdateWorker {
    fn name(&self) -> &'static str {
        WorkerKind::Statistics.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Statistics
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_stats_cycle(self, ctx))
    }
}

async fn do_stats_cycle(
    worker: &StatisticsUpdateWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let started = Instant::now();
    let now_nanos = now_unix_nanos();
    let metadata = ctx.ops.executor.metadata.clone();
    let index = ctx.ops.executor.index.clone();

    let (memory_count, oldest_created, newest_created) = scan_memories(&metadata)?;
    let total_hnsw = index.len() as u64;
    let tombstone_count = index.tombstone_count() as u64;
    let tombstone_ratio = if total_hnsw == 0 {
        0.0
    } else {
        tombstone_count as f32 / total_hnsw as f32
    };

    let snapshot = Stats {
        memory_count,
        tombstone_count,
        tombstone_ratio,
        oldest_memory_age_nanos: oldest_created.map(|c| now_nanos.saturating_sub(c)),
        newest_memory_age_nanos: newest_created.map(|c| now_nanos.saturating_sub(c)),
        arena_used_bytes: None,
        arena_capacity_bytes: None,
        wal_size_bytes: None,
        metadata_size_bytes: None,
        computed_at_unix_nanos: now_nanos,
    };
    *worker.cache.write() = snapshot;

    trace!(
        memory_count,
        tombstone_count,
        cycle_ms = started.elapsed().as_millis() as u64,
        "statistics update cycle"
    );
    Ok(1)
}

fn scan_memories(
    metadata: &brain_planner::SharedMetadataDb,
) -> Result<(u64, Option<u64>, Option<u64>), WorkerError> {
    let db = metadata.lock();
    let rtxn = db
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("stats rtxn: {e:?}")))?;
    let table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;

    let mut count = 0u64;
    let mut oldest: Option<u64> = None;
    let mut newest: Option<u64> = None;
    for entry in table
        .iter()
        .map_err(|e| WorkerError::Ops(format!("MEMORIES iter: {e:?}")))?
    {
        let (_, value) = entry.map_err(|e| WorkerError::Ops(format!("memory row: {e:?}")))?;
        let meta = value.value();
        count += 1;
        let c = meta.created_at_unix_nanos;
        oldest = Some(oldest.map_or(c, |o| o.min(c)));
        newest = Some(newest.map_or(c, |n| n.max(c)));
    }
    Ok((count, oldest, newest))
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
