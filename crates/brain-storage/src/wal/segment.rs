//! WAL segment writer.
//!
//! See `spec/05_storage_arena_wal/04_wal_overview.md` and `05_wal_records.md`
//! §1 (segment header) + §17 (record packing).
//!
//! A `WalSegment` owns one `*.wal` file. The file starts with a 4 KB header
//! (`spec §05/05 §1`) followed by `WalRecord`s packed back-to-back
//! (`spec §05/05 §17`).
//!
//! ## I/O model (sub-task 9.6a)
//!
//! Backed by `glommio::io::BufferedFile`: all open/write/fsync ops go through
//! io_uring on the shard's executor. Durability is `write_at` + `fdatasync`
//! (two io_uring syscalls) — see `docs/spec-deviations.md` SD-2.8-2-b for
//! the rationale vs. the spec's single `pwritev2(RWF_DSYNC)` syscall.
//!
//! ## What's *not* in this layer
//!
//! - **No `O_DIRECT`** — see SD-2.8-1 (still open).
//! - **No reader / open-existing.** The recovery path is `WalReader`
//!   (sub-task 2.7); it reads via `mmap` and stays sync.
//! - **No LSN allocation.** Records arrive with their `lsn` field already
//!   set by the caller. LSN allocation lives in `Wal`.
//! - **No rollover decision.** The segment exposes `size_bytes()` and
//!   `is_full()`; `Wal` decides when to create the next segment.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use glommio::io::{BufferedFile, OpenOptions};

use crate::wal::record::WalRecord;

/// Test-only counter: every successful call to `flush_durable` bumps this.
/// Used by the 2.8 group-commit batching test to verify that N concurrent
/// appends are coalesced into a small number of fsyncs.
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

// Layout invariants enforced at compile time.
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

impl<T> From<glommio::GlommioError<T>> for WalSegmentError {
    fn from(e: glommio::GlommioError<T>) -> Self {
        Self::Io(e.into())
    }
}

// ---------------------------------------------------------------------------
// WalSegment.
// ---------------------------------------------------------------------------

/// One append-only `*.wal` segment file.
///
/// `!Send` / `!Sync` via `BufferedFile` — must live on the executor that
/// opened it. Spec §10/02 single-writer-per-shard is preserved by construction.
pub struct WalSegment {
    file: BufferedFile,
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
    /// Refuses to clobber if the file exists (returns `io::ErrorKind::AlreadyExists`).
    /// Must be called from inside a Glommio executor.
    pub async fn create_new(
        path: impl AsRef<Path>,
        segment_seq: u64,
        starting_lsn: u64,
        shard_uuid: [u8; 16],
    ) -> Result<Self, WalSegmentError> {
        let path = path.as_ref().to_path_buf();

        if path.exists() {
            return Err(WalSegmentError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("segment file already exists at {}", path.display()),
            )));
        }

        let file = BufferedFile::create(&path).await?;

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

        // BufferedFile::write_at takes the buffer by value (io_uring keeps it
        // for the duration of the operation). Make a fresh Vec for the header.
        let header_bytes = bytemuck::bytes_of(&header).to_vec();
        debug_assert_eq!(header_bytes.len(), WAL_SEGMENT_HEADER_LEN);
        let wanted = header_bytes.len();
        let got = file.write_at(header_bytes, 0).await?;
        if got != wanted {
            return Err(WalSegmentError::ShortWrite { wanted, got });
        }
        // Durable header (spec §05/06 §7 step 3).
        file.fdatasync().await?;

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
    /// `WAL_SEGMENT_SIZE_BYTES`.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.size_bytes() >= crate::WAL_SEGMENT_SIZE_BYTES
    }

    /// Append a record's bytes to the in-memory buffer. Returns the number
    /// of bytes the record contributed. Pure in-memory; does *not* touch disk.
    pub fn append_record(&mut self, record: &WalRecord) -> Result<usize, WalSegmentError> {
        let before = self.write_buf.len();
        record.encode_into(&mut self.write_buf);
        Ok(self.write_buf.len() - before)
    }

    /// Drain the in-memory buffer to the file via `write_at` (no fsync).
    /// Records are in the kernel's page cache, not on stable storage.
    /// Idempotent on an empty buffer.
    pub async fn flush(&mut self) -> Result<(), WalSegmentError> {
        if self.write_buf.is_empty() {
            return Ok(());
        }
        let offset = (WAL_SEGMENT_HEADER_LEN + self.bytes_on_disk) as u64;
        let buf = std::mem::take(&mut self.write_buf);
        let wanted = buf.len();
        let got = self.file.write_at(buf, offset).await?;
        if got != wanted {
            return Err(WalSegmentError::ShortWrite { wanted, got });
        }
        self.bytes_on_disk += got;
        Ok(())
    }

    /// Drain the in-memory buffer to the file **durably** via `write_at` +
    /// `fdatasync` (both via io_uring). The kernel guarantees the data is on
    /// stable storage before returning.
    ///
    /// Idempotent on an empty buffer.
    ///
    /// **Spec deviation (SD-2.8-2-b):** two-syscall fsync vs. the spec's
    /// single `pwritev2(RWF_DSYNC)`. Glommio's typed BufferedFile API does
    /// not expose `RWF_DSYNC`; the equivalent is `write_at` + `fdatasync`.
    /// Same durability guarantee; one extra syscall per batch.
    pub async fn flush_durable(&mut self) -> Result<(), WalSegmentError> {
        if self.write_buf.is_empty() {
            return Ok(());
        }

        let offset = (WAL_SEGMENT_HEADER_LEN + self.bytes_on_disk) as u64;
        let buf = std::mem::take(&mut self.write_buf);
        let wanted = buf.len();
        let got = self.file.write_at(buf, offset).await?;
        if got != wanted {
            return Err(WalSegmentError::ShortWrite { wanted, got });
        }
        self.file.fdatasync().await?;

        self.bytes_on_disk += got;

        #[cfg(test)]
        FLUSH_DURABLE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        Ok(())
    }

    /// Number of bytes currently buffered (not yet flushed). The
    /// `GroupCommitter` uses this to decide when to fire a size-based flush.
    #[must_use]
    pub fn write_buf_len(&self) -> usize {
        self.write_buf.len()
    }

    /// Close the underlying file. Best-effort fdatasync of any pending
    /// kernel-side writeback. Called on drop / on rollover.
    pub async fn close(self) -> Result<(), WalSegmentError> {
        self.file.close().await?;
        Ok(())
    }

    /// Open an existing segment file for append. The caller is responsible
    /// for header validation — typically by calling `WalReader::open` first
    /// (which validates every segment's header on the caller thread).
    ///
    /// `bytes_on_disk` is derived from the file's on-disk size at open
    /// time (`file_size - HEADER_LEN`). The caller is responsible for
    /// ensuring the file's contents up to that point are CRC-valid records
    /// (typically by running [`crate::recovery::recover`] first).
    ///
    /// Used by [`crate::wal::wal::Wal::open_existing`] when resuming a
    /// shard after a clean shutdown or crash.
    pub async fn open_for_append(
        path: impl AsRef<Path>,
        shard_uuid: [u8; 16],
        segment_seq: u64,
        starting_lsn: u64,
        bytes_on_disk: usize,
    ) -> Result<Self, WalSegmentError> {
        let path = path.as_ref().to_path_buf();
        // BufferedFile::open is read-only (`OpenOptions::new().read(true)`).
        // We need read+write to append durably, so go through OpenOptions
        // directly. This matches BufferedFile::create's mode but without
        // O_CREAT/O_TRUNC.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .buffered_open(&path)
            .await?;
        Ok(Self {
            file,
            path,
            segment_seq,
            starting_lsn,
            shard_uuid,
            write_buf: Vec::new(),
            bytes_on_disk,
        })
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

// ---------------------------------------------------------------------------
// Test harness helpers.
// ---------------------------------------------------------------------------

/// Run an async closure inside a fresh Glommio `LocalExecutor`. Convenience
/// wrapper for tests in this crate. Equivalent to:
///
/// ```ignore
/// LocalExecutorBuilder::default().spawn(|| async move { f().await }).unwrap().join().unwrap()
/// ```
#[cfg(test)]
pub fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("wal-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor returned")
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::wal::kinds::WalRecordKind;
    use crate::wal::record::{DecodeOutcome, Lsn, WalRecord};

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
        let p = path.clone();
        glommio_run(move || async move {
            let seg = WalSegment::create_new(&p, 0, 1, uuid(7)).await.unwrap();
            assert_eq!(seg.segment_seq(), 0);
            assert_eq!(seg.starting_lsn(), 1);
            assert_eq!(seg.shard_uuid(), uuid(7));
            seg.close().await.unwrap();
        });

        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), WAL_SEGMENT_HEADER_LEN as u64);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], &WAL_SEGMENT_MAGIC);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);
        assert_eq!(&bytes[8..24], &uuid(7));
        assert_eq!(u64::from_le_bytes(bytes[24..32].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(bytes[32..40].try_into().unwrap()), 1);
        assert!(bytes[52..4096].iter().all(|&b| b == 0));

        let stored_crc = u32::from_le_bytes(bytes[48..52].try_into().unwrap());
        let computed = crc32c::crc32c(&bytes[0..WAL_SEGMENT_HEADER_CRC_COVERAGE_END]);
        assert_eq!(stored_crc, computed);
    }

    #[test]
    fn create_new_refuses_to_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let p = path.clone();
        glommio_run(move || async move {
            let s = WalSegment::create_new(&p, 0, 1, uuid(1)).await.unwrap();
            s.close().await.unwrap();
            let err = WalSegment::create_new(&p, 0, 1, uuid(1))
                .await
                .expect_err("must refuse to clobber");
            match err {
                WalSegmentError::Io(io_err) => {
                    assert_eq!(io_err.kind(), std::io::ErrorKind::AlreadyExists);
                }
                other => panic!("expected Io(AlreadyExists), got {other:?}"),
            }
        });
    }

    #[test]
    fn append_does_not_touch_disk_until_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let p = path.clone();
        let r = sample(1, WalRecordKind::Encode, vec![0xAA; 32]);
        let r_len = r.encoded_len();
        glommio_run(move || async move {
            let mut seg = WalSegment::create_new(&p, 0, 1, uuid(1)).await.unwrap();
            let appended = seg.append_record(&r).unwrap();
            assert_eq!(appended, r_len);
            assert_eq!(seg.size_bytes(), WAL_SEGMENT_HEADER_LEN + r_len);
            seg.close().await.unwrap();
        });
        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert_eq!(on_disk, WAL_SEGMENT_HEADER_LEN as u64);
    }

    #[test]
    fn flush_on_empty_buffer_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let p = path.clone();
        glommio_run(move || async move {
            let mut seg = WalSegment::create_new(&p, 0, 1, uuid(1)).await.unwrap();
            seg.flush().await.unwrap();
            seg.flush().await.unwrap();
            assert_eq!(seg.size_bytes(), WAL_SEGMENT_HEADER_LEN);
            seg.close().await.unwrap();
        });
        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert_eq!(on_disk, WAL_SEGMENT_HEADER_LEN as u64);
    }

    #[test]
    fn records_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let p = path.clone();
        let r1 = sample(1, WalRecordKind::Encode, vec![1; 16]);
        let r2 = sample(2, WalRecordKind::Forget, vec![2; 8]);
        let r3 = sample(3, WalRecordKind::Link, vec![3; 38]);
        let r1c = r1.clone();
        let r2c = r2.clone();
        let r3c = r3.clone();
        glommio_run(move || async move {
            let mut seg = WalSegment::create_new(&p, 0, 1, uuid(2)).await.unwrap();
            seg.append_record(&r1c).unwrap();
            seg.append_record(&r2c).unwrap();
            seg.append_record(&r3c).unwrap();
            seg.flush().await.unwrap();
            seg.close().await.unwrap();
        });

        let expected_size =
            WAL_SEGMENT_HEADER_LEN + r1.encoded_len() + r2.encoded_len() + r3.encoded_len();
        let on_disk = std::fs::metadata(&path).unwrap().len() as usize;
        assert_eq!(on_disk, expected_size);

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
        let p = path.clone();
        let r1 = sample(1, WalRecordKind::Encode, vec![1; 4]);
        let r2 = sample(2, WalRecordKind::Encode, vec![2; 4]);
        let r3 = sample(3, WalRecordKind::Encode, vec![3; 4]);
        let r1c = r1.clone();
        let r2c = r2.clone();
        let r3c = r3.clone();
        glommio_run(move || async move {
            let mut seg = WalSegment::create_new(&p, 0, 1, uuid(3)).await.unwrap();
            seg.append_record(&r1c).unwrap();
            seg.flush().await.unwrap();
            seg.append_record(&r2c).unwrap();
            seg.append_record(&r3c).unwrap();
            seg.flush().await.unwrap();
            seg.close().await.unwrap();
        });

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
        let p = path.clone();
        let r1 = sample(1, WalRecordKind::Encode, vec![0; 100]);
        let r2 = sample(2, WalRecordKind::Encode, vec![0; 50]);
        let r1_len = r1.encoded_len();
        let r2_len = r2.encoded_len();
        glommio_run(move || async move {
            let mut seg = WalSegment::create_new(&p, 0, 1, uuid(4)).await.unwrap();
            seg.append_record(&r1).unwrap();
            assert_eq!(seg.size_bytes(), WAL_SEGMENT_HEADER_LEN + r1_len);
            seg.flush().await.unwrap();
            assert_eq!(seg.size_bytes(), WAL_SEGMENT_HEADER_LEN + r1_len);
            seg.append_record(&r2).unwrap();
            assert_eq!(seg.size_bytes(), WAL_SEGMENT_HEADER_LEN + r1_len + r2_len);
            seg.close().await.unwrap();
        });
    }

    #[test]
    fn is_full_reflects_size_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.wal");
        let p = path.clone();
        glommio_run(move || async move {
            let seg = WalSegment::create_new(&p, 0, 1, uuid(5)).await.unwrap();
            assert!(!seg.is_full());
            assert_eq!(
                seg.is_full(),
                seg.size_bytes() >= crate::WAL_SEGMENT_SIZE_BYTES
            );
            seg.close().await.unwrap();
        });
    }
}
