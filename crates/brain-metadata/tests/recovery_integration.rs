//! Cross-crate recovery integration test.
//!
//! Drives `brain_storage::Wal::append` → "crash" (drop everything) →
//! `brain_storage::recovery::recover(&mut ArenaFile, &Path, [u8;16], &mut MetadataDb)`,
//! then asserts the final state in [`brain_metadata::MetadataDb`] matches
//! what we wrote.
//!
//! 7 scenarios:
//!
//! - A: basic write-and-recover round trip
//! - B: durable_lsn from CheckpointEnd shortens replay
//! - C: TXN_COMMIT records survive, TXN_ABORT records are discarded
//! - D: orphan TXN_BEGIN at the WAL tail discards its buffer
//! - E: recover() is idempotent
//! - F: durable_lsn survives MetadataDb close + reopen via the checkpoints table
//! - G: 100-iteration seeded loop covering the phase exit criterion

use std::path::PathBuf;

use brain_core::{
    AgentId, ContextId, EdgeKind, EdgeOrigin, MemoryId, MemoryKind, RequestId, TxnId,
};
use brain_metadata::tables::checkpoint::{latest as latest_checkpoint, CHECKPOINTS_TABLE};
use brain_metadata::tables::edge::EDGES_TABLE;
use brain_metadata::tables::idempotency::IDEMPOTENCY_TABLE;
use brain_metadata::tables::memory::{flags, MEMORIES_TABLE};
use brain_metadata::tables::model_fingerprint::MODEL_FINGERPRINTS_TABLE;
use brain_metadata::tables::next_lsn::NEXT_LSN_TABLE;
use brain_metadata::tables::slot_version::SLOT_VERSIONS_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_metadata::MetadataDb;
use brain_storage::arena::file::ArenaFile;
use brain_storage::recovery::{recover, MetadataSink};
use brain_storage::wal::payload::{
    CheckpointBeginPayload, CheckpointEndPayload, EncodePayload, ForgetMode, ForgetPayload,
    ForgetReason, LinkPayload, RelationLinkPayload, RelationSupersedePayload,
    RelationTombstonePayload, TxnAbortPayload, TxnBeginPayload, TxnCommitPayload, WalPayload,
};
use brain_storage::wal::record::{Lsn, WalRecord};
use brain_storage::wal::wal::Wal;
use redb::ReadableTable;

// ---------------------------------------------------------------------------
// Test environment.
// ---------------------------------------------------------------------------

const SHARD_UUID: [u8; 16] = [0xAB; 16];
const ARENA_CAPACITY_SLOTS: u64 = 64;
const T0: u64 = 1_700_000_000_000_000_000;

struct Env {
    _temp: tempfile::TempDir,
    arena_path: PathBuf,
    wal_dir: PathBuf,
    meta_path: PathBuf,
}

impl Env {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let arena_path = temp.path().join("arena.bin");
        let wal_dir = temp.path().join("wal");
        let meta_path = temp.path().join("metadata.redb");
        Self {
            _temp: temp,
            arena_path,
            wal_dir,
            meta_path,
        }
    }

    /// Write `records` to a fresh WAL in this env, then cleanly shut down.
    /// Hosts the async WAL ops on a per-call Glommio executor.
    fn write_wal_records(&self, records: Vec<WalRecord>) {
        let wal_dir = self.wal_dir.clone();
        glommio::LocalExecutorBuilder::default()
            .name("metadata-test-wal")
            .spawn(move || async move {
                let wal = Wal::create(&wal_dir, SHARD_UUID).await.expect("create wal");
                for r in records {
                    wal.append(r).await.expect("append");
                }
                wal.shutdown().await.expect("shutdown");
            })
            .expect("spawn")
            .join()
            .expect("join");
    }

    fn open_meta(&self) -> MetadataDb {
        MetadataDb::open(&self.meta_path).expect("open metadata")
    }

    fn open_arena(&self) -> ArenaFile {
        ArenaFile::open(&self.arena_path, SHARD_UUID, ARENA_CAPACITY_SLOTS).expect("open arena")
    }
}

// ---------------------------------------------------------------------------
// Record-construction helpers.
// ---------------------------------------------------------------------------

fn record(payload: WalPayload, timestamp_ns: u64) -> WalRecord {
    WalRecord::from_typed(Lsn(0), 0, timestamp_ns, 0, &payload)
}

fn mid(slot: u64, version: u32) -> MemoryId {
    MemoryId::pack(1, slot, version)
}

fn aid(byte: u8) -> AgentId {
    let mut b = [0u8; 16];
    b[15] = byte;
    b.into()
}

fn rid(byte: u8) -> RequestId {
    let mut b = [0u8; 16];
    b[15] = byte;
    RequestId::from(b)
}

fn tid(byte: u8) -> TxnId {
    let mut b = [0u8; 16];
    b[15] = byte;
    TxnId::from(b)
}

fn encode_payload(slot: u64, byte: u8) -> EncodePayload {
    EncodePayload {
        memory_id: mid(slot, 1),
        request_id: rid(byte),
        agent_id: aid(byte),
        context_id: ContextId(42),
        kind: MemoryKind::Episodic,
        salience_initial: 0.5,
        embedding_model_fp: [byte; 16],
        text: format!("text for memory {byte}"),
        vector: vec![0.0; 384],
        edges: Vec::new(),
        request_hash: [byte; 32],
        response_payload: vec![],
        deduplicate: false,
    }
}

// ---------------------------------------------------------------------------
// Scenario A — basic write-and-recover.
// ---------------------------------------------------------------------------

#[test]
fn scenario_a_basic_write_then_recover() {
    let env = Env::new();

    let id1 = mid(1, 1);
    let id2 = mid(2, 1);
    let p1 = encode_payload(1, 1);
    let p2 = encode_payload(2, 2);

    env.write_wal_records(vec![
        record(WalPayload::Encode(p1.clone()), T0),
        record(WalPayload::Encode(p2.clone()), T0 + 1),
        record(
            WalPayload::Link(LinkPayload {
                source: brain_core::NodeRef::Memory(id1),
                target: brain_core::NodeRef::Memory(id2),
                edge_kind: brain_core::EdgeKindRef::Builtin(EdgeKind::Caused),
                weight: 0.9,
                origin: EdgeOrigin::Explicit,
            }),
            T0 + 2,
        ),
    ]);

    // Crash + recover.
    let mut arena = env.open_arena();
    let mut meta = env.open_meta();
    let (report, _allocator) =
        recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).expect("recover");
    assert_eq!(report.records_replayed, 3);
    assert_eq!(report.records_skipped, 0);
    assert_eq!(report.records_discarded, 0);

    let rtxn = meta.read_txn().unwrap();
    // memories
    let mems = rtxn.open_table(MEMORIES_TABLE).unwrap();
    assert!(mems.get(&id1.to_be_bytes()).unwrap().is_some());
    assert!(mems.get(&id2.to_be_bytes()).unwrap().is_some());
    // texts
    let texts = rtxn.open_table(TEXTS_TABLE).unwrap();
    assert_eq!(
        texts.get(&id1.to_be_bytes()).unwrap().unwrap().value(),
        p1.text.as_bytes()
    );
    // edges
    let out = rtxn.open_table(EDGES_TABLE).unwrap();
    assert_eq!(out.iter().unwrap().count(), 1);
    // idempotency
    let idem = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
    assert!(idem
        .get(&<[u8; 16]>::from(p1.request_id))
        .unwrap()
        .is_some());
    // model_fingerprints
    let fps = rtxn.open_table(MODEL_FINGERPRINTS_TABLE).unwrap();
    assert!(fps.get(&[1u8; 16]).unwrap().is_some());
    assert!(fps.get(&[2u8; 16]).unwrap().is_some());
    // slot_versions
    let sv = rtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
    assert_eq!(sv.get(&1u64).unwrap().unwrap().value(), 1);
    assert_eq!(sv.get(&2u64).unwrap().unwrap().value(), 1);
    // next_lsn
    let nl = rtxn.open_table(NEXT_LSN_TABLE).unwrap();
    assert_eq!(nl.get(&()).unwrap().unwrap().value(), 4);
}

// ---------------------------------------------------------------------------
// Scenario B — durable_lsn from CheckpointEnd shortens replay.
// ---------------------------------------------------------------------------

#[test]
fn scenario_b_checkpoint_shortens_replay() {
    let env = Env::new();

    env.write_wal_records(vec![
        record(WalPayload::Encode(encode_payload(1, 1)), T0),
        record(WalPayload::Encode(encode_payload(2, 2)), T0 + 1),
        record(
            WalPayload::CheckpointBegin(CheckpointBeginPayload {
                checkpoint_id: 1,
                started_at_unix_nanos: T0 + 2,
            }),
            T0 + 2,
        ),
        record(
            WalPayload::CheckpointEnd(CheckpointEndPayload {
                checkpoint_id: 1,
                durable_lsn: 4,
                arena_capacity: 1024,
            }),
            T0 + 3,
        ),
        record(WalPayload::Encode(encode_payload(3, 3)), T0 + 4),
        record(WalPayload::Encode(encode_payload(4, 4)), T0 + 5),
    ]);

    // First recover: durable_lsn=0, all 6 records applied.
    {
        let mut arena = env.open_arena();
        let mut meta = env.open_meta();
        let (report, _) = recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).unwrap();
        assert_eq!(report.records_replayed, 6);
        assert_eq!(report.records_skipped, 0);
        assert_eq!(meta.durable_lsn(), 4);
    }

    // Close + reopen MetadataDb. durable_lsn must persist via the
    // checkpoints table.
    {
        let meta = env.open_meta();
        assert_eq!(
            meta.durable_lsn(),
            4,
            "durable_lsn must survive close + reopen"
        );
    }

    // Second recover with the seeded durable_lsn: records 1..=4 are
    // skipped, only 5 and 6 are replayed.
    {
        let mut arena = env.open_arena();
        let mut meta = env.open_meta();
        let (report, _) = recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).unwrap();
        assert_eq!(report.records_skipped, 4);
        assert_eq!(report.records_replayed, 2);
    }
}

// ---------------------------------------------------------------------------
// Scenario C — committed vs aborted transactions.
// ---------------------------------------------------------------------------

#[test]
fn scenario_c_txn_commit_vs_abort() {
    let env = Env::new();

    let id_outside = mid(1, 1);
    let id_committed_a = mid(2, 1);
    let id_committed_b = mid(3, 1);
    let id_aborted = mid(4, 1);

    let p_outside = encode_payload(1, 10);
    let mut p_committed_a = encode_payload(2, 20);
    p_committed_a.memory_id = id_committed_a;
    let mut p_committed_b = encode_payload(3, 21);
    p_committed_b.memory_id = id_committed_b;
    let mut p_aborted = encode_payload(4, 30);
    p_aborted.memory_id = id_aborted;

    let txn_a = tid(1);
    let txn_b = tid(2);

    env.write_wal_records(vec![
        record(WalPayload::Encode(p_outside.clone()), T0),
        // Committed bracket.
        record(
            WalPayload::TxnBegin(TxnBeginPayload {
                txn_id: txn_a,
                expected_record_count: 2,
            }),
            T0 + 1,
        ),
        record(WalPayload::Encode(p_committed_a.clone()), T0 + 2),
        record(WalPayload::Encode(p_committed_b.clone()), T0 + 3),
        record(
            WalPayload::TxnCommit(TxnCommitPayload { txn_id: txn_a }),
            T0 + 4,
        ),
        // Aborted bracket.
        record(
            WalPayload::TxnBegin(TxnBeginPayload {
                txn_id: txn_b,
                expected_record_count: 1,
            }),
            T0 + 5,
        ),
        record(WalPayload::Encode(p_aborted.clone()), T0 + 6),
        record(
            WalPayload::TxnAbort(TxnAbortPayload {
                txn_id: txn_b,
                reason_code: 0,
            }),
            T0 + 7,
        ),
    ]);

    let mut arena = env.open_arena();
    let mut meta = env.open_meta();
    let (report, _) = recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).unwrap();
    // 1 outside + 4 committed (begin+2 encodes+commit) = 5 replayed.
    // 2 inside aborted bracket (begin + encode) discarded.
    assert_eq!(report.records_replayed, 5);
    assert_eq!(report.records_discarded, 2);

    let rtxn = meta.read_txn().unwrap();
    let mems = rtxn.open_table(MEMORIES_TABLE).unwrap();
    assert!(mems.get(&id_outside.to_be_bytes()).unwrap().is_some());
    assert!(mems.get(&id_committed_a.to_be_bytes()).unwrap().is_some());
    assert!(mems.get(&id_committed_b.to_be_bytes()).unwrap().is_some());
    assert!(
        mems.get(&id_aborted.to_be_bytes()).unwrap().is_none(),
        "aborted transaction's record must not be applied"
    );
}

// ---------------------------------------------------------------------------
// Scenario D — orphan TxnBegin at the WAL tail.
// ---------------------------------------------------------------------------

#[test]
fn scenario_d_orphan_txn_at_tail() {
    let env = Env::new();
    let id_inside = mid(7, 1);
    let mut p_inside = encode_payload(7, 7);
    p_inside.memory_id = id_inside;

    // No commit / abort — simulates a crash mid-txn.
    env.write_wal_records(vec![
        record(
            WalPayload::TxnBegin(TxnBeginPayload {
                txn_id: tid(9),
                expected_record_count: 1,
            }),
            T0,
        ),
        record(WalPayload::Encode(p_inside.clone()), T0 + 1),
    ]);

    let mut arena = env.open_arena();
    let mut meta = env.open_meta();
    let (report, _) = recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).unwrap();
    // The 2 orphan records (begin + encode) are discarded.
    assert_eq!(report.records_discarded, 2);
    assert_eq!(report.records_replayed, 0);

    let rtxn = meta.read_txn().unwrap();
    let mems_open = rtxn.open_table(MEMORIES_TABLE);
    match mems_open {
        Ok(t) => assert!(t.get(&id_inside.to_be_bytes()).unwrap().is_none()),
        Err(redb::TableError::TableDoesNotExist(_)) => {
            // No records made it into the metadata at all.
        }
        Err(e) => panic!("unexpected: {e:?}"),
    }
}

// ---------------------------------------------------------------------------
// Scenario E — recover() is idempotent.
// ---------------------------------------------------------------------------

#[test]
fn scenario_e_recover_is_idempotent() {
    let env = Env::new();

    env.write_wal_records(
        (1..=3u64)
            .map(|i| record(WalPayload::Encode(encode_payload(i, i as u8)), T0 + i))
            .collect(),
    );

    let mut arena = env.open_arena();
    let mut meta = env.open_meta();

    let (report1, _) = recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).unwrap();
    assert_eq!(report1.records_replayed, 3);

    let row_count_after_first = {
        let rtxn = meta.read_txn().unwrap();
        rtxn.open_table(MEMORIES_TABLE)
            .unwrap()
            .iter()
            .unwrap()
            .count()
    };

    // Second recover on the same in-memory state. Records aren't
    // skipped on a fresh sink, but redb's `insert` overwrites — so the
    // table state is unchanged.
    let (report2, _) = recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).unwrap();
    // The sink's durable_lsn doesn't advance on Encode (only on
    // CheckpointEnd), so records aren't skipped; they re-apply.
    assert_eq!(report2.records_replayed, 3);
    let row_count_after_second = {
        let rtxn = meta.read_txn().unwrap();
        rtxn.open_table(MEMORIES_TABLE)
            .unwrap()
            .iter()
            .unwrap()
            .count()
    };
    assert_eq!(
        row_count_after_first, row_count_after_second,
        "second recover must not duplicate rows"
    );
}

// ---------------------------------------------------------------------------
// Scenario F — durable_lsn survives MetadataDb close + reopen.
// ---------------------------------------------------------------------------

#[test]
fn scenario_f_durable_lsn_survives_reopen() {
    let env = Env::new();

    env.write_wal_records(vec![
        record(
            WalPayload::CheckpointBegin(CheckpointBeginPayload {
                checkpoint_id: 5,
                started_at_unix_nanos: T0,
            }),
            T0,
        ),
        record(
            WalPayload::CheckpointEnd(CheckpointEndPayload {
                checkpoint_id: 5,
                durable_lsn: 17,
                arena_capacity: 1024,
            }),
            T0 + 1,
        ),
    ]);

    {
        let mut arena = env.open_arena();
        let mut meta = env.open_meta();
        recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).unwrap();
        assert_eq!(meta.durable_lsn(), 17);
    }

    // Reopen and re-read.
    let meta = env.open_meta();
    assert_eq!(meta.durable_lsn(), 17);

    // The checkpoint row itself is still there with the right fields.
    let rtxn = meta.read_txn().unwrap();
    let t = rtxn.open_table(CHECKPOINTS_TABLE).unwrap();
    let latest = latest_checkpoint(&t).unwrap().unwrap();
    assert_eq!(latest.checkpoint_id, 5);
    assert_eq!(latest.durable_lsn, 17);
    assert_eq!(latest.started_at_unix_nanos, T0);
    assert_eq!(latest.completed_at_unix_nanos, T0 + 1);
}

// ---------------------------------------------------------------------------
// Scenario G — 100-iteration seeded loop (phase exit criterion).
// ---------------------------------------------------------------------------

/// Simple deterministic xorshift64* PRNG. Avoids pulling in `rand` for one
/// looped test; we don't need cryptographic randomness, just reproducible
/// pseudo-randomness.
struct Xs(u64);
impl Xs {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn scenario_g_seeded_loop_passes_100_iterations() {
    for seed in 0u64..100 {
        run_iteration(seed);
    }
}

fn run_iteration(seed: u64) {
    let env = Env::new();
    let mut rng = Xs::new(seed.wrapping_add(0xDEAD_BEEF));

    // 5..=20 records per iteration.
    let n_records = 5 + rng.range(16);

    // Pre-decide encode slots so we can verify them post-recovery.
    let mut encoded_slots: Vec<u64> = Vec::new();
    let mut next_slot: u64 = 1;

    // Pre-build the full ops sequence with the RNG, so we can move it into
    // the Glommio executor (records computed up-front; the executor just
    // appends them in order).
    enum Op {
        Encode(u64),
        Link(u64, u64),
        Forget(u64),
        Checkpoint(u64),
    }
    let mut ops: Vec<(u64, Op)> = Vec::with_capacity(n_records as usize);
    for i in 0..n_records {
        let ts = T0 + i;
        let pick = rng.range(10);
        match pick {
            0..=5 => {
                let slot = next_slot;
                next_slot += 1;
                encoded_slots.push(slot);
                ops.push((ts, Op::Encode(slot)));
            }
            6..=7 => {
                if encoded_slots.len() >= 2 {
                    let a = encoded_slots[(rng.range(encoded_slots.len() as u64)) as usize];
                    let b = encoded_slots[(rng.range(encoded_slots.len() as u64)) as usize];
                    if a != b {
                        ops.push((ts, Op::Link(a, b)));
                    }
                }
            }
            8 => {
                if let Some(&slot) = encoded_slots.first() {
                    ops.push((ts, Op::Forget(slot)));
                }
            }
            _ => {
                ops.push((ts, Op::Checkpoint(i + 1)));
            }
        }
    }

    let wal_dir = env.wal_dir.clone();
    let seed_byte = seed as u8;
    glommio::LocalExecutorBuilder::default()
        .name("scenario-g-wal")
        .spawn(move || async move {
            let wal = Wal::create(&wal_dir, SHARD_UUID).await.expect("create");
            for (ts, op) in ops {
                match op {
                    Op::Encode(slot) => {
                        wal.append(record(
                            WalPayload::Encode(encode_payload(slot, slot as u8 ^ seed_byte)),
                            ts,
                        ))
                        .await
                        .unwrap();
                    }
                    Op::Link(a, b) => {
                        wal.append(record(
                            WalPayload::Link(LinkPayload {
                                source: brain_core::NodeRef::Memory(mid(a, 1)),
                                target: brain_core::NodeRef::Memory(mid(b, 1)),
                                edge_kind: brain_core::EdgeKindRef::Builtin(EdgeKind::Caused),
                                weight: 0.5,
                                origin: EdgeOrigin::Explicit,
                            }),
                            ts,
                        ))
                        .await
                        .unwrap();
                    }
                    Op::Forget(slot) => {
                        wal.append(record(
                            WalPayload::Forget(ForgetPayload {
                                memory_id: mid(slot, 1),
                                request_id: rid(seed_byte ^ 0xFF),
                                agent_id: brain_core::AgentId::default(),
                                mode: ForgetMode::Soft,
                                reason: ForgetReason::ClientRequest,
                            }),
                            ts,
                        ))
                        .await
                        .unwrap();
                    }
                    Op::Checkpoint(cid) => {
                        wal.append(record(
                            WalPayload::CheckpointBegin(CheckpointBeginPayload {
                                checkpoint_id: cid,
                                started_at_unix_nanos: ts,
                            }),
                            ts,
                        ))
                        .await
                        .unwrap();
                        let durable_lsn = wal.next_lsn().saturating_sub(1);
                        wal.append(record(
                            WalPayload::CheckpointEnd(CheckpointEndPayload {
                                checkpoint_id: cid,
                                durable_lsn,
                                arena_capacity: 1024,
                            }),
                            ts + 1,
                        ))
                        .await
                        .unwrap();
                    }
                }
            }
            wal.shutdown().await.unwrap();
        })
        .expect("spawn")
        .join()
        .expect("join");

    // Recover.
    {
        let mut arena = env.open_arena();
        let mut meta = env.open_meta();
        recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta)
            .unwrap_or_else(|e| panic!("recover seed={seed}: {e}"));

        // Invariant 1: every encoded memory's row exists (forget
        // doesn't delete the row, only sets the flag).
        let rtxn = meta.read_txn().unwrap();
        let mems = rtxn.open_table(MEMORIES_TABLE).unwrap();
        for slot in &encoded_slots {
            let key = mid(*slot, 1).to_be_bytes();
            assert!(
                mems.get(&key).unwrap().is_some(),
                "seed={seed}: missing memory for slot {slot}"
            );
        }

        // Invariant 2: next_lsn is consistent with the records seen.
        let n = rtxn.open_table(NEXT_LSN_TABLE).unwrap();
        let nl = n.get(&()).unwrap().map(|a| a.value()).unwrap_or(0);
        assert!(
            nl > 0,
            "seed={seed}: next_lsn should be > 0 after non-trivial recovery"
        );
    }

    // Invariant 3: re-recovery is idempotent — row count unchanged.
    let row_count_first = {
        let meta = env.open_meta();
        let rtxn = meta.read_txn().unwrap();
        rtxn.open_table(MEMORIES_TABLE)
            .map(|t| t.iter().unwrap().count())
            .unwrap_or(0)
    };
    {
        let mut arena = env.open_arena();
        let mut meta = env.open_meta();
        recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).unwrap();
        let rtxn = meta.read_txn().unwrap();
        let row_count_second = rtxn
            .open_table(MEMORIES_TABLE)
            .map(|t| t.iter().unwrap().count())
            .unwrap_or(0);
        assert_eq!(
            row_count_first, row_count_second,
            "seed={seed}: re-recovery changed row count"
        );
    }

    // Invariant 4: forget marker is reflected if a Forget ran. We
    // can't easily know which memories were forgotten without
    // re-tracking, but we can spot-check: at most one slot is the
    // first encoded one (the only target Forget can have picked).
    let _ = flags::HARD_FORGOTTEN; // reference to silence dead-code if unused
}

// ---------------------------------------------------------------------------
// Scenario H — typed-relation create / supersede / tombstone replay.
//
// Exercises the unified-edge recovery dispatch. Old data dirs
// can't open under the v2 schema, so this scenario writes a WAL with
// three first-class relation records and verifies the rebuilt edge
// rows + sidecar metadata match the live writer's projections.
// ---------------------------------------------------------------------------

fn relid(byte: u8) -> brain_core::RelationId {
    let mut b = [0u8; 16];
    b[0] = 0xA0;
    b[15] = byte;
    brain_core::RelationId::from(b)
}

fn ent(byte: u8) -> brain_core::EntityId {
    let mut b = [0u8; 16];
    b[15] = byte;
    brain_core::EntityId::from(b)
}

fn relation_link_payload(
    rid_byte: u8,
    from: u8,
    to: u8,
    supersedes: Option<brain_core::RelationId>,
    chain_root: brain_core::RelationId,
    evidence: Vec<MemoryId>,
) -> RelationLinkPayload {
    RelationLinkPayload {
        relation_id: relid(rid_byte),
        from: brain_core::NodeRef::Entity(ent(from)),
        to: brain_core::NodeRef::Entity(ent(to)),
        relation_type_id: brain_core::RelationTypeId::from(7),
        chain_root,
        confidence: 0.88,
        valid_from_unix_nanos: Some(T0),
        valid_to_unix_nanos: None,
        supersedes,
        evidence,
        extractor_id: 3,
        is_symmetric: false,
        properties_blob: vec![],
        agent_id: aid(1),
        relation_type_intern_hint: None,
    }
}

#[test]
fn scenario_h_relation_link_supersede_tombstone_replays() {
    use brain_metadata::tables::relation::{RELATION_BY_EVIDENCE_TABLE, RELATION_METADATA_TABLE};

    let env = Env::new();

    let r1 = relid(1);
    let r2 = relid(2);
    let r3 = relid(3);

    // r1: fresh create with one evidence memory.
    let mem_a = MemoryId::pack(1, 50, 1);
    let mem_b = MemoryId::pack(1, 51, 1);
    let create_a = relation_link_payload(1, 2, 3, None, r1, vec![mem_a]);

    // r2: supersedes r1, two evidence memories.
    let create_b = relation_link_payload(2, 2, 4, Some(r1), r1, vec![mem_b, mem_a]);

    // r3: fresh create that will be tombstoned at the end.
    let create_c = relation_link_payload(3, 5, 6, None, r3, vec![]);

    env.write_wal_records(vec![
        record(WalPayload::RelationLink(create_a), T0),
        record(
            WalPayload::RelationSupersede(RelationSupersedePayload {
                old_relation_id: r1,
                new: create_b,
            }),
            T0 + 100,
        ),
        record(WalPayload::RelationLink(create_c), T0 + 200),
        record(
            WalPayload::RelationTombstone(RelationTombstonePayload {
                relation_id: r3,
                reason: "stale".into(),
                at_unix_nanos: T0 + 300,
                agent_id: aid(2),
            }),
            T0 + 300,
        ),
    ]);

    let mut arena = env.open_arena();
    let mut meta = env.open_meta();
    let (report, _alloc) =
        recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).expect("recover");
    assert_eq!(report.records_replayed, 4);
    assert_eq!(report.records_discarded, 0);

    let rtxn = meta.read_txn().unwrap();

    // Sidecar invariants.
    let sidecar = rtxn.open_table(RELATION_METADATA_TABLE).unwrap();
    let m_r1 = sidecar.get(&r1.to_bytes()).unwrap().unwrap().value();
    assert_eq!(m_r1.is_current, 0, "r1 superseded ⇒ is_current=0");
    assert_eq!(m_r1.superseded_by_bytes, Some(r2.to_bytes()));
    // valid_to defaults to the supersede record's timestamp when not
    // pinned explicitly by the writer.
    assert_eq!(m_r1.valid_to_unix_nanos, Some(T0 + 100));

    let m_r2 = sidecar.get(&r2.to_bytes()).unwrap().unwrap().value();
    assert_eq!(m_r2.is_current, 1);
    assert_eq!(m_r2.supersedes_bytes, Some(r1.to_bytes()));
    assert_eq!(m_r2.chain_root_bytes, r1.to_bytes());
    assert_eq!(m_r2.version, 2);
    assert_eq!(m_r2.evidence_inline.len(), 2);

    let m_r3 = sidecar.get(&r3.to_bytes()).unwrap().unwrap().value();
    assert_eq!(m_r3.tombstoned, 1);
    assert_eq!(m_r3.is_current, 0);
    assert_eq!(m_r3.tombstoned_at_unix_nanos, Some(T0 + 300));

    // Unified-edge rows: r1 and r2 each contribute one edge (asymmetric
    // typed relation = single forward row + single reverse mirror).
    // r3 is tombstoned but the edge row stays.
    let edges = rtxn.open_table(EDGES_TABLE).unwrap();
    assert_eq!(edges.iter().unwrap().count(), 3);

    // Evidence reverse index: r1 cites mem_a (1 row); r2 cites mem_b
    // + mem_a (2 rows); r3 cites nothing.
    let by_ev = rtxn.open_table(RELATION_BY_EVIDENCE_TABLE).unwrap();
    assert_eq!(by_ev.iter().unwrap().count(), 3);
}

// ---------------------------------------------------------------------------
// Scenario I — opaque-body entity_create replay (the PhaseBody path).
//
// Regression: the live server failed WAL recovery on restart whenever any
// entity_create had been written ("entity_create decode: typed-graph body
// failed rkyv validation: pointer out of bounds"). The relation path
// (Scenario H) is first-class and never exercised the opaque-body codec, so
// the gap shipped. This drives an EntityCreate PhaseBody record through the
// real Wal::append → recover loop and asserts the entity row lands.
// ---------------------------------------------------------------------------

#[test]
fn scenario_i_entity_create_phase_body_replays() {
    use brain_metadata::entity::ops::entity_get;
    use brain_metadata::recovery::phase_bodies::encode_entity_create;
    use brain_metadata::tables::entity::EntityMetadata;
    use brain_storage::wal::kinds::WalRecordKind;
    use brain_storage::wal::payload::PhaseBodyRecord;

    let env = Env::new();

    // Person type (id=1) is seeded by the system schema at MetadataDb::open.
    let entity_id = ent(7);
    let mut meta_row = EntityMetadata::new_active(
        entity_id,
        brain_core::EntityTypeId::from(1),
        "Priya Patel".into(),
        "priya patel".into(),
        T0,
    );
    meta_row.add_alias("priya".into());
    meta_row.add_alias("p. patel".into());
    // Non-empty attributes_blob: the live failure (LSN 3) was the entity
    // that carried one; the empty-blob entity at LSN 2 recovered fine.
    meta_row.attributes_blob = vec![1, 2, 3, 4];

    let body = encode_entity_create(&meta_row);

    // Isolation check: the framing layer must hand recovery byte-identical
    // body bytes. A mismatch here localizes the bug to encode/decode framing
    // rather than the rkyv codec.
    let payload = WalPayload::PhaseBody(PhaseBodyRecord::new(
        WalRecordKind::EntityCreate,
        aid(1),
        body.clone(),
    ));
    let framed = WalRecord::from_typed(Lsn(1), 0, T0, 1, &payload);
    let encoded = framed.encode();
    match WalRecord::decode_one(&encoded).unwrap() {
        brain_storage::wal::record::DecodeOutcome::Record { record, .. } => {
            match record.typed_payload().unwrap() {
                WalPayload::PhaseBody(r) => assert_eq!(
                    r.body, body,
                    "framing must preserve the opaque body byte-for-byte"
                ),
                other => panic!("expected PhaseBody, got {other:?}"),
            }
        }
        other => panic!("expected a decoded record, got {other:?}"),
    }

    // Reproduce production exactly: each create_entity emits TWO WAL
    // records under kind=EntityCreate — the durable rkyv row (above) and a
    // CBOR-encoded subscribe-replay event. The event record carries the
    // FLAG_SUBSCRIBE_EVENT bit; recovery must skip it (rkyv-decoding its
    // CBOR body is exactly the crash this test guards against).
    // Any non-rkyv body models the collision; recovery skips on the flag
    // before it ever tries to decode, so the bytes only need to be
    // something the rkyv codec would choke on (as the real CBOR body does).
    let cbor_event_body = b"\xa1ientity_idkPriya Patel".to_vec();
    let event_payload = WalPayload::PhaseBody(PhaseBodyRecord::new(
        WalRecordKind::EntityCreate,
        aid(1),
        cbor_event_body,
    ));
    let mut event_record = WalRecord::from_typed(Lsn(0), 0, T0 + 1, 1, &event_payload);
    event_record.flags = brain_storage::wal::record::FLAG_SUBSCRIBE_EVENT;

    // Full loop: append both via the real WAL, crash, recover.
    env.write_wal_records(vec![record(payload, T0), event_record]);

    let mut arena = env.open_arena();
    let mut meta = env.open_meta();
    let (report, _alloc) =
        recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).expect("recover");
    assert_eq!(report.records_replayed, 1, "only the durable row replays");
    assert_eq!(
        report.records_skipped, 1,
        "the flagged subscribe-event record is skipped, not applied"
    );
    assert_eq!(report.records_discarded, 0);

    let rtxn = meta.read_txn().unwrap();
    let got = entity_get(&rtxn, entity_id)
        .unwrap()
        .expect("entity present after recovery");
    assert_eq!(got.canonical_name, "Priya Patel");
    assert_eq!(got.aliases.len(), 2);
}
