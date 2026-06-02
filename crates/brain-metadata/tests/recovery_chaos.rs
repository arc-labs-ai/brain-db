//! Chaos tests for recovery of typed-relation payloads
//! under partial WAL writes, replay idempotency, and corruption
//! diagnostics on a tombstone without its sidecar.
//!
//! These tests deliberately corrupt or truncate the WAL on disk
//! between write and recover so the recovery path's CRC + sidecar
//! invariants are exercised in the same way a real mid-fsync crash
//! would surface them.

use std::fs::OpenOptions;
use std::path::PathBuf;

use brain_core::{
    AgentId, ContextId, EdgeKind, EdgeKindRef, EdgeOrigin, EntityId, MemoryId, MemoryKind, NodeRef,
    RelationId, RelationTypeId, RequestId,
};
use brain_metadata::tables::edge::EDGES_TABLE;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::relation::{RELATION_BY_EVIDENCE_TABLE, RELATION_METADATA_TABLE};
use brain_metadata::MetadataDb;
use brain_storage::arena::file::ArenaFile;
use brain_storage::recovery::{recover, MetadataSinkError, RecoveryError};
use brain_storage::wal::payload::{
    EncodePayload, LinkPayload, RelationLinkPayload, RelationTombstonePayload, WalPayload,
};
use brain_storage::wal::record::{Lsn, WalRecord};
use brain_storage::wal::wal::Wal;
use redb::ReadableTable;

const SHARD_UUID: [u8; 16] = [0xCD; 16];
const ARENA_CAPACITY_SLOTS: u64 = 64;
const T0: u64 = 1_700_000_000_000_000_000;

// ---------------------------------------------------------------------------
// Test environment.
// ---------------------------------------------------------------------------

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

    fn write_wal_records(&self, records: Vec<WalRecord>) {
        let wal_dir = self.wal_dir.clone();
        glommio::LocalExecutorBuilder::default()
            .name("chaos-wal")
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

    /// Truncate the first WAL segment to `total_bytes` (including the
    /// 4 KB segment header). Simulates a crash where the fsync did not
    /// complete past the truncation point.
    fn truncate_first_segment_to(&self, total_bytes: u64) {
        let path = self.wal_dir.join("0000000000.wal");
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(total_bytes).unwrap();
        let _ = f.sync_all();
    }

    fn first_segment_size(&self) -> u64 {
        let path = self.wal_dir.join("0000000000.wal");
        std::fs::metadata(&path).unwrap().len()
    }
}

fn record(payload: WalPayload, ts: u64) -> WalRecord {
    WalRecord::from_typed(Lsn(0), 0, ts, 0, &payload)
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

fn relid(byte: u8) -> RelationId {
    let mut b = [0u8; 16];
    b[0] = 0xA0;
    b[15] = byte;
    RelationId::from(b)
}

fn ent(byte: u8) -> EntityId {
    let mut b = [0u8; 16];
    b[15] = byte;
    EntityId::from(b)
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
        text: format!("chaos {byte}"),
        vector: vec![0.0; 384],
        edges: Vec::new(),
        request_hash: [byte; 32],
        response_payload: vec![],
        deduplicate: false,
    }
}

fn sample_relation_link(rid_byte: u8) -> RelationLinkPayload {
    RelationLinkPayload {
        relation_id: relid(rid_byte),
        from: NodeRef::Entity(ent(1)),
        to: NodeRef::Entity(ent(2)),
        relation_type_id: RelationTypeId::from(7),
        chain_root: relid(rid_byte),
        confidence: 0.9,
        valid_from_unix_nanos: Some(T0),
        valid_to_unix_nanos: None,
        supersedes: None,
        evidence: vec![mid(10, 1), mid(11, 1)],
        extractor_id: 3,
        is_symmetric: false,
        properties_blob: vec![0xDE, 0xAD, 0xBE, 0xEF],
        agent_id: aid(7),
        relation_type_intern_hint: None,
    }
}

// ---------------------------------------------------------------------------
// Test 1 — kill mid-RelationLink: the truncated record fails CRC, and the
// prior two records replay cleanly. The sidecar must NOT contain the
// partial relation.
// ---------------------------------------------------------------------------

#[test]
fn truncated_relation_link_rejected_prior_records_replay() {
    let env = Env::new();

    let p1 = encode_payload(1, 1);
    let p2 = encode_payload(2, 2);

    // First write only the two Encodes — measure where the WAL
    // ends after them. The third (RelationLink) record then writes
    // into bytes [encodes_end, encodes_end + rl_size]. Truncating
    // to encodes_end + (rl_size / 2) guarantees we cut INSIDE the
    // RelationLink record without touching the prior two.
    env.write_wal_records(vec![
        record(WalPayload::Encode(p1.clone()), T0),
        record(WalPayload::Encode(p2.clone()), T0 + 1),
    ]);
    let encodes_end = env.first_segment_size();
    // Reset WAL and rewrite with all three records.
    std::fs::remove_dir_all(&env.wal_dir).unwrap();
    env.write_wal_records(vec![
        record(WalPayload::Encode(p1.clone()), T0),
        record(WalPayload::Encode(p2.clone()), T0 + 1),
        record(WalPayload::RelationLink(sample_relation_link(0xA1)), T0 + 2),
    ]);
    let full_end = env.first_segment_size();
    assert!(
        full_end > encodes_end,
        "RelationLink record must add bytes (got encodes_end={encodes_end}, full_end={full_end})",
    );
    // Truncate mid-RelationLink: keep `encodes_end + 8` bytes (just
    // past the 2 Encodes + a few bytes of the relation record's
    // header so its CRC is guaranteed to fail).
    env.truncate_first_segment_to(encodes_end + 8);

    let mut arena = env.open_arena();
    let mut meta = env.open_meta();
    let result = recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta);

    // Recovery treats the truncated tail as "no more records" — the
    // CRC check fails and the reader stops, surfacing a report with
    // the 2 prior records replayed and the partial discarded (or
    // returning an error if the truncation lands inside the record
    // header). Either outcome is acceptable; what is NOT acceptable
    // is the partial relation appearing in the sidecar.
    let (report, _alloc) = result.expect("truncated tail: recovery stops, does not panic");
    assert!(
        report.records_replayed >= 2,
        "first two Encode records must replay (got {})",
        report.records_replayed,
    );

    let rtxn = meta.read_txn().unwrap();
    let mems = rtxn.open_table(MEMORIES_TABLE).unwrap();
    assert!(mems.get(&mid(1, 1).to_be_bytes()).unwrap().is_some());
    assert!(mems.get(&mid(2, 1).to_be_bytes()).unwrap().is_some());

    // RELATION_METADATA_TABLE is created lazily on first apply. If
    // the partial RelationLink had landed it would exist; not
    // existing is the strongest possible evidence that the truncated
    // record was rejected before sink dispatch.
    match rtxn.open_table(RELATION_METADATA_TABLE) {
        Ok(sidecar) => {
            assert!(
                sidecar.get(&relid(0xA1).to_bytes()).unwrap().is_none(),
                "partial RelationLink record must NOT land in the sidecar",
            );
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {
            // Best outcome — the table was never created because no
            // relation ever applied successfully.
        }
        Err(e) => panic!("unexpected redb error reading sidecar: {e:?}"),
    }

    // EDGES_TABLE is also created lazily; not existing implies zero
    // edges, which is exactly what we want for this assertion.
    match rtxn.open_table(EDGES_TABLE) {
        Ok(t) => assert_eq!(t.iter().unwrap().count(), 0, "no edges from partial replay"),
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => panic!("unexpected redb error: {e:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 2 — atomicity: a complete RelationLink that has not yet been
// applied to redb must, on recovery, land as a *single* atomic unit —
// edge row + sidecar + evidence index either ALL appear or NONE do.
// We test the "ALL appear" leg directly (the WAL is intact, recovery
// runs); the "NONE" leg is tested by Test 1.
// ---------------------------------------------------------------------------

#[test]
fn complete_relation_link_recovers_atomically() {
    let env = Env::new();

    let rel = sample_relation_link(0xB2);
    let rid_bytes = rel.relation_id.to_bytes();

    env.write_wal_records(vec![record(WalPayload::RelationLink(rel), T0)]);

    let mut arena = env.open_arena();
    let mut meta = env.open_meta();
    let (report, _alloc) =
        recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).expect("recover");
    assert_eq!(report.records_replayed, 1);

    let rtxn = meta.read_txn().unwrap();

    // Sidecar row.
    let sidecar = rtxn.open_table(RELATION_METADATA_TABLE).unwrap();
    assert!(
        sidecar.get(&rid_bytes).unwrap().is_some(),
        "sidecar present"
    );

    // Unified edge row (one forward — asymmetric typed relation, no
    // mirror).
    let edges = rtxn.open_table(EDGES_TABLE).unwrap();
    assert_eq!(edges.iter().unwrap().count(), 1, "one forward edge");

    // Evidence index — two evidence memories in `sample_relation_link`.
    let by_ev = rtxn.open_table(RELATION_BY_EVIDENCE_TABLE).unwrap();
    assert_eq!(by_ev.iter().unwrap().count(), 2, "evidence index entries");
}

// ---------------------------------------------------------------------------
// Test 3 — replay idempotency: applying the same WAL twice produces the
// same end state. Counts unchanged on the second pass, no duplicate
// evidence rows.
// ---------------------------------------------------------------------------

#[test]
fn replay_idempotent_relation_link_no_duplicates() {
    let env = Env::new();

    let rel = sample_relation_link(0xC3);
    env.write_wal_records(vec![
        record(WalPayload::Encode(encode_payload(20, 5)), T0),
        record(WalPayload::RelationLink(rel.clone()), T0 + 1),
        record(
            WalPayload::Link(LinkPayload {
                source: NodeRef::Memory(mid(20, 1)),
                target: NodeRef::Memory(mid(21, 1)),
                edge_kind: EdgeKindRef::Builtin(EdgeKind::Caused),
                weight: 0.5,
                origin: EdgeOrigin::Explicit,
            }),
            T0 + 2,
        ),
    ]);

    // First recover.
    let counts_after_first = {
        let mut arena = env.open_arena();
        let mut meta = env.open_meta();
        let (report, _) =
            recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).expect("first recover");
        assert_eq!(report.records_replayed, 3);

        let rtxn = meta.read_txn().unwrap();
        let edges = rtxn
            .open_table(EDGES_TABLE)
            .unwrap()
            .iter()
            .unwrap()
            .count();
        let sidecar = rtxn
            .open_table(RELATION_METADATA_TABLE)
            .unwrap()
            .iter()
            .unwrap()
            .count();
        let by_ev = rtxn
            .open_table(RELATION_BY_EVIDENCE_TABLE)
            .unwrap()
            .iter()
            .unwrap()
            .count();
        (edges, sidecar, by_ev)
    };

    // Second recover on the same metadata file + same WAL. Without
    // a CheckpointEnd in the WAL, durable_lsn stays at 0 and every
    // record re-applies — but redb's `insert` overwrites by key so
    // the final state must be byte-identical (no duplicate rows).
    {
        let mut arena = env.open_arena();
        let mut meta = env.open_meta();
        let (report, _) =
            recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta).expect("second recover");
        assert_eq!(report.records_replayed, 3, "all 3 records re-apply");
    }

    let counts_after_second = {
        let meta = env.open_meta();
        let rtxn = meta.read_txn().unwrap();
        let edges = rtxn
            .open_table(EDGES_TABLE)
            .unwrap()
            .iter()
            .unwrap()
            .count();
        let sidecar = rtxn
            .open_table(RELATION_METADATA_TABLE)
            .unwrap()
            .iter()
            .unwrap()
            .count();
        let by_ev = rtxn
            .open_table(RELATION_BY_EVIDENCE_TABLE)
            .unwrap()
            .iter()
            .unwrap()
            .count();
        (edges, sidecar, by_ev)
    };

    assert_eq!(
        counts_after_first, counts_after_second,
        "second-pass counts must equal first-pass counts (idempotent replay)",
    );
}

// ---------------------------------------------------------------------------
// Test 4 — tombstone without sidecar errors as Corruption. Wave 3b
// promised: "Tombstone replay errors as Corruption if the sidecar is
// missing." Verify that exact error path through the full recover()
// pipeline, not just the in-memory sink.
// ---------------------------------------------------------------------------

#[test]
fn tombstone_without_sidecar_errors_as_corruption() {
    let env = Env::new();

    let ghost = relid(0xFF);
    env.write_wal_records(vec![record(
        WalPayload::RelationTombstone(RelationTombstonePayload {
            relation_id: ghost,
            reason: "no prior create".into(),
            at_unix_nanos: T0,
            agent_id: aid(0xAA),
        }),
        T0,
    )]);

    let mut arena = env.open_arena();
    let mut meta = env.open_meta();
    let err = recover(&mut arena, &env.wal_dir, SHARD_UUID, &mut meta)
        .expect_err("tombstone-without-sidecar must error");

    match err {
        RecoveryError::SinkError {
            source: MetadataSinkError::Corruption(msg),
            ..
        } => {
            assert!(
                msg.contains("relation_tombstone") && msg.contains("missing sidecar"),
                "expected Corruption message mentioning the missing sidecar, got: {msg}",
            );
        }
        other => panic!("expected SinkError::Corruption, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Helper smoke test — ensures the truncation helper actually shrinks the
// file. If a future redb / glommio upgrade changes the on-disk segment
// naming, this catches it before the chaos tests silently degrade into
// no-ops.
// ---------------------------------------------------------------------------

#[test]
fn truncation_helper_shrinks_first_segment() {
    let env = Env::new();
    env.write_wal_records(vec![record(WalPayload::Encode(encode_payload(1, 1)), T0)]);
    let before = env.first_segment_size();
    assert!(before > 4096, "segment must be larger than the header");
    env.truncate_first_segment_to(4096);
    let after = env.first_segment_size();
    assert_eq!(after, 4096);
}
