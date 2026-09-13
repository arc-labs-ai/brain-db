//! Tantivy recovery on shard startup.
//!
//! Walks the [`TantivyShardStartup`] reported by `TantivyShard::open`
//! and runs the rebuild functions for any scope whose status
//! is `NeedsRebuild`, then re-opens to pick up the fresh on-disk
//! state.

use std::path::Path;
use std::sync::Arc;

use brain_index::{
    IndexStatus, LexicalScope, TantivyShard, TantivyShardError, TantivyShardStartup,
};
use brain_metadata::MetadataDb;
use brain_ops::index::text_indexer::{
    rebuild_memory_text, rebuild_statements, RebuildError, RebuildReport,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("tantivy open: {0}")]
    Open(#[from] TantivyShardError),
    #[error("memory_text rebuild: {0}")]
    MemoryRebuild(#[source] RebuildError),
    #[error("statements rebuild: {0}")]
    StatementsRebuild(#[source] RebuildError),
}

/// Recover the tantivy indexes for one shard. Returns a fresh
/// `Arc<TantivyShard>` whose indexes are guaranteed `Ready`.
///
/// `metadata` is the per-shard `MetadataDb`. Recovery reads from
/// it (no writes); the caller is responsible for ensuring no
/// writer is racing this function — at shard spawn, that's free.
pub fn recover_tantivy_on_open(
    shard_dir: &Path,
    metadata: &MetadataDb,
    startup: TantivyShardStartup,
) -> Result<Arc<TantivyShard>, RecoveryError> {
    let TantivyShardStartup {
        shard,
        memory_status,
        statements_status,
    } = startup;

    let memory_needs_rebuild = matches!(memory_status, IndexStatus::NeedsRebuild { .. });
    let statements_needs_rebuild = matches!(statements_status, IndexStatus::NeedsRebuild { .. });

    if !memory_needs_rebuild && !statements_needs_rebuild {
        return Ok(shard);
    }

    // Drop the existing Arc — the rebuild renames the live dir
    // out from underneath, and any held tantivy `Index` value
    // would keep the directory pinned via its inner Arc.
    drop(shard);

    if memory_needs_rebuild {
        log_reason(LexicalScope::MemoryText, &memory_status);
        let report =
            rebuild_memory_text(shard_dir, metadata).map_err(RecoveryError::MemoryRebuild)?;
        log_report(&report);
    }
    if statements_needs_rebuild {
        log_reason(LexicalScope::StatementText, &statements_status);
        let report =
            rebuild_statements(shard_dir, metadata).map_err(RecoveryError::StatementsRebuild)?;
        log_report(&report);
    }

    // Re-open. Both scopes must be `Ready` now; if they aren't,
    // the rebuild produced a broken state (bug, not data).
    let fresh = TantivyShard::open(shard_dir)?;
    debug_assert!(
        matches!(fresh.memory_status, IndexStatus::Ready),
        "memory_text still NeedsRebuild after rebuild: {:?}",
        fresh.memory_status,
    );
    debug_assert!(
        matches!(fresh.statements_status, IndexStatus::Ready),
        "statements still NeedsRebuild after rebuild: {:?}",
        fresh.statements_status,
    );
    Ok(fresh.shard)
}

fn log_reason(scope: LexicalScope, status: &IndexStatus) {
    if let IndexStatus::NeedsRebuild { reason } = status {
        tracing::warn!(
            target: "brain_server::shard",
            ?scope,
            ?reason,
            "tantivy rebuild scheduled at shard startup",
        );
    }
}

fn log_report(report: &RebuildReport) {
    tracing::info!(
        target: "brain_server::shard",
        scope = ?report.scope,
        rows = report.rows_processed,
        duration_ms = report.duration.as_millis() as u64,
        "tantivy rebuild complete",
    );
}

#[cfg(test)]
mod tests;
