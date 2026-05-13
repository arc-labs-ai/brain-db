//! Embedder cache eviction worker (sub-task 8.12). Spec §11/08 §4.
//!
//! The LRU on `brain_embed::CachingDispatcher` evicts on access
//! automatically (size-bound). This worker handles the
//! **age-based** prune (spec §4.2 — "entries older than 7 days").
//!
//! ## v1 deviation (documented)
//!
//! brain-embed's `CachingDispatcher` doesn't expose a
//! `prune_older_than(Duration)` method, and brain-ops's
//! `OpsContext.executor.embedder` is `Arc<dyn Dispatcher>` — no way
//! to reach the cache through the trait. So v1 ships the worker
//! against a pluggable seam ([`CacheEvictionSource`]) with a no-op
//! default. Phase 9 adds the brain-embed method and an
//! `Arc<CachingDispatcher>` carrier on OpsContext, then injects a
//! real source. Same shape as the HNSW maintenance (8.5) and WAL
//! retention (8.8) workers.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Spec §4.2 — default 7-day age threshold. Tunable per worker via
/// [`CacheEvictionWorker::with_max_age`].
pub const DEFAULT_CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Error)]
pub enum CacheEvictionError {
    /// v1 default — no real cache hookup yet.
    #[error("cache eviction source disabled")]
    Disabled,
    /// Underlying error from the cache (e.g., poisoned lock).
    #[error("cache eviction source failed: {0}")]
    Failed(String),
}

pub type PruneFuture<'a> = Pin<Box<dyn Future<Output = Result<usize, CacheEvictionError>> + 'a>>;

/// Pluggable seam. Production deployments inject an impl backed by
/// `brain_embed::CachingDispatcher::prune_older_than` (Phase 9). v1
/// default is `DisabledCacheEvictionSource`.
/// Post-9.8 `!Send + !Sync` to match the other per-shard source
/// traits. CacheEvictionSource stays Disabled* by default until
/// 9.10 wires a real `CachingDispatcher` per shard.
pub trait CacheEvictionSource: 'static {
    fn prune_older_than(&self, max_age: Duration) -> PruneFuture<'_>;
}

/// Default no-op source.
pub struct DisabledCacheEvictionSource;

impl CacheEvictionSource for DisabledCacheEvictionSource {
    fn prune_older_than(&self, _max_age: Duration) -> PruneFuture<'_> {
        Box::pin(async { Err(CacheEvictionError::Disabled) })
    }
}

pub struct CacheEvictionWorker {
    config: WorkerConfig,
    max_age: Duration,
    source: Arc<dyn CacheEvictionSource>,
}

impl CacheEvictionWorker {
    #[must_use]
    pub fn new(source: Arc<dyn CacheEvictionSource>) -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::EmbedderCacheEvict),
            max_age: DEFAULT_CACHE_MAX_AGE,
            source,
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn with_max_age(mut self, age: Duration) -> Self {
        self.max_age = age;
        self
    }

    #[must_use]
    pub fn max_age(&self) -> Duration {
        self.max_age
    }
}

impl Worker for CacheEvictionWorker {
    fn name(&self) -> &'static str {
        WorkerKind::EmbedderCacheEvict.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::EmbedderCacheEvict
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_evict_cycle(self, ctx))
    }
}

async fn do_evict_cycle(
    worker: &CacheEvictionWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    if worker.config.batch_size == 0 {
        return Ok(0);
    }
    if ctx.is_shutdown() {
        return Ok(0);
    }
    match worker.source.prune_older_than(worker.max_age).await {
        Ok(n) => {
            trace!(
                evicted = n,
                max_age_secs = worker.max_age.as_secs(),
                "cache eviction cycle"
            );
            Ok(n)
        }
        Err(CacheEvictionError::Disabled) => Ok(0),
        Err(CacheEvictionError::Failed(e)) => Err(WorkerError::Ops(format!("cache prune: {e}"))),
    }
}
