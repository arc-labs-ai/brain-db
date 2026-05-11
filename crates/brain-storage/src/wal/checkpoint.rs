//! Checkpoint writer.
//!
//! Implements the spec §05/09 §3 procedure:
//!
//! 1. Write `CHECKPOINT_BEGIN` to the WAL (durable on return).
//! 2. `msync(MS_SYNC)` the whole arena so every pre-checkpoint slot write
//!    reaches stable storage.
//! 3. Write `CHECKPOINT_END(durable_lsn = target_lsn)` to the WAL.
//!
//! Failure of any step leaves the previous checkpoint as the recovery
//! target (spec §09 §12.1). The sink learns about the new checkpoint via
//! `apply(CheckpointEnd)` on the next `recover` — we don't push it
//! at runtime to avoid runtime sink/WAL disagreement windows.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::arena::file::ArenaFile;
use crate::wal::payload::{CheckpointBeginPayload, CheckpointEndPayload, WalPayload};
use crate::wal::record::{Lsn, WalRecord};
use crate::wal::wal::{Wal, WalError};

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// Caller-supplied input to [`write_checkpoint`].
#[derive(Debug, Clone, Copy)]
pub struct CheckpointPlan {
    /// Monotonic id, assigned by the caller (typically a checkpoint worker).
    pub checkpoint_id: u64,
    /// The LSN this checkpoint promises is durable in arena + metadata.
    ///
    /// `None` → use `wal.next_lsn() - 1` at call time (the LSN of the
    /// most recent durably-written record).
    pub target_lsn: Option<u64>,
}

/// Returned by [`write_checkpoint`] on success.
#[derive(Debug, Clone, Copy)]
pub struct CheckpointReport {
    pub checkpoint_id: u64,
    pub durable_lsn: u64,
    pub lsn_begin: u64,
    pub lsn_end: u64,
    pub arena_capacity_at_checkpoint: u64,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
}

#[derive(thiserror::Error, Debug)]
pub enum CheckpointError {
    #[error("WAL error during checkpoint: {0}")]
    Wal(#[from] WalError),

    #[error("arena msync failed: {source}")]
    Msync {
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------
// write_checkpoint.
// ---------------------------------------------------------------------------

/// Write a checkpoint per spec §05/09 §3.
///
/// On success, the WAL contains `CHECKPOINT_BEGIN` followed by
/// `CHECKPOINT_END(durable_lsn)` records, and every arena page that was
/// dirty at the moment of step 3 is durable. On the next `recover`, the
/// sink picks up the new `durable_lsn` via `apply(CheckpointEnd)`.
///
/// If step 3 (arena `msync`) fails, `CHECKPOINT_END` is not written and the
/// caller receives [`CheckpointError::Msync`]. The next recovery sees a
/// `CHECKPOINT_BEGIN` without a matching `END` and ignores it (spec
/// §09 §12.1) — the previous checkpoint stays valid.
pub fn write_checkpoint(
    wal: &mut Wal,
    arena: &ArenaFile,
    plan: CheckpointPlan,
) -> Result<CheckpointReport, CheckpointError> {
    let started_at_unix_nanos = unix_nanos_now();
    let target_lsn = plan.target_lsn.unwrap_or_else(|| {
        // The most recently durably-written LSN (or 0 if no records).
        wal.next_lsn().saturating_sub(1)
    });
    let arena_capacity = arena.capacity_slots();

    // Step 1: CHECKPOINT_BEGIN.
    let begin_payload = WalPayload::CheckpointBegin(CheckpointBeginPayload {
        checkpoint_id: plan.checkpoint_id,
        started_at_unix_nanos,
    });
    let begin_record = WalRecord::from_typed(Lsn(0), 0, started_at_unix_nanos, 0, &begin_payload);
    let lsn_begin = wal.append(begin_record)?.raw();

    // Step 3: msync arena.
    arena
        .msync_all()
        .map_err(|source| CheckpointError::Msync { source })?;

    // Step 6: CHECKPOINT_END.
    let end_payload = WalPayload::CheckpointEnd(CheckpointEndPayload {
        checkpoint_id: plan.checkpoint_id,
        durable_lsn: target_lsn,
        arena_capacity,
    });
    let end_record = WalRecord::from_typed(Lsn(0), 0, unix_nanos_now(), 0, &end_payload);
    let lsn_end = wal.append(end_record)?.raw();

    let completed_at_unix_nanos = unix_nanos_now();
    Ok(CheckpointReport {
        checkpoint_id: plan.checkpoint_id,
        durable_lsn: target_lsn,
        lsn_begin,
        lsn_end,
        arena_capacity_at_checkpoint: arena_capacity,
        started_at_unix_nanos,
        completed_at_unix_nanos,
    })
}

fn unix_nanos_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

// Tests instantiate `Wal` + `ArenaFile`. Gated under miri; see
// `.claude/plans/phase-02-miri.md`.
#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::arena::file::{ArenaFile, MSYNC_ALL_CALLS};
    use crate::recovery::{recover, InMemoryMetadataSink, MetadataSink};
    use crate::wal::kinds::WalRecordKind;
    use crate::wal::record::WalRecord;
    use crate::wal::segment::WalSegment;
    use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, RequestId};
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    fn uuid(byte: u8) -> [u8; 16] {
        [byte; 16]
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
        use crate::wal::payload::EncodePayload;
        let memory_id = MemoryId::pack(1, slot, 1);
        let p = EncodePayload {
            memory_id,
            request_id: RequestId::from([0u8; 16]),
            agent_id: AgentId::from([0u8; 16]),
            context_id: ContextId(0),
            kind: MemoryKind::Episodic,
            salience_initial: 0.5,
            embedding_model_fp: [0; 16],
            text: String::new(),
            vector: vec![0.0; 384],
            edges: vec![],
        };
        WalRecord::from_typed(
            Lsn(0),
            0,
            1_700_000_000_000_000_000,
            0,
            &WalPayload::Encode(p),
        )
    }

    // ----- Basic mechanics ------------------------------------------------

    #[test]
    fn write_checkpoint_on_fresh_wal() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let arena = fresh_arena(&dir, 16);
        let mut wal = Wal::create(&wal_dir, uuid(1)).unwrap();

        let report = write_checkpoint(
            &mut wal,
            &arena,
            CheckpointPlan {
                checkpoint_id: 1,
                target_lsn: None,
            },
        )
        .unwrap();

        assert_eq!(report.checkpoint_id, 1);
        assert_eq!(report.durable_lsn, 0);
        assert_eq!(report.lsn_begin, 1);
        assert_eq!(report.lsn_end, 2);
        assert_eq!(report.arena_capacity_at_checkpoint, 16);
        wal.shutdown().unwrap();
    }

    #[test]
    fn target_lsn_defaults_to_last_written() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let arena = fresh_arena(&dir, 32);
        let mut wal = Wal::create(&wal_dir, uuid(1)).unwrap();
        for slot in 0..10 {
            wal.append(encode_record(slot)).unwrap();
        }
        let report = write_checkpoint(
            &mut wal,
            &arena,
            CheckpointPlan {
                checkpoint_id: 7,
                target_lsn: None,
            },
        )
        .unwrap();
        assert_eq!(report.durable_lsn, 10);
        assert_eq!(report.lsn_begin, 11);
        assert_eq!(report.lsn_end, 12);
        wal.shutdown().unwrap();
    }

    #[test]
    fn explicit_target_lsn_is_honored() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let arena = fresh_arena(&dir, 32);
        let mut wal = Wal::create(&wal_dir, uuid(1)).unwrap();
        for slot in 0..10 {
            wal.append(encode_record(slot)).unwrap();
        }
        let report = write_checkpoint(
            &mut wal,
            &arena,
            CheckpointPlan {
                checkpoint_id: 7,
                target_lsn: Some(5),
            },
        )
        .unwrap();
        assert_eq!(report.durable_lsn, 5);
        wal.shutdown().unwrap();
    }

    // ----- Recovery integration (phase doc done-when) --------------------

    #[test]
    fn checkpoint_advances_recovery_start_point() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let arena = fresh_arena(&dir, 32);
        let mut wal = Wal::create(&wal_dir, uuid(1)).unwrap();
        for slot in 0..10 {
            wal.append(encode_record(slot)).unwrap();
        }
        let _report = write_checkpoint(
            &mut wal,
            &arena,
            CheckpointPlan {
                checkpoint_id: 1,
                target_lsn: None,
            },
        )
        .unwrap();
        wal.shutdown().unwrap();
        drop(arena);

        // First recovery on a fresh sink — replays everything; the sink
        // ends with durable_lsn=10 from the CHECKPOINT_END payload.
        let mut arena = fresh_arena(&dir, 32);
        let mut sink = InMemoryMetadataSink::new();
        let (report1, _) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        // 10 encode records + BEGIN + END = 12 records replayed.
        assert_eq!(report1.records_replayed, 12);
        assert_eq!(sink.durable_lsn(), 10);

        // Second recovery on the same sink — records 1..=10 skipped;
        // BEGIN (lsn 11) and END (lsn 12) are above durable_lsn so
        // still applied.
        let mut arena = fresh_arena(&dir, 32);
        let (report2, _) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        assert_eq!(report2.records_skipped, 10);
        assert_eq!(report2.records_replayed, 2); // BEGIN + END
        assert_eq!(sink.durable_lsn(), 10);
    }

    #[test]
    fn multiple_checkpoints_recovery_uses_latest() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let arena = fresh_arena(&dir, 32);
        let mut wal = Wal::create(&wal_dir, uuid(1)).unwrap();

        // Records 1..=10
        for slot in 0..10 {
            wal.append(encode_record(slot)).unwrap();
        }
        // Checkpoint @ durable_lsn=10 (consumes LSNs 11 and 12).
        write_checkpoint(
            &mut wal,
            &arena,
            CheckpointPlan {
                checkpoint_id: 1,
                target_lsn: None,
            },
        )
        .unwrap();
        // Records 13..=22 (10 more)
        for slot in 10..20 {
            wal.append(encode_record(slot)).unwrap();
        }
        // Checkpoint @ durable_lsn=22 (consumes LSNs 23 and 24).
        write_checkpoint(
            &mut wal,
            &arena,
            CheckpointPlan {
                checkpoint_id: 2,
                target_lsn: None,
            },
        )
        .unwrap();
        wal.shutdown().unwrap();
        drop(arena);

        // Recover; sink should end with durable_lsn=22.
        let mut arena = fresh_arena(&dir, 32);
        let mut sink = InMemoryMetadataSink::new();
        let _ = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        assert_eq!(sink.durable_lsn(), 22);

        // Run recovery again — the latest checkpoint's durable_lsn is
        // honored; records 1..=22 are skipped.
        let mut arena = fresh_arena(&dir, 32);
        let (report, _) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        assert_eq!(report.records_skipped, 22);
        assert_eq!(report.records_replayed, 2); // the second BEGIN/END
        assert_eq!(sink.durable_lsn(), 22);
    }

    #[test]
    fn recovery_is_idempotent_across_multiple_runs() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let arena = fresh_arena(&dir, 32);
        let mut wal = Wal::create(&wal_dir, uuid(1)).unwrap();
        for slot in 0..5 {
            wal.append(encode_record(slot)).unwrap();
        }
        write_checkpoint(
            &mut wal,
            &arena,
            CheckpointPlan {
                checkpoint_id: 1,
                target_lsn: None,
            },
        )
        .unwrap();
        wal.shutdown().unwrap();
        drop(arena);

        // Run recover three times. Capture each report.
        let mut sink = InMemoryMetadataSink::new();
        let mut reports = Vec::new();
        for _ in 0..3 {
            let mut arena = fresh_arena(&dir, 32);
            let (r, _) = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
            reports.push(r);
        }
        // The second and third should be identical (durable_lsn stable
        // after the first pass).
        assert_eq!(reports[1].records_replayed, reports[2].records_replayed);
        assert_eq!(reports[1].records_skipped, reports[2].records_skipped);
        assert_eq!(sink.durable_lsn(), 5);
    }

    // ----- Failure handling ----------------------------------------------

    #[test]
    fn begin_without_end_does_not_advance_durable_lsn() {
        // Simulate a crash between CHECKPOINT_BEGIN and CHECKPOINT_END by
        // appending only a BEGIN record manually to the WAL.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        std::fs::create_dir_all(&wal_dir).unwrap();
        let seg_path = wal_dir.join("0000000000.wal");
        let mut seg = WalSegment::create_new(&seg_path, 0, 1, uuid(1)).unwrap();

        // 3 encode records (LSNs 1, 2, 3).
        for slot in 0..3u64 {
            let mut r = encode_record(slot);
            r.lsn = Lsn(slot + 1);
            seg.append_record(&r).unwrap();
        }
        // CHECKPOINT_BEGIN at LSN 4 — no matching END.
        let begin = WalRecord::from_typed(
            Lsn(4),
            0,
            1_700_000_000_000_000_000,
            0,
            &WalPayload::CheckpointBegin(CheckpointBeginPayload {
                checkpoint_id: 1,
                started_at_unix_nanos: 1_700_000_000_000_000_000,
            }),
        );
        seg.append_record(&begin).unwrap();
        seg.flush().unwrap();
        drop(seg);

        let mut arena = fresh_arena(&dir, 16);
        let mut sink = InMemoryMetadataSink::new();
        let _ = recover(&mut arena, &wal_dir, uuid(1), &mut sink).unwrap();
        // Sink saw BEGIN but no END → durable_lsn unchanged.
        assert_eq!(sink.durable_lsn(), 0);
    }

    #[test]
    fn write_checkpoint_msyncs_the_arena() {
        // Verifies (via the test-only counter) that msync_all is invoked
        // between BEGIN and END.
        MSYNC_ALL_CALLS.store(0, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let arena = fresh_arena(&dir, 16);
        let mut wal = Wal::create(&wal_dir, uuid(1)).unwrap();
        write_checkpoint(
            &mut wal,
            &arena,
            CheckpointPlan {
                checkpoint_id: 1,
                target_lsn: None,
            },
        )
        .unwrap();
        let count = MSYNC_ALL_CALLS.load(Ordering::SeqCst);
        assert!(
            count >= 1,
            "expected at least one msync_all call, got {count}"
        );
        wal.shutdown().unwrap();
    }

    // ----- Smoke ---------------------------------------------------------

    #[test]
    fn msync_all_on_fresh_arena_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let arena = fresh_arena(&dir, 16);
        arena.msync_all().unwrap();
    }

    #[test]
    fn record_kinds_are_checkpoint_records() {
        // Sanity: the records the writer produces have the expected kinds.
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = fresh_wal_dir(&dir);
        let arena = fresh_arena(&dir, 16);
        let mut wal = Wal::create(&wal_dir, uuid(1)).unwrap();
        write_checkpoint(
            &mut wal,
            &arena,
            CheckpointPlan {
                checkpoint_id: 1,
                target_lsn: None,
            },
        )
        .unwrap();
        wal.shutdown().unwrap();

        // Read back via WalReader; expect [CheckpointBegin, CheckpointEnd].
        let reader = crate::wal::reader::WalReader::open(&wal_dir, uuid(1)).unwrap();
        let kinds: Vec<WalRecordKind> = reader.map(|r| r.unwrap().kind).collect();
        assert_eq!(
            kinds,
            vec![WalRecordKind::CheckpointBegin, WalRecordKind::CheckpointEnd]
        );
    }
}
