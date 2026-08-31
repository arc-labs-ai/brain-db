//! WAL-based recovery driver.
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
//! 4. Maintains a TXN buffer: records between
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
    ConsolidatePayload, EncodePayload, ForgetMode, ForgetPayload, MigrateEmbeddingPayload,
    ReclaimPayload, WalPayload, WalPayloadError,
};
use crate::wal::reader::{WalReadError, WalReader};
use crate::wal::record::{WalRecord, FLAG_SUBSCRIBE_EVENT};
use crate::wal::segment::WAL_SEGMENT_HEADER_LEN;

// ---------------------------------------------------------------------------
// MetadataSink trait + in-memory impl.
// ---------------------------------------------------------------------------

/// Boundary between the storage crate and the metadata store
/// (`brain-metadata`).
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
/// by LSN. Useful for unit tests; the real impl lives in `brain-metadata`.
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

    #[error(
        "transaction {txn_id:?} buffered more records ({buffered}) than its declared \
         expected_record_count ({expected}) without a commit — WAL is corrupt"
    )]
    TxnBufferOverflow {
        txn_id: TxnId,
        buffered: usize,
        expected: u32,
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
    ///
    /// This is `last_good_lsn + 1` where `last_good_lsn` is the LSN of the
    /// last record in the durably-good, fully-applied prefix — it excludes
    /// records that were discarded (a dangling transaction's begin +
    /// members) or never fully decoded (a torn tail). Resuming here lets
    /// new appends reuse the LSNs of the truncated-away bytes with no gap.
    pub next_lsn: u64,
    /// Byte length the **active** (highest-`segment_seq`) segment must be
    /// truncated to before resuming appends: the file position immediately
    /// after the last durably-good, fully-applied record in that segment,
    /// or `WAL_SEGMENT_HEADER_LEN` if every record in the active segment was
    /// discarded/torn (or the active segment is empty). Everything past this
    /// offset is torn-tail bytes or a never-committed dangling-transaction
    /// prefix that a crash left behind; [`crate::wal::wal::Wal::open_existing`]
    /// physically `set_len`s the segment to this length so new appends
    /// overwrite rather than follow the garbage.
    pub active_tail_offset: u64,
    /// `segment_seq` of the active segment `active_tail_offset` refers to,
    /// or `None` if the WAL had no segments. Lets the reopen path assert it
    /// is truncating the same segment recovery validated.
    pub active_segment_seq: Option<u64>,
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
    let mut reader = WalReader::open(wal_dir, shard_uuid)?;
    let active_segment_seq = reader.active_segment_seq();

    let mut records_replayed: u64 = 0;
    let mut records_skipped: u64 = 0;
    let mut records_discarded: u64 = 0;

    // The durably-good, fully-applied tail: the LSN + physical position
    // (segment_seq, end offset) of the last record we're certain is
    // committed and applied. Advances only at a clean boundary — never
    // while a transaction is open — so a dangling `TxnBegin` (+ members)
    // at the end of the WAL leaves the tail pointing *before* the begin.
    // `last_good_lsn` seeds at `durable_lsn` so an all-skipped or empty WAL
    // resumes at `durable_lsn + 1`.
    let mut last_good_lsn: u64 = durable_lsn;
    let mut good_tail_offset: Option<usize> = None;
    let mut good_tail_segment_seq: Option<u64> = None;

    // TXN state machine.
    let mut active_txn: Option<TxnId> = None;
    let mut active_txn_expected: u32 = 0;
    let mut txn_buffer: Vec<(WalRecord, WalPayload)> = Vec::new();

    while let Some(item) = reader.next() {
        let record = item?;
        let lsn = record.lsn.raw();

        // Records at or below the durable checkpoint are already applied
        // and committed. They are part of the good prefix, so advance the
        // tail; but they can never sit inside an open transaction (the
        // checkpoint boundary is itself a commit boundary), so this is
        // always a clean boundary.
        if lsn <= durable_lsn {
            records_skipped += 1;
            advance_tail(
                &reader,
                lsn,
                &mut last_good_lsn,
                &mut good_tail_offset,
                &mut good_tail_segment_seq,
            );
            continue;
        }

        // Subscribe-replay change-feed events ride the WAL under the same
        // record kinds as the durable write records, distinguished only by
        // this flag. They are not state mutations — the durable record
        // carries the data recovery needs — so skip them here. (Decoding
        // their CBOR body as a typed-graph row would fail outright.) A
        // flagged event outside a transaction is a clean boundary; one
        // buffered inside an open txn is not, so only advance when idle.
        if record.flags & FLAG_SUBSCRIBE_EVENT != 0 {
            records_skipped += 1;
            if active_txn.is_none() {
                advance_tail(
                    &reader,
                    lsn,
                    &mut last_good_lsn,
                    &mut good_tail_offset,
                    &mut good_tail_segment_seq,
                );
            }
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
                    active_txn_expected = 0;
                    for (b_record, b_payload) in &buffered {
                        apply(arena, sink, b_record, b_payload)?;
                    }
                    records_replayed += buffered.len() as u64;
                    // Commit closes the txn: this is a clean boundary.
                    advance_tail(
                        &reader,
                        lsn,
                        &mut last_good_lsn,
                        &mut good_tail_offset,
                        &mut good_tail_segment_seq,
                    );
                }
                WalPayload::TxnAbort(p) if p.txn_id == current_txn => {
                    records_discarded += txn_buffer.len() as u64;
                    txn_buffer.clear();
                    active_txn = None;
                    active_txn_expected = 0;
                    // Abort closes the txn without applying anything, but
                    // the abort record itself is durably good — the tail
                    // may advance to just past it.
                    advance_tail(
                        &reader,
                        lsn,
                        &mut last_good_lsn,
                        &mut good_tail_offset,
                        &mut good_tail_segment_seq,
                    );
                }
                // A second `TxnBegin` while one is open means the first
                // never committed (dangling). Discard ONLY the first txn's
                // buffered records, then start the new one — a dangling
                // txn must never consume unrelated records.
                WalPayload::TxnBegin(p) => {
                    records_discarded += txn_buffer.len() as u64;
                    txn_buffer.clear();
                    active_txn = Some(p.txn_id);
                    active_txn_expected = p.expected_record_count;
                    txn_buffer.push((record, payload));
                }
                _ => {
                    // Defense-in-depth: bound the buffer by the declared
                    // member count so a corrupt/never-terminated txn can't
                    // buffer unboundedly (OOM). The buffer legitimately
                    // holds begin (1) + expected members; a further member
                    // before commit/abort means the WAL is corrupt.
                    if txn_buffer.len() > active_txn_expected as usize {
                        return Err(RecoveryError::TxnBufferOverflow {
                            txn_id: current_txn,
                            buffered: txn_buffer.len(),
                            expected: active_txn_expected,
                        });
                    }
                    txn_buffer.push((record, payload));
                }
            }
        } else {
            // Normal mode.
            match &payload {
                WalPayload::TxnBegin(p) => {
                    active_txn = Some(p.txn_id);
                    active_txn_expected = p.expected_record_count;
                    txn_buffer.push((record, payload));
                    // Opening a txn is NOT a boundary: the tail stays put
                    // until we see the matching commit.
                }
                _ => {
                    apply(arena, sink, &record, &payload)?;
                    records_replayed += 1;
                    advance_tail(
                        &reader,
                        lsn,
                        &mut last_good_lsn,
                        &mut good_tail_offset,
                        &mut good_tail_segment_seq,
                    );
                }
            }
        }
    }

    // Partial transaction at end of WAL: discard ONLY its own buffered
    // records. The tail already points before the dangling begin.
    if active_txn.is_some() {
        records_discarded += txn_buffer.len() as u64;
    }

    // The active segment is truncated to the tail offset iff the tail lives
    // in that segment. Otherwise the active segment holds only discarded /
    // torn bytes (or is an empty post-rollover segment) and is cut back to
    // its header. New appends then resume at `last_good_lsn + 1`, reusing
    // the truncated LSNs with no gap.
    let active_tail_offset = match (good_tail_segment_seq, active_segment_seq) {
        (Some(good_seq), Some(active_seq)) if good_seq == active_seq => {
            good_tail_offset.unwrap_or(WAL_SEGMENT_HEADER_LEN) as u64
        }
        _ => WAL_SEGMENT_HEADER_LEN as u64,
    };
    let next_lsn = last_good_lsn + 1;

    let allocator = SlotAllocator::rebuild_from_arena(arena);
    Ok((
        RecoveryReport {
            records_replayed,
            records_skipped,
            records_discarded,
            next_lsn,
            active_tail_offset,
            active_segment_seq,
        },
        allocator,
    ))
}

/// Advance the durably-good tail to the record the reader just yielded.
/// Reads the reader's post-decode cursor position so the offset is the byte
/// immediately after the record in its segment file.
fn advance_tail(
    reader: &WalReader,
    lsn: u64,
    last_good_lsn: &mut u64,
    good_tail_offset: &mut Option<usize>,
    good_tail_segment_seq: &mut Option<u64>,
) {
    *last_good_lsn = lsn;
    *good_tail_offset = reader.last_record_end_offset();
    *good_tail_segment_seq = reader.last_record_segment_seq();
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
        | WalPayload::UpdateSession(_)
        | WalPayload::CheckpointBegin(_)
        | WalPayload::CheckpointEnd(_)
        | WalPayload::TxnBegin(_)
        | WalPayload::TxnCommit(_)
        | WalPayload::TxnAbort(_)
        | WalPayload::RelationLink(_)
        | WalPayload::RelationSupersede(_)
        | WalPayload::RelationTombstone(_)
        // A soft FORGET never zeroed the arena slot (soft keeps the
        // vector until grace), so un-tombstoning it touches redb only.
        | WalPayload::RestoreMemory(_) => Ok(()),
        // typed-graph records: substrate apply-paths ignore these.
        // Phases 16+ hydrate typed-graph state via their own sinks. Sub-task 15.2.
        WalPayload::PhaseBody(r) => {
            tracing::trace!(
                kind = ?r.kind,
                body_len = r.body.len(),
                lsn = record.lsn.raw(),
                "recovery: skipping opaque-body record (substrate arena unaffected)"
            );
            Ok(())
        }
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
    // Hard forget: zero the vector and set HARD_FORGOTTEN. The ENCODE
    // record for this memory is still in the WAL (it carries the
    // plaintext vector for self-sufficient replay) and is replayed
    // *before* this FORGET record, so it re-materializes the vector into
    // the slot. Re-zeroing here is what makes hard forget hold across a
    // crash + recovery — otherwise a recovered arena would resurrect the
    // forgotten plaintext until the GC worker's wipe ran. Idempotent.
    if p.mode == ForgetMode::Hard {
        slot.vector = [0.0; VECTOR_DIM];
        slot.set_flag(flags::HARD_FORGOTTEN, true);
    }
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

// Tests instantiate `ArenaFile` + `Wal` (full file I/O). Gated out
// under miri.
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
    use crate::wal::segment::{WalSegment, WAL_SEGMENT_HEADER_LEN};
    use crate::wal::wal::{Wal, WalConfig};
    use brain_core::{MemoryId, MemoryKind, RequestId, SessionId, SpaceId, TxnId};
    use std::path::{Path, PathBuf};

    fn uuid(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn aid(byte: u8) -> SpaceId {
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
            space_id: aid(0),
            namespace_id: brain_core::NamespaceId::SYSTEM,
            session_id: SessionId(0),
            kind: MemoryKind::Episodic,
            salience_initial: 0.5,
            embedding_model_fp: [0xAB; 16],
            text: "hello".to_string(),
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
            0xCAFE,
            &WalPayload::Encode(p),
        )
    }

    fn forget_record(slot: u64, version: u32) -> WalRecord {
        let memory_id = MemoryId::pack(1, slot, version);
        let p = ForgetPayload {
            memory_id,
            request_id: rid(0),
            space_id: brain_core::SpaceId::default(),
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

    fn hard_forget_record(slot: u64, version: u32) -> WalRecord {
        let memory_id = MemoryId::pack(1, slot, version);
        let p = ForgetPayload {
            memory_id,
            request_id: rid(0),
            space_id: brain_core::SpaceId::default(),
            mode: ForgetMode::Hard,
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

    /// Clean-shutdown guard: after a graceful close (no torn tail), the
    /// recovered tail offset MUST equal the on-disk size of the active
    /// segment so `open_existing` truncates nothing — otherwise it would
    /// chop acknowledged, committed records and lose data on restart. This
    /// is the deterministic unit-level counterpart to the server's
    /// `acknowledged_writes_survive_graceful_shutdown_and_restart`.
    #[test]
    fn clean_shutdown_tail_offset_equals_file_size_no_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let records: Vec<_> = (0..12).map(encode_record).collect();
        write_via_wal(&wal_dir, records);

        let seg_path = wal_dir.join("0000000000.wal");
        let file_size = std::fs::metadata(&seg_path).unwrap().len();

        let mut arena = fresh_arena(&dir, 32);
        let mut sink = InMemoryMetadataSink::new();
        let (report, _alloc) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();

        assert_eq!(report.records_replayed, 12);
        assert_eq!(report.active_segment_seq, Some(0));
        assert_eq!(
            report.active_tail_offset, file_size,
            "clean shutdown must recover a tail at end-of-file; a shorter \
             offset would truncate committed records on reopen"
        );
        assert!(report.active_tail_offset > WAL_SEGMENT_HEADER_LEN as u64);

        // And reopening with that offset performs no truncation: the file
        // keeps every byte and all records still read back.
        let wal_dir2 = wal_dir.clone();
        let next_lsn = report.next_lsn;
        let offset = report.active_tail_offset;
        crate::wal::segment::glommio_run(move || async move {
            let wal =
                Wal::open_existing(&wal_dir2, uuid(1), next_lsn, offset, WalConfig::default())
                    .await
                    .unwrap();
            wal.shutdown().await.unwrap();
        });
        assert_eq!(
            std::fs::metadata(&seg_path).unwrap().len(),
            file_size,
            "open_existing must not shrink a cleanly-closed segment"
        );
        let reader = WalReader::open(&wal_dir, uuid(1)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, (1..=12).collect::<Vec<_>>());
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
    fn hard_forget_zeroes_vector_on_recovery() {
        // ENCODE (vector = 0.5s) then a HARD forget for the same slot.
        // Recovery replays ENCODE first (re-materializing the vector),
        // then the hard FORGET, which must re-zero it — so a crash can't
        // resurrect the forgotten plaintext into the arena.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        write_via_wal(&wal_dir, vec![encode_record(0), hard_forget_record(0, 1)]);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();

        let s = arena.slot(0);
        assert!(s.is_tombstoned(), "hard forget tombstones the slot");
        assert!(s.is_hard_forgotten(), "HARD_FORGOTTEN flag must be set");
        assert!(
            s.vector.iter().all(|&x| x == 0.0),
            "hard forget must zero the vector on recovery"
        );
        assert!(s.is_valid(), "CRC must be refreshed after zeroing");
    }

    #[test]
    fn soft_forget_keeps_vector_on_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        write_via_wal(&wal_dir, vec![encode_record(0), forget_record(0, 1)]);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();

        let s = arena.slot(0);
        assert!(s.is_tombstoned());
        assert!(
            !s.is_hard_forgotten(),
            "soft forget must not set HARD_FORGOTTEN"
        );
        assert!(
            s.vector.iter().any(|&x| x != 0.0),
            "soft forget keeps the vector (recoverable during grace)"
        );
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
        // "active but tombstoned" — both bits set.
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
            rec.space_id_lo64,
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
            rec.space_id_lo64,
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

    // ----- typed-graph -----------------------------------------------

    /// Build a opaque-body record with an arbitrary opaque body. Used
    /// by `recovery_skips_graph_records` to interleave typed-graph
    /// frames between substrate ones.
    fn graph_record(kind: WalRecordKind, body: Vec<u8>) -> WalRecord {
        use crate::wal::payload::PhaseBodyRecord;
        WalRecord::from_typed(
            Lsn(0),
            0,
            1_700_000_000_000_000_002,
            0xBEEF,
            &WalPayload::PhaseBody(PhaseBodyRecord::new(
                kind,
                brain_core::SpaceId::default(),
                body,
            )),
        )
    }

    #[test]
    fn recovery_replays_graph_records_without_touching_arena() {
        // A WAL containing substrate + typed-graph + substrate records.
        // Recovery treats the typed-graph frame as a no-op for the
        // substrate apply-paths (arena + substrate sink) but still
        // advances the LSN counter — `records_replayed` includes it.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let records = vec![
            encode_record(0),
            graph_record(WalRecordKind::EntityCreate, vec![0xDE, 0xAD, 0xBE, 0xEF]),
            encode_record(1),
            graph_record(WalRecordKind::SchemaUpdate, vec![]),
            graph_record(WalRecordKind::Audit, vec![1, 2, 3, 4, 5]),
            encode_record(2),
        ];
        write_via_wal(&wal_dir, records);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        let (report, _alloc) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();

        // All 6 records counted as replayed (typed-graph no-ops still
        // advance the LSN watermark).
        assert_eq!(report.records_replayed, 6);
        assert_eq!(report.records_skipped, 0);
        assert_eq!(report.records_discarded, 0);

        // Substrate slots 0/1/2 are populated by the Encode records.
        for slot in 0..3u64 {
            let s = arena.slot(slot);
            assert!(s.is_occupied(), "slot {slot} should be occupied");
            assert_eq!(s.metadata.slot_version, 1);
        }

        // The typed-graph frames passed through the sink (so checkpoint
        // logic sees them) but as opaque payloads.
        let applied = sink.applied();
        let graph_count = applied
            .values()
            .filter(|p| matches!(p, WalPayload::PhaseBody(_)))
            .count();
        assert_eq!(graph_count, 3);
    }

    /// Build a flagged `StageCompleted` notification record the way
    /// `OpsContext::publish_notification` does: opaque-body kind,
    /// `FLAG_SUBSCRIBE_EVENT` set, arbitrary CBOR-shaped body (recovery
    /// never decodes it — the flag alone routes it to skip).
    fn stage_completed_record(body: Vec<u8>) -> WalRecord {
        use crate::wal::payload::PhaseBodyRecord;
        let mut rec = WalRecord::from_typed(
            Lsn(0),
            0,
            1_700_000_000_000_000_003,
            0xF00D,
            &WalPayload::PhaseBody(PhaseBodyRecord::new(
                WalRecordKind::StageCompleted,
                brain_core::SpaceId::default(),
                body,
            )),
        );
        rec.flags = FLAG_SUBSCRIBE_EVENT;
        rec
    }

    #[test]
    fn recovery_skips_flagged_stage_completed_records() {
        // Unlike typed-graph kinds, `StageCompleted` has no separate
        // durable write record at all — the flagged notification record
        // IS the only WAL trace of the event. Recovery must still skip
        // it (it's not state to hydrate into any table): this is the
        // crash-recovery guard mirroring the typed-graph
        // `FLAG_SUBSCRIBE_EVENT` skip precedent
        // (`brain-metadata/tests/recovery_integration.rs`), scoped to
        // this crate's own `InMemoryMetadataSink` harness.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        // A body that would fail to rkyv-decode as any substrate row —
        // proves recovery never attempts to interpret it, just skips on
        // the flag.
        let cbor_like_body = b"\xa1istage_kindA".to_vec();
        let records = vec![
            encode_record(0),
            stage_completed_record(cbor_like_body),
            encode_record(1),
        ];
        write_via_wal(&wal_dir, records);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        let (report, _alloc) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();

        assert_eq!(
            report.records_replayed, 2,
            "only the two substrate Encode records replay"
        );
        assert_eq!(
            report.records_skipped, 1,
            "the flagged StageCompleted notification record is skipped, not applied"
        );
        assert_eq!(report.records_discarded, 0);

        // Substrate slots 0/1 populated normally; the flagged record
        // never reached the sink.
        for slot in 0..2u64 {
            let s = arena.slot(slot);
            assert!(s.is_occupied(), "slot {slot} should be occupied");
        }
        let applied = sink.applied();
        assert_eq!(applied.len(), 2, "sink sees only the two Encode records");
        assert!(
            applied.values().all(|p| matches!(p, WalPayload::Encode(_))),
            "no StageCompleted payload reached the sink"
        );
    }

    // ----- Multi-cycle crash consistency (reopen truncates the tail) ----
    //
    // The single-cycle torn-tail / dangling-txn tests above prove one
    // recovery pass. These prove the *reopen boundary*: after recovery
    // computes a tail offset, `Wal::open_existing` must physically truncate
    // the active segment to it so records appended after the reopen land on
    // clean bytes and survive a *second* recovery — with no LSN reuse and no
    // redb/arena divergence.

    fn begin_record(lsn: u64, txn: TxnId, expected: u32) -> WalRecord {
        let mut r = WalRecord::from_typed(
            Lsn(lsn),
            0,
            1_700_000_000_000_000_000,
            0xCAFE,
            &WalPayload::TxnBegin(TxnBeginPayload {
                txn_id: txn,
                expected_record_count: expected,
            }),
        );
        r.lsn = Lsn(lsn);
        r
    }

    #[test]
    fn torn_tail_reopen_truncates_then_second_recovery_keeps_new_records() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);

        // Cycle 1: write 10 records (LSN 1..=10, slots 0..=9) durably.
        let records: Vec<_> = (0..10).map(encode_record).collect();
        write_via_wal(&wal_dir, records);

        // Crash A: tear the tail (partial record 10 / slot 9).
        let seg_path = wal_dir.join("0000000000.wal");
        let torn_size = std::fs::metadata(&seg_path).unwrap().len() - 30;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&seg_path)
            .unwrap()
            .set_len(torn_size)
            .unwrap();

        // Recover #1: 9 good, torn record dropped.
        let mut arena = fresh_arena(&dir, 64);
        let mut sink = InMemoryMetadataSink::new();
        let (report1, _a) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        assert_eq!(report1.records_replayed, 9);
        assert_eq!(report1.next_lsn, 10);
        assert_eq!(report1.active_segment_seq, Some(0));
        // Tail sits strictly before the torn bytes.
        assert!(report1.active_tail_offset < torn_size);
        assert!(report1.active_tail_offset >= WAL_SEGMENT_HEADER_LEN as u64);

        // Reopen: open_existing truncates to the recovered tail, then we
        // append 5 NEW records (LSN 10..=14, slots 10..=14) and shut down
        // cleanly.
        let wal_dir2 = wal_dir.clone();
        let next_lsn1 = report1.next_lsn;
        let offset1 = report1.active_tail_offset;
        crate::wal::segment::glommio_run(move || async move {
            let wal =
                Wal::open_existing(&wal_dir2, uuid(1), next_lsn1, offset1, WalConfig::default())
                    .await
                    .unwrap();
            for slot in 10..15u64 {
                let lsn = wal.append(encode_record(slot)).await.unwrap();
                assert!(lsn.raw() >= 10, "new appends must not reuse LSNs <10");
            }
            wal.shutdown().await.unwrap();
        });

        // The truncation must have overwritten the torn region, not grown
        // past it: file no longer contains the old torn bytes as garbage.
        // Recover #2 on a fresh arena.
        let mut arena2 = fresh_arena(&dir, 64);
        let mut sink2 = InMemoryMetadataSink::new();
        let (report2, _a2) = recover(&mut arena2, &wal_dir, uuid(1), &mut sink2).unwrap();

        // BUG 1 regression: all 14 records (9 original + 5 post-reopen)
        // replay. The old code buried the torn bytes and a second recovery
        // stopped at record 9, silently dropping the 5 new records.
        assert_eq!(report2.records_replayed, 14, "post-reopen records lost");
        assert_eq!(report2.next_lsn, 15);

        // No divergence: slots 0..=8 and 10..=14 occupied; the torn slot 9
        // never came back.
        for slot in 0..9u64 {
            assert!(arena2.slot(slot).is_occupied(), "slot {slot} lost");
        }
        assert!(!arena2.slot(9).is_occupied(), "torn slot 9 resurrected");
        for slot in 10..15u64 {
            assert!(arena2.slot(slot).is_occupied(), "new slot {slot} lost");
        }

        // No LSN reuse: contiguous 1..=14 on disk.
        let reader = WalReader::open(&wal_dir, uuid(1)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, (1..=14).collect::<Vec<_>>());
    }

    #[test]
    fn dangling_txn_reopen_truncates_then_committed_records_survive() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let txn = tid(77);

        // Cycle 1: one committed encode (LSN 1, slot 0), then a dangling
        // TxnBegin (LSN 2) + 2 members (LSN 3,4) with NO commit — the
        // classic "multi-phase write fsynced, crash before commit" case.
        let mut e0 = encode_record(0);
        e0.lsn = Lsn(1);
        let begin = begin_record(2, txn, 2);
        let mut m1 = encode_record(1);
        m1.lsn = Lsn(3);
        let mut m2 = encode_record(2);
        m2.lsn = Lsn(4);
        write_via_segment(&wal_dir, &[e0, begin, m1, m2]);

        // Recover #1: LSN 1 applied; the dangling txn (begin + 2 members)
        // discarded; tail lands after LSN 1, before the begin.
        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        let (report1, _a) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        assert_eq!(report1.records_replayed, 1);
        assert_eq!(report1.records_discarded, 3);
        assert_eq!(report1.next_lsn, 2, "resume before the dangling begin");
        assert!(arena.slot(0).is_occupied());
        assert!(!arena.slot(1).is_occupied());
        assert!(!arena.slot(2).is_occupied());

        // Reopen: truncate away the dangling prefix, append 2 real
        // committed encodes (LSN 2,3 / slots 1,2), shut down.
        let wal_dir2 = wal_dir.clone();
        let next_lsn1 = report1.next_lsn;
        let offset1 = report1.active_tail_offset;
        crate::wal::segment::glommio_run(move || async move {
            let wal =
                Wal::open_existing(&wal_dir2, uuid(1), next_lsn1, offset1, WalConfig::default())
                    .await
                    .unwrap();
            let l1 = wal.append(encode_record(1)).await.unwrap();
            let l2 = wal.append(encode_record(2)).await.unwrap();
            assert_eq!(
                (l1.raw(), l2.raw()),
                (2, 3),
                "LSNs of dangling txn reused cleanly"
            );
            wal.shutdown().await.unwrap();
        });

        // Recover #2: LSN 1,2,3 → slots 0,1,2. The dangling txn's records
        // are gone (BUG 2 regression: the old code would have buffered the
        // post-reopen committed records into the still-open txn and
        // discarded them at EOL).
        let mut arena2 = fresh_arena(&dir, 16);
        let mut sink2 = InMemoryMetadataSink::new();
        let (report2, _a2) = recover(&mut arena2, &wal_dir, uuid(1), &mut sink2).unwrap();
        assert_eq!(report2.records_replayed, 3, "committed records lost");
        assert_eq!(report2.records_discarded, 0, "no dangling txn remains");
        assert_eq!(report2.next_lsn, 4);
        for slot in 0..3u64 {
            assert!(arena2.slot(slot).is_occupied(), "slot {slot} lost");
        }
        let reader = WalReader::open(&wal_dir, uuid(1)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, vec![1, 2, 3]);
    }

    #[test]
    fn dangling_txn_buffer_is_bounded_by_expected_count() {
        // A TxnBegin declaring 2 members followed by a flood of members and
        // no commit must halt with TxnBufferOverflow rather than buffering
        // every record to EOL (OOM protection).
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let txn = tid(88);

        let mut records = vec![begin_record(1, txn, 2)];
        // 6 members, no commit. The bound trips at member #3.
        for i in 0..6u64 {
            let mut m = encode_record(i);
            m.lsn = Lsn(2 + i);
            records.push(m);
        }
        write_via_segment(&wal_dir, &records);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        let err = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap_err();
        match err {
            RecoveryError::TxnBufferOverflow {
                buffered, expected, ..
            } => {
                assert_eq!(expected, 2);
                // begin (1) + expected members (2) buffered; the next
                // member overflows.
                assert_eq!(buffered, 3);
            }
            other => panic!("expected TxnBufferOverflow, got {other:?}"),
        }
    }
}
