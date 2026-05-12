//! Integration tests for `RealWriterHandle` (sub-task 7.2).
//!
//! Each test wires:
//! - A tempdir `MetadataDb` wrapped in `SharedMetadataDb`.
//! - A fresh `SharedHnsw::<384>` + `HnswWriter`.
//! - The `RealWriterHandle` against both.
//!
//! Asserts the spec §07/06 + §08/04 + §08/06 protocols.

use std::sync::Arc;

use brain_core::{ContextId, EdgeKind, MemoryId, MemoryKind, RequestId};
use brain_embed::VECTOR_DIM;
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::idempotency::IDEMPOTENCY_TABLE;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::MetadataDb;
use brain_ops::RealWriterHandle;
use brain_planner::{
    EdgeOutcome, EncodeOp, EncodeOpEdge, ForgetOp, ForgetOutcome, SharedMetadataDb, WriterError,
    WriterHandle,
};
use brain_protocol::request::ForgetMode;
use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Fixture builder.
// ---------------------------------------------------------------------------

struct Fixture {
    writer: Arc<RealWriterHandle>,
    metadata: SharedMetadataDb,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));

    let (_shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

    Fixture {
        writer,
        metadata,
        _tempdir: tempdir,
    }
}

fn make_encode_op(request_id: [u8; 16], text: &str) -> EncodeOp {
    EncodeOp {
        request_id: RequestId::from(request_id),
        context_id: ContextId(42),
        kind: MemoryKind::Episodic,
        text: text.into(),
        vector: [0.0; VECTOR_DIM],
        salience_initial: 0.5,
        fingerprint: [0x11; 16],
        edges: vec![],
    }
}

fn make_forget_op(request_id: [u8; 16], memory_id: MemoryId, mode: ForgetMode) -> ForgetOp {
    ForgetOp {
        request_id: RequestId::from(request_id),
        memory_id,
        mode,
    }
}

// ---------------------------------------------------------------------------
// Encode tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn encode_round_trips_and_writes_metadata() {
    let fix = build_fixture();
    let ack = fix
        .writer
        .submit_encode(make_encode_op([1; 16], "hello"))
        .await
        .unwrap();
    assert_eq!(ack.memory_id.shard(), 0);
    assert_eq!(ack.memory_id.slot(), 1);
    assert!(!ack.replayed);

    // The memory row is in metadata.
    let db = fix.metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
    let row = table.get(ack.memory_id.to_be_bytes()).unwrap();
    assert!(row.is_some(), "memory row should exist post-encode");
    drop(table);

    // The idempotency entry is also there.
    let idem = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
    let entry = idem.get([1u8; 16]).unwrap();
    assert!(
        entry.is_some(),
        "idempotency entry should exist post-encode"
    );
}

#[tokio::test]
async fn idempotent_replay_returns_cached_ack() {
    let fix = build_fixture();
    let op = make_encode_op([2; 16], "hello");

    let first = fix.writer.submit_encode(op.clone()).await.unwrap();
    assert!(!first.replayed);

    let second = fix.writer.submit_encode(op).await.unwrap();
    assert!(second.replayed, "second submit must replay");
    assert_eq!(first.memory_id, second.memory_id);
    assert_eq!(first.edge_results, second.edge_results);
}

#[tokio::test]
async fn idempotency_conflict_on_different_params() {
    let fix = build_fixture();
    let first = fix
        .writer
        .submit_encode(make_encode_op([3; 16], "hello"))
        .await
        .unwrap();
    assert!(!first.replayed);

    // Same request_id, different text → conflict.
    let err = fix
        .writer
        .submit_encode(make_encode_op([3; 16], "HELLO"))
        .await
        .unwrap_err();
    match err {
        WriterError::Conflict(msg) => assert!(msg.contains("hash mismatch")),
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn distinct_request_ids_produce_distinct_memory_ids() {
    let fix = build_fixture();
    let a = fix
        .writer
        .submit_encode(make_encode_op([10; 16], "a"))
        .await
        .unwrap();
    let b = fix
        .writer
        .submit_encode(make_encode_op([11; 16], "b"))
        .await
        .unwrap();
    let c = fix
        .writer
        .submit_encode(make_encode_op([12; 16], "c"))
        .await
        .unwrap();
    assert_ne!(a.memory_id, b.memory_id);
    assert_ne!(b.memory_id, c.memory_id);
    assert_ne!(a.memory_id, c.memory_id);
    assert_eq!(a.memory_id.slot(), 1);
    assert_eq!(b.memory_id.slot(), 2);
    assert_eq!(c.memory_id.slot(), 3);
}

#[tokio::test]
async fn edges_are_evaluated_against_existing_memories() {
    let fix = build_fixture();

    // First insert a memory we can target.
    let target = fix
        .writer
        .submit_encode(make_encode_op([20; 16], "target"))
        .await
        .unwrap();

    // Now an encode with two edges: one to the existing memory,
    // one to a non-existent id.
    let mut op = make_encode_op([21; 16], "linker");
    op.edges = vec![
        EncodeOpEdge {
            target: target.memory_id,
            kind: EdgeKind::References,
            weight: 0.5,
        },
        EncodeOpEdge {
            target: MemoryId::from(0xDEAD_BEEF_u128),
            kind: EdgeKind::References,
            weight: 0.5,
        },
    ];

    let ack = fix.writer.submit_encode(op).await.unwrap();
    assert_eq!(ack.edge_results.len(), 2);
    assert_eq!(ack.edge_results[0], EdgeOutcome::Inserted);
    assert_eq!(ack.edge_results[1], EdgeOutcome::TargetMissing);

    // Replay returns the same edge outcomes.
    let mut op = make_encode_op([21; 16], "linker");
    op.edges = vec![
        EncodeOpEdge {
            target: target.memory_id,
            kind: EdgeKind::References,
            weight: 0.5,
        },
        EncodeOpEdge {
            target: MemoryId::from(0xDEAD_BEEF_u128),
            kind: EdgeKind::References,
            weight: 0.5,
        },
    ];
    let replay = fix.writer.submit_encode(op).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.edge_results, ack.edge_results);
}

// ---------------------------------------------------------------------------
// Forget tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_round_trips_via_real_writer() {
    let fix = build_fixture();
    let enc = fix
        .writer
        .submit_encode(make_encode_op([30; 16], "forgetme"))
        .await
        .unwrap();
    let ack = fix
        .writer
        .submit_forget(make_forget_op([31; 16], enc.memory_id, ForgetMode::Soft))
        .await
        .unwrap();
    assert_eq!(ack.memory_id, enc.memory_id);
    assert_eq!(ack.outcome, ForgetOutcome::Tombstoned);
    assert!(!ack.replayed);
}

#[tokio::test]
async fn forget_idempotent_replay() {
    let fix = build_fixture();
    let enc = fix
        .writer
        .submit_encode(make_encode_op([40; 16], "forgetme"))
        .await
        .unwrap();
    let op = make_forget_op([41; 16], enc.memory_id, ForgetMode::Soft);

    let first = fix.writer.submit_forget(op).await.unwrap();
    assert!(!first.replayed);

    let second = fix.writer.submit_forget(op).await.unwrap();
    assert!(second.replayed);
    assert_eq!(first.memory_id, second.memory_id);
    assert_eq!(first.outcome, second.outcome);
}

#[tokio::test]
async fn forget_already_tombstoned_for_new_request_id() {
    let fix = build_fixture();
    let enc = fix
        .writer
        .submit_encode(make_encode_op([50; 16], "x"))
        .await
        .unwrap();

    // First forget succeeds with Tombstoned.
    let first = fix
        .writer
        .submit_forget(make_forget_op([51; 16], enc.memory_id, ForgetMode::Soft))
        .await
        .unwrap();
    assert_eq!(first.outcome, ForgetOutcome::Tombstoned);

    // Second forget with a DIFFERENT request_id → AlreadyTombstoned.
    let second = fix
        .writer
        .submit_forget(make_forget_op([52; 16], enc.memory_id, ForgetMode::Soft))
        .await
        .unwrap();
    assert_eq!(second.outcome, ForgetOutcome::AlreadyTombstoned);
    assert!(!second.replayed);
}

#[tokio::test]
async fn forget_memory_not_found() {
    let fix = build_fixture();
    let phantom = MemoryId::pack(0, 999, 1);
    let ack = fix
        .writer
        .submit_forget(make_forget_op([60; 16], phantom, ForgetMode::Soft))
        .await
        .unwrap();
    assert_eq!(ack.outcome, ForgetOutcome::MemoryNotFound);
}

// ---------------------------------------------------------------------------
// Concurrency.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_encodes_get_distinct_memory_ids() {
    let fix = build_fixture();
    let writer = Arc::clone(&fix.writer);

    let mut handles = Vec::new();
    for i in 0..8u8 {
        let w = Arc::clone(&writer);
        handles.push(tokio::spawn(async move {
            let mut req_id = [0u8; 16];
            req_id[0] = 0xA0 + i;
            w.submit_encode(make_encode_op(req_id, "concurrent"))
                .await
                .unwrap()
        }));
    }

    let mut memory_ids = Vec::with_capacity(8);
    for h in handles {
        let ack = h.await.unwrap();
        memory_ids.push(ack.memory_id);
    }
    memory_ids.sort();
    memory_ids.dedup();
    assert_eq!(
        memory_ids.len(),
        8,
        "all 8 encodes must get distinct MemoryIds"
    );
}
