//! Graceful shutdown helpers.
//!
//! By the time these helpers run, the Tokio runtime has already
//! observed the shared `ShutdownSignal` and both the connection
//! listener and the admin server have exited their accept loops.
//! What remains is to:
//!
//! 1. Drop every `ShardHandle` clone so each shard's
//!    `flume::Receiver<ShardRequest>` returns `Err` → the shard's
//!    `shard_main_loop` exits → in-shard drain runs (scheduler →
//!    WAL → arena msync).
//! 2. `join()` every `ShardJoiner` with a per-shard timeout so a
//!    stuck Glommio executor can't block process exit.
//!
//! Returning `ExitCode::FAILURE` on timeout surfaces the problem
//! through the process's exit status — observability friendly.

#![cfg(target_os = "linux")]

use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{error, info};

use crate::shard::{ShardHandle, ShardJoiner};

/// Default upper bound on the shard-drain phase. The scheduler's own
/// `SHUTDOWN_DRAIN_BUDGET` is 5s; WAL flush is ~100 µs;
/// arena msync is best-effort. 30s is comfortable for a clean exit
/// and forgiving on a heavily-loaded shard.
pub const DEFAULT_SHARD_DRAIN_BUDGET: Duration = Duration::from_secs(30);

/// Drop every `ShardHandle` clone, then `join()` each
/// `ShardJoiner` with a per-shard timeout. The total wall-clock
/// budget is `drain_budget`; if it runs out, the remaining joiners
/// are leaked (`std::mem::forget`) and the function returns
/// `ExitCode::FAILURE`.
///
/// `shards` is the `Arc<Vec<ShardHandle>>` that
/// `Topology`/`AdminState`/the event-hub bridge tasks all share.
/// By the time this function runs, those consumers have already
/// dropped their clones (their futures completed when the runtime's
/// block_on returned). The `Arc` we hold is therefore the last
/// strong ref — dropping it closes every shard's request channel.
///
/// Caller must run this outside any async runtime (it uses
/// `std::thread::spawn` + `mpsc::recv_timeout`).
#[must_use]
pub fn graceful_shutdown_shards(
    shards: Arc<Vec<ShardHandle>>,
    joiners: Vec<ShardJoiner>,
    drain_budget: Duration,
) -> ExitCode {
    let shard_count = joiners.len();
    info!(
        shard_count,
        budget_secs = drain_budget.as_secs(),
        "shard drain starting",
    );

    // Step 1: close every shard's request channel.
    let outstanding = Arc::strong_count(&shards);
    drop(shards);
    if outstanding > 1 {
        // Other clones live somewhere — shard_main_loop's
        // `rx.recv_async()` won't return until those drop. The
        // per-shard timeout below catches this case.
        info!(
            outstanding,
            "shard handle Arc had outstanding clones at drain start"
        );
    }

    // Step 2: per-shard timed join.
    let deadline = Instant::now() + drain_budget;
    let mut rc = ExitCode::SUCCESS;
    for joiner in joiners {
        let shard_id = joiner.shard_id();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            error!(
                shard_id,
                "shard drain budget exhausted; leaking joiner thread"
            );
            rc = ExitCode::FAILURE;
            std::mem::forget(joiner); // suppress Drop's secondary WARN
            continue;
        }
        match join_with_timeout(joiner, remaining) {
            JoinOutcome::Clean => {
                info!(shard_id, "shard joined cleanly");
            }
            JoinOutcome::Failed(e) => {
                error!(shard_id, error = %e, "shard join failed");
                rc = ExitCode::FAILURE;
            }
            JoinOutcome::TimedOut => {
                error!(
                    shard_id,
                    timeout_ms = remaining.as_millis() as u64,
                    "shard join timed out; leaking thread"
                );
                rc = ExitCode::FAILURE;
            }
        }
    }
    rc
}

enum JoinOutcome {
    Clean,
    Failed(String),
    TimedOut,
}

fn join_with_timeout(joiner: ShardJoiner, timeout: Duration) -> JoinOutcome {
    let (tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);
    std::thread::spawn(move || {
        let res = joiner.join().map_err(|e| e.to_string());
        let _ = tx.send(res);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(())) => JoinOutcome::Clean,
        Ok(Err(e)) => JoinOutcome::Failed(e),
        Err(_) => JoinOutcome::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    // Module-level tests live in `tests/shutdown.rs` because they need
    // a Tokio runtime + real shards. This module exists for the
    // `#[cfg(test)]` build to find unit-test imports.
}
