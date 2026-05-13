//! WAL-based recovery driver.
//!
//! See `spec/05_storage_arena_wal/08_recovery.md` (algorithm),
//! `09_checkpointing.md` §2–3 (durable_lsn semantics), and
//! `spec/15_failure_recovery/02_crash_recovery.md` §§4–6.
//!
//! [`recover`] is the entry point. The caller supplies:
//!
//! - An already-opened [`ArenaFile`] for the shard.
//! - The WAL directory.
//! - The shard's storage UUID.
//! - A [`MetadataSink`] implementation (the redb-backed real impl lives
//!   in `brain-metadata`; an in-memory [`InMemoryMetadataSink`] is
//!   provided here for tests).
//!
//! The driver:
//!
//! 1. Opens a [`WalReader`] over `wal_dir`.
//! 2. Iterates records in strict LSN order.
//! 3. Skips records with `lsn <= sink.durable_lsn()`.
//! 4. Maintains a TXN buffer per spec §05/08 §6: records between
//!    `TXN_BEGIN` and the matching `TXN_COMMIT` are queued; `COMMIT`
//!    flushes them; `ABORT` or end-of-WAL with no commit discards them.
//! 5. For each applied record, writes the slot to the arena (vector +
//!    metadata) and calls `sink.apply`.
//! 6. Rebuilds the slot allocator from the post-replay arena.
//!
//! Returns a [`RecoveryReport`] and the rebuilt [`SlotAllocator`].

use std::collections::BTreeMap;
use std::path::Path;

use brain_core::TxnId;

use crate::arena::allocator::SlotAllocator;
use crate::arena::file::ArenaFile;
use crate::arena::slot::{flags, VECTOR_DIM};
use crate::wal::payload::{
    ConsolidatePayload, EncodePayload, ForgetPayload, MigrateEmbeddingPayload, ReclaimPayload,
    WalPayload, WalPayloadError,
};
use crate::wal::reader::{WalReadError, WalReader};
use crate::wal::record::WalRecord;

// ---------------------------------------------------------------------------
// MetadataSink trait + in-memory impl.
// ---------------------------------------------------------------------------

/// Boundary between the storage crate and the metadata store
/// (`brain-metadata` in Phase 3).
///
/// The recovery driver feeds every applied record to the sink. The sink
/// is responsible for idempotency — `apply(lsn, timestamp_ns, payload)`
/// may be called more than once with the same `lsn` if recovery re-runs.
pub trait MetadataSink {
    /// The LSN through which the sink's state is durable. Recovery skips
    /// records whose `lsn <= durable_lsn()`. Returns 0 for a fresh sink.
    fn durable_lsn(&self) -> u64;

    /// Apply one record. Must be idempotent on `lsn`.
    ///
    /// `timestamp_ns` is the WAL record's wall-clock timestamp (unix
    /// nanos), threaded through so sinks can populate timestamped
    /// metadata rows (e.g. `CheckpointMeta.completed_at_unix_nanos`)
    /// without buffering record state externally.
    fn apply(
        &mut self,
        lsn: u64,
        timestamp_ns: u64,
        payload: &WalPayload,
    ) -> Result<(), MetadataSinkError>;
}

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum MetadataSinkError {
    #[error("transient: {0}")]
    Transient(String),
    #[error("corruption: {0}")]
    Corruption(String),
}

/// In-process test sink — records every `(lsn, payload)` pair, deduping
/// by LSN. Useful for unit tests; the real `brain-metadata` impl will
/// land in Phase 3.
pub struct InMemoryMetadataSink {
    by_lsn: BTreeMap<u64, WalPayload>,
    durable_lsn: u64,
}

impl InMemoryMetadataSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_lsn: BTreeMap::new(),
            durable_lsn: 0,
        }
    }

    #[must_use]
    pub fn with_durable_lsn(lsn: u64) -> Self {
        Self {
            by_lsn: BTreeMap::new(),
            durable_lsn: lsn,
        }
    }

    pub fn set_durable_lsn(&mut self, lsn: u64) {
        self.durable_lsn = lsn;
    }

    #[must_use]
    pub fn applied(&self) -> &BTreeMap<u64, WalPayload> {
        &self.by_lsn
    }
}

impl Default for InMemoryMetadataSink {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataSink for InMemoryMetadataSink {
    fn durable_lsn(&self) -> u64 {
        self.durable_lsn
    }

    fn apply(
        &mut self,
        lsn: u64,
        _timestamp_ns: u64,
        payload: &WalPayload,
    ) -> Result<(), MetadataSinkError> {
        // BTreeMap::insert overwrites — idempotent on (lsn, payload).
        self.by_lsn.insert(lsn, payload.clone());
        // CHECKPOINT_END advances `durable_lsn`. The defensive `max`
        // guards against out-of-order replay (recovery iterates in LSN
        // order today, but a future caller might not).
        if let WalPayload::CheckpointEnd(p) = payload {
            self.durable_lsn = self.durable_lsn.max(p.durable_lsn);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Errors + report.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum RecoveryError {
    #[error("WAL read error: {0}")]
    WalRead(#[from] WalReadError),

    #[error("payload decode error at LSN {lsn}: {source}")]
    PayloadDecodeError {
        lsn: u64,
        #[source]
        source: WalPayloadError,
    },

    #[error("arena slot {idx} out of range (capacity {capacity}) at LSN {lsn}")]
    ArenaOutOfCapacity { idx: u64, capacity: u64, lsn: u64 },

    #[error("vector dimension mismatch at LSN {lsn}: expected {expected}, got {found}")]
    VectorDimMismatch {
        lsn: u64,
        expected: usize,
        found: usize,
    },

    #[error("metadata sink rejected record at LSN {lsn}: {source}")]
    SinkError {
        lsn: u64,
        #[source]
        source: MetadataSinkError,
    },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Number of records the driver applied (counts records inside a
    /// committed transaction as applied).
    pub records_replayed: u64,
    /// Records skipped because `lsn <= sink.durable_lsn()`.
    pub records_skipped: u64,
    /// Records discarded by `TXN_ABORT` or a partial transaction at EOL.
    pub records_discarded: u64,
    /// LSN of the next record the WAL should write. After recovery, the
    /// caller resumes appending starting here.
    pub next_lsn: u64,
}

// ---------------------------------------------------------------------------
// recover().
// ---------------------------------------------------------------------------

/// Replay every WAL record under `wal_dir` onto `arena` and `sink`, then
/// rebuild the slot allocator. See the module docs for the full algorithm.
pub fn recover(
    arena: &mut ArenaFile,
    wal_dir: &Path,
    shard_uuid: [u8; 16],
    sink: &mut dyn MetadataSink,
) -> Result<(RecoveryReport, SlotAllocator), RecoveryError> {
    let durable_lsn = sink.durable_lsn();
    let reader = WalReader::open(wal_dir, shard_uuid)?;

    let mut records_replayed: u64 = 0;
    let mut records_skipped: u64 = 0;
    let mut records_discarded: u64 = 0;
    let mut next_lsn: u64 = durable_lsn + 1;

    // TXN state machine per spec §05/08 §6.
    let mut active_txn: Option<TxnId> = None;
    let mut txn_buffer: Vec<(WalRecord, WalPayload)> = Vec::new();

    for item in reader {
        let record = item?;
        let lsn = record.lsn.raw();
        next_lsn = lsn + 1;

        if lsn <= durable_lsn {
            records_skipped += 1;
            continue;
        }

        let payload = record
            .typed_payload()
            .map_err(|source| RecoveryError::PayloadDecodeError { lsn, source })?;

        if let Some(current_txn) = active_txn {
            // Inside a transaction.
            match &payload {
                WalPayload::TxnCommit(p) if p.txn_id == current_txn => {
                    txn_buffer.push((record, payload));
                    // Replay the whole batch.
                    let buffered = std::mem::take(&mut txn_buffer);
                    active_txn = None;
                    for (b_record, b_payload) in &buffered {
                        apply(arena, sink, b_record, b_payload)?;
                    }
                    records_replayed += buffered.len() as u64;
                }
                WalPayload::TxnAbort(p) if p.txn_id == current_txn => {
                    records_discarded += txn_buffer.len() as u64;
                    txn_buffer.clear();
                    active_txn = None;
                }
                _ => {
                    txn_buffer.push((record, payload));
                }
            }
        } else {
            // Normal mode.
            match &payload {
                WalPayload::TxnBegin(p) => {
                    active_txn = Some(p.txn_id);
                    txn_buffer.push((record, payload));
                }
                _ => {
                    apply(arena, sink, &record, &payload)?;
                    records_replayed += 1;
                }
            }
        }
    }

    // Partial transaction at end of WAL: discard per spec §05/08 §6.
    if active_txn.is_some() {
        records_discarded += txn_buffer.len() as u64;
    }

    let allocator = SlotAllocator::rebuild_from_arena(arena);
    Ok((
        RecoveryReport {
            records_replayed,
            records_skipped,
            records_discarded,
            next_lsn,
        },
        allocator,
    ))
}

// ---------------------------------------------------------------------------
// Apply helpers.
// ---------------------------------------------------------------------------

fn apply(
    arena: &mut ArenaFile,
    sink: &mut dyn MetadataSink,
    record: &WalRecord,
    payload: &WalPayload,
) -> Result<(), RecoveryError> {
    apply_to_arena(arena, record, payload)?;
    sink.apply(record.lsn.raw(), record.timestamp_ns, payload)
        .map_err(|source| RecoveryError::SinkError {
            lsn: record.lsn.raw(),
            source,
        })?;
    Ok(())
}

fn apply_to_arena(
    arena: &mut ArenaFile,
    record: &WalRecord,
    payload: &WalPayload,
) -> Result<(), RecoveryError> {
    match payload {
        WalPayload::Encode(p) => write_encoded_slot(arena, record, p),
        WalPayload::Forget(p) => mark_slot_tombstoned(arena, record, p),
        WalPayload::Reclaim(p) => reclaim_slot(arena, record, p),
        WalPayload::Consolidate(p) => write_consolidated_slot(arena, record, p),
        WalPayload::MigrateEmbedding(p) => migrate_slot_vector(arena, record, p),
        // Metadata-only or no-op on the arena.
        WalPayload::Link(_)
        | WalPayload::Unlink(_)
        | WalPayload::UpdateSalience(_)
        | WalPayload::UpdateKind(_)
        | WalPayload::UpdateContext(_)
        | WalPayload::CheckpointBegin(_)
        | WalPayload::CheckpointEnd(_)
        | WalPayload::TxnBegin(_)
        | WalPayload::TxnCommit(_)
        | WalPayload::TxnAbort(_) => Ok(()),
    }
}

fn write_encoded_slot(
    arena: &mut ArenaFile,
    record: &WalRecord,
    p: &EncodePayload,
) -> Result<(), RecoveryError> {
    let lsn = record.lsn.raw();
    let slot_idx = p.memory_id.slot();
    check_slot_in_range(arena, slot_idx, lsn)?;
    check_vector_dim(&p.vector, lsn)?;

    let slot = arena.slot_mut(slot_idx);
    if !p.vector.is_empty() {
        slot.vector.copy_from_slice(&p.vector);
    }
    slot.metadata.slot_version = p.memory_id.version();
    slot.metadata.flags = flags::OCCUPIED;
    slot.metadata.embedding_model_fp_short = p.embedding_model_fp;
    slot.metadata.created_at_unix_nanos = record.timestamp_ns;
    slot.metadata.last_modified_at_unix_nanos = record.timestamp_ns;
    slot.refresh_crc();
    Ok(())
}

fn mark_slot_tombstoned(
    arena: &mut ArenaFile,
    record: &WalRecord,
    p: &ForgetPayload,
) -> Result<(), RecoveryError> {
    let lsn = record.lsn.raw();
    let slot_idx = p.memory_id.slot();
    check_slot_in_range(arena, slot_idx, lsn)?;
    let slot = arena.slot_mut(slot_idx);
    slot.set_flag(flags::TOMBSTONED, true);
    slot.metadata.last_modified_at_unix_nanos = record.timestamp_ns;
    // Hard-forget (vector zeroing + HARD_FORGOTTEN flag) is deferred —
    // separate sub-task. See plan §6.
    slot.refresh_crc();
    Ok(())
}

fn reclaim_slot(
    arena: &mut ArenaFile,
    record: &WalRecord,
    p: &ReclaimPayload,
) -> Result<(), RecoveryError> {
    let lsn = record.lsn.raw();
    check_slot_in_range(arena, p.slot_id, lsn)?;
    let slot = arena.slot_mut(p.slot_id);
    slot.metadata.slot_version = p.new_version;
    slot.metadata.flags = 0;
    slot.metadata.last_modified_at_unix_nanos = record.timestamp_ns;
    slot.refresh_crc();
    Ok(())
}

fn write_consolidated_slot(
    arena: &mut ArenaFile,
    record: &WalRecord,
    p: &ConsolidatePayload,
) -> Result<(), RecoveryError> {
    let lsn = record.lsn.raw();
    let slot_idx = p.new_memory_id.slot();
    check_slot_in_range(arena, slot_idx, lsn)?;
    check_vector_dim(&p.vector, lsn)?;

    let slot = arena.slot_mut(slot_idx);
    if !p.vector.is_empty() {
        slot.vector.copy_from_slice(&p.vector);
    }
    slot.metadata.slot_version = p.new_memory_id.version();
    slot.metadata.flags = flags::OCCUPIED;
    slot.metadata.embedding_model_fp_short = p.embedding_model_fp;
    slot.metadata.created_at_unix_nanos = record.timestamp_ns;
    slot.metadata.last_modified_at_unix_nanos = record.timestamp_ns;
    slot.refresh_crc();
    Ok(())
}

fn migrate_slot_vector(
    arena: &mut ArenaFile,
    record: &WalRecord,
    p: &MigrateEmbeddingPayload,
) -> Result<(), RecoveryError> {
    let lsn = record.lsn.raw();
    let slot_idx = p.memory_id.slot();
    check_slot_in_range(arena, slot_idx, lsn)?;
    check_vector_dim(&p.new_vector, lsn)?;
    let slot = arena.slot_mut(slot_idx);
    if !p.new_vector.is_empty() {
        slot.vector.copy_from_slice(&p.new_vector);
    }
    slot.metadata.embedding_model_fp_short = p.new_fingerprint;
    slot.metadata.last_modified_at_unix_nanos = record.timestamp_ns;
    slot.refresh_crc();
    Ok(())
}

fn check_slot_in_range(arena: &ArenaFile, slot_idx: u64, lsn: u64) -> Result<(), RecoveryError> {
    if slot_idx >= arena.capacity_slots() {
        return Err(RecoveryError::ArenaOutOfCapacity {
            idx: slot_idx,
            capacity: arena.capacity_slots(),
            lsn,
        });
    }
    Ok(())
}

fn check_vector_dim(vector: &[f32], lsn: u64) -> Result<(), RecoveryError> {
    if !vector.is_empty() && vector.len() != VECTOR_DIM {
        return Err(RecoveryError::VectorDimMismatch {
            lsn,
            expected: VECTOR_DIM,
            found: vector.len(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

// Tests instantiate `ArenaFile` + `Wal` (full file I/O). Gated under
// miri; see `.claude/plans/phase-02-miri.md`.
#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::arena::file::ArenaFile;
    use crate::wal::kinds::WalRecordKind;
    use crate::wal::payload::{
        EncodePayload, ForgetMode, ForgetPayload, ForgetReason, ReclaimPayload, TxnBeginPayload,
        TxnCommitPayload,
    };
    use crate::wal::record::{Lsn, WalRecord};
    use crate::wal::segment::WalSegment;
    use crate::wal::wal::Wal;
    use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, RequestId, TxnId};
    use std::path::{Path, PathBuf};

    fn uuid(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn aid(byte: u8) -> AgentId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn rid(byte: u8) -> RequestId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn tid(byte: u8) -> TxnId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn fresh_arena(dir: &tempfile::TempDir, capacity: u64) -> ArenaFile {
        ArenaFile::open(dir.path().join("arena.bin"), uuid(1), capacity).unwrap()
    }

    fn fresh_wal_dir(parent: &tempfile::TempDir) -> PathBuf {
        let p = parent.path().join("wal");
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn encode_record(slot: u64) -> WalRecord {
        let memory_id = MemoryId::pack(1, slot, 1);
        let p = EncodePayload {
            memory_id,
            request_id: rid(0),
            agent_id: aid(0),
            context_id: ContextId(0),
            kind: MemoryKind::Episodic,
            salience_initial: 0.5,
            embedding_model_fp: [0xAB; 16],
            text: "hello".to_string(),
            vector: vec![0.5; VECTOR_DIM],
            edges: vec![],
        };
        WalRecord::from_typed(
            Lsn(0),
            0,
            1_700_000_000_000_000_000,
            0xCAFE,
            &WalPayload::Encode(p),
        )
    }

    fn forget_record(slot: u64, version: u32) -> WalRecord {
        let memory_id = MemoryId::pack(1, slot, version);
        let p = ForgetPayload {
            memory_id,
            request_id: rid(0),
            mode: ForgetMode::Soft,
            reason: ForgetReason::ClientRequest,
        };
        WalRecord::from_typed(
            Lsn(0),
            0,
            1_700_000_000_000_000_001,
            0xCAFE,
            &WalPayload::Forget(p),
        )
    }

    fn reclaim_record(slot: u64, old_v: u32, new_v: u32) -> WalRecord {
        let p = ReclaimPayload {
            slot_id: slot,
            old_version: old_v,
            new_version: new_v,
            memory_id: brain_core::MemoryId::pack(1, slot, old_v),
        };
        WalRecord::from_typed(
            Lsn(0),
            0,
            1_700_000_000_000_000_002,
            0xCAFE,
            &WalPayload::Reclaim(p),
        )
    }

    /// Write records via the full `Wal` (LSN allocation + group commit).
    /// Hosts the async ops on a per-call Glommio executor.
    fn write_via_wal(wal_dir: &Path, records: Vec<WalRecord>) {
        let wal_dir = wal_dir.to_owned();
        crate::wal::segment::glommio_run(move || async move {
            let wal = Wal::create(&wal_dir, uuid(1)).await.unwrap();
            for r in records {
                wal.append(r).await.unwrap();
            }
            wal.shutdown().await.unwrap();
        });
    }

    /// Bypass `Wal` and write records directly into segment 0. Used for
    /// hand-crafted WALs (TXN markers, malformed payloads).
    fn write_via_segment(wal_dir: &Path, records: &[WalRecord]) {
        std::fs::create_dir_all(wal_dir).unwrap();
        let seg_path = wal_dir.join("0000000000.wal");
        let records: Vec<WalRecord> = records.to_vec();
        crate::wal::segment::glommio_run(move || async move {
            let mut seg = WalSegment::create_new(&seg_path, 0, 1, uuid(1))
                .await
                .unwrap();
            for r in &records {
                seg.append_record(r).unwrap();
            }
            seg.flush().await.unwrap();
            seg.close().await.unwrap();
        });
    }

    // ----- Empty cases --------------------------------------------------

    #[test]
    fn empty_wal_recovery_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        // Empty WAL dir → WalReader will see 0 segments.
        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        let (report, _alloc) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        assert_eq!(report.records_replayed, 0);
        assert_eq!(report.records_skipped, 0);
        assert_eq!(report.records_discarded, 0);
        assert_eq!(report.next_lsn, 1);
        assert!(sink.applied().is_empty());
    }

    #[test]
    fn all_records_below_durable_lsn_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let mut records = Vec::new();
        for slot in 0..10 {
            let mut r = encode_record(slot);
            r.lsn = Lsn(slot + 1);
            records.push(r);
        }
        write_via_wal(&wal_dir, records);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::with_durable_lsn(100);
        let (report, _alloc) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        assert_eq!(report.records_replayed, 0);
        assert_eq!(report.records_skipped, 10);
        assert!(sink.applied().is_empty());
    }

    // ----- End-to-end (phase doc done-when) -----------------------------

    #[test]
    fn replay_after_write_matches_writer_state() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let records: Vec<_> = (0..20).map(encode_record).collect();
        write_via_wal(&wal_dir, records);

        let mut arena = fresh_arena(&dir, 64);
        let mut sink = InMemoryMetadataSink::new();
        let (report, alloc) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        assert_eq!(report.records_replayed, 20);
        assert_eq!(report.records_skipped, 0);
        assert_eq!(report.next_lsn, 21);
        assert_eq!(sink.applied().len(), 20);

        // Every targeted slot is OCCUPIED with version 1; allocator's
        // next_fresh advances past the last slot we wrote (slot 19).
        for slot in 0..20u64 {
            let s = arena.slot(slot);
            assert!(s.is_occupied(), "slot {slot} should be occupied");
            assert_eq!(s.metadata.slot_version, 1);
            assert!(s.is_valid());
        }
        assert!(alloc.next_fresh() >= 20);
    }

    #[test]
    fn recovery_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let records: Vec<_> = (0..10).map(encode_record).collect();
        write_via_wal(&wal_dir, records);

        // First pass.
        let mut arena = fresh_arena(&dir, 32);
        let mut sink = InMemoryMetadataSink::new();
        let (report1, alloc1) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        let applied1: Vec<u64> = sink.applied().keys().copied().collect();
        let next_fresh1 = alloc1.next_fresh();
        drop(arena);

        // Second pass on a fresh arena + sink.
        let mut arena = fresh_arena(&dir, 32);
        let mut sink = InMemoryMetadataSink::new();
        let (report2, alloc2) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        let applied2: Vec<u64> = sink.applied().keys().copied().collect();
        let next_fresh2 = alloc2.next_fresh();

        assert_eq!(report1, report2);
        assert_eq!(applied1, applied2);
        assert_eq!(next_fresh1, next_fresh2);
    }

    // ----- Torn tail ----------------------------------------------------

    #[test]
    fn torn_tail_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let records: Vec<_> = (0..10).map(encode_record).collect();
        write_via_wal(&wal_dir, records);

        // Truncate the file mid-record-10.
        let seg_path = wal_dir.join("0000000000.wal");
        let current_size = std::fs::metadata(&seg_path).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&seg_path)
            .unwrap()
            .set_len(current_size - 30)
            .unwrap();

        let mut arena = fresh_arena(&dir, 32);
        let mut sink = InMemoryMetadataSink::new();
        let (report, _alloc) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        // Last record was torn; 9 surviving records were applied.
        assert_eq!(report.records_replayed, 9);
        assert_eq!(report.next_lsn, 10);
        assert_eq!(sink.applied().len(), 9);
    }

    // ----- Arena application -------------------------------------------

    #[test]
    fn encode_writes_vector_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        write_via_wal(&wal_dir, vec![encode_record(7)]);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        let s = arena.slot(7);
        assert!(s.is_valid());
        assert!(s.is_occupied());
        assert_eq!(s.metadata.slot_version, 1);
        assert_eq!(s.metadata.embedding_model_fp_short, [0xAB; 16]);
        assert!((s.vector[0] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn forget_sets_tombstoned_bit() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        write_via_wal(&wal_dir, vec![encode_record(3), forget_record(3, 1)]);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        let s = arena.slot(3);
        // Spec §05/02 §3.2: "active but tombstoned" — both bits set.
        assert!(s.is_occupied(), "OCCUPIED stays set through soft FORGET");
        assert!(s.is_tombstoned());
        assert!(s.is_valid());
    }

    #[test]
    fn reclaim_bumps_version_and_clears_flags() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        write_via_wal(
            &wal_dir,
            vec![
                encode_record(5),
                forget_record(5, 1),
                reclaim_record(5, 1, 2),
            ],
        );

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        let s = arena.slot(5);
        assert!(!s.is_occupied());
        assert!(!s.is_tombstoned());
        assert_eq!(s.metadata.slot_version, 2);
        assert!(s.is_valid());
    }

    // ----- TXN ----------------------------------------------------------

    #[test]
    fn complete_transaction_applies_all_records() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let txn = tid(42);
        let begin = WalRecord::from_typed(
            Lsn(1),
            0,
            1_700_000_000_000_000_000,
            0xCAFE,
            &WalPayload::TxnBegin(TxnBeginPayload {
                txn_id: txn,
                expected_record_count: 3,
            }),
        );
        let mut r1 = encode_record(1);
        r1.lsn = Lsn(2);
        let mut r2 = encode_record(2);
        r2.lsn = Lsn(3);
        let mut r3 = encode_record(3);
        r3.lsn = Lsn(4);
        let commit = WalRecord::from_typed(
            Lsn(5),
            0,
            1_700_000_000_000_000_000,
            0xCAFE,
            &WalPayload::TxnCommit(TxnCommitPayload { txn_id: txn }),
        );
        write_via_segment(&wal_dir, &[begin, r1, r2, r3, commit]);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        let (report, _alloc) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        // All 5 records (begin, r1, r2, r3, commit) are "replayed".
        assert_eq!(report.records_replayed, 5);
        assert_eq!(report.records_discarded, 0);
        // The 3 encode records' slots are occupied.
        for slot in 1..=3u64 {
            assert!(
                arena.slot(slot).is_occupied(),
                "slot {slot} should be occupied"
            );
        }
    }

    #[test]
    fn partial_transaction_at_eol_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let txn = tid(43);
        let begin = WalRecord::from_typed(
            Lsn(1),
            0,
            1_700_000_000_000_000_000,
            0xCAFE,
            &WalPayload::TxnBegin(TxnBeginPayload {
                txn_id: txn,
                expected_record_count: 3,
            }),
        );
        let mut r1 = encode_record(1);
        r1.lsn = Lsn(2);
        let mut r2 = encode_record(2);
        r2.lsn = Lsn(3);
        // No commit/abort.
        write_via_segment(&wal_dir, &[begin, r1, r2]);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        let (report, _alloc) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        assert_eq!(report.records_replayed, 0);
        assert_eq!(report.records_discarded, 3);
        // The encode records inside the (uncommitted) txn were NOT applied.
        assert!(!arena.slot(1).is_occupied());
        assert!(!arena.slot(2).is_occupied());
        assert!(sink.applied().is_empty());
    }

    // ----- Error paths --------------------------------------------------

    #[test]
    fn vector_dimension_mismatch_errors() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        // Hand-craft an Encode record with the wrong vector dimension.
        let mut rec = encode_record(0);
        let WalPayload::Encode(mut payload) = rec.typed_payload().unwrap() else {
            unreachable!()
        };
        payload.vector = vec![0.0; 100]; // != VECTOR_DIM
        rec = WalRecord::from_typed(
            Lsn(1),
            0,
            rec.timestamp_ns,
            rec.agent_id_lo64,
            &WalPayload::Encode(payload),
        );
        write_via_segment(&wal_dir, &[rec]);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        let err = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap_err();
        match err {
            RecoveryError::VectorDimMismatch {
                expected, found, ..
            } => {
                assert_eq!(expected, VECTOR_DIM);
                assert_eq!(found, 100);
            }
            other => panic!("expected VectorDimMismatch, got {other:?}"),
        }
    }

    #[test]
    fn out_of_range_slot_errors() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        // Encode with slot=9999 against a 16-slot arena.
        let mut rec = encode_record(0);
        let WalPayload::Encode(mut payload) = rec.typed_payload().unwrap() else {
            unreachable!()
        };
        payload.memory_id = MemoryId::pack(1, 9999, 1);
        rec = WalRecord::from_typed(
            Lsn(1),
            0,
            rec.timestamp_ns,
            rec.agent_id_lo64,
            &WalPayload::Encode(payload),
        );
        write_via_segment(&wal_dir, &[rec]);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        let err = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap_err();
        match err {
            RecoveryError::ArenaOutOfCapacity { idx, capacity, .. } => {
                assert_eq!(idx, 9999);
                assert_eq!(capacity, 16);
            }
            other => panic!("expected ArenaOutOfCapacity, got {other:?}"),
        }
    }

    // ----- WalRecordKind smoke -----------------------------------------

    #[test]
    fn records_have_expected_kinds() {
        // Sanity: make sure our test helpers produce records with the
        // expected kind. Cheap belt-and-suspenders against future drift.
        assert_eq!(encode_record(0).kind, WalRecordKind::Encode);
        assert_eq!(forget_record(0, 1).kind, WalRecordKind::Forget);
        assert_eq!(reclaim_record(0, 0, 1).kind, WalRecordKind::Reclaim);
    }
}
