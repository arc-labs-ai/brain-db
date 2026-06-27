//! I/O fault injection chaos test.
//!
//! Models a metadata-sink failure during recovery — the on-disk WAL
//! is healthy, but the downstream sink (e.g. redb) returns an error
//! when we try to apply a record. Verifies that:
//!
//! 1. `recover` propagates the sink error rather than silently
//!    swallowing it.
//! 2. No records past the failure point appear in the sink (no
//!    half-applied state).
//! 3. Repeating the recovery with a non-failing sink (after the
//!    transient fault clears) replays from the durable LSN and ends
//!    in a consistent state.
//!
//! The contract is 's "expected vs unexpected" rule:
//! an I/O error during recovery is *expected* — surfacing it cleanly
//! is the substrate's job; the operator decides how to retry.

#![allow(clippy::cast_possible_truncation)]

use std::fs;
use std::path::Path;

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, RequestId};
use brain_storage::arena::ArenaFile;
use brain_storage::recovery::{recover, MetadataSink, MetadataSinkError};
use brain_storage::wal::{EncodePayload, Lsn, Wal, WalPayload, WalRecord};

const N_RECORDS: u64 = 20;
const VECTOR_DIM: usize = 384;

fn shard_uuid() -> [u8; 16] {
    let mut u = [0u8; 16];
    u[0] = 0xC0;
    u[1] = 0xDE;
    u[15] = 0x42;
    u
}

fn bytes16_from(seed: u64) -> [u8; 16] {
    let lo = seed.to_le_bytes();
    let hi = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo);
    out[8..].copy_from_slice(&hi);
    out
}

fn gen_record(slot: u64) -> WalRecord {
    let payload = EncodePayload {
        memory_id: MemoryId::pack(1, slot, 1),
        request_id: RequestId::from(bytes16_from(slot * 7 + 1)),
        agent_id: AgentId::from(bytes16_from(slot * 11 + 2)),
        namespace_id: brain_core::NamespaceId::SYSTEM,
        context_id: ContextId(slot * 13 + 3),
        kind: MemoryKind::Episodic,
        salience_initial: 0.5,
        embedding_model_fp: bytes16_from(slot * 17 + 4),
        text: format!("slot {slot}"),
        vector: vec![0.5; VECTOR_DIM],
        edges: vec![],
        request_hash: [0; 32],
        response_payload: vec![],
        deduplicate: false,
        occurred_at_unix_nanos: None,
    };
    WalRecord::from_typed(
        Lsn(0),
        0,
        1_700_000_000_000_000_000,
        slot,
        &WalPayload::Encode(payload),
    )
}

fn write_records(wal_dir: &Path) {
    let wal_dir_buf = wal_dir.to_path_buf();
    glommio::LocalExecutorBuilder::default()
        .name("io-fault-wal")
        .spawn(move || async move {
            let wal = Wal::create(&wal_dir_buf, shard_uuid())
                .await
                .expect("Wal::create");
            for slot in 0..N_RECORDS {
                wal.append(gen_record(slot)).await.expect("Wal::append");
            }
            wal.shutdown().await.expect("Wal::shutdown");
        })
        .expect("spawn")
        .join()
        .expect("join");
}

/// Sink that fails on the Nth `apply` call. Mirrors the InMemory
/// shape so successful applies still build up state — the test asserts
/// the partial-state property of recovery on error.
struct FaultingSink {
    fail_at_call: usize,
    calls: usize,
    durable_lsn: u64,
    applied_lsns: Vec<u64>,
}

impl FaultingSink {
    fn new(fail_at_call: usize) -> Self {
        Self {
            fail_at_call,
            calls: 0,
            durable_lsn: 0,
            applied_lsns: Vec::new(),
        }
    }
}

impl MetadataSink for FaultingSink {
    fn durable_lsn(&self) -> u64 {
        self.durable_lsn
    }

    fn apply(
        &mut self,
        lsn: u64,
        _ts: u64,
        _payload: &WalPayload,
    ) -> Result<(), MetadataSinkError> {
        self.calls += 1;
        if self.calls == self.fail_at_call {
            return Err(MetadataSinkError::Transient(format!(
                "synthetic I/O fault at call #{}",
                self.calls
            )));
        }
        self.applied_lsns.push(lsn);
        Ok(())
    }
}

fn run_recover(
    wal_dir: &Path,
    arena_path: &Path,
    sink: &mut dyn MetadataSink,
) -> Result<(), String> {
    let mut arena = ArenaFile::open(arena_path, shard_uuid(), 256).expect("arena open");
    match recover(&mut arena, wal_dir, shard_uuid(), sink) {
        Ok(_summary) => Ok(()),
        Err(e) => Err(format!("recover: {e:?}")),
    }
}

#[test]
fn recovery_propagates_sink_error_at_fixed_call() {
    let tmp = tempfile::tempdir().expect("tmp");
    let arena_path = tmp.path().join("arena.bin");
    let wal_dir = tmp.path().join("wal");
    fs::create_dir_all(&wal_dir).expect("mkdir");
    let _arena = ArenaFile::open(&arena_path, shard_uuid(), 256).expect("arena pre-create");
    write_records(&wal_dir);

    let mut sink = FaultingSink::new(5);
    let result = run_recover(&wal_dir, &arena_path, &mut sink);

    assert!(result.is_err(), "recovery should propagate the sink error");
    assert_eq!(
        sink.applied_lsns.len(),
        4,
        "sink saw exactly 4 successful applies before the 5th-call failure (got: {:?})",
        sink.applied_lsns,
    );
}

#[test]
fn recovery_replays_cleanly_after_sink_recovers() {
    let tmp = tempfile::tempdir().expect("tmp");
    let arena_path = tmp.path().join("arena.bin");
    let wal_dir = tmp.path().join("wal");
    fs::create_dir_all(&wal_dir).expect("mkdir");
    let _arena = ArenaFile::open(&arena_path, shard_uuid(), 256).expect("arena pre-create");
    write_records(&wal_dir);

    // First attempt fails partway.
    let mut failing = FaultingSink::new(10);
    let _ = run_recover(&wal_dir, &arena_path, &mut failing);
    assert!(
        failing.applied_lsns.len() < N_RECORDS as usize,
        "failing sink should not have applied all records"
    );

    // Second attempt with a non-failing sink, starting from zero
    // (operator's "I rebuilt the sink, retry" path). The WAL is
    // unchanged on disk so replay should run to completion.
    let mut clean = FaultingSink::new(usize::MAX);
    let result = run_recover(&wal_dir, &arena_path, &mut clean);
    assert!(
        result.is_ok(),
        "fresh sink replay should succeed: {result:?}"
    );
    assert_eq!(
        clean.applied_lsns.len(),
        N_RECORDS as usize,
        "fresh sink saw all {} records on replay",
        N_RECORDS
    );
}

#[test]
fn first_call_failure_yields_zero_partial_state() {
    let tmp = tempfile::tempdir().expect("tmp");
    let arena_path = tmp.path().join("arena.bin");
    let wal_dir = tmp.path().join("wal");
    fs::create_dir_all(&wal_dir).expect("mkdir");
    let _arena = ArenaFile::open(&arena_path, shard_uuid(), 256).expect("arena pre-create");
    write_records(&wal_dir);

    let mut sink = FaultingSink::new(1);
    let result = run_recover(&wal_dir, &arena_path, &mut sink);

    assert!(result.is_err());
    assert!(
        sink.applied_lsns.is_empty(),
        "sink saw no successful applies (got: {:?})",
        sink.applied_lsns,
    );
}
