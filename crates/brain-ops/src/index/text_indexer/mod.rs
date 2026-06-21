//! Tantivy text-indexer workers.
//!
//! Implements the post-commit text-indexing pipeline.
//!
//! Two indexers per shard (memory + statement), both:
//!
//! - Run on the near-foreground priority lane.
//! - Use a bounded `flume` channel; on overflow the foreground
//!   awaits the send (backpressure).
//! - Drain via an async loop that owns the per-scope `IndexWriter`.
//! - Group-commit on N=256 writes OR T=1 s, env-overridable.
//! - Stamp the brain schema-version payload on
//!   every commit so subsequent opens see a current
//!   version.
//! - Retry once on commit failure, then escalate to shard-fatal
//!   — text indexing is correctness, not best-effort.

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

/// Default queue capacity — bounded queues with
/// capacity 4096 by default.
pub const DEFAULT_QUEUE_CAPACITY: usize = 4096;

/// Default commit cadence — group commit every
/// 256 writes or 1 second, whichever first.
pub const DEFAULT_COMMIT_N: usize = 256;
pub const DEFAULT_COMMIT_MS: u64 = 1000;

/// Commit cadence config. Built from `[index]` config at shard
/// startup via [`CommitPolicy::new`]; hot-reload is post-v1.
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
    /// Build from explicit values (the `[index]` config knobs
    /// `tantivy_commit_n` / `tantivy_commit_ms`, plumbed at shard
    /// startup).
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
}
