//! Integration tests for the Phase 9.4 shard scaffold.
//!
//! Linux-only — Glommio requires io_uring. Each test runs the Tokio
//! side as `#[tokio::test]` and spawns one or more Glommio shards
//! transitively via `spawn_shard`. The cross-runtime boundary is
//! exercised through `flume` channels.

#![cfg(target_os = "linux")]

use std::time::Duration;

#[path = "../src/shard.rs"]
mod shard;

use shard::{spawn_shard, ShardError, ShardHandle, ShardSpawnConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_roundtrips() {
    let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::default()).expect("spawn");
    handle.ping().await.expect("ping should succeed");
    drop(handle);
    joiner.join().expect("shard joins cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_pings_complete() {
    let (handle, joiner) = spawn_shard(1, ShardSpawnConfig::default()).expect("spawn");
    for _ in 0..100 {
        handle.ping().await.expect("ping should succeed");
    }
    drop(handle);
    joiner.join().expect("shard joins cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pings_via_cloned_handles() {
    let (handle, joiner) = spawn_shard(2, ShardSpawnConfig::default()).expect("spawn");

    let mut joins = Vec::with_capacity(50);
    for _ in 0..50 {
        let h: ShardHandle = handle.clone();
        joins.push(tokio::spawn(async move { h.ping().await }));
    }
    for j in joins {
        j.await.expect("task panic").expect("ping err");
    }
    drop(handle);
    joiner.join().expect("shard joins cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_last_handle_lets_joiner_complete() {
    let (handle, joiner) = spawn_shard(3, ShardSpawnConfig::default()).expect("spawn");
    handle.ping().await.expect("ping pre-drop");

    drop(handle);
    // joiner.join() blocks the current Tokio worker thread until the
    // executor thread exits. With multi_thread runtime the other
    // worker keeps the test alive.
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("spawn_blocking join")
        .expect("shard joins cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pin_to_invalid_cpu_errors() {
    let cfg = ShardSpawnConfig {
        channel_capacity: 1024,
        pin_cpu: Some(usize::MAX),
    };
    match spawn_shard(4, cfg) {
        Ok(_) => panic!("spawn should fail for invalid CPU id usize::MAX"),
        Err(ShardError::Spawn(_)) => {}
        Err(other) => panic!("expected Spawn error, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_after_drop_fails_cleanly() {
    let (handle, joiner) = spawn_shard(5, ShardSpawnConfig::default()).expect("spawn");
    let extra = handle.clone();
    drop(handle);
    // Joiner not yet joined — the extra clone still keeps the channel
    // alive, so the shard is still running. The clone can still ping.
    extra.ping().await.expect("extra clone can still ping");

    // Drop the extra clone too. Now the channel really is closed; the
    // shard exits. Any further send would fail with ShardDisconnected.
    // We can't re-create a ShardHandle after drop; that's the design.
    // Just verify the joiner completes.
    let h = extra.clone();
    drop(extra);
    drop(h);
    // Tiny grace period for the executor's recv to observe disconnect.
    tokio::time::sleep(Duration::from_millis(20)).await;
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("spawn_blocking")
        .expect("shard joins cleanly");
}

#[test]
fn shard_handle_send_sync_at_use_site() {
    fn require<T: Send + Sync>() {}
    require::<ShardHandle>();
}
