//! WAL segment writer.
//!
//! See `spec/05_storage_arena_wal/04_wal_overview.md` and `05_wal_records.md`
//! §1 (segment header) + §17 (record packing).
//!
//! A `WalSegment` owns one `*.wal` file. The file starts with a 4 KB header
//! (`spec §05/05 §1`) followed by `WalRecord`s packed back-to-back
//! (`spec §05/05 §17`).
//!
//! ## What's *not* in this layer
//!
//! - **No fsync.** `flush` calls `File::write_all` only. Per phase doc 2.6,
//!   durability lands in sub-task 2.8 with `pwritev2(RWF_DSYNC)` and group
//!   commit. Records written here survive a normal process exit but not a
//!   kernel panic / power loss.
//! - **No `fallocate` / `O_DIRECT`** — also 2.8.
//! - **No reader / open-existing.** The recovery path that finds the tail
//!   of a crashed segment is sub-task 2.10; iterating records is
//!   `WalReader` in 2.7.
//! - **No LSN allocation.** Records arrive with their `lsn` field already
//!   set by the caller. LSN allocation that spans segment rollovers lives
//!   in the `Wal` public type (sub-task 2.9).
//! - **No rollover decision.** The segment exposes `size_bytes()` and
//!   `is_full()`; the manager (2.9) decides when to create the next
//!   segment.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::wal::record::WalRecord;

/// `RWF_DSYNC` value from Linux UAPI (`include/uapi/linux/fs.h`).
/// Defined locally for portability across libc versions.
const RWF_DSYNC: i32 = 0x2;

/// Test-only counter: every call to `flush_durable` bumps this. Used by the
/// 2.8 group-commit batching test to verify that N concurrent appends are
/// coalesced into a small number of fsyncs.
#[cfg(test)]
pub static FLUSH_DURABLE_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// Constants.
// ---------------------------------------------------------------------------

/// Segment header size in bytes (spec §05/05 §1).
pub const WAL_SEGMENT_HEADER_LEN: usize = 4096;

/// Magic bytes at offset 0 of every segment header.
pub const WAL_SEGMENT_MAGIC: [u8; 4] = *b"BWAL";

/// Format version written into new segments.
pub const WAL_SEGMENT_FORMAT_VERSION_V1: u32 = 1;

/// End of the CRC-covered region within the segment header.
///
/// Unlike the slot CRC (`[0..40]` after typo-reading), the arena header CRC
/// (`[0..80]` after typo-reading), and the WAL record CRC (header + payload),
/// the segment header CRC's coverage is unambiguous: every field before
/// `header_crc32c` itself ends exactly at offset 48.
pub const WAL_SEGMENT_HEADER_CRC_COVERAGE_END: usize = 48;

// ---------------------------------------------------------------------------
// Header.
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct WalSegmentHeaderRaw {
    magic: [u8; 4],
    format_version: u32,
    shard_uuid: [u8; 16],
    segment_seq: u64,
    starting_lsn: u64,
    created_at_unix_nanos: u64,
    header_crc32c: u32,
    reserved: [u8; 4044],
}

// SAFETY: `#[repr(C)]`, all Pod fields, no implicit padding (verified by the
// const_asserts below). Every bit pattern of every field is a valid value.
unsafe impl bytemuck::Zeroable for WalSegmentHeaderRaw {}
unsafe impl bytemuck::Pod for WalSegmentHeaderRaw {}

// Layout invariants enforced at compile time. Any future field reorder that
// introduces padding fails to build.
const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<WalSegmentHeaderRaw>() == WAL_SEGMENT_HEADER_LEN);
    assert!(align_of::<WalSegmentHeaderRaw>() == 8);
    assert!(offset_of!(WalSegmentHeaderRaw, magic) == 0);
    assert!(offset_of!(WalSegmentHeaderRaw, format_version) == 4);
    assert!(offset_of!(WalSegmentHeaderRaw, shard_uuid) == 8);
    assert!(offset_of!(WalSegmentHeaderRaw, segment_seq) == 24);
    assert!(offset_of!(WalSegmentHeaderRaw, starting_lsn) == 32);
    assert!(offset_of!(WalSegmentHeaderRaw, created_at_unix_nanos) == 40);
    assert!(offset_of!(WalSegmentHeaderRaw, header_crc32c) == 48);
    assert!(offset_of!(WalSegmentHeaderRaw, reserved) == 52);
};

fn unix_nanos_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn compute_segment_header_crc(header: &WalSegmentHeaderRaw) -> u32 {
    let bytes: &[u8] = bytemuck::bytes_of(header);
    crc32c::crc32c(&bytes[0..WAL_SEGMENT_HEADER_CRC_COVERAGE_END])
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum WalSegmentError {
    #[error("WAL segment io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("short write: wanted {wanted} bytes, got {got}")]
    ShortWrite { wanted: usize, got: usize },
}

// ---------------------------------------------------------------------------
// WalSegment.
// ---------------------------------------------------------------------------

/// One append-only `*.wal` segment file.
pub struct WalSegment {
    file: File,
    path: PathBuf,
    segment_seq: u64,
    starting_lsn: u64,
    shard_uuid: [u8; 16],
    /// Records appended but not yet flushed to the file.
    write_buf: Vec<u8>,
    /// Bytes already written to the file *excluding the 4 KB header*.
    /// `size_bytes() = WAL_SEGMENT_HEADER_LEN + bytes_on_disk + write_buf.len()`.
    bytes_on_disk: usize,
}

impl WalSegment {
    /// Create a new segment file at `path` with a fresh 4 KB header.
    ///
    /// Refuses to clobber: returns an `io::ErrorKind::AlreadyExists` error
    /// if the file already exists.
    pub fn create_new(
        path: impl AsRef<Path>,
        segment_seq: u64,
        starting_lsn: u64,
        shard_uuid: [u8; 16],
    ) -> Result<Self, WalSegmentError> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;

        // Build the header, compute CRC, write the 4 KB block.
        let mut header = WalSegmentHeaderRaw {
            magic: WAL_SEGMENT_MAGIC,
            format_version: WAL_SEGMENT_FORMAT_VERSION_V1,
            shard_uuid,
            segment_seq,
            starting_lsn,
            created_at_unix_nanos: unix_nanos_now(),
            header_crc32c: 0,
            reserved: [0; 4044],
        };
        header.header_crc32c = compute_segment_header_crc(&header);

        // SAFETY: `WalSegmentHeaderRaw` is `Pod`; bytes_of yields a slice
        // exactly `WAL_SEGMENT_HEADER_LEN` long.
        let header_bytes = bytemuck::bytes_of(&header);
        debug_assert_eq!(header_bytes.len(), WAL_SEGMENT_HEADER_LEN);
        file.write_all(header_bytes)?;
        // Cursor is now at offset 4096, ready for record appends.

        Ok(Self {
            file,
            path,
            segment_seq,
            starting_lsn,
            shard_uuid,
            write_buf: Vec::new(),
            bytes_on_disk: 0,
        })
    }

    #[must_use]
    pub fn segment_seq(&self) -> u64 {
        self.segment_seq
    }

    #[must_use]
    pub fn starting_lsn(&self) -> u64 {
        self.starting_lsn
    }

    #[must_use]
    pub fn shard_uuid(&self) -> [u8; 16] {
        self.shard_uuid
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Total bytes occupied by this segment: header + records on disk +
    /// records still buffered.
    #[must_use]
    pub fn size_bytes(&self) -> usize {
        WAL_SEGMENT_HEADER_LEN + self.bytes_on_disk + self.write_buf.len()
    }

    /// True iff appending another record would push the segment beyond
    /// `WAL_SEGMENT_SIZE_BYTES`. The manager (sub-task 2.9) uses this to
    /// decide rollover.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.size_bytes() >= crate::WAL_SEGMENT_SIZE_BYTES
    }

    /// Append a record's bytes to the in-memory buffer. Returns the number
    /// of bytes the record contributed.
    ///
    /// Does *not* hit disk. The caller (or the manager in 2.9) calls
    /// `flush` to drain the buffer; durability happens later in 2.8 with
    /// `pwritev2(RWF_DSYNC)`.
    pub fn append_record(&mut self, record: &WalRecord) -> Result<usize, WalSegmentError> {
        let before = self.write_buf.len();
        record.encode_into(&mut self.write_buf);
        Ok(self.write_buf.len() - before)
    }

    /// Drain the in-memory buffer to the file via `write_all`.
    ///
    /// **No fsync.** Records are in the page cache, not on stable storage.
    /// Crash durability lands via [`Self::flush_durable`].
    ///
    /// Idempotent on an empty buffer (no-op, returns `Ok`).
    pub fn flush(&mut self) -> Result<(), WalSegmentError> {
        if self.write_buf.is_empty() {
            return Ok(());
        }
        self.file.write_all(&self.write_buf)?;
        self.bytes_on_disk += self.write_buf.len();
        self.write_buf.clear();
        Ok(())
    }

    /// Drain the in-memory buffer to the file *durably* via
    /// `pwritev2(RWF_DSYNC)`. The kernel guarantees the data is on stable
    /// storage before returning.
    ///
    /// Uses an explicit file offset (`HEADER_LEN + bytes_on_disk`) rather
    /// than the file's seek cursor, so it composes cleanly with the
    /// non-durable [`Self::flush`] above. The cursor is updated post-write
    /// to keep both paths interoperable (otherwise a subsequent
    /// `flush` would write at a stale cursor).
    ///
    /// Spec deviation: this path uses synchronous `pwritev2` rather than
    /// the spec's prescribed `io_uring` (SD-2.8-2 in `docs/spec-deviations.md`).
    /// Functional behavior is equivalent; only the submission shape differs.
    pub fn flush_durable(&mut self) -> Result<(), WalSegmentError> {
        if self.write_buf.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        FLUSH_DURABLE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let fd = self.file.as_raw_fd();
        let offset =
            i64::try_from(WAL_SEGMENT_HEADER_LEN + self.bytes_on_disk).expect("offset fits in i64");
        let wanted = self.write_buf.len();
        let iov = libc::iovec {
            iov_base: self.write_buf.as_ptr() as *mut c_void,
            iov_len: wanted,
        };
        // SAFETY: `fd` is owned by `self.file` and valid for the duration of
        // this call. `iov` points at `self.write_buf`, which lives until the
        // end of this function. iovcnt=1 matches the single iovec. The
        // RWF_DSYNC flag (0x2) is documented in Linux UAPI fs.h.
        let n = unsafe { libc::pwritev2(fd, &iov, 1, offset, RWF_DSYNC) };
        if n < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let got = n as usize;
        if got != wanted {
            return Err(WalSegmentError::ShortWrite { wanted, got });
        }

        // Keep the file's seek cursor in sync with `bytes_on_disk` so a
        // future call to `flush` (cursor-based) doesn't overwrite the
        // durably-written region. Best-effort; failure is non-fatal because
        // the durable write already succeeded.
        use std::io::Seek;
        let _ = self
            .file
            .seek(std::io::SeekFrom::Start((offset as u64) + got as u64));

        self.bytes_on_disk += got;
        self.write_buf.clear();
        Ok(())
    }

    /// Number of bytes currently buffered (not yet flushed). The
    /// `GroupCommitter` uses this to decide when to fire a size-based flush.
    #[must_use]
    pub fn write_buf_len(&self) -> usize {
        self.write_buf.len()
    }
}

impl core::fmt::Debug for WalSegment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WalSegment")
            .field("path", &self.path)
            .field("segment_seq", &self.segment_seq)
            .field("starting_lsn", &self.starting_lsn)
            .field("shard_uuid", &self.shard_uuid)
            .field("bytes_on_disk", &self.bytes_on_disk)
            .field("buffered", &self.write_buf.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::kinds::WalRecordKind;
    use crate::wal::record::{DecodeOutcome, Lsn, WalRecord};
    use std::io;

    fn uuid(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn sample(lsn: u64, kind: WalRecordKind, payload: Vec<u8>) -> WalRecord {
        WalRecord {
            lsn: Lsn(lsn),
            kind,
            flags: 0,
            timestamp_ns: 1_700_000_000_000_000_000,
            agent_id_lo64: 0xDEAD_BEEF_CAFE_F00D,
            payload,
        }
    }

    #[test]
    fn create_new_writes_valid_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let seg = WalSegment::create_new(&path, 0, 1, uuid(7)).unwrap();
        assert_eq!(seg.segment_seq(), 0);
        assert_eq!(seg.starting_lsn(), 1);
        assert_eq!(seg.shard_uuid(), uuid(7));

        // The file is exactly the header size: 4096 bytes.
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), WAL_SEGMENT_HEADER_LEN as u64);

        // Read the header back; verify field-by-field.
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], &WAL_SEGMENT_MAGIC);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);
        assert_eq!(&bytes[8..24], &uuid(7));
        assert_eq!(u64::from_le_bytes(bytes[24..32].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(bytes[32..40].try_into().unwrap()), 1);

        // Reserved tail (offsets 52..4096) is all zero.
        assert!(bytes[52..4096].iter().all(|&b| b == 0));

        // CRC verifies over [0..48].
        let stored_crc = u32::from_le_bytes(bytes[48..52].try_into().unwrap());
        let computed = crc32c::crc32c(&bytes[0..WAL_SEGMENT_HEADER_CRC_COVERAGE_END]);
        assert_eq!(stored_crc, computed);
    }

    #[test]
    fn create_new_refuses_to_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let _seg = WalSegment::create_new(&path, 0, 1, uuid(1)).unwrap();
        let err = WalSegment::create_new(&path, 0, 1, uuid(1)).unwrap_err();
        match err {
            WalSegmentError::Io(io_err) => {
                assert_eq!(io_err.kind(), io::ErrorKind::AlreadyExists);
            }
            other => panic!("expected Io(AlreadyExists), got {other:?}"),
        }
    }

    #[test]
    fn append_does_not_touch_disk_until_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let mut seg = WalSegment::create_new(&path, 0, 1, uuid(1)).unwrap();

        let rec = sample(1, WalRecordKind::Encode, vec![0xAA; 32]);
        let appended = seg.append_record(&rec).unwrap();
        assert_eq!(appended, rec.encoded_len());

        // File on disk is still just the header.
        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert_eq!(on_disk, WAL_SEGMENT_HEADER_LEN as u64);

        // size_bytes() includes the buffered record.
        assert_eq!(seg.size_bytes(), WAL_SEGMENT_HEADER_LEN + rec.encoded_len());
    }

    #[test]
    fn flush_on_empty_buffer_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let mut seg = WalSegment::create_new(&path, 0, 1, uuid(1)).unwrap();
        seg.flush().unwrap();
        seg.flush().unwrap();
        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert_eq!(on_disk, WAL_SEGMENT_HEADER_LEN as u64);
        assert_eq!(seg.size_bytes(), WAL_SEGMENT_HEADER_LEN);
    }

    #[test]
    fn records_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let mut seg = WalSegment::create_new(&path, 0, 1, uuid(2)).unwrap();

        let r1 = sample(1, WalRecordKind::Encode, vec![1; 16]);
        let r2 = sample(2, WalRecordKind::Forget, vec![2; 8]);
        let r3 = sample(3, WalRecordKind::Link, vec![3; 38]);
        seg.append_record(&r1).unwrap();
        seg.append_record(&r2).unwrap();
        seg.append_record(&r3).unwrap();
        seg.flush().unwrap();

        // File size = header + sum of encoded record lengths.
        let expected_size =
            WAL_SEGMENT_HEADER_LEN + r1.encoded_len() + r2.encoded_len() + r3.encoded_len();
        let on_disk = std::fs::metadata(&path).unwrap().len() as usize;
        assert_eq!(on_disk, expected_size);

        // Read raw bytes and decode every record using WalRecord::decode_one.
        let bytes = std::fs::read(&path).unwrap();
        let mut cursor = WAL_SEGMENT_HEADER_LEN;
        for expected in [&r1, &r2, &r3] {
            let outcome = WalRecord::decode_one(&bytes[cursor..]).unwrap();
            match outcome {
                DecodeOutcome::Record { record, consumed } => {
                    assert_eq!(&record, expected);
                    cursor += consumed;
                }
                DecodeOutcome::Truncated => panic!("unexpected truncation"),
            }
        }
        assert_eq!(cursor, on_disk, "all bytes after header consumed");
    }

    #[test]
    fn mixed_append_flush_sequence_preserves_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let mut seg = WalSegment::create_new(&path, 0, 1, uuid(3)).unwrap();

        let r1 = sample(1, WalRecordKind::Encode, vec![1; 4]);
        let r2 = sample(2, WalRecordKind::Encode, vec![2; 4]);
        let r3 = sample(3, WalRecordKind::Encode, vec![3; 4]);

        seg.append_record(&r1).unwrap();
        seg.flush().unwrap();
        seg.append_record(&r2).unwrap();
        seg.append_record(&r3).unwrap();
        seg.flush().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let mut cursor = WAL_SEGMENT_HEADER_LEN;
        for expected in [&r1, &r2, &r3] {
            let DecodeOutcome::Record { record, consumed } =
                WalRecord::decode_one(&bytes[cursor..]).unwrap()
            else {
                panic!("expected record");
            };
            assert_eq!(&record, expected);
            cursor += consumed;
        }
        assert_eq!(cursor, bytes.len());
    }

    #[test]
    fn size_bytes_accounts_for_buffered_and_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let mut seg = WalSegment::create_new(&path, 0, 1, uuid(4)).unwrap();
        let r1 = sample(1, WalRecordKind::Encode, vec![0; 100]);
        let r2 = sample(2, WalRecordKind::Encode, vec![0; 50]);

        seg.append_record(&r1).unwrap();
        // Buffered, not on disk.
        assert_eq!(seg.size_bytes(), WAL_SEGMENT_HEADER_LEN + r1.encoded_len());
        seg.flush().unwrap();
        // Same total, now all on disk.
        assert_eq!(seg.size_bytes(), WAL_SEGMENT_HEADER_LEN + r1.encoded_len());

        seg.append_record(&r2).unwrap();
        // Total = header + r1 (on disk) + r2 (buffered).
        assert_eq!(
            seg.size_bytes(),
            WAL_SEGMENT_HEADER_LEN + r1.encoded_len() + r2.encoded_len()
        );
    }

    #[test]
    fn is_full_reflects_size_bytes() {
        // Pure math regression: is_full() == (size_bytes() >= WAL_SEGMENT_SIZE_BYTES).
        // We can't realistically grow a segment to 256 MiB in a unit test,
        // but we can confirm a fresh segment isn't full.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let seg = WalSegment::create_new(&path, 0, 1, uuid(5)).unwrap();
        assert!(!seg.is_full());
        assert_eq!(
            seg.is_full(),
            seg.size_bytes() >= crate::WAL_SEGMENT_SIZE_BYTES
        );
    }
}
