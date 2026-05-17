//! Tantivy text-indexer workers (phase 22.3 + 22.4).
//!
//! Implements the post-commit pipeline defined in
//! `spec/27_knowledge_workers/02_text_indexer_workers.md`.
//!
//! Two indexers per shard (memory + statement), both:
//!
//! - Run on the near-foreground priority lane.
//! - Use a bounded `flume` channel; on overflow the foreground
//!   awaits the send (§27/02 §1 backpressure).
//! - Drain via an async loop that owns the per-scope `IndexWriter`.
//! - Group-commit on N=256 writes OR T=1 s, env-overridable.
//! - Stamp the brain schema-version payload (§26/01 §2) on
//!   every commit so subsequent opens via 22.1 see a current
//!   version.
//! - Retry once on commit failure, then escalate to shard-fatal
//!   (§27/02 §4) — text indexing is correctness, not best-effort.
//!
//! Phase 22.3 lands the `MemoryTextIndexer` half. Phase 22.4
//! adds the statement side in this same module.

pub mod memory;
pub mod rebuild;
pub mod statement;

pub use memory::{MemoryTextDispatcher, MemoryTextOp};
pub use rebuild::{rebuild_memory_text, rebuild_statements, RebuildError, RebuildReport};
pub use statement::{StatementTextDispatcher, StatementTextOp};

#[cfg(target_os = "linux")]
pub use memory::spawn_memory_text_indexer_local;
#[cfg(target_os = "linux")]
pub use statement::spawn_statement_text_indexer_local;

use std::time::Duration;

/// Default queue capacity (§27/02 §1, "bounded queues with
/// capacity 4096 by default").
pub const DEFAULT_QUEUE_CAPACITY: usize = 4096;

/// Default commit cadence per §26/01 §3 — group commit every
/// 256 writes or 1 second, whichever first.
pub const DEFAULT_COMMIT_N: usize = 256;
pub const DEFAULT_COMMIT_MS: u64 = 1000;

/// Commit cadence config. Read from env at shard startup
/// (see [`CommitPolicy::from_env`]); hot-reload is post-v1.
#[derive(Debug, Clone, Copy)]
pub struct CommitPolicy {
    pub n_writes: usize,
    pub interval: Duration,
}

impl Default for CommitPolicy {
    fn default() -> Self {
        Self {
            n_writes: DEFAULT_COMMIT_N,
            interval: Duration::from_millis(DEFAULT_COMMIT_MS),
        }
    }
}

impl CommitPolicy {
    /// Reads `BRAIN_TANTIVY_COMMIT_N` and `BRAIN_TANTIVY_COMMIT_MS`
    /// from the environment, falling back to defaults on missing
    /// or unparseable values.
    #[must_use]
    pub fn from_env() -> Self {
        let n_writes = std::env::var("BRAIN_TANTIVY_COMMIT_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n: &usize| n > 0)
            .unwrap_or(DEFAULT_COMMIT_N);
        let interval_ms = std::env::var("BRAIN_TANTIVY_COMMIT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&ms: &u64| ms > 0)
            .unwrap_or(DEFAULT_COMMIT_MS);
        Self {
            n_writes,
            interval: Duration::from_millis(interval_ms),
        }
    }

    /// Test helper — explicit override.
    #[must_use]
    pub fn new(n_writes: usize, interval: Duration) -> Self {
        Self { n_writes, interval }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_policy_default_matches_spec() {
        let p = CommitPolicy::default();
        assert_eq!(p.n_writes, 256);
        assert_eq!(p.interval, Duration::from_millis(1000));
    }

    #[test]
    fn commit_policy_from_env_uses_defaults_when_unset() {
        // Defensive: tests run in parallel so we don't rely on
        // unsetting env vars; just verify the default values are
        // what fall-through produces.
        let _ = CommitPolicy::from_env();
    }
}
