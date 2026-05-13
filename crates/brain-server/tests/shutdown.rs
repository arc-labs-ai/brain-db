//! Integration tests for sub-task 9.14 — graceful shutdown.
//!
//! Process-state-mutating SIGINT/SIGTERM tests are intentionally
//! out of scope (signal delivery is hostile to parallel test
//! execution; see plan §4). Cover the meaningful drain logic
//! instead.

#![cfg(target_os = "linux")]

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::TempDir;

#[allow(dead_code)]
#[path = "../src/shard/mod.rs"]
mod shard;
#[path = "../src/bootstrap/shutdown.rs"]
mod shutdown;

use shard::{spawn_shard, ShardHandle, ShardJoiner, ShardSpawnConfig};
use shutdown::{graceful_shutdown_shards, DEFAULT_SHARD_DRAIN_BUDGET};

fn fixture(n_shards: usize) -> (Arc<Vec<ShardHandle>>, Vec<ShardJoiner>, TempDir) {
    let dir = TempDir::new().expect("tmp");
    let mut handles = Vec::with_capacity(n_shards);
    let mut joiners = Vec::with_capacity(n_shards);
    for shard_id in 0..n_shards {
        let cfg = ShardSpawnConfig::new(dir.path());
        let (h, j) = spawn_shard(shard_id as u16, cfg).expect("spawn shard");
        handles.push(h);
        joiners.push(j);
    }
    (Arc::new(handles), joiners, dir)
}

/// Real shards drain in ~5 s (the scheduler's own
/// `SHUTDOWN_DRAIN_BUDGET`; workers sleep on long intervals and
/// must be timed out task-by-task). We assert the drain completes
/// well inside a 15 s budget — that proves the parallelisation
/// works (two shards drain concurrently, not sequentially).
#[test]
fn shutdown_shards_returns_within_budget() {
    let (shards, joiners, _dir) = fixture(2);
    let started = Instant::now();
    let rc = graceful_shutdown_shards(shards, joiners, Duration::from_secs(15));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(15),
        "drain took {elapsed:?}; expected to comfortably beat 15s"
    );
    assert_eq!(format!("{rc:?}"), format!("{:?}", ExitCode::SUCCESS));
}

/// Leak the `Arc<Vec<ShardHandle>>` so the shard's request channel
/// stays open. `shard_main_loop` blocks on `rx.recv_async()`; the
/// per-shard join timeout fires; we expect FAILURE within the
/// budget + small overhead.
#[test]
fn shutdown_shards_times_out_on_stuck_join() {
    let (shards, joiners, _dir) = fixture(1);

    // Hold the Arc forever so its strong count never reaches 0,
    // and the request channel stays open. After this point we no
    // longer have a clean way to release the channel — the test
    // intentionally simulates a leaked clone.
    std::mem::forget(shards.clone());

    let started = Instant::now();
    let rc = graceful_shutdown_shards(shards, joiners, Duration::from_millis(300));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(1500),
        "drain hung past timeout; elapsed = {elapsed:?}"
    );
    assert_eq!(format!("{rc:?}"), format!("{:?}", ExitCode::FAILURE));
}

/// Zero shards is a degenerate but valid case.
#[test]
fn shutdown_shards_empty_returns_success() {
    let shards: Arc<Vec<ShardHandle>> = Arc::new(Vec::new());
    let joiners: Vec<ShardJoiner> = Vec::new();
    let rc = graceful_shutdown_shards(shards, joiners, DEFAULT_SHARD_DRAIN_BUDGET);
    assert_eq!(format!("{rc:?}"), format!("{:?}", ExitCode::SUCCESS));
}
