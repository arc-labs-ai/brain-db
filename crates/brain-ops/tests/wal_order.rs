//! Spec §05/07 ordering checks: every write op must append a WAL
//! record BEFORE mutating redb. Tests use a [`RecordingWalSink`] or
//! [`FailingWalSink`] so we can assert what reached the WAL and what
//! reached redb, without standing up a real shard.

use std::sync::Arc;

use brain_core::{ContextId, EdgeKind, MemoryKind, RequestId};
use brain_embed::VECTOR_DIM;
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::MetadataDb;
use brain_ops::test_support::run_in_glommio;
use brain_ops::ops::writer::{FailingWalSink, RecordingWalSink, WalSink};
use brain_ops::RealWriterHandle;
use brain_planner::{
    EncodeOp, ForgetOp, LinkOp, SharedMetadataDb, TxnBatch, TxnEncode, WriterError, WriterHandle,
};
use brain_protocol::request::ForgetMode;
use brain_storage::wal::kinds::WalRecordKind;
use brain_storage::wal::payload::WalPayload;
use parking_lot::Mutex;
use redb::ReadableTable;

fn fixture_with_sink(sink: Arc<dyn WalSink>) -> (Arc<RealWriterHandle>, SharedMetadataDb) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (_shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(
        RealWriterHandle::new(metadata.clone(), hnsw_writer)
            .with_wal_sink(sink),
    );
    // Leak the tempdir so the DB lives as long as the test; the dir is
    // wiped by tempfile's Drop at scope end of the *test* via the
    // returned writer holding the DB file open.
    std::mem::forget(dir);
    (writer, metadata)
}

fn make_encode_op(request_id: [u8; 16], text: &str) -> EncodeOp {
    EncodeOp {
        request_id: RequestId::from(request_id),
        context_id: ContextId(42),
        kind: MemoryKind::Episodic,
        text: text.into(),
        vector: [0.5; VECTOR_DIM],
        salience_initial: 0.5,
        fingerprint: [0xAA; 16],
        edges: vec![],
        deduplicate: false,
        content_hash: [0u8; 32],
        agent_id: brain_core::AgentId::default(),
    }
}

/// Test 7: a successful ENCODE appends one WAL record AND writes the
/// memory to redb. The recording sink captures the payload so we can
/// verify the fields round-trip correctly.
#[test]
fn encode_appends_wal_before_redb_commit() {
    run_in_glommio(|| async {
        let sink = Arc::new(RecordingWalSink::new());
        let sink_for_writer: Arc<dyn WalSink> = sink.clone();
        let (writer, metadata) = fixture_with_sink(sink_for_writer);

        let ack = writer
            .submit_encode(make_encode_op([7; 16], "wal-order"))
            .await
            .unwrap();

        // Exactly one WAL record was appended.
        let recs = sink.appended();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].kind, WalRecordKind::Encode);
        // The LSN is the recording sink's monotonic 1.
        assert_eq!(recs[0].lsn.raw(), 1);
        // And the payload decodes to the same memory_id as the redb
        // row — proving the writer didn't write a different id to the
        // two stores.
        let payload = WalPayload::decode(recs[0].kind, &recs[0].payload).unwrap();
        match payload {
            WalPayload::Encode(p) => {
                assert_eq!(p.memory_id, ack.memory_id);
                assert_eq!(p.text, "wal-order");
                assert_eq!(p.context_id, ContextId(42));
                assert!(
                    !p.deduplicate,
                    "this op didn't opt into dedup; payload must reflect that"
                );
                assert!(
                    !p.response_payload.is_empty(),
                    "response_payload must carry the cached EncodeResponse bytes"
                );
            }
            other => panic!("expected Encode payload, got {other:?}"),
        }

        let db = metadata.lock();
        let rtxn = db.read_txn().unwrap();
        let mems = rtxn.open_table(MEMORIES_TABLE).unwrap();
        assert!(
            mems.get(ack.memory_id.to_be_bytes()).unwrap().is_some(),
            "memory row should be in redb"
        );
    });
}

/// Test 8: when the WAL sink fails, the op aborts WITHOUT writing
/// anything to redb. This is the spec §05/07 invariant: WAL failure
/// is an absolute barrier.
#[test]
fn encode_wal_append_failure_aborts_op_without_redb_write() {
    run_in_glommio(|| async {
        let sink: Arc<dyn WalSink> = Arc::new(FailingWalSink::new("simulated disk full"));
        let (writer, metadata) = fixture_with_sink(sink);

        let err = writer
            .submit_encode(make_encode_op([8; 16], "must-not-persist"))
            .await
            .expect_err("wal failure must propagate as a writer error");
        match err {
            WriterError::Internal(msg) => {
                assert!(msg.contains("wal append"), "got {msg}");
                assert!(msg.contains("simulated disk full"), "got {msg}");
            }
            other => panic!("expected Internal, got {other:?}"),
        }

        // No memory row was written.
        let db = metadata.lock();
        let rtxn = db.read_txn().unwrap();
        let mems = rtxn.open_table(MEMORIES_TABLE).unwrap();
        assert_eq!(
            mems.iter().unwrap().count(),
            0,
            "redb must be untouched when WAL append fails"
        );
    });
}

/// Test 10: a Tombstoned FORGET appends a WAL record (kind = Forget)
/// after the original ENCODE record. MemoryNotFound / AlreadyTombstoned
/// outcomes are pure no-ops on the WAL.
#[test]
fn forget_appends_wal_record_only_for_tombstoned_outcome() {
    run_in_glommio(|| async {
        let sink = Arc::new(RecordingWalSink::new());
        let sink_for_writer: Arc<dyn WalSink> = sink.clone();
        let (writer, _metadata) = fixture_with_sink(sink_for_writer);

        // First encode a memory so the FORGET can tombstone it.
        let enc = writer
            .submit_encode(make_encode_op([10; 16], "to-forget"))
            .await
            .unwrap();
        assert_eq!(sink.appended().len(), 1);

        // Now forget it — produces a Forget WAL record.
        writer
            .submit_forget(ForgetOp {
                request_id: RequestId::from([11u8; 16]),
                memory_id: enc.memory_id,
                mode: ForgetMode::Soft,
                agent_id: brain_core::AgentId::default(),
            })
            .await
            .unwrap();
        let recs = sink.appended();
        assert_eq!(recs.len(), 2, "expected Encode + Forget");
        assert_eq!(recs[1].kind, WalRecordKind::Forget);
        match WalPayload::decode(recs[1].kind, &recs[1].payload).unwrap() {
            WalPayload::Forget(p) => {
                assert_eq!(p.memory_id, enc.memory_id);
            }
            other => panic!("expected Forget, got {other:?}"),
        }

        // A duplicate FORGET (different request id, same memory) hits
        // the AlreadyTombstoned branch and MUST NOT WAL.
        writer
            .submit_forget(ForgetOp {
                request_id: RequestId::from([12u8; 16]),
                memory_id: enc.memory_id,
                mode: ForgetMode::Soft,
                agent_id: brain_core::AgentId::default(),
            })
            .await
            .unwrap();
        assert_eq!(
            sink.appended().len(),
            2,
            "AlreadyTombstoned must not emit a WAL record"
        );
    });
}

/// Test 11: LINK appends a WAL record before the redb edge insert.
#[test]
fn link_appends_wal_before_redb_commit() {
    run_in_glommio(|| async {
        let sink = Arc::new(RecordingWalSink::new());
        let sink_for_writer: Arc<dyn WalSink> = sink.clone();
        let (writer, _metadata) = fixture_with_sink(sink_for_writer);

        // Two memories so LINK has valid endpoints.
        let a = writer
            .submit_encode(make_encode_op([20; 16], "alpha"))
            .await
            .unwrap();
        let b = writer
            .submit_encode(make_encode_op([21; 16], "beta"))
            .await
            .unwrap();

        writer
            .submit_link(LinkOp {
                request_id: RequestId::from([22u8; 16]),
                source: a.memory_id,
                target: b.memory_id,
                kind: EdgeKind::Caused,
                weight: 0.7,
                agent_id: brain_core::AgentId::default(),
            })
            .await
            .unwrap();

        let recs = sink.appended();
        assert_eq!(recs.len(), 3, "Encode + Encode + Link");
        assert_eq!(recs[2].kind, WalRecordKind::Link);
        match WalPayload::decode(recs[2].kind, &recs[2].payload).unwrap() {
            WalPayload::Link(p) => {
                assert_eq!(p.source, a.memory_id);
                assert_eq!(p.target, b.memory_id);
                assert_eq!(p.edge_kind, EdgeKind::Caused);
                assert!((p.weight - 0.7).abs() < f32::EPSILON);
            }
            other => panic!("expected Link, got {other:?}"),
        }
    });
}

/// Test 12: TXN_COMMIT writes a TxnBegin, then the staged ops, then a
/// TxnCommit — in that exact order. Recovery's TXN buffer applies the
/// whole sequence atomically.
#[test]
fn txn_commit_writes_txn_begin_ops_txn_commit_in_order() {
    run_in_glommio(|| async {
        let sink = Arc::new(RecordingWalSink::new());
        let sink_for_writer: Arc<dyn WalSink> = sink.clone();
        let (writer, _metadata) = fixture_with_sink(sink_for_writer);

        // Reserve two memory ids before building the batch (matches
        // the txn-buffer ergonomics — reserve, build, submit).
        let m1 = writer.reserve_memory_id().await.unwrap();
        let m2 = writer.reserve_memory_id().await.unwrap();

        let batch = TxnBatch {
            memories: vec![
                TxnEncode {
                    memory_id: m1,
                    request_id: RequestId::from([30u8; 16]),
                    request_hash: [0; 32],
                    context_id: ContextId(7),
                    kind: MemoryKind::Episodic,
                    text: "batched-a".into(),
                    vector: [0.0; VECTOR_DIM],
                    salience_initial: 0.5,
                    fingerprint: [0xAA; 16],
                    edges: vec![],
                    created_at_unix_nanos: 1,
                    agent_id: brain_core::AgentId::default(),
                },
                TxnEncode {
                    memory_id: m2,
                    request_id: RequestId::from([31u8; 16]),
                    request_hash: [0; 32],
                    context_id: ContextId(7),
                    kind: MemoryKind::Episodic,
                    text: "batched-b".into(),
                    vector: [0.0; VECTOR_DIM],
                    salience_initial: 0.5,
                    fingerprint: [0xAA; 16],
                    edges: vec![],
                    created_at_unix_nanos: 2,
                    agent_id: brain_core::AgentId::default(),
                },
            ],
            links: vec![],
            unlinks: vec![],
            forgets: vec![],
        };

        writer.submit_batch(batch).await.unwrap();

        let recs = sink.appended();
        assert_eq!(
            recs.len(),
            4,
            "expected TxnBegin + 2 Encodes + TxnCommit, got {}",
            recs.len()
        );
        assert_eq!(recs[0].kind, WalRecordKind::TxnBegin);
        assert_eq!(recs[1].kind, WalRecordKind::Encode);
        assert_eq!(recs[2].kind, WalRecordKind::Encode);
        assert_eq!(recs[3].kind, WalRecordKind::TxnCommit);

        // Both Encode records carry the right memory ids.
        match WalPayload::decode(recs[1].kind, &recs[1].payload).unwrap() {
            WalPayload::Encode(p) => assert_eq!(p.memory_id, m1),
            other => panic!("expected Encode, got {other:?}"),
        }
        match WalPayload::decode(recs[2].kind, &recs[2].payload).unwrap() {
            WalPayload::Encode(p) => assert_eq!(p.memory_id, m2),
            other => panic!("expected Encode, got {other:?}"),
        }

        // The TxnBegin's expected_record_count matches the staged ops
        // (2 encodes, no links/unlinks/forgets).
        match WalPayload::decode(recs[0].kind, &recs[0].payload).unwrap() {
            WalPayload::TxnBegin(p) => assert_eq!(p.expected_record_count, 2),
            other => panic!("expected TxnBegin, got {other:?}"),
        }

        // TxnBegin and TxnCommit share the same txn_id.
        let begin_id = match WalPayload::decode(recs[0].kind, &recs[0].payload).unwrap() {
            WalPayload::TxnBegin(p) => p.txn_id,
            _ => unreachable!(),
        };
        let commit_id = match WalPayload::decode(recs[3].kind, &recs[3].payload).unwrap() {
            WalPayload::TxnCommit(p) => p.txn_id,
            _ => unreachable!(),
        };
        assert_eq!(begin_id, commit_id);
    });
}

/// Test 9: the publish event carries the WAL-assigned LSN, not the
/// EventBus's internal allocator stamp. Subscribe-replay relies on
/// this alignment.
#[test]
fn encode_publish_event_carries_wal_assigned_lsn() {
    run_in_glommio(|| async {
        let sink = Arc::new(RecordingWalSink::new());
        let sink_for_writer: Arc<dyn WalSink> = sink.clone();
        // Bring up an event bus + subscriber to capture the published
        // envelope; only the writer's `with_event_bus` wires it.
        let bus = Arc::new(brain_ops::subscribe::EventBus::default());
        let mut rx = bus.receiver();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("metadata.redb");
        let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
        let (_shared, hnsw_writer) =
            SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
        let writer = Arc::new(
            RealWriterHandle::new(metadata, hnsw_writer)
                .with_wal_sink(sink_for_writer)
                .with_event_bus(bus.clone()),
        );

        writer
            .submit_encode(make_encode_op([9; 16], "lsn-alignment"))
            .await
            .unwrap();

        // The published envelope's LSN equals the WAL-assigned LSN
        // (1, since the recording sink starts there).
        let env = rx.try_recv().expect("expected one event");
        assert_eq!(env.lsn, 1);
        let recs = sink.appended();
        assert_eq!(env.lsn, recs[0].lsn.raw());
        std::mem::forget(dir);
    });
}
