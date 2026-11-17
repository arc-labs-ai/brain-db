//! Durability contract between the writer's contiguous-prefix watermark
//! and WAL recovery.
//!
//! The watermark is what a checkpoint publishes as `CHECKPOINT_END.durable_lsn`,
//! and recovery skips every WAL record at or below that LSN
//! (`crates/brain-storage/src/recovery.rs`). These tests close that loop
//! through the real recovery driver: a write whose redb transaction has not
//! landed must still be replayed after a restart, and one that has landed
//! must not be.

use std::path::PathBuf;

use brain_core::{MemoryId, MemoryKind, NamespaceId, RequestId, SessionId, SpaceId};
use brain_embed::VECTOR_DIM;
use brain_ops::test_support::run_in_glommio;
use brain_ops::RedbCommittedWatermark;
use brain_storage::arena::ArenaFile;
use brain_storage::recovery::{recover, InMemoryMetadataSink};
use brain_storage::wal::{EncodePayload, Lsn, Wal, WalPayload, WalRecord};

const SHARD_UUID: [u8; 16] = [7u8; 16];

/// One ENCODE record per arena slot — the simplest payload recovery
/// replays end to end.
fn encode_record(slot: u64) -> WalRecord {
    let payload = EncodePayload {
        memory_id: MemoryId::pack(1, slot, 1),
        request_id: RequestId::from([0u8; 16]),
        space_id: SpaceId::from([0u8; 16]),
        namespace_id: NamespaceId::SYSTEM,
        session_id: SessionId(0),
        kind: MemoryKind::Episodic,
        salience_initial: 0.5,
        embedding_model_fp: [0xAB; 16],
        text: "watermark recovery contract".to_string(),
        vector: vec![0.5; VECTOR_DIM],
        edges: vec![],
        request_hash: [0u8; 32],
        response_payload: vec![],
        deduplicate: false,
        occurred_at_unix_nanos: None,
    };
    WalRecord::from_typed(
        Lsn(0),
        0,
        1_700_000_000_000_000_000,
        0xCAFE,
        &WalPayload::Encode(payload),
    )
}

/// Four WAL-durable ENCODE records, returned with the LSNs the WAL
/// assigned them.
fn wal_with_four_encodes(root: &std::path::Path) -> (PathBuf, Vec<u64>) {
    let wal_dir = root.join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();
    let wal_dir_for_write = wal_dir.clone();
    let lsns = run_in_glommio(move || async move {
        let wal = Wal::create(&wal_dir_for_write, SHARD_UUID).await.unwrap();
        let mut assigned = Vec::new();
        for slot in 0..4 {
            assigned.push(wal.append(encode_record(slot)).await.unwrap().raw());
        }
        wal.shutdown().await.unwrap();
        assigned
    });
    (wal_dir, lsns)
}

/// Two writes are durable in the WAL; only the second one's redb
/// transaction committed. The frontier must stay below the first, so
/// recovery replays both — acking a write whose metadata never landed and
/// then skipping it on restart is silent data loss.
#[test]
fn recovery_replays_records_the_frontier_has_not_claimed() {
    let dir = tempfile::tempdir().unwrap();
    let (wal_dir, lsns) = wal_with_four_encodes(dir.path());

    let watermark = RedbCommittedWatermark::new();
    watermark.reserve(lsns[0], lsns[1]);
    watermark.reserve(lsns[2], lsns[3]);
    watermark.commit(lsns[2], lsns[3]);

    let mut arena = ArenaFile::open(dir.path().join("arena.bin"), SHARD_UUID, 16).unwrap();
    let mut sink = InMemoryMetadataSink::with_durable_lsn(watermark.load());
    let (report, _allocator) = recover(&mut arena, &wal_dir, SHARD_UUID, &mut sink).unwrap();

    assert_eq!(
        report.records_skipped, 0,
        "the first write never committed, so no record may be treated as durable"
    );
    assert_eq!(report.records_replayed, 4);
    for lsn in &lsns {
        assert!(
            sink.applied().contains_key(lsn),
            "LSN {lsn} was acked but never reapplied: the write is lost"
        );
    }
}

/// The mirror image, which keeps the test above honest: once every
/// reservation commits, the frontier covers the whole log and recovery
/// has nothing to replay. A frontier stuck at zero would satisfy the
/// durability half while making every restart reapply the entire WAL.
#[test]
fn recovery_skips_records_the_frontier_has_claimed() {
    let dir = tempfile::tempdir().unwrap();
    let (wal_dir, lsns) = wal_with_four_encodes(dir.path());

    let watermark = RedbCommittedWatermark::new();
    watermark.reserve(lsns[0], lsns[1]);
    watermark.reserve(lsns[2], lsns[3]);
    watermark.commit(lsns[2], lsns[3]);
    watermark.commit(lsns[0], lsns[1]);
    assert_eq!(watermark.load(), lsns[3]);

    let mut arena = ArenaFile::open(dir.path().join("arena.bin"), SHARD_UUID, 16).unwrap();
    let mut sink = InMemoryMetadataSink::with_durable_lsn(watermark.load());
    let (report, _allocator) = recover(&mut arena, &wal_dir, SHARD_UUID, &mut sink).unwrap();

    assert_eq!(report.records_replayed, 0);
    assert_eq!(report.records_skipped, 4);
    assert!(sink.applied().is_empty());
}
