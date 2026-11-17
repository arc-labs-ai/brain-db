//! Tier 1 + Tier 2 of the aggregate engine.
//!
//! - **Tier 1, [`export`]:** a periodic Parquet snapshot of the memory
//!   fields aggregates touch (namespace, space, kind, status, salience,
//!   access count, created time, session), from one sequential scan.
//!   Each row group is sorted by `(namespace, space, created_at)`, so space
//!   and time filters read only matching pages. Never the vector arena.
//! - **Tier 2, [`Analytics::query`]:** DataFusion over that snapshot, on
//!   its own tokio runtime (never the Glommio shard threads), under a
//!   memory cap that spills instead of failing.
//!
//! Tier 0 (exact running counters) lives in `brain_metadata::tables::agg`;
//! the `MEMORY_AGGREGATE` handler routes to this crate only for what the
//! counters cannot answer (salience / time filters, MIN / MAX, per-day).
//!
//! **Panics.** Release builds set `panic = "abort"` (as the whole server
//! does), so a panic in an export or scan aborts the process like any other
//! panic in Brain; nothing here tries to catch one.
//!
//! **Freshness.** Which memories a result counts is as of the latest
//! export (results carry its time); salience in it is evaluated at query
//! time. Exports are incremental: each interval writes a delta of the
//! memories changed since the snapshot (from the shard's change log), and
//! once the delta passes [`DELTA_MAX_FRACTION`] of the snapshot it is
//! compacted into a new one from Parquet alone. Salience no longer changes
//! rows as it decays, so the log holds only real writes. See [`export`].

pub mod export;
mod query;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::Duration;

use brain_metadata::{Lifeline, MetadataDb};
use datafusion::execution::disk_manager::{DiskManager, DiskManagerMode};
use datafusion::execution::memory_pool::FairSpillPool;
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use tokio::sync::Semaphore;

pub use export::{export_delta, export_snapshot, SnapshotInfo};

/// A delta may hold up to this fraction of its snapshot's rows (at least
/// [`DELTA_MIN_ROWS`]) before it is compacted into a new snapshot.
pub const DELTA_MAX_FRACTION: f64 = 0.10;
/// Smallest delta size limit, so a small snapshot isn't rewritten
/// constantly.
pub const DELTA_MIN_ROWS: usize = 10_000;

/// What queries read: a full snapshot plus, usually, a delta on top.
#[derive(Debug, Clone)]
pub struct CurrentSnapshot {
    pub base: SnapshotInfo,
    pub delta: Option<SnapshotInfo>,
}

impl CurrentSnapshot {
    /// The time every row a query counts is as of.
    #[must_use]
    pub fn as_of_unix_nanos(&self) -> u64 {
        self.delta
            .as_ref()
            .unwrap_or(&self.base)
            .taken_at_unix_nanos
    }
}
pub use query::{ScanGroup, ScanQuery, ScanResult};

/// Errors from export or query.
#[derive(Debug, thiserror::Error)]
pub enum AnalyticsError {
    #[error("metadata read: {0}")]
    Metadata(String),
    #[error("snapshot io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parquet: {0}")]
    Parquet(#[from] datafusion::parquet::errors::ParquetError),
    #[error("arrow: {0}")]
    Arrow(#[from] datafusion::arrow::error::ArrowError),
    #[error("query engine: {0}")]
    Engine(#[from] datafusion::error::DataFusionError),
    /// No snapshot exported yet (first export still running, or disabled).
    #[error("no analytics snapshot yet; the first export has not finished")]
    NoSnapshot,
    #[error("analytics query exceeded {0:?}")]
    Timeout(Duration),
    #[error("analytics runtime stopped")]
    Stopped,
    #[error("invalid analytics config: {0}")]
    Config(String),
}

/// Operator knobs. See `[analytics]` in the server config.
#[derive(Debug, Clone)]
pub struct AnalyticsConfig {
    /// How often to re-export the snapshot. Also the worst-case staleness.
    pub export_interval: Duration,
    /// Upper bound on this shard's DataFusion working memory; beyond it,
    /// grouped aggregation spills to disk.
    pub memory_limit_bytes: usize,
    /// Upper bound on spill files (under `<dir>/spill`). A query that
    /// would exceed it fails instead of filling the disk.
    pub spill_limit_bytes: u64,
    /// Query wall-clock cap. Keep below `export_interval`: the snapshot a
    /// query reads is deleted two exports later.
    pub query_timeout: Duration,
    /// Tokio worker threads for exports + queries (off the shard cores).
    pub threads: usize,
    /// Heavy queries allowed at once per shard; the rest wait (within
    /// `query_timeout`).
    pub max_concurrent_queries: usize,
    /// Snapshot exports allowed at once across the whole process. Each
    /// export buffers ~3 row groups (~50 MiB each) per thread, outside the
    /// DataFusion memory budget; this caps that on many-shard servers.
    /// The first service started in a process fixes the value.
    pub max_concurrent_exports: usize,
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self {
            export_interval: Duration::from_secs(3600),
            memory_limit_bytes: default_memory_limit(),
            spill_limit_bytes: 50 << 30,
            query_timeout: Duration::from_secs(30),
            threads: 2,
            max_concurrent_queries: 2,
            max_concurrent_exports: 2,
        }
    }
}

/// A quarter of physical RAM (`/proc/meminfo`), or 1 GiB if unreadable.
#[must_use]
pub fn default_memory_limit() -> usize {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<usize>().ok())
        })
        .map_or(1 << 30, |kb| kb * 1024 / 4)
}

/// Process-wide counting gate on concurrent exports.
struct ExportGate {
    free: Mutex<usize>,
    cv: Condvar,
}

impl ExportGate {
    fn global(slots: usize) -> &'static ExportGate {
        static GATE: OnceLock<ExportGate> = OnceLock::new();
        GATE.get_or_init(|| ExportGate {
            free: Mutex::new(slots.max(1)),
            cv: Condvar::new(),
        })
    }

    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        {
            let mut free = self.free.lock().expect("export gate");
            while *free == 0 {
                free = self.cv.wait(free).expect("export gate");
            }
            *free -= 1;
        }
        struct Release<'a>(&'a ExportGate);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                *self.0.free.lock().expect("export gate") += 1;
                self.0.cv.notify_one();
            }
        }
        let _release = Release(self);
        f()
    }
}

pub(crate) struct Inner {
    /// Weak: the shard owns the DB. Once it drops it, exports stop instead
    /// of keeping a closed shard's file open.
    pub(crate) db: Weak<MetadataDb>,
    pub(crate) dir: PathBuf,
    pub(crate) cfg: AnalyticsConfig,
    pub(crate) ctx: SessionContext,
    pub(crate) current: Mutex<Option<CurrentSnapshot>>,
    /// The service's runtime, for compaction (async DataFusion) from the
    /// blocking export path.
    rt: tokio::runtime::Handle,
    /// Change-log entries at or below this sequence are no longer needed
    /// (the current base covers them); the shard prunes them.
    prune_upto: Arc<AtomicU64>,
    pub(crate) permits: Semaphore,
    /// The shard's lifeline (see `brain_metadata::lifeline`); `None` when
    /// started without one (tests, tools).
    lifeline: Option<Lifeline>,
    export_lock: Mutex<()>,
    exports_total: AtomicU64,
    export_failures_total: AtomicU64,
    last_export_ms: AtomicU64,
}

/// Export counters for `/metrics`.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnalyticsStats {
    pub exports_total: u64,
    pub export_failures_total: u64,
    pub last_export_ms: u64,
}

/// Per-shard analytics service: owns the export loop and runs queries.
pub struct Analytics {
    rt: Option<tokio::runtime::Runtime>,
    inner: Arc<Inner>,
}

impl Analytics {
    /// Start the service for one shard. Snapshots live in `dir`. Picks up
    /// the newest existing snapshot so a restart can serve immediately,
    /// then exports on `cfg.export_interval` (first export right away if
    /// none exists).
    pub fn start(
        db: Arc<MetadataDb>,
        dir: impl AsRef<Path>,
        cfg: AnalyticsConfig,
    ) -> Result<Self, AnalyticsError> {
        Self::start_with_lifeline(db, dir, cfg, None)
    }

    /// [`Self::start`] tied to a shard's lifeline: exports enter it and
    /// stop mid-scan once the shard stops, so a closing shard never waits
    /// on, or is outlived by, an export holding its DB.
    pub fn start_with_lifeline(
        db: Arc<MetadataDb>,
        dir: impl AsRef<Path>,
        cfg: AnalyticsConfig,
        lifeline: Option<Lifeline>,
    ) -> Result<Self, AnalyticsError> {
        let dir = dir.as_ref().to_path_buf();
        // A query keeps reading its snapshot file; the file survives one
        // more export (we keep two), so a query must end within one interval.
        if cfg.query_timeout >= cfg.export_interval {
            return Err(AnalyticsError::Config(format!(
                "query_timeout ({:?}) must be shorter than export_interval ({:?})",
                cfg.query_timeout, cfg.export_interval
            )));
        }
        if cfg.memory_limit_bytes == 0 {
            return Err(AnalyticsError::Config(
                "memory_limit_bytes must be > 0".into(),
            ));
        }
        let spill_dir = dir.join("spill");
        // Spill files from a crashed run are garbage.
        let _ = std::fs::remove_dir_all(&spill_dir);
        std::fs::create_dir_all(&spill_dir)?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(cfg.threads.max(1))
            .thread_name("brain-analytics")
            .enable_time()
            .build()?;
        let runtime_env = RuntimeEnvBuilder::new()
            .with_memory_pool(Arc::new(FairSpillPool::new(cfg.memory_limit_bytes)))
            // Spill next to the snapshots (the data volume), not the OS
            // temp dir, and cap it.
            .with_disk_manager_builder(
                DiskManager::builder()
                    .with_mode(DiskManagerMode::Directories(vec![spill_dir]))
                    .with_max_temp_directory_size(cfg.spill_limit_bytes),
            )
            .build_arc()?;
        let ctx = SessionContext::new_with_config_rt(
            SessionConfig::new().with_target_partitions(cfg.threads.max(1)),
            runtime_env,
        );
        // Deltas need an unbroken change log since their base: if tracking
        // was off, no old snapshot can be extended.
        {
            let wtxn = db
                .write_txn()
                .map_err(|e| AnalyticsError::Metadata(e.to_string()))?;
            let fresh = brain_metadata::tables::snapshot_changes::set_tracking(&wtxn, true)
                .map_err(|e| AnalyticsError::Metadata(e.to_string()))?;
            wtxn.commit()
                .map_err(|e| AnalyticsError::Metadata(e.to_string()))?;
            if fresh {
                export::remove_all(&dir)?;
            }
        }
        let existing = export::latest_snapshot(&dir)?;
        if cfg.threads == 0 || cfg.max_concurrent_queries == 0 || cfg.max_concurrent_exports == 0 {
            return Err(AnalyticsError::Config(
                "threads, max_concurrent_queries and max_concurrent_exports must be >= 1".into(),
            ));
        }
        let inner = Arc::new(Inner {
            rt: rt.handle().clone(),
            db: Arc::downgrade(&db),
            dir,
            permits: Semaphore::new(cfg.max_concurrent_queries.max(1)),
            cfg,
            ctx,
            prune_upto: Arc::new(AtomicU64::new(
                existing.as_ref().map_or(0, |b| b.change_seq),
            )),
            current: Mutex::new(existing.map(|base| CurrentSnapshot { base, delta: None })),
            lifeline,
            export_lock: Mutex::new(()),
            exports_total: AtomicU64::new(0),
            export_failures_total: AtomicU64::new(0),
            last_export_ms: AtomicU64::new(0),
        });

        let loop_inner = inner.clone();
        rt.spawn(async move {
            loop {
                let job = loop_inner.clone();
                match tokio::task::spawn_blocking(move || job.export_and_swap(false)).await {
                    Ok(Ok(info)) => tracing::info!(
                        rows = info.rows,
                        bytes = info.bytes,
                        elapsed_ms = info.elapsed_ms,
                        "analytics snapshot exported"
                    ),
                    // The shard closed its DB: nothing left to export.
                    Ok(Err(AnalyticsError::Stopped)) => return,
                    Ok(Err(e)) => tracing::warn!(error = %e, "analytics snapshot export failed"),
                    Err(e) => tracing::warn!(error = %e, "analytics export task failed"),
                }
                tokio::time::sleep(loop_inner.cfg.export_interval).await;
            }
        });

        Ok(Self {
            rt: Some(rt),
            inner,
        })
    }

    /// Take a full snapshot now and make it current (blocking).
    pub fn export_now(&self) -> Result<SnapshotInfo, AnalyticsError> {
        self.inner.export_and_swap(true)
    }

    /// Bring the snapshot up to date now, as the export loop does: a delta
    /// when possible, else a full snapshot (blocking). Returns what it
    /// wrote.
    pub fn refresh_now(&self) -> Result<SnapshotInfo, AnalyticsError> {
        self.inner.export_and_swap(false)
    }

    /// The change-log sequence up to which the shard may prune (shared;
    /// see `brain_metadata::tables::snapshot_changes`).
    #[must_use]
    pub fn prune_upto(&self) -> Arc<AtomicU64> {
        self.inner.prune_upto.clone()
    }

    /// Export counters.
    #[must_use]
    pub fn stats(&self) -> AnalyticsStats {
        AnalyticsStats {
            exports_total: self.inner.exports_total.load(Ordering::Relaxed),
            export_failures_total: self.inner.export_failures_total.load(Ordering::Relaxed),
            last_export_ms: self.inner.last_export_ms.load(Ordering::Relaxed),
        }
    }

    /// The full snapshot queries currently read, if any.
    #[must_use]
    pub fn current_snapshot(&self) -> Option<SnapshotInfo> {
        self.current().map(|c| c.base)
    }

    /// Everything queries currently read: base and delta.
    #[must_use]
    pub fn current(&self) -> Option<CurrentSnapshot> {
        self.inner.current.lock().expect("snapshot lock").clone()
    }

    /// Run an aggregate over the current snapshot. Runtime-agnostic: the
    /// work runs on this service's tokio runtime, and the returned future
    /// can be awaited from a Glommio shard.
    pub async fn query(&self, q: ScanQuery) -> Result<ScanResult, AnalyticsError> {
        let rt = self.rt.as_ref().ok_or(AnalyticsError::Stopped)?;
        let inner = self.inner.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        rt.spawn(async move {
            let _ = tx.send(query::run(&inner, q).await);
        });
        rx.await.map_err(|_| AnalyticsError::Stopped)?
    }
}

/// What one export step produced.
enum Exported {
    Snapshot(SnapshotInfo),
    Delta(SnapshotInfo),
}

impl Inner {
    /// Bring the snapshot up to date and make the result current: a full
    /// export when `full` or there is nothing to extend; otherwise a delta,
    /// compacted into a new snapshot once it passes
    /// [`DELTA_MAX_FRACTION`]. Returns what it wrote.
    fn export_and_swap(&self, full: bool) -> Result<SnapshotInfo, AnalyticsError> {
        // One export per shard at a time (loop vs `export_now`), so the
        // "after the previous snapshot" name below can't be picked twice.
        let _one = self.export_lock.lock().expect("export lock");
        let cur = self.current.lock().expect("snapshot lock").clone();
        let gate = ExportGate::global(self.cfg.max_concurrent_exports);
        let res = gate.run(|| {
            // Lifeline guard before the DB handle, so the handle drops
            // first and the shard never exits while we hold its DB.
            let _guard = match self.lifeline.as_ref().map(Lifeline::enter) {
                Some(None) => return Err(AnalyticsError::Stopped),
                g => g.flatten(),
            };
            // Upgrade only once our turn comes: waiting on other shards'
            // exports must not pin this shard's DB.
            let Some(db) = self.db.upgrade() else {
                return Err(AnalyticsError::Stopped);
            };
            // File names must sort in export order even if the wall clock
            // steps back, or pruning would delete the newest one.
            let after = cur.as_ref().map_or(0, CurrentSnapshot::as_of_unix_nanos);
            match (full, &cur) {
                (false, Some(c)) => {
                    export::export_delta(&db, &self.dir, &c.base, after).map(Exported::Delta)
                }
                _ => {
                    let stop = || self.lifeline.as_ref().is_some_and(|l| !l.is_alive());
                    export::export_snapshot(&db, &self.dir, self.cfg.threads, after, &stop)
                        .map(Exported::Snapshot)
                }
            }
        });
        let res = res.and_then(|e| match (e, &cur) {
            (Exported::Delta(d), Some(c)) => {
                let limit =
                    ((c.base.rows as f64 * DELTA_MAX_FRACTION) as u64).max(DELTA_MIN_ROWS as u64);
                if d.rows <= limit {
                    return Ok(Exported::Delta(d));
                }
                // Parquet only: the DB is not held while compacting.
                let compacted = gate.run(|| {
                    self.rt
                        .block_on(export::compact(&self.ctx, &self.dir, &c.base, &d))
                });
                let _ = std::fs::remove_file(&d.path);
                compacted.map(Exported::Snapshot)
            }
            (e, _) => Ok(e),
        });
        let exported = match res {
            Err(AnalyticsError::Stopped) => return Err(AnalyticsError::Stopped),
            Ok(e) => e,
            Err(e) => {
                self.export_failures_total.fetch_add(1, Ordering::Relaxed);
                return Err(e);
            }
        };
        let (next, info) = match exported {
            Exported::Snapshot(s) => (
                CurrentSnapshot {
                    base: s.clone(),
                    delta: None,
                },
                s,
            ),
            Exported::Delta(d) => (
                CurrentSnapshot {
                    base: cur.expect("a delta extends a snapshot").base,
                    delta: Some(d.clone()),
                },
                d,
            ),
        };
        self.exports_total.fetch_add(1, Ordering::Relaxed);
        self.last_export_ms
            .store(info.elapsed_ms, Ordering::Relaxed);
        let mut keep: Vec<PathBuf> = next.base.files.clone();
        keep.extend(next.delta.iter().map(|d| d.path.clone()));
        if next.delta.is_none() {
            // The new snapshot covers every change up to its sequence.
            self.prune_upto
                .fetch_max(next.base.change_seq, Ordering::Relaxed);
        }
        *self.current.lock().expect("snapshot lock") = Some(next);
        export::prune_old_snapshots(&self.dir, 2, &keep)?;
        Ok(info)
    }
}

impl Drop for Analytics {
    fn drop(&mut self) {
        // Dropping a tokio runtime blocks; this may run on a Glommio thread.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}
