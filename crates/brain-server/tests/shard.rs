//! Integration tests for the Phase 9.4 + 9.5 shard scaffold.
//!
//! Linux-only — Glommio requires io_uring; brain-storage requires
//! mmap + pwritev2. Each test runs the Tokio side as `#[tokio::test]`
//! and spawns a Glommio shard via `spawn_shard`. The cross-runtime
//! boundary is exercised through `flume` channels.

#![cfg(target_os = "linux")]

use std::time::Duration;

use tempfile::TempDir;

// shard.rs uses `crate::shard_adapters::…`; pull both source files into
// the test binary so that `crate::` resolves the same as in main.rs.
// The 9.10 `dispatch_op` surface is used only by `tests/dispatch.rs`;
// silence the dead-code lint from this binary's perspective.
#[allow(dead_code)]
#[path = "../src/shard/mod.rs"]
mod shard;

use shard::{
    spawn_shard, AllocSlotError, AppendWalError, ShardError, ShardHandle, ShardOpError,
    ShardSpawnConfig,
};

// ---------------------------------------------------------------------------
// 9.4 — Ping + lifecycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_roundtrips() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::new(dir.path())).expect("spawn");
    handle.ping().await.expect("ping should succeed");
    drop(handle);
    joiner.join().expect("shard joins cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_pings_complete() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(1, ShardSpawnConfig::new(dir.path())).expect("spawn");
    for _ in 0..100 {
        handle.ping().await.expect("ping should succeed");
    }
    drop(handle);
    joiner.join().expect("shard joins cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pings_via_cloned_handles() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(2, ShardSpawnConfig::new(dir.path())).expect("spawn");

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
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(3, ShardSpawnConfig::new(dir.path())).expect("spawn");
    handle.ping().await.expect("ping pre-drop");

    drop(handle);
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("spawn_blocking join")
        .expect("shard joins cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pin_to_invalid_cpu_errors() {
    let dir = TempDir::new().unwrap();
    let cfg = ShardSpawnConfig {
        pin_cpu: Some(usize::MAX),
        ..ShardSpawnConfig::new(dir.path().to_owned())
    };
    match spawn_shard(4, cfg) {
        Ok(_) => panic!("spawn should fail for invalid CPU id usize::MAX"),
        Err(ShardError::Spawn(_)) => {}
        Err(other) => panic!("expected Spawn error, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_after_drop_fails_cleanly() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(5, ShardSpawnConfig::new(dir.path())).expect("spawn");
    let extra = handle.clone();
    drop(handle);
    extra.ping().await.expect("extra clone can still ping");

    let h = extra.clone();
    drop(extra);
    drop(h);
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

// ---------------------------------------------------------------------------
// 9.5 — Arena hookup
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arena_first_spawn_creates_files() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::new(dir.path())).expect("spawn");
    drop(handle);
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("blocking join")
        .expect("join");

    let shard_dir = dir.path().join("0");
    assert!(shard_dir.is_dir(), "shard dir created at {shard_dir:?}");
    assert!(shard_dir.join("arena.bin").is_file(), "arena.bin present");
    assert!(shard_dir.join("shard.uuid").is_file(), "shard.uuid present");
    let uuid_bytes = std::fs::read(shard_dir.join("shard.uuid")).unwrap();
    assert_eq!(uuid_bytes.len(), 16);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arena_alloc_returns_sequential_indices() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::new(dir.path())).expect("spawn");
    let a = handle.alloc_slot().await.expect("alloc 1");
    let b = handle.alloc_slot().await.expect("alloc 2");
    let c = handle.alloc_slot().await.expect("alloc 3");
    // On a fresh arena, allocator hands out 0, 1, 2 sequentially.
    assert_eq!(a.0, 0);
    assert_eq!(b.0, 1);
    assert_eq!(c.0, 2);
    // Each fresh slot starts at version 1 per `brain_storage::arena`.
    assert_eq!(a.1, 1);
    assert_eq!(b.1, 1);
    assert_eq!(c.1, 1);
    drop(handle);
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("blocking join")
        .expect("join");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arena_uuid_persists_across_restarts() {
    let dir = TempDir::new().unwrap();
    let arena_path = dir.path().join("0").join("arena.bin");
    let uuid_path = dir.path().join("0").join("shard.uuid");

    let (uuid_before, arena_len_before) = {
        let (handle, joiner) =
            spawn_shard(0, ShardSpawnConfig::new(dir.path())).expect("spawn 1st");
        // Alloc twice. These slots end up in PENDING_WRITE state — the
        // encoder (9.7) is responsible for promoting to OCCUPIED; on the
        // current scaffold they're correctly reclaimed by the allocator
        // on restart, so we don't assert anything about next-alloc-index
        // across restart here. See 9.6+ tests for allocator+WAL semantics.
        let _ = handle.alloc_slot().await.expect("alloc 1");
        let _ = handle.alloc_slot().await.expect("alloc 2");
        let u = std::fs::read(&uuid_path).unwrap();
        let len = std::fs::metadata(&arena_path).unwrap().len();
        drop(handle);
        tokio::task::spawn_blocking(move || joiner.join())
            .await
            .expect("blocking 1")
            .expect("join 1");
        (u, len)
    };
    // Re-spawn on the same dir.
    {
        let (handle, joiner) =
            spawn_shard(0, ShardSpawnConfig::new(dir.path())).expect("spawn 2nd");
        let uuid_after = std::fs::read(&uuid_path).unwrap();
        assert_eq!(uuid_before, uuid_after, "UUID must persist across restarts");
        let arena_len_after = std::fs::metadata(&arena_path).unwrap().len();
        assert_eq!(
            arena_len_before, arena_len_after,
            "arena.bin size must persist (capacity unchanged)"
        );
        // One more alloc on the reopened arena — just proves the executor
        // accepted the rebuilt allocator. We don't assert the returned
        // index because PENDING_WRITE slots from the prior run are
        // reclaimable (free_list LIFO) and the encoder/WAL plumbing that
        // turns them into committed slots lands in 9.6+/9.7.
        let _ = handle.alloc_slot().await.expect("alloc post-reopen");
        drop(handle);
        tokio::task::spawn_blocking(move || joiner.join())
            .await
            .expect("blocking 2")
            .expect("join 2");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shard_uuid_mismatch_errors_on_reopen() {
    let dir = TempDir::new().unwrap();

    // First spawn writes the UUID.
    let (handle, joiner) =
        spawn_shard(0, ShardSpawnConfig::new(dir.path())).expect("spawn initial");
    drop(handle);
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("blocking")
        .expect("join");

    // Corrupt shard.uuid with a different value while arena.bin still
    // carries the original.
    let uuid_path = dir.path().join("0").join("shard.uuid");
    std::fs::write(&uuid_path, [0u8; 16]).unwrap();

    match spawn_shard(0, ShardSpawnConfig::new(dir.path())) {
        Ok(_) => panic!("expected mismatch error"),
        Err(ShardError::ArenaOpen(e)) => {
            // Either ShardUuidMismatch (the spec-shaped case) or one of
            // the surrounding header errors — both are acceptable signals
            // that we refused the mismatched UUID file.
            let msg = e.to_string();
            assert!(
                msg.contains("shard_uuid mismatch")
                    || msg.contains("header")
                    || msg.contains("UUID"),
                "expected uuid-mismatch-shaped error, got: {msg}"
            );
        }
        Err(other) => panic!("expected ArenaOpen error, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_dir_under_nested_path() {
    let dir = TempDir::new().unwrap();
    let nested = dir.path().join("a").join("b").join("c");
    let (handle, joiner) = spawn_shard(7, ShardSpawnConfig::new(&nested)).expect("spawn");
    handle.ping().await.expect("ping");
    drop(handle);
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("blocking join")
        .expect("join");
    assert!(nested.join("7").join("arena.bin").is_file());
}

// ---------------------------------------------------------------------------
// 9.6 — Real WAL hookup
// ---------------------------------------------------------------------------

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, RequestId};
use brain_storage::wal::payload::{EncodePayload, WalPayload};
use brain_storage::wal::reader::WalReader;
use brain_storage::wal::record::{Lsn, WalRecord};

fn encode_record(slot: u64, byte: u8) -> WalRecord {
    let p = EncodePayload {
        memory_id: MemoryId::pack(1, slot, 1),
        request_id: RequestId::from([byte; 16]),
        agent_id: AgentId::from([byte; 16]),
        context_id: ContextId(0),
        kind: MemoryKind::Episodic,
        salience_initial: 0.5,
        embedding_model_fp: [byte; 16],
        text: format!("memory {slot}"),
        vector: vec![0.0; 384],
        edges: vec![],
    };
    WalRecord::from_typed(
        Lsn(0),
        0,
        1_700_000_000_000_000_000,
        u64::from(byte),
        &WalPayload::Encode(p),
    )
}

/// Read the shard's UUID file. Used by tests that need to construct a
/// `WalReader` matching the shard's identity.
fn read_shard_uuid(shard_dir: &std::path::Path) -> [u8; 16] {
    let bytes = std::fs::read(shard_dir.join("shard.uuid")).expect("read shard.uuid");
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wal_first_spawn_creates_segment_zero() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::new(dir.path())).expect("spawn");
    drop(handle);
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("blocking join")
        .expect("join");

    let wal_dir = dir.path().join("0").join("wal");
    assert!(wal_dir.is_dir(), "wal dir present");
    assert!(
        wal_dir.join("0000000000.wal").is_file(),
        "segment 0 file present"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_wal_record_returns_lsn() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::new(dir.path())).expect("spawn");
    let lsn1 = handle.append_wal_record(encode_record(0, 1)).await.unwrap();
    let lsn2 = handle.append_wal_record(encode_record(1, 2)).await.unwrap();
    let lsn3 = handle.append_wal_record(encode_record(2, 3)).await.unwrap();
    assert_eq!(lsn1, 1);
    assert_eq!(lsn2, 2);
    assert_eq!(lsn3, 3);
    drop(handle);
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("blocking join")
        .expect("join");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wal_records_visible_to_reader_after_shutdown() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::new(dir.path())).expect("spawn");
    for slot in 0..3u64 {
        handle
            .append_wal_record(encode_record(slot, slot as u8))
            .await
            .unwrap();
    }
    drop(handle);
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("blocking join")
        .expect("join");

    let shard_dir = dir.path().join("0");
    let uuid = read_shard_uuid(&shard_dir);
    let reader = WalReader::open(shard_dir.join("wal"), uuid).unwrap();
    let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
    assert_eq!(lsns, vec![1, 2, 3]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wal_persists_across_restart() {
    let dir = TempDir::new().unwrap();
    let data_path = dir.path().to_owned();

    // Run 1: write 2 records, clean shutdown.
    {
        let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::new(&data_path)).expect("spawn 1");
        handle.append_wal_record(encode_record(0, 1)).await.unwrap();
        handle.append_wal_record(encode_record(1, 2)).await.unwrap();
        drop(handle);
        tokio::task::spawn_blocking(move || joiner.join())
            .await
            .expect("blocking 1")
            .expect("join 1");
    }
    // Run 2: re-spawn; recovery seeds next_lsn at 3; next append returns 3.
    {
        let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::new(&data_path)).expect("spawn 2");
        let lsn = handle.append_wal_record(encode_record(2, 3)).await.unwrap();
        assert_eq!(lsn, 3, "LSN continues across restart");
        drop(handle);
        tokio::task::spawn_blocking(move || joiner.join())
            .await
            .expect("blocking 2")
            .expect("join 2");
    }
    // WAL now has 3 records (LSNs 1, 2, 3).
    let shard_dir = data_path.join("0");
    let uuid = read_shard_uuid(&shard_dir);
    let reader = WalReader::open(shard_dir.join("wal"), uuid).unwrap();
    let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
    assert_eq!(lsns, vec![1, 2, 3]);
}

// ---------------------------------------------------------------------------
// 9.7b — Per-shard OpsContext + workers wired in
// ---------------------------------------------------------------------------

/// Spawn → full OpsContext stack constructed (metadata + hnsw + writer +
/// ops + scheduler with 12 workers) → ping → alloc → append → drop →
/// joiner.join. Asserts no panic; smoke-checks the wire-up end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shard_constructs_full_ops_stack() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::new(dir.path())).expect("spawn");
    handle.ping().await.expect("ping");
    let (slot, _ver) = handle.alloc_slot().await.expect("alloc");
    assert_eq!(slot, 0);
    handle
        .append_wal_record(encode_record(0, 1))
        .await
        .expect("append");
    drop(handle);
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("blocking join")
        .expect("join");

    // metadata.redb is present + has been touched by recovery (writes
    // happen on next_lsn bookkeeping).
    assert!(dir.path().join("0").join("metadata.redb").is_file());
}

/// Spawn → drop immediately → joiner.join. Exercises the scheduler's
/// shutdown drain (12 workers terminate cleanly within the 5s budget).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shard_shutdown_drains_workers_cleanly() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::new(dir.path())).expect("spawn");
    handle
        .ping()
        .await
        .expect("ping (verifies workers started)");
    drop(handle);
    let start = std::time::Instant::now();
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("blocking join")
        .expect("join");
    let elapsed = start.elapsed();
    // Workers default to long intervals; shutdown should return promptly
    // once the flag is set + each task's next sleep yields.
    assert!(
        elapsed < Duration::from_secs(10),
        "shutdown took {elapsed:?}, expected < 10s"
    );
}

// ---------------------------------------------------------------------------
// Error-type plumbing sanity
// ---------------------------------------------------------------------------

#[test]
fn alloc_slot_error_carries_op_variant() {
    fn _accepts(e: ShardOpError) -> AllocSlotError {
        e.into()
    }
}

#[test]
fn append_wal_error_carries_op_variant() {
    fn _accepts(e: ShardOpError) -> AppendWalError {
        e.into()
    }
}
