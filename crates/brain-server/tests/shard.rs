//! Integration tests for the shard scaffold.
//!
//! Linux-only — Glommio requires io_uring; brain-storage requires
//! mmap + pwritev2. Each test runs the Tokio side as `#[tokio::test]`
//! and spawns a Glommio shard via `spawn_shard`. The cross-runtime
//! boundary is exercised through `flume` channels.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::Duration;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use tempfile::TempDir;

// shard.rs uses `crate::shard_adapters::…`; pull both source files into
// the test binary so that `crate::` resolves the same as in main.rs.
// The `dispatch_op` surface is used only by `tests/dispatch.rs`;
// silence the dead-code lint from this binary's perspective.
#[allow(dead_code)]
#[path = "../src/shard/mod.rs"]
mod shard;

use shard::{spawn_shard, ShardError, ShardHandle, ShardSpawnConfig};

/// File-local stub: the substrate tests in this file don't exercise
/// embedding quality. Real CpuDispatcher loads in production via
/// `linux_main::build_dispatcher`.
struct TestStubDispatcher;
impl Dispatcher for TestStubDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}
fn stub() -> Arc<dyn Dispatcher> {
    Arc::new(TestStubDispatcher)
}

// ---------------------------------------------------------------------------
// Ping + lifecycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pings_via_cloned_handles() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) =
        spawn_shard(2, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn");

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
    let (handle, joiner) =
        spawn_shard(3, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn");
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
        ..ShardSpawnConfig::new(dir.path().to_owned(), stub())
    };
    match spawn_shard(4, cfg) {
        Ok(_) => panic!("spawn should fail for invalid CPU id usize::MAX"),
        Err(ShardError::Spawn(_)) => {}
        Err(other) => panic!("expected Spawn error, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Arena hookup
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arena_first_spawn_creates_files() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) =
        spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn");
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
    let (handle, joiner) =
        spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn");
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
            spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn 1st");
        // Alloc twice. These slots end up in PENDING_WRITE state — the
        // encoder is responsible for promoting to OCCUPIED; on the
        // current scaffold they're correctly reclaimed by the allocator
        // on restart, so we don't assert anything about next-alloc-index
        // across restart here. See the allocator+WAL tests for those semantics.
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
            spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn 2nd");
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
        // turns them into committed slots lands elsewhere.
        let _ = handle.alloc_slot().await.expect("alloc post-reopen");
        drop(handle);
        tokio::task::spawn_blocking(move || joiner.join())
            .await
            .expect("blocking 2")
            .expect("join 2");
    }
}

/// Smoke: `take_snapshot()` no longer errors out with
/// `SnapshotNotYetImplemented`. Drives the worker through the new save path
/// against an empty HNSW (the basic test harness can't populate the live
/// HNSW — `append_wal_record` only logs and arena populates at recovery
/// time). The empty-HNSW guard in `save_snapshot` skips the hnsw_rs
/// `file_dump` (which errors on empty graphs) so the worker succeeds and the
/// snapshot directory is created with arena/metadata/manifest siblings, but
/// no `hnsw.*` files. Recovery on the next spawn then falls back to the
/// arena-rebuild path (proven by the existing
/// `memory_hnsw_reseeds_from_arena_after_restart`).
///
/// A higher-fidelity test that exercises actual snapshot-load + tail-replay
/// needs the submit-level write path that drives the live HNSW pre-snapshot;
/// the brain-index unit test `save_load_round_trips_epoch_and_lsn` covers
/// the round-trip at the index layer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn take_snapshot_succeeds_on_empty_hnsw_after_pq_pivot() {
    let dir = TempDir::new().unwrap();
    {
        let (handle, joiner) =
            spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn 1");
        handle.append_wal_record(encode_record(0, 1)).await.unwrap();
        // Previously this errored with `SnapshotNotYetImplemented`; the PQ
        // save_snapshot now succeeds (no-op on the empty HNSW, full write
        // when the index is populated — see brain-index round-trip test).
        let _snap_id = handle.take_snapshot().await.expect("take_snapshot");
        drop(handle);
        tokio::task::spawn_blocking(move || joiner.join())
            .await
            .expect("blocking 1")
            .expect("join 1");
    }

    // Snapshot directory was created by the worker for arena/metadata/manifest
    // (hnsw.* files are absent because the HNSW was empty — see the empty
    // guard in `SharedHnswImpl::save_snapshot`).
    let snapshots_root = dir.path().join("0").join("snapshots");
    assert!(
        snapshots_root.exists(),
        "snapshot worker should have created {}",
        snapshots_root.display()
    );
    let snap_subdirs: Vec<_> = std::fs::read_dir(&snapshots_root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();
    assert!(
        !snap_subdirs.is_empty(),
        "expected at least one snapshot subdirectory under {}",
        snapshots_root.display()
    );

    // Respawn: recovery sees an empty snapshot dir (no `.brain`) so it falls
    // through to the arena rebuild path. The single ENCODE record replays
    // into the arena, then the rebuild puts it into the HNSW.
    {
        let (handle, joiner) =
            spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn 2");
        let counts = handle.hnsw_snapshot().await.expect("hnsw snapshot");
        assert_eq!(
            counts.node_count, 1,
            "recovery fallback (arena rebuild) must restore the single ENCODE \
             after a no-op snapshot-load attempt; got {}",
            counts.node_count
        );
        drop(handle);
        tokio::task::spawn_blocking(move || joiner.join())
            .await
            .expect("blocking 2")
            .expect("join 2");
    }
}

/// Regression: the memory HNSW is in-RAM only and rebuilt on startup from the
/// arena. Before the startup reseed landed, a restart left
/// the index empty — memories survived in the arena/metadata but were invisible
/// to semantic recall. This proves the reseed: WAL ENCODEs replayed into the
/// arena reappear as HNSW nodes after a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_hnsw_reseeds_from_arena_after_restart() {
    let dir = TempDir::new().unwrap();

    // Run 1: append three ENCODE records to the WAL, then shut down cleanly.
    // `append_wal_record` only logs; the arena is populated by replay on the
    // next open, so this run's HNSW stays empty.
    {
        let (handle, joiner) =
            spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn 1");
        handle.append_wal_record(encode_record(0, 1)).await.unwrap();
        handle.append_wal_record(encode_record(1, 2)).await.unwrap();
        handle.append_wal_record(encode_record(2, 3)).await.unwrap();
        drop(handle);
        tokio::task::spawn_blocking(move || joiner.join())
            .await
            .expect("blocking 1")
            .expect("join 1");
    }

    // Run 2: respawn on the same dir. Recovery replays the three ENCODEs into
    // the arena, and the startup reseed rebuilds the memory HNSW from it.
    {
        let (handle, joiner) =
            spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn 2");
        let counts = handle.hnsw_snapshot().await.expect("hnsw snapshot");
        assert_eq!(
            counts.node_count, 3,
            "memory HNSW must be reseeded from the arena on restart (got {})",
            counts.node_count
        );
        assert_eq!(counts.tombstone_count, 0);
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
        spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn initial");
    drop(handle);
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("blocking")
        .expect("join");

    // Corrupt shard.uuid with a different value while arena.bin still
    // carries the original.
    let uuid_path = dir.path().join("0").join("shard.uuid");
    std::fs::write(&uuid_path, [0u8; 16]).unwrap();

    match spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())) {
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
    let (handle, joiner) = spawn_shard(7, ShardSpawnConfig::new(&nested, stub())).expect("spawn");
    handle.ping().await.expect("ping");
    drop(handle);
    tokio::task::spawn_blocking(move || joiner.join())
        .await
        .expect("blocking join")
        .expect("join");
    assert!(nested.join("7").join("arena.bin").is_file());
}

// ---------------------------------------------------------------------------
// Real WAL hookup
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
        request_hash: [byte; 32],
        response_payload: vec![],
        occurred_at_unix_nanos: None,
        deduplicate: false,
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
    let (handle, joiner) =
        spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn");
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
async fn wal_persists_across_restart() {
    let dir = TempDir::new().unwrap();
    let data_path = dir.path().to_owned();

    // Run 1: write 2 records, clean shutdown.
    {
        let (handle, joiner) =
            spawn_shard(0, ShardSpawnConfig::new(&data_path, stub())).expect("spawn 1");
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
        let (handle, joiner) =
            spawn_shard(0, ShardSpawnConfig::new(&data_path, stub())).expect("spawn 2");
        let lsn = handle.append_wal_record(encode_record(2, 3)).await.unwrap();
        assert_eq!(lsn, 3, "LSN continues across restart");
        drop(handle);
        tokio::task::spawn_blocking(move || joiner.join())
            .await
            .expect("blocking 2")
            .expect("join 2");
    }
    // The three ENCODE records persist at LSNs 1, 2, 3. The shutdown
    // snapshot (enabled now that PQ persistence is wired) may append
    // CHECKPOINT_BEGIN/END records after them when the recovered HNSW
    // is non-empty — those are expected and not part of this test's
    // contract, so assert the encode records specifically rather than
    // the full WAL contents.
    use brain_storage::wal::kinds::WalRecordKind;
    let shard_dir = data_path.join("0");
    let uuid = read_shard_uuid(&shard_dir);
    let reader = WalReader::open(shard_dir.join("wal"), uuid).unwrap();
    let records: Vec<_> = reader.map(|r| r.unwrap()).collect();
    let encode_lsns: Vec<u64> = records
        .iter()
        .filter(|r| r.kind == WalRecordKind::Encode)
        .map(|r| r.lsn.raw())
        .collect();
    assert_eq!(
        encode_lsns,
        vec![1, 2, 3],
        "the three encodes must persist at sequential LSNs across restart"
    );
    // Any trailing records must be checkpoint bookkeeping, and the LSN
    // sequence must stay gap-free overall (the reader already enforces
    // this — a gap would have surfaced as a WalReadError above).
    for r in &records {
        assert!(
            matches!(
                r.kind,
                WalRecordKind::Encode
                    | WalRecordKind::CheckpointBegin
                    | WalRecordKind::CheckpointEnd
            ),
            "unexpected WAL record kind {:?}",
            r.kind
        );
    }
}

// ---------------------------------------------------------------------------
// Per-shard OpsContext + workers wired in
// ---------------------------------------------------------------------------

/// Spawn → full OpsContext stack constructed (metadata + hnsw + writer +
/// ops + scheduler with 12 workers) → ping → alloc → append → drop →
/// joiner.join. Asserts no panic; smoke-checks the wire-up end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shard_constructs_full_ops_stack() {
    let dir = TempDir::new().unwrap();
    let (handle, joiner) =
        spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn");
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
    let (handle, joiner) =
        spawn_shard(0, ShardSpawnConfig::new(dir.path(), stub())).expect("spawn");
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
