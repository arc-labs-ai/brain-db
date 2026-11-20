//! Deliberate operator repair for WALs that cannot be read through to the end.
//!
//! Boot tolerates the one damage shape that is provably loss-free: a torn
//! multi-page batch in the active segment, where nothing beyond the hole was
//! acknowledged because the committer acknowledges only after a full-length
//! `write_at` and `fdatasync`, and `fdatasync` flushes the whole file. Every
//! other unreadable shape fails shard spawn on purpose, because cutting there
//! discards writes a client was told had succeeded: mid-segment corruption in
//! a fully rolled-over segment (`MidSegmentCorruption`), an LSN gap
//! (`LsnGap` or `LsnGapAtSegmentBoundary`).
//! This module is the deliberate, two-step, loud operator path for those cases
//! and is never called from boot.
//!
//! Repair makes the log readable again; it does not decide what is applicable.
//! The next normal boot runs [`crate::recovery::recover`], which applies the
//! transaction rules and computes the real tail.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::recovery::WalTail;
use crate::wal::reader::{scan_survivors, WalReadError, WalReader};
use crate::wal::segment::WAL_SEGMENT_HEADER_LEN;

/// One segment's share of what a repair discards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentDiscard {
    pub segment_seq: u64,
    pub path: PathBuf,
    /// Byte length the segment is cut to, or `None` when the whole segment is removed.
    pub truncate_to: Option<u64>,
    pub discarded_bytes: u64,
}

/// What a repair would discard, and the token that authorizes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRepairPlan {
    /// Where the log stops being readable, and so where the cut falls: the
    /// segment holding the last record that decodes and the byte position
    /// just past it.
    pub cut: WalTail,
    /// Reason the log is unreadable past the cut, rendered for an operator.
    pub damage: String,
    /// LSN of the last record that survives the cut; 0 when none does.
    pub last_surviving_lsn: u64,
    /// First LSN the cut discards.
    pub first_discarded_lsn: u64,
    /// Highest LSN still recognizable among the discarded bytes, when any
    /// record there decodes.
    pub last_discarded_lsn: Option<u64>,
    /// Well-formed records the cut discards. A floor, not a total: records
    /// whose bytes fall inside a hole cannot be counted at all.
    pub discarded_record_count: u64,
    pub discarded_bytes: u64,
    /// Segments the cut touches, lowest seq first.
    pub discarded_segments: Vec<SegmentDiscard>,
    /// Confirmation token, not a security boundary: it makes the operator
    /// name the exact damage under review, so a stale plan cannot authorize a
    /// cut through bytes that have changed.
    pub authorization: String,
}

/// What a repair did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRepairOutcome {
    pub plan: WalRepairPlan,
    /// Files the discarded bytes were copied to before the cut, lowest seq
    /// first.
    pub preserved: Vec<PathBuf>,
}

#[derive(thiserror::Error, Debug)]
pub enum WalRepairError {
    #[error("WAL read error while planning repair: {0}")]
    Read(#[from] WalReadError),

    #[error("directory {dir:?} contains no *.wal segment files; nothing to repair")]
    NoSegments { dir: PathBuf },

    #[error("the WAL reads cleanly end to end; there is nothing to repair")]
    NothingToRepair,

    #[error(
        "authorization {supplied:?} does not match this WAL's repair plan \
         ({expected:?}); re-plan and pass the token from the plan you intend \
         to apply"
    )]
    Unauthorized { expected: String, supplied: String },

    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Plan a cut through the first unreadable WAL bytes without changing disk.
pub fn plan_wal_repair(
    wal_dir: &Path,
    shard_uuid: [u8; 16],
) -> Result<WalRepairPlan, WalRepairError> {
    let mut reader = WalReader::open(wal_dir, shard_uuid)?;
    if reader.segments().is_empty() {
        return Err(WalRepairError::NoSegments {
            dir: wal_dir.to_path_buf(),
        });
    }

    let damage = loop {
        match reader.next() {
            Some(Ok(_)) => continue,
            Some(Err(error)) => break error.to_string(),
            None => return Err(WalRepairError::NothingToRepair),
        }
    };

    let last_surviving_lsn = reader.last_decoded_lsn().unwrap_or(0);
    let first_discarded_lsn = reader.next_expected_lsn();
    let cut = WalTail {
        segment_seq: reader
            .last_record_segment_seq()
            .or_else(|| reader.oldest_segment_seq())
            .expect("non-empty reader has an oldest segment"),
        offset: reader
            .last_record_end_offset()
            .unwrap_or(WAL_SEGMENT_HEADER_LEN) as u64,
    };

    let mut discarded_segments = Vec::new();
    let mut discarded_record_count = 0;
    let mut last_discarded_lsn = None;
    for segment in reader.segments() {
        if segment.segment_seq < cut.segment_seq {
            continue;
        }
        let scan_start = if segment.segment_seq == cut.segment_seq {
            cut.offset as usize
        } else {
            WAL_SEGMENT_HEADER_LEN
        };
        let bytes = fs::read(&segment.path)?;
        let scan = scan_survivors(bytes.get(scan_start..).unwrap_or(&[]), first_discarded_lsn);
        discarded_record_count += scan.count;
        if scan.last_lsn.is_some() {
            last_discarded_lsn = scan.last_lsn;
        }
        let discarded_bytes = if segment.segment_seq == cut.segment_seq {
            segment.file_size.saturating_sub(cut.offset)
        } else {
            segment.file_size
        };
        discarded_segments.push(SegmentDiscard {
            segment_seq: segment.segment_seq,
            path: segment.path.clone(),
            truncate_to: (segment.segment_seq == cut.segment_seq).then_some(cut.offset),
            discarded_bytes,
        });
    }

    let discarded_bytes = discarded_segments
        .iter()
        .map(|segment| segment.discarded_bytes)
        .sum();
    let authorization = repair_authorization(
        shard_uuid,
        cut,
        first_discarded_lsn,
        last_discarded_lsn,
        discarded_record_count,
        &discarded_segments,
    );

    Ok(WalRepairPlan {
        cut,
        damage,
        last_surviving_lsn,
        first_discarded_lsn,
        last_discarded_lsn,
        discarded_record_count,
        discarded_bytes,
        discarded_segments,
        authorization,
    })
}

/// Apply an operator-authorized plan against the bytes currently on disk.
pub fn repair_wal(
    wal_dir: &Path,
    shard_uuid: [u8; 16],
    authorization: &str,
) -> Result<WalRepairOutcome, WalRepairError> {
    let plan = plan_wal_repair(wal_dir, shard_uuid)?;
    if authorization != plan.authorization {
        return Err(WalRepairError::Unauthorized {
            expected: plan.authorization,
            supplied: authorization.to_string(),
        });
    }

    let last = plan
        .last_discarded_lsn
        .map_or_else(|| "unknown".to_string(), |lsn| lsn.to_string());
    let mut preserved = Vec::with_capacity(plan.discarded_segments.len());
    for segment in &plan.discarded_segments {
        let path = wal_dir.join(format!(
            "{:010}.discarded-lsn{}-{}.bin",
            segment.segment_seq, plan.first_discarded_lsn, last
        ));
        let start = segment.truncate_to.map_or(0, |_| plan.cut.offset);
        copy_extent(&segment.path, start, segment.discarded_bytes, &path)?;
        preserved.push(path);
    }

    for segment in &plan.discarded_segments {
        if let Some(truncate_to) = segment.truncate_to {
            let file = OpenOptions::new().write(true).open(&segment.path)?;
            file.set_len(truncate_to)?;
            file.sync_all()?;
        } else {
            fs::remove_file(&segment.path)?;
        }
    }
    fsync_dir(wal_dir)?;

    tracing::warn!(
        cut = ?plan.cut,
        damage = %plan.damage,
        last_surviving_lsn = plan.last_surviving_lsn,
        first_discarded_lsn = plan.first_discarded_lsn,
        last_discarded_lsn = ?plan.last_discarded_lsn,
        discarded_record_count = plan.discarded_record_count,
        discarded_bytes = plan.discarded_bytes,
        discarded_segments = ?plan.discarded_segments,
        authorization = %plan.authorization,
        preserved = ?preserved,
        "operator-authorized WAL repair discarded unreadable bytes"
    );
    Ok(WalRepairOutcome { plan, preserved })
}

fn repair_authorization(
    shard_uuid: [u8; 16],
    cut: WalTail,
    first_discarded_lsn: u64,
    last_discarded_lsn: Option<u64>,
    discarded_record_count: u64,
    segments: &[SegmentDiscard],
) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&shard_uuid);
    bytes.extend_from_slice(&cut.segment_seq.to_le_bytes());
    bytes.extend_from_slice(&cut.offset.to_le_bytes());
    bytes.extend_from_slice(&first_discarded_lsn.to_le_bytes());
    bytes.push(last_discarded_lsn.is_some() as u8);
    bytes.extend_from_slice(&last_discarded_lsn.unwrap_or(0).to_le_bytes());
    bytes.extend_from_slice(&discarded_record_count.to_le_bytes());
    for segment in segments {
        bytes.extend_from_slice(&segment.segment_seq.to_le_bytes());
        let file_size = segment
            .truncate_to
            .map_or(segment.discarded_bytes, |truncate_to| {
                segment.discarded_bytes + truncate_to
            });
        bytes.extend_from_slice(&file_size.to_le_bytes());
    }
    format!("wal-repair-{:08x}", crc32c::crc32c(&bytes))
}

fn copy_extent(source: &Path, start: u64, length: u64, destination: &Path) -> io::Result<()> {
    let mut input = File::open(source)?;
    input.seek(SeekFrom::Start(start))?;
    let mut output = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(destination)?;
    io::copy(&mut input.by_ref().take(length), &mut output)?;
    output.sync_all()
}

fn fsync_dir(dir: &Path) -> io::Result<()> {
    let cstr = CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory path contains an interior NUL byte",
        )
    })?;
    // SAFETY: `cstr` is a valid NUL-terminated path; flags are O_RDONLY.
    let fd = unsafe { libc::open(cstr.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a valid open fd until `libc::close`.
    let result = if unsafe { libc::fsync(fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    };
    // SAFETY: `fd` was obtained from `libc::open` and not yet closed.
    unsafe { libc::close(fd) };
    result
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::wal::kinds::WalRecordKind;
    use crate::wal::reader::WalReader;
    use crate::wal::record::{Lsn, WalRecord};
    use crate::wal::segment::{glommio_run, WalSegment};
    use std::io::{Seek, SeekFrom, Write};

    fn uuid(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn record(lsn: u64) -> WalRecord {
        WalRecord {
            lsn: Lsn(lsn),
            kind: WalRecordKind::Encode,
            flags: 0,
            timestamp_ns: 1_700_000_000_000_000_000,
            space_id_lo64: 0x1234_5678_9ABC_DEF0,
            payload: vec![0xAB; 32],
        }
    }

    fn write_segment(wal_dir: &Path, segment_seq: u64, starting_lsn: u64, records: &[WalRecord]) {
        let path = wal_dir.join(format!("{segment_seq:010}.wal"));
        let records = records.to_vec();
        glommio_run(move || async move {
            let mut segment = WalSegment::create_new(&path, segment_seq, starting_lsn, uuid(1))
                .await
                .unwrap();
            for record in &records {
                segment.append_record(record).unwrap();
            }
            segment.flush_durable().await.unwrap();
            segment.close().await.unwrap();
        });
    }

    fn intact_two_segment_wal(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        write_segment(dir, 0, 1, &[record(1), record(2)]);
        write_segment(dir, 1, 3, &[record(3), record(4)]);
    }

    fn damaged_two_segment_wal(dir: &Path) {
        intact_two_segment_wal(dir);
        let path = dir.join("0000000000.wal");
        let record_bytes = record(1).encoded_len();
        let offset = WAL_SEGMENT_HEADER_LEN as u64 + record_bytes as u64;
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&vec![0; record_bytes]).unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn plan_is_read_only_and_refuses_a_clean_wal() {
        let dir = tempfile::tempdir().unwrap();
        intact_two_segment_wal(dir.path());
        let before0 = std::fs::read(dir.path().join("0000000000.wal")).unwrap();
        let before1 = std::fs::read(dir.path().join("0000000001.wal")).unwrap();
        assert!(matches!(
            plan_wal_repair(dir.path(), uuid(1)),
            Err(WalRepairError::NothingToRepair)
        ));
        assert_eq!(
            std::fs::read(dir.path().join("0000000000.wal")).unwrap(),
            before0
        );
        assert_eq!(
            std::fs::read(dir.path().join("0000000001.wal")).unwrap(),
            before1
        );
    }

    #[test]
    fn repair_refuses_a_foreign_authorization() {
        let dir = tempfile::tempdir().unwrap();
        damaged_two_segment_wal(dir.path());
        let before0 = std::fs::metadata(dir.path().join("0000000000.wal"))
            .unwrap()
            .len();
        let before1 = std::fs::metadata(dir.path().join("0000000001.wal"))
            .unwrap()
            .len();
        let err = repair_wal(dir.path(), uuid(1), "wal-repair-00000000").unwrap_err();
        assert!(matches!(err, WalRepairError::Unauthorized { .. }));
        assert!(dir.path().join("0000000000.wal").exists());
        assert!(dir.path().join("0000000001.wal").exists());
        assert_eq!(
            std::fs::metadata(dir.path().join("0000000000.wal"))
                .unwrap()
                .len(),
            before0
        );
        assert_eq!(
            std::fs::metadata(dir.path().join("0000000001.wal"))
                .unwrap()
                .len(),
            before1
        );
    }

    #[test]
    fn repair_cuts_at_the_last_good_record_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        damaged_two_segment_wal(dir.path());
        let plan = plan_wal_repair(dir.path(), uuid(1)).unwrap();
        let expected_count = plan.discarded_record_count;
        let expected_first = plan.first_discarded_lsn;
        let expected_last = plan.last_discarded_lsn;
        assert_eq!(expected_count, 2);
        assert_eq!(expected_first, 2);
        assert_eq!(expected_last, Some(4));
        let cut = plan.cut;
        let outcome = repair_wal(dir.path(), uuid(1), &plan.authorization).unwrap();

        assert!(!dir.path().join("0000000001.wal").exists());
        assert_eq!(
            std::fs::metadata(dir.path().join("0000000000.wal"))
                .unwrap()
                .len(),
            cut.offset
        );
        assert!(std::fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".discarded-lsn")
        }));
        assert_eq!(outcome.plan.discarded_record_count, expected_count);
        assert_eq!(outcome.plan.first_discarded_lsn, expected_first);
        assert_eq!(outcome.plan.last_discarded_lsn, expected_last);
        let reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let lsns: Vec<_> = reader.map(|record| record.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, vec![1]);
    }
}
