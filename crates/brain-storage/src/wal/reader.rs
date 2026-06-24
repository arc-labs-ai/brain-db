//! WAL reader — streams `WalRecord`s from a directory of segments.
//!
//! [`WalReader::open`] enumerates `*.wal` segment files, validates each
//! segment's 4 KB header, and sorts by `segment_seq`. The `Iterator` impl
//! then streams records in strict LSN order across segments, applying
//! the tail-vs-mid-segment rule (see below).
//!
//! ## Tail-vs-mid-segment rule
//!
//! Recovery treats some failures as "clean end of the WAL" and others as
//! corruption:
//!
//! | Decode outcome | At end of last segment | Elsewhere |
//! |---|---|---|
//! | `Truncated` | clean tail → `None` | `MidSegmentCorruption` |
//! | `CrcMismatch` | clean tail → `None` | `MidSegmentCorruption` (3) |
//! | `UnknownRecordType` / `NonZeroReserved` / `PayloadTooLarge` | `RecordError` | `RecordError` |
//!
//! Plus boundary checks:
//!
//! - `segment_seq` sequence must be contiguous.
//! - Next segment's `starting_lsn` must equal `last_decoded_lsn + 1`
//!   ('s "strict LSN order" extended across boundaries).
//!
//! Violations of either are hard errors.

use std::fs;
use std::path::{Path, PathBuf};

use crate::wal::record::{DecodeOutcome, WalRecord, WalRecordError};
use crate::wal::segment::{
    WAL_SEGMENT_FORMAT_VERSION_V1, WAL_SEGMENT_HEADER_CRC_COVERAGE_END, WAL_SEGMENT_HEADER_LEN,
    WAL_SEGMENT_MAGIC,
};

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// Metadata about one segment, read from its 4 KB header at `WalReader::open`.
#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub path: PathBuf,
    pub segment_seq: u64,
    pub starting_lsn: u64,
    pub file_size: u64,
}

/// Streams `WalRecord`s across a directory of WAL segments.
///
/// `Iterator::next` returns `None` on a clean end (including a truncated
/// tail of the last segment). Mid-segment corruption is reported via
/// `Some(Err(_))`. After the first `None` or `Err`, subsequent `next`
/// calls return `None` — see the [`FusedIterator`] impl.
///
/// [`FusedIterator`]: core::iter::FusedIterator
pub struct WalReader {
    segments: Vec<SegmentInfo>,
    #[allow(dead_code)]
    shard_uuid: [u8; 16],
    current_idx: usize,
    current: Option<LoadedSegment>,
    expected_next_lsn: u64,
    last_decoded_lsn: Option<u64>,
    finished: bool,
}

struct LoadedSegment {
    bytes: Vec<u8>,
    cursor: usize,
}

#[derive(thiserror::Error, Debug)]
pub enum WalReadError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error(
        "WAL segment {path:?} too small: expected at least {} bytes, got {size}",
        WAL_SEGMENT_HEADER_LEN
    )]
    SegmentTooSmall { path: PathBuf, size: u64 },

    #[error("invalid segment header magic in {path:?}: expected b\"BWAL\", got {found:?}")]
    InvalidSegmentMagic { path: PathBuf, found: [u8; 4] },

    #[error("unsupported segment format_version {version} in {path:?}")]
    UnsupportedSegmentFormatVersion { path: PathBuf, version: u32 },

    #[error(
        "segment header CRC mismatch in {path:?}: stored {stored:#010x}, computed {computed:#010x}"
    )]
    SegmentHeaderCrcMismatch {
        path: PathBuf,
        stored: u32,
        computed: u32,
    },

    #[error("shard_uuid mismatch in {path:?}: expected {expected:?}, header says {found:?}")]
    SegmentShardUuidMismatch {
        path: PathBuf,
        expected: [u8; 16],
        found: [u8; 16],
    },

    #[error("filename segment_seq {filename_seq} doesn't match header segment_seq {header_seq} in {path:?}")]
    FilenameSegmentSeqMismatch {
        path: PathBuf,
        filename_seq: u64,
        header_seq: u64,
    },

    #[error("filename {filename:?} is not a valid 10-digit segment_seq with .wal extension")]
    InvalidSegmentFilename { filename: String },

    #[error("segment sequence gap: segment {found} appears after {after}")]
    SegmentSequenceGap { after: u64, found: u64 },

    #[error(
        "LSN gap at segment boundary: segment {segment_seq} starts at LSN {found_starting_lsn}, expected {expected_lsn}"
    )]
    LsnGapAtSegmentBoundary {
        segment_seq: u64,
        expected_lsn: u64,
        found_starting_lsn: u64,
    },

    #[error(
        "LSN gap in segment {in_segment}: expected LSN {expected_lsn}, record has {found_lsn}"
    )]
    LsnGap {
        in_segment: u64,
        expected_lsn: u64,
        found_lsn: u64,
    },

    #[error("mid-segment corruption in segment {segment_seq}")]
    MidSegmentCorruption { segment_seq: u64 },

    #[error("record error in segment {in_segment} at expected LSN {expected_lsn}: {source}")]
    RecordError {
        in_segment: u64,
        expected_lsn: u64,
        #[source]
        source: WalRecordError,
    },
}

// ---------------------------------------------------------------------------
// WalReader::open.
// ---------------------------------------------------------------------------

impl WalReader {
    /// Open all `*.wal` segments under `dir`, validating each segment's
    /// header against `shard_uuid` and the v1 format constants.
    pub fn open(dir: impl AsRef<Path>, shard_uuid: [u8; 16]) -> Result<Self, WalReadError> {
        let dir = dir.as_ref();
        let mut infos = Vec::new();

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            // We filter on `.wal` extension; anything else is silently ignored
            // (allows operators to drop `.bak`, `README`, etc. into the dir).
            if path.extension().and_then(|s| s.to_str()) != Some("wal") {
                continue;
            }
            let filename_seq = parse_segment_seq_from_filename(&path)?;
            let info = read_and_validate_segment_header(&path, shard_uuid, filename_seq)?;
            infos.push(info);
        }

        // Sort by segment_seq.
        infos.sort_by_key(|info| info.segment_seq);

        // Validate the seq sequence is contiguous.
        for w in infos.windows(2) {
            if w[1].segment_seq != w[0].segment_seq + 1 {
                return Err(WalReadError::SegmentSequenceGap {
                    after: w[0].segment_seq,
                    found: w[1].segment_seq,
                });
            }
        }

        let expected_next_lsn = infos.first().map_or(1, |s| s.starting_lsn);
        let finished = infos.is_empty();

        Ok(Self {
            segments: infos,
            shard_uuid,
            current_idx: 0,
            current: None,
            expected_next_lsn,
            last_decoded_lsn: None,
            finished,
        })
    }

    /// The segments this reader will scan, in `segment_seq` order.
    #[must_use]
    pub fn segments(&self) -> &[SegmentInfo] {
        &self.segments
    }

    /// Open a reader positioned at `start_lsn`: drops leading segments
    /// whose entire LSN range is below `start_lsn`, then filters records
    /// emitted from the first retained segment so the first record
    /// yielded has `lsn >= start_lsn`.
    ///
    /// Subscribe uses this for the WAL-replay leg of the cutover.
    pub fn iter_from(
        dir: impl AsRef<Path>,
        shard_uuid: [u8; 16],
        start_lsn: u64,
    ) -> Result<IterFrom, WalReadError> {
        let mut reader = Self::open(dir, shard_uuid)?;

        // Drop leading segments whose successor starts at or below
        // start_lsn — their entire payload is below the cutoff. The
        // last retained segment is then the one that may contain
        // start_lsn (or where iteration ends without reaching it).
        let drop_count = reader
            .segments
            .windows(2)
            .take_while(|w| w[1].starting_lsn <= start_lsn)
            .count();
        if drop_count > 0 {
            reader.segments.drain(..drop_count);
        }

        // Reset cursor state to point at the first retained segment.
        reader.current_idx = 0;
        reader.current = None;
        reader.expected_next_lsn = reader
            .segments
            .first()
            .map_or(start_lsn, |s| s.starting_lsn);
        reader.last_decoded_lsn = None;
        reader.finished = reader.segments.is_empty();

        Ok(IterFrom {
            inner: reader,
            start_lsn,
        })
    }

    /// The LSN of the most recently emitted record, or `None` if the reader
    /// hasn't yielded any records yet (or the WAL is empty).
    #[must_use]
    pub fn last_decoded_lsn(&self) -> Option<u64> {
        self.last_decoded_lsn
    }

    /// The LSN the reader expects from the next record. If iteration is
    /// finished, this is `last_decoded_lsn + 1` (or the first segment's
    /// `starting_lsn` if no records were decoded).
    #[must_use]
    pub fn next_expected_lsn(&self) -> u64 {
        self.expected_next_lsn
    }
}

// ---------------------------------------------------------------------------
// Iterator.
// ---------------------------------------------------------------------------

impl Iterator for WalReader {
    type Item = Result<WalRecord, WalReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        loop {
            // (a) Ensure a segment is loaded.
            if self.current.is_none() {
                if self.current_idx >= self.segments.len() {
                    self.finished = true;
                    return None;
                }
                let info = &self.segments[self.current_idx];
                if info.starting_lsn != self.expected_next_lsn {
                    self.finished = true;
                    return Some(Err(WalReadError::LsnGapAtSegmentBoundary {
                        segment_seq: info.segment_seq,
                        expected_lsn: self.expected_next_lsn,
                        found_starting_lsn: info.starting_lsn,
                    }));
                }
                let bytes = match fs::read(&info.path) {
                    Ok(b) => b,
                    Err(e) => {
                        self.finished = true;
                        return Some(Err(e.into()));
                    }
                };
                self.current = Some(LoadedSegment {
                    bytes,
                    cursor: WAL_SEGMENT_HEADER_LEN,
                });
            }

            let seg = self
                .current
                .as_mut()
                .expect("invariant: segment was just loaded into self.current above, or we returned");

            // (b) End of this segment? Move to the next.
            if seg.cursor >= seg.bytes.len() {
                self.current = None;
                self.current_idx += 1;
                continue;
            }

            // (c) Try to decode the next record.
            match WalRecord::decode_one(&seg.bytes[seg.cursor..]) {
                Ok(DecodeOutcome::Record { record, consumed }) => {
                    let lsn = record.lsn.raw();
                    if lsn != self.expected_next_lsn {
                        self.finished = true;
                        return Some(Err(WalReadError::LsnGap {
                            in_segment: self.segments[self.current_idx].segment_seq,
                            expected_lsn: self.expected_next_lsn,
                            found_lsn: lsn,
                        }));
                    }
                    seg.cursor += consumed;
                    self.expected_next_lsn = lsn + 1;
                    self.last_decoded_lsn = Some(lsn);
                    return Some(Ok(record));
                }
                Ok(DecodeOutcome::Truncated) | Err(WalRecordError::CrcMismatch { .. }) => {
                    let is_last = self.current_idx + 1 >= self.segments.len();
                    let segment_seq = self.segments[self.current_idx].segment_seq;
                    self.finished = true;
                    if is_last {
                        // tail truncation is the expected
                        // post-crash case. Log once for diagnostics; the
                        // iterator ends cleanly.
                        tracing::info!(
                            segment_seq,
                            last_lsn = ?self.last_decoded_lsn,
                            "WAL tail truncation (clean end)"
                        );
                        return None;
                    } else {
                        return Some(Err(WalReadError::MidSegmentCorruption { segment_seq }));
                    }
                }
                Err(other) => {
                    // UnknownRecordType / NonZeroReserved / PayloadTooLarge —
                    // not confused with truncation regardless of position.
                    let in_segment = self.segments[self.current_idx].segment_seq;
                    let expected_lsn = self.expected_next_lsn;
                    self.finished = true;
                    return Some(Err(WalReadError::RecordError {
                        in_segment,
                        expected_lsn,
                        source: other,
                    }));
                }
            }
        }
    }
}

impl core::iter::FusedIterator for WalReader {}

/// Iterator returned by [`WalReader::iter_from`]. Yields only records
/// with `lsn >= start_lsn`, in LSN order. Any decoding error from the
/// underlying reader propagates unchanged.
pub struct IterFrom {
    inner: WalReader,
    start_lsn: u64,
}

impl Iterator for IterFrom {
    type Item = Result<WalRecord, WalReadError>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.inner.next()? {
                Ok(rec) if rec.lsn.raw() < self.start_lsn => continue,
                other => return Some(other),
            }
        }
    }
}

impl core::iter::FusedIterator for IterFrom {}

impl core::fmt::Debug for WalReader {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WalReader")
            .field("segments", &self.segments.len())
            .field("current_idx", &self.current_idx)
            .field("expected_next_lsn", &self.expected_next_lsn)
            .field("last_decoded_lsn", &self.last_decoded_lsn)
            .field("finished", &self.finished)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Parse the segment_seq from a `wal/0000000000.wal`-style path. Returns an
/// `InvalidSegmentFilename` error if the stem isn't exactly 10 ASCII digits.
fn parse_segment_seq_from_filename(path: &Path) -> Result<u64, WalReadError> {
    let filename = path.file_name().and_then(|s| s.to_str()).ok_or_else(|| {
        WalReadError::InvalidSegmentFilename {
            filename: path.display().to_string(),
        }
    })?;
    // Strip ".wal".
    let stem =
        filename
            .strip_suffix(".wal")
            .ok_or_else(|| WalReadError::InvalidSegmentFilename {
                filename: filename.to_string(),
            })?;
    if stem.len() != 10 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return Err(WalReadError::InvalidSegmentFilename {
            filename: filename.to_string(),
        });
    }
    stem.parse::<u64>()
        .map_err(|_| WalReadError::InvalidSegmentFilename {
            filename: filename.to_string(),
        })
}

/// Read the first `WAL_SEGMENT_HEADER_LEN` bytes of `path`, validate magic /
/// format / CRC / shard_uuid / filename-vs-header `segment_seq`, and return
/// a `SegmentInfo`.
fn read_and_validate_segment_header(
    path: &Path,
    expected_shard_uuid: [u8; 16],
    filename_seq: u64,
) -> Result<SegmentInfo, WalReadError> {
    let metadata = fs::metadata(path)?;
    let file_size = metadata.len();
    if file_size < WAL_SEGMENT_HEADER_LEN as u64 {
        return Err(WalReadError::SegmentTooSmall {
            path: path.to_path_buf(),
            size: file_size,
        });
    }

    // Read just the header. Avoid `fs::read` on the whole file — segments
    // can be 256 MiB; we'd rather not hold them all in memory during open.
    let header_bytes = read_first_n_bytes(path, WAL_SEGMENT_HEADER_LEN)?;

    let magic: [u8; 4] = header_bytes[0..4]
        .try_into()
        .expect("invariant: slice [0..4] is exactly 4 bytes, header length-checked above");
    if magic != WAL_SEGMENT_MAGIC {
        return Err(WalReadError::InvalidSegmentMagic {
            path: path.to_path_buf(),
            found: magic,
        });
    }

    let format_version = u32::from_le_bytes(
        header_bytes[4..8]
            .try_into()
            .expect("invariant: slice [4..8] is exactly 4 bytes, header length-checked above"),
    );
    if format_version != WAL_SEGMENT_FORMAT_VERSION_V1 {
        return Err(WalReadError::UnsupportedSegmentFormatVersion {
            path: path.to_path_buf(),
            version: format_version,
        });
    }

    // Verify CRC over [0..48].
    let stored_crc = u32::from_le_bytes(
        header_bytes[48..52]
            .try_into()
            .expect("invariant: slice [48..52] is exactly 4 bytes, header length-checked above"),
    );
    let computed = crc32c::crc32c(&header_bytes[0..WAL_SEGMENT_HEADER_CRC_COVERAGE_END]);
    if stored_crc != computed {
        return Err(WalReadError::SegmentHeaderCrcMismatch {
            path: path.to_path_buf(),
            stored: stored_crc,
            computed,
        });
    }

    let header_shard_uuid: [u8; 16] = header_bytes[8..24]
        .try_into()
        .expect("invariant: slice [8..24] is exactly 16 bytes, header length-checked above");
    if header_shard_uuid != expected_shard_uuid {
        return Err(WalReadError::SegmentShardUuidMismatch {
            path: path.to_path_buf(),
            expected: expected_shard_uuid,
            found: header_shard_uuid,
        });
    }

    let header_segment_seq = u64::from_le_bytes(
        header_bytes[24..32]
            .try_into()
            .expect("invariant: slice [24..32] is exactly 8 bytes, header length-checked above"),
    );
    if header_segment_seq != filename_seq {
        return Err(WalReadError::FilenameSegmentSeqMismatch {
            path: path.to_path_buf(),
            filename_seq,
            header_seq: header_segment_seq,
        });
    }

    let starting_lsn = u64::from_le_bytes(
        header_bytes[32..40]
            .try_into()
            .expect("invariant: slice [32..40] is exactly 8 bytes, header length-checked above"),
    );

    Ok(SegmentInfo {
        path: path.to_path_buf(),
        segment_seq: header_segment_seq,
        starting_lsn,
        file_size,
    })
}

fn read_first_n_bytes(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut buf = vec![0u8; n];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

// Tests open WAL segment files. Gated out under miri.
#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::wal::kinds::WalRecordKind;
    use crate::wal::record::{Lsn, WalRecord};
    use crate::wal::segment::WalSegment;

    fn uuid(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn segment_path(dir: &Path, seq: u64) -> PathBuf {
        dir.join(format!("{:010}.wal", seq))
    }

    fn make_record(lsn: u64, kind: WalRecordKind, payload: Vec<u8>) -> WalRecord {
        WalRecord {
            lsn: Lsn(lsn),
            kind,
            flags: 0,
            timestamp_ns: 1_700_000_000_000_000_000,
            agent_id_lo64: 0xAA55_AA55_AA55_AA55,
            payload,
        }
    }

    /// Build one segment with the given records (LSNs must be contiguous and
    /// start at `starting_lsn`). Wraps Glommio for the async WAL ops; returns
    /// when the segment is closed.
    fn write_segment(
        dir: &Path,
        seq: u64,
        starting_lsn: u64,
        shard_uuid: [u8; 16],
        records: &[WalRecord],
    ) {
        let path = segment_path(dir, seq);
        let records: Vec<WalRecord> = records.to_vec();
        crate::wal::segment::glommio_run(move || async move {
            let mut seg = WalSegment::create_new(&path, seq, starting_lsn, shard_uuid)
                .await
                .unwrap();
            for r in &records {
                seg.append_record(r).unwrap();
            }
            seg.flush().await.unwrap();
            seg.close().await.unwrap();
        });
    }

    // ----- Open ---------------------------------------------------------

    #[test]
    fn open_one_empty_segment_yields_no_records() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 1, uuid(1), &[]);
        let mut reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        assert_eq!(reader.segments().len(), 1);
        assert!(reader.next().is_none());
        assert_eq!(reader.last_decoded_lsn(), None);
    }

    #[test]
    fn open_with_wrong_shard_uuid_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 1, uuid(1), &[]);
        let err = WalReader::open(dir.path(), uuid(2)).unwrap_err();
        assert!(
            matches!(err, WalReadError::SegmentShardUuidMismatch { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn open_with_corrupted_magic_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 1, uuid(1), &[]);
        // Corrupt the magic byte 0.
        let path = segment_path(dir.path(), 0);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] = 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        let err = WalReader::open(dir.path(), uuid(1)).unwrap_err();
        assert!(
            matches!(err, WalReadError::InvalidSegmentMagic { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn open_with_segment_sequence_gap_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 1, uuid(1), &[]);
        // Skip seq 1; create seq 2 with starting_lsn 2 (would be valid in
        // sequence, but the seq numbering has a hole).
        write_segment(dir.path(), 2, 2, uuid(1), &[]);
        let err = WalReader::open(dir.path(), uuid(1)).unwrap_err();
        match err {
            WalReadError::SegmentSequenceGap { after, found } => {
                assert_eq!(after, 0);
                assert_eq!(found, 2);
            }
            other => panic!("expected SegmentSequenceGap, got {other:?}"),
        }
    }

    // ----- Round-trip ---------------------------------------------------

    #[test]
    fn write_1000_records_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let records: Vec<WalRecord> = (1..=1000)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![(i & 0xFF) as u8; 16]))
            .collect();
        write_segment(dir.path(), 0, 1, uuid(7), &records);

        let mut reader = WalReader::open(dir.path(), uuid(7)).unwrap();
        let mut count = 0u64;
        for (i, item) in reader.by_ref().enumerate() {
            let record = item.unwrap();
            assert_eq!(record, records[i]);
            count += 1;
        }
        assert_eq!(count, 1000);
        assert_eq!(reader.last_decoded_lsn(), Some(1000));
    }

    #[test]
    fn records_across_two_segments() {
        let dir = tempfile::tempdir().unwrap();
        let seg0: Vec<WalRecord> = (1..=10)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8; 4]))
            .collect();
        let seg1: Vec<WalRecord> = (11..=20)
            .map(|i| make_record(i, WalRecordKind::Forget, vec![i as u8; 4]))
            .collect();
        write_segment(dir.path(), 0, 1, uuid(2), &seg0);
        write_segment(dir.path(), 1, 11, uuid(2), &seg1);

        let mut reader = WalReader::open(dir.path(), uuid(2)).unwrap();
        let mut all = Vec::new();
        for item in reader.by_ref() {
            all.push(item.unwrap());
        }
        let expected: Vec<WalRecord> = seg0.iter().chain(seg1.iter()).cloned().collect();
        assert_eq!(all, expected);
        assert_eq!(reader.last_decoded_lsn(), Some(20));
    }

    #[test]
    fn records_across_three_segments() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(
            dir.path(),
            0,
            1,
            uuid(3),
            &(1..=5)
                .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8]))
                .collect::<Vec<_>>(),
        );
        write_segment(
            dir.path(),
            1,
            6,
            uuid(3),
            &(6..=10)
                .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8]))
                .collect::<Vec<_>>(),
        );
        write_segment(
            dir.path(),
            2,
            11,
            uuid(3),
            &(11..=15)
                .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8]))
                .collect::<Vec<_>>(),
        );
        let reader = WalReader::open(dir.path(), uuid(3)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, (1..=15).collect::<Vec<_>>());
    }

    // ----- Tail truncation ----------------------------------------------

    #[test]
    fn tail_truncation_ends_iterator_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let records: Vec<WalRecord> = (1..=5)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8; 16]))
            .collect();
        write_segment(dir.path(), 0, 1, uuid(4), &records);

        // Truncate the file mid-way through the 5th record.
        let path = segment_path(dir.path(), 0);
        let current_size = std::fs::metadata(&path).unwrap().len();
        // Last record's encoded_len is ~ 32 + 16 + 8 = 56 bytes; chop off
        // the trailing 30 bytes (which slices well into the last record).
        let new_size = current_size - 30;
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(new_size).unwrap();

        let mut reader = WalReader::open(dir.path(), uuid(4)).unwrap();
        let mut got = Vec::new();
        for item in reader.by_ref() {
            got.push(item.unwrap());
        }
        assert_eq!(got.len(), 4, "the partial 5th record is dropped");
        assert_eq!(got, records[0..4]);
        assert_eq!(reader.last_decoded_lsn(), Some(4));
    }

    #[test]
    fn last_record_crc_corruption_ends_iterator_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let records: Vec<WalRecord> = (1..=3)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8; 8]))
            .collect();
        write_segment(dir.path(), 0, 1, uuid(5), &records);

        // Corrupt the last record's stored CRC (last 8 bytes are the footer:
        // 4 bytes CRC + 4 bytes reserved). Flip the first CRC byte.
        let path = segment_path(dir.path(), 0);
        let mut bytes = std::fs::read(&path).unwrap();
        let footer_crc_off = bytes.len() - 8;
        bytes[footer_crc_off] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = WalReader::open(dir.path(), uuid(5)).unwrap();
        let mut got = Vec::new();
        for item in reader.by_ref() {
            got.push(item.unwrap());
        }
        // First 2 records survive; the 3rd is dropped (CRC mismatch on the
        // last segment's last record → "assume truncation").
        assert_eq!(got.len(), 2);
        assert_eq!(reader.last_decoded_lsn(), Some(2));
    }

    // ----- Mid-segment corruption ---------------------------------------

    #[test]
    fn mid_segment_truncation_is_error() {
        let dir = tempfile::tempdir().unwrap();
        // Segment 0 with 5 records; segment 1 with 5 more.
        let seg0: Vec<WalRecord> = (1..=5)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8; 8]))
            .collect();
        let seg1: Vec<WalRecord> = (6..=10)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8; 8]))
            .collect();
        write_segment(dir.path(), 0, 1, uuid(6), &seg0);
        write_segment(dir.path(), 1, 6, uuid(6), &seg1);

        // Truncate segment 0 mid-stream (cut to its first record + a few
        // bytes into the second).
        let path = segment_path(dir.path(), 0);
        let header_plus_one_and_a_half = WAL_SEGMENT_HEADER_LEN as u64
            + seg0[0].encoded_len() as u64
            + (seg0[1].encoded_len() as u64) / 2;
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(header_plus_one_and_a_half).unwrap();

        let mut reader = WalReader::open(dir.path(), uuid(6)).unwrap();
        // First record decodes fine.
        let first = reader.next().unwrap().unwrap();
        assert_eq!(first, seg0[0]);
        // Next decode attempt hits the truncation; because segment 0 isn't
        // the last segment, this is mid-segment corruption.
        let err = reader.next().unwrap().unwrap_err();
        assert!(
            matches!(err, WalReadError::MidSegmentCorruption { segment_seq: 0 }),
            "got {err:?}"
        );
        // Fused: further calls return None.
        assert!(reader.next().is_none());
    }

    #[test]
    fn mid_segment_crc_corruption_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let seg0: Vec<WalRecord> = (1..=5)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8; 8]))
            .collect();
        let seg1: Vec<WalRecord> = (6..=10)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8; 8]))
            .collect();
        write_segment(dir.path(), 0, 1, uuid(7), &seg0);
        write_segment(dir.path(), 1, 6, uuid(7), &seg1);

        // Corrupt the 3rd record's payload byte in segment 0. Compute its
        // offset: header + sum(encoded_len of records 0..2) + 32 (record
        // header) + 0 (first payload byte of record 2).
        let path = segment_path(dir.path(), 0);
        let mut bytes = std::fs::read(&path).unwrap();
        let target_offset = WAL_SEGMENT_HEADER_LEN
            + seg0[0].encoded_len()
            + seg0[1].encoded_len()
            + crate::wal::record::HEADER_LEN;
        bytes[target_offset] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = WalReader::open(dir.path(), uuid(7)).unwrap();
        // Records 0 and 1 succeed.
        assert_eq!(reader.next().unwrap().unwrap().lsn.raw(), 1);
        assert_eq!(reader.next().unwrap().unwrap().lsn.raw(), 2);
        // Record 2 hits the corruption; segment 0 isn't last → MidSegmentCorruption.
        let err = reader.next().unwrap().unwrap_err();
        assert!(
            matches!(err, WalReadError::MidSegmentCorruption { segment_seq: 0 }),
            "got {err:?}"
        );
    }

    // ----- LSN ordering -------------------------------------------------

    #[test]
    fn lsn_gap_within_segment_is_error() {
        let dir = tempfile::tempdir().unwrap();
        // Hand-build a segment with records LSN 1, 2, 4 (gap).
        let records = vec![
            make_record(1, WalRecordKind::Encode, vec![1; 4]),
            make_record(2, WalRecordKind::Encode, vec![2; 4]),
            make_record(4, WalRecordKind::Encode, vec![4; 4]),
        ];
        write_segment(dir.path(), 0, 1, uuid(8), &records);

        let mut reader = WalReader::open(dir.path(), uuid(8)).unwrap();
        assert_eq!(reader.next().unwrap().unwrap().lsn.raw(), 1);
        assert_eq!(reader.next().unwrap().unwrap().lsn.raw(), 2);
        let err = reader.next().unwrap().unwrap_err();
        assert!(
            matches!(
                err,
                WalReadError::LsnGap {
                    expected_lsn: 3,
                    found_lsn: 4,
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn lsn_gap_across_segments_is_error() {
        let dir = tempfile::tempdir().unwrap();
        // Segment 0: LSN 1..=3. Segment 1: starting_lsn = 5 (skips 4).
        let seg0 = (1..=3)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8; 4]))
            .collect::<Vec<_>>();
        let seg1 = (5..=7)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![i as u8; 4]))
            .collect::<Vec<_>>();
        write_segment(dir.path(), 0, 1, uuid(9), &seg0);
        write_segment(dir.path(), 1, 5, uuid(9), &seg1);

        let mut reader = WalReader::open(dir.path(), uuid(9)).unwrap();
        // First three records decode.
        for expected_lsn in 1..=3 {
            assert_eq!(reader.next().unwrap().unwrap().lsn.raw(), expected_lsn);
        }
        // At the segment boundary, expected_next_lsn = 4 but seg1.starting_lsn = 5.
        let err = reader.next().unwrap().unwrap_err();
        assert!(
            matches!(
                err,
                WalReadError::LsnGapAtSegmentBoundary {
                    segment_seq: 1,
                    expected_lsn: 4,
                    found_starting_lsn: 5,
                }
            ),
            "got {err:?}"
        );
    }

    // ----- iter_from (subscribe replay) --------------------------------

    #[test]
    fn iter_from_zero_yields_everything() {
        let dir = tempfile::tempdir().unwrap();
        let recs = (1..=5u64)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![0xAB; 8]))
            .collect::<Vec<_>>();
        write_segment(dir.path(), 0, 1, uuid(1), &recs);
        let iter = WalReader::iter_from(dir.path(), uuid(1), 0).unwrap();
        let lsns: Vec<u64> = iter.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn iter_from_filters_records_below_start_within_single_segment() {
        let dir = tempfile::tempdir().unwrap();
        let recs = (1..=5u64)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![0xAB; 8]))
            .collect::<Vec<_>>();
        write_segment(dir.path(), 0, 1, uuid(1), &recs);
        let iter = WalReader::iter_from(dir.path(), uuid(1), 3).unwrap();
        let lsns: Vec<u64> = iter.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, vec![3, 4, 5]);
    }

    #[test]
    fn iter_from_drops_leading_segments_fully_below_start() {
        // Two segments: seg 0 carries LSNs 1-3, seg 1 carries LSNs 4-6.
        // start_lsn=4 must skip seg 0 entirely (no read of those record
        // bytes) and yield 4..=6 from seg 1.
        let dir = tempfile::tempdir().unwrap();
        let recs_a = (1..=3u64)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![0xAB; 8]))
            .collect::<Vec<_>>();
        let recs_b = (4..=6u64)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![0xAB; 8]))
            .collect::<Vec<_>>();
        write_segment(dir.path(), 0, 1, uuid(1), &recs_a);
        write_segment(dir.path(), 1, 4, uuid(1), &recs_b);
        let iter = WalReader::iter_from(dir.path(), uuid(1), 4).unwrap();
        let lsns: Vec<u64> = iter.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, vec![4, 5, 6]);
    }

    #[test]
    fn iter_from_past_tail_yields_nothing() {
        // start_lsn > all on-disk LSNs: subscribe expects this to be a
        // no-op replay (caller transitions straight to live).
        let dir = tempfile::tempdir().unwrap();
        let recs = (1..=3u64)
            .map(|i| make_record(i, WalRecordKind::Encode, vec![0xAB; 8]))
            .collect::<Vec<_>>();
        write_segment(dir.path(), 0, 1, uuid(1), &recs);
        let iter = WalReader::iter_from(dir.path(), uuid(1), 100).unwrap();
        let collected: Vec<_> = iter.collect();
        assert!(
            collected.is_empty(),
            "expected empty replay, got {collected:?}"
        );
    }

    // ----- Filename hygiene --------------------------------------------

    #[test]
    fn filename_segment_seq_mismatch_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // Create a segment with header.segment_seq = 5, then rename the
        // file to look like seq 0.
        let path_5 = segment_path(dir.path(), 5);
        let path_5_c = path_5.clone();
        crate::wal::segment::glommio_run(move || async move {
            let seg = WalSegment::create_new(&path_5_c, 5, 1, uuid(1))
                .await
                .unwrap();
            seg.close().await.unwrap();
        });
        let path_0 = segment_path(dir.path(), 0);
        std::fs::rename(&path_5, &path_0).unwrap();

        let err = WalReader::open(dir.path(), uuid(1)).unwrap_err();
        assert!(
            matches!(
                err,
                WalReadError::FilenameSegmentSeqMismatch {
                    filename_seq: 0,
                    header_seq: 5,
                    ..
                }
            ),
            "got {err:?}"
        );
    }
}
