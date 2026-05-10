//! `Wal` — the public per-shard WAL handle.
//!
//! Composes [`WalSegment`] (sub-task 2.6), [`GroupCommitter`] (sub-task 2.8),
//! and [`WalReader`] (sub-task 2.7) into one type that:
//!
//! - Allocates monotonic LSNs per spec §05/04 §3 (LSN 0 reserved; first
//!   record after fresh creation is LSN 1).
//! - Owns the active segment via the committer thread.
//! - Triggers segment rollover when the active segment plus the next
//!   record would exceed `max_segment_bytes`. Rollover follows spec §05/06
//!   §7: drain current commit → close old segment → create new segment →
//!   fsync directory → restart committer.
//!
//! ## Spec deviations
//!
//! - **SD-2.9-1**: `append(&mut self, ...)` is synchronous, not
//!   `async fn append(&self, ...)`. Carries forward SD-2.8-2 (no async
//!   runtime yet). The `&mut self` signature mirrors spec §07 §15's
//!   single-writer-per-shard discipline at the type level.
//!
//! ## Reopen / recovery
//!
//! `Wal::create` requires an empty directory. Reopening an existing WAL
//! (with previously-written segments) is the recovery driver's job in
//! sub-task 2.10.

use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::wal::group_commit::{CommitError, GroupCommitConfig, GroupCommitter};
use crate::wal::reader::{WalReadError, WalReader};
use crate::wal::record::{Lsn, WalRecord};
use crate::wal::segment::{WalSegment, WalSegmentError, WAL_SEGMENT_HEADER_LEN};
use crate::WAL_SEGMENT_SIZE_BYTES;

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// Configuration for [`Wal`].
#[derive(Debug, Clone, Copy)]
pub struct WalConfig {
    /// Group commit configuration passed to each [`GroupCommitter`] the
    /// WAL spawns (one per active segment).
    pub group_commit: GroupCommitConfig,
    /// Hard cap on segment file size; once `header + records + next_record`
    /// would exceed this, the WAL rolls over into a fresh segment.
    /// Default: `WAL_SEGMENT_SIZE_BYTES` (256 MiB, spec §05/04 §4).
    pub max_segment_bytes: usize,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            group_commit: GroupCommitConfig::default(),
            max_segment_bytes: WAL_SEGMENT_SIZE_BYTES,
        }
    }
}

/// Per-shard WAL handle.
pub struct Wal {
    dir: PathBuf,
    shard_uuid: [u8; 16],
    next_lsn: u64,
    active_segment_seq: u64,
    /// Local size tracker for the active segment. Advanced after each
    /// successful append; reset to 0 on rollover. Excludes the 4 KB header.
    bytes_in_active_segment: usize,
    /// `Option` so rollover can `take` and replace.
    committer: Option<GroupCommitter>,
    config: WalConfig,
}

#[derive(thiserror::Error, Debug)]
pub enum WalError {
    #[error("directory {dir:?} already contains *.wal files; use the recovery driver to reopen")]
    DirectoryNotEmpty { dir: PathBuf },

    #[error("record encoded size ({record_bytes}) exceeds max_segment_bytes ({segment_max})")]
    RecordExceedsSegmentLimit {
        record_bytes: usize,
        segment_max: usize,
    },

    #[error("WAL segment error: {0}")]
    Segment(#[from] WalSegmentError),

    #[error("commit error: {0}")]
    Commit(#[from] CommitError),

    #[error("WAL read error: {0}")]
    Read(#[from] WalReadError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Wal::create.
// ---------------------------------------------------------------------------

impl Wal {
    /// Create a fresh WAL in `dir` with the default [`WalConfig`].
    pub fn create(dir: impl AsRef<Path>, shard_uuid: [u8; 16]) -> Result<Self, WalError> {
        Self::create_with_config(dir, shard_uuid, WalConfig::default())
    }

    /// Create a fresh WAL in `dir`.
    ///
    /// - Creates `dir` if absent.
    /// - Errors with [`WalError::DirectoryNotEmpty`] if any `*.wal` file
    ///   already exists.
    /// - Writes the first segment file (`0000000000.wal`) with
    ///   `starting_lsn = 1` (spec §05/04 §3).
    /// - `fsync`s the directory so the new segment's directory entry is
    ///   durable (spec §05/06 §7 step 4).
    /// - Spawns the [`GroupCommitter`] that owns the segment.
    pub fn create_with_config(
        dir: impl AsRef<Path>,
        shard_uuid: [u8; 16],
        config: WalConfig,
    ) -> Result<Self, WalError> {
        let dir_path = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir_path)?;

        // Refuse to clobber an existing WAL.
        for entry in fs::read_dir(&dir_path)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("wal") {
                return Err(WalError::DirectoryNotEmpty { dir: dir_path });
            }
        }

        let seg_path = segment_path(&dir_path, 0);
        let segment = WalSegment::create_new(&seg_path, 0, 1, shard_uuid)?;
        fsync_dir(&dir_path)?;

        let committer = GroupCommitter::start(segment, config.group_commit);

        Ok(Self {
            dir: dir_path,
            shard_uuid,
            next_lsn: 1,
            active_segment_seq: 0,
            bytes_in_active_segment: 0,
            committer: Some(committer),
            config,
        })
    }

    #[must_use]
    pub fn shard_uuid(&self) -> [u8; 16] {
        self.shard_uuid
    }

    #[must_use]
    pub fn next_lsn(&self) -> u64 {
        self.next_lsn
    }

    #[must_use]
    pub fn active_segment_seq(&self) -> u64 {
        self.active_segment_seq
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

// ---------------------------------------------------------------------------
// Wal::append.
// ---------------------------------------------------------------------------

impl Wal {
    /// Append `record` to the WAL. The caller's `record.lsn` is overwritten
    /// with the next monotonic LSN. Triggers segment rollover if appending
    /// would exceed `max_segment_bytes`. Blocks until the record is durable.
    ///
    /// # Single-writer
    /// `&mut self` enforces the spec §07 §15 single-writer-per-shard
    /// discipline at the type level.
    pub fn append(&mut self, mut record: WalRecord) -> Result<Lsn, WalError> {
        // Validate the record size up front. Both projections below depend
        // on this not exceeding the per-segment cap; without the early
        // check, the rollover path would loop indefinitely on an oversized
        // record.
        let record_bytes = record.encoded_len();
        let segment_capacity_bytes = self
            .config
            .max_segment_bytes
            .saturating_sub(WAL_SEGMENT_HEADER_LEN);
        if record_bytes > segment_capacity_bytes {
            return Err(WalError::RecordExceedsSegmentLimit {
                record_bytes,
                segment_max: self.config.max_segment_bytes,
            });
        }

        let lsn = self.next_lsn;
        record.lsn = Lsn(lsn);

        // Project: would this record push the active segment past the cap?
        let projected = WAL_SEGMENT_HEADER_LEN + self.bytes_in_active_segment + record_bytes;
        if projected > self.config.max_segment_bytes {
            self.rollover()?;
        }

        let committer = self
            .committer
            .as_ref()
            .expect("committer present between rollovers");
        let handle = committer.append(record.clone())?;
        let durable_lsn = handle.wait()?;
        debug_assert_eq!(durable_lsn, lsn, "committer ack returned wrong LSN");

        self.bytes_in_active_segment += record_bytes;
        self.next_lsn = lsn + 1;
        Ok(Lsn(lsn))
    }

    fn rollover(&mut self) -> Result<(), WalError> {
        // Spec §05/06 §7:
        //   1. Current group commit completes (drain + flush via shutdown).
        //   2. New segment file created.
        //   3. New segment header written + fsync'd (WalSegment::create_new
        //      writes the header durably).
        //   4. Directory containing segments fsync'd (so the new file's
        //      directory entry is durable).
        //   5. Subsequent records go to the new segment.
        let old_committer = self
            .committer
            .take()
            .expect("committer present at rollover entry");
        let old_segment = old_committer.shutdown()?;
        drop(old_segment); // close the old segment file

        let new_seq = self.active_segment_seq + 1;
        let new_path = segment_path(&self.dir, new_seq);
        let new_segment =
            WalSegment::create_new(&new_path, new_seq, self.next_lsn, self.shard_uuid)?;

        fsync_dir(&self.dir)?;

        self.committer = Some(GroupCommitter::start(new_segment, self.config.group_commit));
        self.active_segment_seq = new_seq;
        self.bytes_in_active_segment = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Wal::reader.
// ---------------------------------------------------------------------------

impl Wal {
    /// Construct a [`WalReader`] over the WAL directory.
    ///
    /// The reader's segment list is fixed at `open()` time, but each
    /// segment's *contents* are read at iteration time — so records that
    /// became durable after the reader was constructed but before
    /// iteration are visible. Records still buffered in the committer
    /// (not yet durable) are not visible. After `wal.append(...)` returns,
    /// the returned LSN is durable.
    ///
    /// If [`Wal::append`] triggers a rollover *after* this reader is
    /// constructed, the reader doesn't see the new segment — call
    /// `reader()` again for an up-to-date view.
    pub fn reader(&self) -> Result<WalReader, WalError> {
        Ok(WalReader::open(&self.dir, self.shard_uuid)?)
    }
}

// ---------------------------------------------------------------------------
// Wal::shutdown / Drop.
// ---------------------------------------------------------------------------

impl Wal {
    /// Drain the queue, flush the final batch, and close the active
    /// segment cleanly. Consumes self.
    pub fn shutdown(mut self) -> Result<(), WalError> {
        if let Some(committer) = self.committer.take() {
            let _segment = committer.shutdown()?;
        }
        Ok(())
    }
}

impl Drop for Wal {
    fn drop(&mut self) {
        if let Some(committer) = self.committer.take() {
            // Best-effort: ignore errors during cleanup.
            let _ = committer.shutdown();
        }
    }
}

impl core::fmt::Debug for Wal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Wal")
            .field("dir", &self.dir)
            .field("shard_uuid", &self.shard_uuid)
            .field("next_lsn", &self.next_lsn)
            .field("active_segment_seq", &self.active_segment_seq)
            .field("bytes_in_active_segment", &self.bytes_in_active_segment)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn segment_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{:010}.wal", seq))
}

/// `fsync` the parent directory so the recently-created segment file's
/// directory entry is durable (spec §05/06 §7 step 4). The Linux fsync
/// of a directory FD persists the directory's contents.
fn fsync_dir(dir: &Path) -> Result<(), WalError> {
    let cstr = CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory path contains an interior NUL byte",
        )
    })?;
    // SAFETY: `cstr` is a valid NUL-terminated path; flags are O_RDONLY.
    let fd = unsafe { libc::open(cstr.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: `fd` is a valid open fd until `libc::close`.
    let rc = unsafe { libc::fsync(fd) };
    let fsync_err = if rc != 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    // SAFETY: `fd` was obtained from `libc::open` and not yet closed.
    unsafe { libc::close(fd) };
    if let Some(e) = fsync_err {
        return Err(e.into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::kinds::WalRecordKind;

    fn uuid(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn record_with_payload_size(payload_bytes: usize) -> WalRecord {
        WalRecord {
            lsn: Lsn(0), // overwritten by Wal::append
            kind: WalRecordKind::Encode,
            flags: 0,
            timestamp_ns: 1_700_000_000_000_000_000,
            agent_id_lo64: 0xCAFE_BABE_DEAD_BEEF,
            payload: vec![0xAB; payload_bytes],
        }
    }

    fn record(lsn_hint: u64) -> WalRecord {
        let mut r = record_with_payload_size(16);
        r.lsn = Lsn(lsn_hint);
        r
    }

    // ----- Create -------------------------------------------------------

    #[test]
    fn create_on_empty_dir_starts_at_lsn_1() {
        let dir = tempfile::tempdir().unwrap();
        let wal = Wal::create(dir.path(), uuid(1)).unwrap();
        assert_eq!(wal.next_lsn(), 1);
        assert_eq!(wal.active_segment_seq(), 0);
        assert_eq!(wal.shard_uuid(), uuid(1));
        // Segment 0 file exists.
        assert!(dir.path().join("0000000000.wal").exists());
        wal.shutdown().unwrap();
    }

    #[test]
    fn create_on_dir_with_existing_wal_errors() {
        let dir = tempfile::tempdir().unwrap();
        let wal = Wal::create(dir.path(), uuid(1)).unwrap();
        wal.shutdown().unwrap();
        let err = Wal::create(dir.path(), uuid(1)).unwrap_err();
        assert!(
            matches!(err, WalError::DirectoryNotEmpty { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn create_creates_dir_if_absent() {
        let parent = tempfile::tempdir().unwrap();
        let nested = parent.path().join("nested/wal");
        let wal = Wal::create(&nested, uuid(1)).unwrap();
        assert!(nested.is_dir());
        wal.shutdown().unwrap();
    }

    // ----- LSN allocation ----------------------------------------------

    #[test]
    fn five_appends_have_lsns_one_through_five() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::create(dir.path(), uuid(1)).unwrap();
        let mut got = Vec::new();
        for i in 1..=5 {
            let lsn = wal.append(record(0)).unwrap();
            got.push(lsn.raw());
            assert_eq!(wal.next_lsn(), i + 1);
        }
        assert_eq!(got, vec![1, 2, 3, 4, 5]);
        wal.shutdown().unwrap();
    }

    #[test]
    fn caller_supplied_lsn_is_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::create(dir.path(), uuid(1)).unwrap();
        // Caller supplies an absurd LSN — Wal must overwrite.
        let lsn = wal.append(record(99)).unwrap();
        assert_eq!(lsn, Lsn(1));
        wal.shutdown().unwrap();

        // Reopen via reader; the on-disk record should have LSN 1.
        let reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let r = reader.into_iter().next().unwrap().unwrap();
        assert_eq!(r.lsn, Lsn(1));
    }

    // ----- End-to-end round-trip (phase doc done-when) -----------------

    #[test]
    fn hundred_records_round_trip_through_wal() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::create(dir.path(), uuid(2)).unwrap();
        for _ in 1..=100u64 {
            let _ = wal.append(record(0)).unwrap();
        }
        // Snapshot reader before shutdown.
        let reader = wal.reader().unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, (1..=100).collect::<Vec<_>>());
        wal.shutdown().unwrap();
    }

    // ----- Rollover -----------------------------------------------------

    #[test]
    fn rollover_when_segment_fills() {
        // Tiny segment cap so two records overflow.
        let dir = tempfile::tempdir().unwrap();
        let small_cap = WAL_SEGMENT_HEADER_LEN + 200; // header + room for one record
        let cfg = WalConfig {
            group_commit: GroupCommitConfig::default(),
            max_segment_bytes: small_cap,
        };
        let mut wal = Wal::create_with_config(dir.path(), uuid(3), cfg).unwrap();

        // Each record is ~104 bytes encoded (header 32 + payload 64 + footer 8).
        let payload = 64;
        let _lsn1 = wal.append(record_with_payload_size(payload)).unwrap();
        assert_eq!(wal.active_segment_seq(), 0);
        let _lsn2 = wal.append(record_with_payload_size(payload)).unwrap();
        // Second record should have triggered a rollover.
        assert!(wal.active_segment_seq() >= 1);

        let lsns: Vec<u64> = wal
            .reader()
            .unwrap()
            .map(|r| r.unwrap().lsn.raw())
            .collect();
        assert_eq!(lsns, vec![1, 2]);

        // Both segment files exist on disk.
        assert!(dir.path().join("0000000000.wal").exists());
        assert!(dir.path().join("0000000001.wal").exists());

        wal.shutdown().unwrap();
    }

    #[test]
    fn many_rollovers_keep_lsns_contiguous() {
        let dir = tempfile::tempdir().unwrap();
        let small_cap = WAL_SEGMENT_HEADER_LEN + 200;
        let cfg = WalConfig {
            group_commit: GroupCommitConfig::default(),
            max_segment_bytes: small_cap,
        };
        let mut wal = Wal::create_with_config(dir.path(), uuid(4), cfg).unwrap();
        for _ in 0..20 {
            let _ = wal.append(record_with_payload_size(64)).unwrap();
        }
        // Many segments; each rolled over after one record.
        assert!(wal.active_segment_seq() >= 10, "expected several rollovers");
        let lsns: Vec<u64> = wal
            .reader()
            .unwrap()
            .map(|r| r.unwrap().lsn.raw())
            .collect();
        assert_eq!(lsns, (1..=20).collect::<Vec<_>>());
        wal.shutdown().unwrap();
    }

    #[test]
    fn record_larger_than_segment_cap_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let small_cap = WAL_SEGMENT_HEADER_LEN + 100;
        let cfg = WalConfig {
            group_commit: GroupCommitConfig::default(),
            max_segment_bytes: small_cap,
        };
        let mut wal = Wal::create_with_config(dir.path(), uuid(5), cfg).unwrap();

        let too_big = record_with_payload_size(200);
        let err = wal.append(too_big).unwrap_err();
        assert!(
            matches!(err, WalError::RecordExceedsSegmentLimit { .. }),
            "got {err:?}"
        );
        // State unchanged.
        assert_eq!(wal.next_lsn(), 1);
        wal.shutdown().unwrap();
    }

    // ----- Reader -------------------------------------------------------

    #[test]
    fn reader_sees_durably_appended_records() {
        // The reader's segment list is fixed at `open()` time, but each
        // segment's *contents* are read at iteration time. Records that
        // become durable after the reader is constructed are visible if
        // the iteration happens after they're flushed.
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::create(dir.path(), uuid(6)).unwrap();
        wal.append(record(0)).unwrap();
        wal.append(record(0)).unwrap();
        wal.append(record(0)).unwrap();
        let lsns: Vec<u64> = wal
            .reader()
            .unwrap()
            .map(|r| r.unwrap().lsn.raw())
            .collect();
        assert_eq!(lsns, vec![1, 2, 3]);
        wal.shutdown().unwrap();
    }

    // ----- Shutdown / Drop ---------------------------------------------

    #[test]
    fn shutdown_leaves_consistent_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::create(dir.path(), uuid(7)).unwrap();
        for _ in 0..3 {
            wal.append(record(0)).unwrap();
        }
        wal.shutdown().unwrap();
        // Reopen via WalReader directly (Wal::create on a populated dir
        // would error).
        let reader = WalReader::open(dir.path(), uuid(7)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, vec![1, 2, 3]);
    }

    #[test]
    fn drop_without_shutdown_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let mut wal = Wal::create(&path, uuid(8)).unwrap();
            wal.append(record(0)).unwrap();
            wal.append(record(0)).unwrap();
            // Implicit drop here.
        }
        let reader = WalReader::open(&path, uuid(8)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, vec![1, 2]);
    }
}
