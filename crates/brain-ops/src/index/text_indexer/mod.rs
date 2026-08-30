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

// ---------------------------------------------------------------------------
// Live-rebuild control plane.
//
// A hot tantivy rebuild must take the exclusive per-directory writer lock
// away from the running indexer, rebuild the index from authoritative redb,
// swap it into place, then hand the indexer a writer on the new index — all
// without the read side ever serving stale-mixed lexical data (invariant #7).
// Both per-shard text indexers listen on an `IndexerControl` channel in
// parallel with their op queue so the shard's rebuild dance can drive them.
// ---------------------------------------------------------------------------

/// Control-plane message for a live tantivy rebuild.
#[cfg(target_os = "linux")]
pub enum IndexerControl {
    /// Drop the current writer (releasing tantivy's exclusive per-directory
    /// lock) and pause draining so the shard can rebuild the on-disk index
    /// from authoritative redb and swap it into place. Acked once the writer
    /// is closed. Ops keep buffering in the op channel while paused, and are
    /// re-drained after [`Resume`](Self::Resume) — every buffered op was
    /// applied post-redb-commit, so re-applying it to the rebuilt index is
    /// idempotent (delete-term then add).
    Quiesce { ack: flume::Sender<()> },
    /// Resume draining against a fresh writer built from `handle` — a handle
    /// on the reopened, post-swap index. Acked once running again.
    Resume {
        handle: brain_index::IndexHandle,
        ack: flume::Sender<()>,
    },
}

/// Build the group-commit `IndexWriter` shared by both indexers: 50 MB
/// heap, one writer thread. Tantivy enforces a ~15 MB minimum; 50 MB is
/// comfortable for the 256-doc batch shape.
#[cfg(target_os = "linux")]
pub(crate) fn build_indexer_writer(
    handle: &brain_index::IndexHandle,
) -> Result<tantivy::IndexWriter, tantivy::TantivyError> {
    handle.index.writer_with_num_threads(1, 50_000_000)
}

/// Park a drain loop after a `Quiesce`: the writer is already dropped, so
/// this waits only for `Resume` (build + return a fresh writer on the new
/// index) or shutdown. A stray `Quiesce` while paused is idempotent (acked,
/// stays paused). Ops remain buffered in the op channel throughout and are
/// drained after resume.
///
/// Returns `Some(writer)` on `Resume` and `None` on shutdown. A shutdown
/// observed *while paused* returns `None` without a final commit — there is
/// no writer to commit through. In the shard this is unreachable: the
/// rebuild dance (`Quiesce` … `Resume`) runs to completion within one
/// single-threaded main-loop turn, so the shard's own teardown signal cannot
/// interleave between them; the arm exists for the standalone-task tests and
/// as a defensive backstop.
#[cfg(target_os = "linux")]
pub(crate) async fn wait_while_paused(
    control: &flume::Receiver<IndexerControl>,
    shutdown: &flume::Receiver<()>,
) -> Option<tantivy::IndexWriter> {
    use futures_lite::FutureExt;
    loop {
        let ctrl = async { Some(control.recv_async().await) };
        let stop = async {
            let _ = shutdown.recv_async().await;
            None
        };
        match ctrl.or(stop).await {
            Some(Ok(IndexerControl::Resume { handle, ack })) => match build_indexer_writer(&handle)
            {
                Ok(w) => {
                    let _ = ack.send_async(()).await;
                    return Some(w);
                }
                Err(e) => {
                    tracing::error!(
                        target: "brain_ops::text_indexer",
                        error = %e,
                        "indexer resume: writer rebuild failed; terminating drain loop",
                    );
                    // Do NOT ack success. A success ack would make the
                    // rebuild orchestrator's `await_ack` return `Ok`, so the
                    // rebuild reports success — yet this drain loop is about to
                    // terminate and drop its op-channel receiver, after which
                    // every later ENCODE/FORGET silently discards its lexical
                    // write forever (invariant #7). Dropping the ack sender
                    // instead closes the ack channel, so `await_ack` observes
                    // the drop and the rebuild fails loudly.
                    drop(ack);
                    return None;
                }
            },
            Some(Ok(IndexerControl::Quiesce { ack })) => {
                // Already paused; ack and keep waiting for Resume.
                let _ = ack.send_async(()).await;
            }
            // Control channel closed, or shutdown signalled: tear down.
            Some(Err(_)) | None => return None,
        }
    }
}

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
