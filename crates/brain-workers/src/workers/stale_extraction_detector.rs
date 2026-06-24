//! Stale extraction detector.
//!
//! Periodic Low-priority worker that **counts** statements whose
//! `schema_version` is behind the current schema. v1 does NOT
//! write a per-row flag (that's a `StatementRow.flags` bump, post-
//! v1); instead, the worker logs the count + exposes it via
//! metrics. The schema-migration worker is the side that
//! re-extracts.
//!
//! ## v1 scope cuts
//!
//! - No per-row flag-write (would need a `StatementRow.flags` v3
//!   schema bump). Operators query stale count via the
//!   `sweeper_swept_total{worker="stale_extraction_detector"}`
//!   metric.
//! - Per-namespace schema version lookup is approximated as
//!   "max schema_version across all `SCHEMA_ACTIVE_VERSIONS_TABLE`
//!   rows"; per-(memory, namespace) precise lookup is post-v1.

use std::future::Future;
use std::pin::Pin;

use brain_metadata::extractor::sweep::scan_stale_statements;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

pub struct StaleExtractionDetector {
    config: WorkerConfig,
}

impl StaleExtractionDetector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::StaleExtractionDetector),
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    async fn run_once(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        let metadata = ctx.ops.executor.metadata.as_ref();
        let rtxn = metadata
            .read_txn()
            .map_err(|e| WorkerError::Internal(format!("stale detector rtxn: {e}")))?;
        // v1 max-across-namespaces approximation. The exact
        // current_schema_version comes from
        // `SCHEMA_ACTIVE_VERSIONS_TABLE`; for v1 we read the
        // single-namespace value (matches the typical
        // single-deployment-namespace case).
        let current_version = current_schema_version(&rtxn).unwrap_or(0);
        if current_version == 0 {
            return Ok(0);
        }
        let summary = scan_stale_statements(&rtxn, current_version, self.config.batch_size)
            .map_err(|e| WorkerError::Internal(format!("stale scan: {e}")))?;
        tracing::debug!(
            target: "brain_workers::stale_extraction_detector",
            scanned = summary.scanned,
            stale_count = summary.dry_run_would_delete,
            "stale extraction scan complete",
        );
        Ok(summary.dry_run_would_delete as usize)
    }
}

fn current_schema_version(rtxn: &redb::ReadTransaction) -> Option<u32> {
    use brain_metadata::tables::schema_version::SCHEMA_ACTIVE_VERSIONS_TABLE;
    let table = match rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE) {
        Ok(t) => t,
        Err(_) => return None,
    };
    let mut max = 0u32;
    let iter = match redb::ReadableTable::iter(&table) {
        Ok(it) => it,
        Err(_) => return None,
    };
    for entry in iter {
        let Ok((_, v)) = entry else { continue };
        let version = v.value();
        if version > max {
            max = version;
        }
    }
    if max == 0 {
        None
    } else {
        Some(max)
    }
}

impl Default for StaleExtractionDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for StaleExtractionDetector {
    fn name(&self) -> &'static str {
        WorkerKind::StaleExtractionDetector.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::StaleExtractionDetector
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(self.run_once(ctx))
    }
}

