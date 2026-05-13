//! `Wal` — the public per-shard WAL handle.
//!
//! Composes [`WalSegment`] (sub-task 2.6), [`GroupCommitter`] (sub-task 2.8 →
//! ported to Glommio in 9.6a), and [`WalReader`] (sub-task 2.7) into one type
//! that:
//!
//! - Allocates monotonic LSNs per spec §05/04 §3 (LSN 0 reserved; first
//!   record after fresh creation is LSN 1).
//! - Owns the active segment via the committer task.
//! - Triggers segment rollover when the active segment plus the next record
//!   would exceed `max_segment_bytes`. Rollover follows spec §05/06 §7:
//!   drain current commit → close old segment → create new segment → fsync
//!   directory → restart committer.
//!
//! ## Async on `&self`
//!
//! After 9.6a, `Wal::append` is `async fn(&self, ...)`. Single-writer-per-shard
//! (spec §10/02) is enforced by living on a single Glommio executor: there's
//! no cross-thread access, and the borrow checker over the internal
//! `RefCell<WalInner>` catches any same-task `borrow_mut` reentrance at runtime.
//!
//! **Invariant:** never hold a `RefCell::borrow_mut()` across an `.await`.
//! The append path borrows briefly to mutate counters + enqueue, drops the
//! borrow, then awaits the committer's ack. Documented inline.

use std::cell::RefCell;
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

#[derive(Debug, Clone, Copy)]
pub struct WalConfig {
    pub group_commit: GroupCommitConfig,
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

/// Per-shard WAL handle. `!Send` / `!Sync` — lives on one Glommio executor.
pub struct Wal {
    inner: RefCell<WalInner>,
}

struct WalInner {
    dir: PathBuf,
    shard_uuid: [u8; 16],
    next_lsn: u64,
    active_segment_seq: u64,
    bytes_in_active_segment: usize,
    committer: Option<GroupCommitter>,
    config: WalConfig,
}

#[derive(thiserror::Error, Debug)]
pub enum WalError {
    #[error("directory {dir:?} already contains *.wal files; use the recovery driver to reopen")]
    DirectoryNotEmpty { dir: PathBuf },

    #[error("directory {dir:?} contains no *.wal segment files; cannot open_existing")]
    NoSegmentsFound { dir: PathBuf },

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
    pub async fn create(dir: impl AsRef<Path>, shard_uuid: [u8; 16]) -> Result<Self, WalError> {
        Self::create_with_config(dir, shard_uuid, WalConfig::default()).await
    }

    /// Create a fresh WAL in `dir`. Must be called from inside a Glommio
    /// executor (the segment + committer live there).
    pub async fn create_with_config(
        dir: impl AsRef<Path>,
        shard_uuid: [u8; 16],
        config: WalConfig,
    ) -> Result<Self, WalError> {
        let dir_path = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir_path)?;

        for entry in fs::read_dir(&dir_path)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("wal") {
                return Err(WalError::DirectoryNotEmpty { dir: dir_path });
            }
        }

        let seg_path = segment_path(&dir_path, 0);
        let segment = WalSegment::create_new(&seg_path, 0, 1, shard_uuid).await?;
        fsync_dir(&dir_path)?;

        let committer = GroupCommitter::start(segment, config.group_commit);

        Ok(Self {
            inner: RefCell::new(WalInner {
                dir: dir_path,
                shard_uuid,
                next_lsn: 1,
                active_segment_seq: 0,
                bytes_in_active_segment: 0,
                committer: Some(committer),
                config,
            }),
        })
    }

    #[must_use]
    pub fn shard_uuid(&self) -> [u8; 16] {
        self.inner.borrow().shard_uuid
    }

    #[must_use]
    pub fn next_lsn(&self) -> u64 {
        self.inner.borrow().next_lsn
    }

    #[must_use]
    pub fn active_segment_seq(&self) -> u64 {
        self.inner.borrow().active_segment_seq
    }

    #[must_use]
    pub fn dir(&self) -> PathBuf {
        self.inner.borrow().dir.clone()
    }

    /// Open an existing WAL for append, resuming at `next_lsn`.
    ///
    /// Caller must have already run [`crate::recovery::recover`] to determine
    /// `next_lsn` — supplying a wrong value risks LSN reuse, which the WAL
    /// reader will then reject as corruption on the next recovery.
    ///
    /// Selects the highest-`segment_seq` segment as the active one and
    /// re-opens it for append at the end of its existing on-disk bytes.
    /// Subsequent appends extend that segment (or roll over to a new one
    /// per the usual capacity rule).
    pub async fn open_existing(
        dir: impl AsRef<Path>,
        shard_uuid: [u8; 16],
        next_lsn: u64,
        config: WalConfig,
    ) -> Result<Self, WalError> {
        let dir_path = dir.as_ref().to_path_buf();

        // Enumerate segments via WalReader (it validates every segment's
        // 4 KB header against shard_uuid + format version + CRC). Pull out
        // only what we need, then drop the reader before opening the
        // segment for async append.
        let (active_segment_seq, active_starting_lsn, bytes_on_disk_pre) = {
            let reader = WalReader::open(&dir_path, shard_uuid)?;
            let last = reader
                .segments()
                .last()
                .ok_or_else(|| WalError::NoSegmentsFound {
                    dir: dir_path.clone(),
                })?;
            let seq = last.segment_seq;
            let starting_lsn = last.starting_lsn;
            let bytes = (last.file_size as usize).saturating_sub(WAL_SEGMENT_HEADER_LEN);
            (seq, starting_lsn, bytes)
        };
        let active_path = segment_path(&dir_path, active_segment_seq);

        // Re-open the active segment for append. Header was already
        // validated by WalReader above; here we just establish the
        // async BufferedFile handle for io_uring writes.
        let segment = WalSegment::open_for_append(
            &active_path,
            shard_uuid,
            active_segment_seq,
            active_starting_lsn,
            bytes_on_disk_pre,
        )
        .await?;

        let committer = GroupCommitter::start(segment, config.group_commit);

        Ok(Self {
            inner: RefCell::new(WalInner {
                dir: dir_path,
                shard_uuid,
                next_lsn,
                active_segment_seq,
                bytes_in_active_segment: bytes_on_disk_pre,
                committer: Some(committer),
                config,
            }),
        })
    }
}

// ---------------------------------------------------------------------------
// Wal::append.
// ---------------------------------------------------------------------------

impl Wal {
    /// Append `record` to the WAL. The caller's `record.lsn` is overwritten
    /// with the next monotonic LSN. Triggers segment rollover if appending
    /// would exceed `max_segment_bytes`. Awaits until the record is durable.
    pub async fn append(&self, mut record: WalRecord) -> Result<Lsn, WalError> {
        let record_bytes = record.encoded_len();

        // Phase A: short borrow — validate size, decide rollover, assign LSN.
        // Crucial: drop the borrow before any `.await`.
        enum Action {
            Append { lsn: u64 },
            Rollover { lsn_before_rollover: u64 },
        }
        let mut action = {
            let inner = self.inner.borrow();
            let segment_capacity_bytes = inner
                .config
                .max_segment_bytes
                .saturating_sub(WAL_SEGMENT_HEADER_LEN);
            if record_bytes > segment_capacity_bytes {
                return Err(WalError::RecordExceedsSegmentLimit {
                    record_bytes,
                    segment_max: inner.config.max_segment_bytes,
                });
            }
            let lsn = inner.next_lsn;
            let projected = WAL_SEGMENT_HEADER_LEN + inner.bytes_in_active_segment + record_bytes;
            if projected > inner.config.max_segment_bytes {
                Action::Rollover {
                    lsn_before_rollover: lsn,
                }
            } else {
                Action::Append { lsn }
            }
        };

        // Phase B: if rollover is needed, do it (drops + re-spawns committer)
        // while no borrow is held.
        if let Action::Rollover {
            lsn_before_rollover,
        } = action
        {
            self.rollover().await?;
            action = Action::Append {
                lsn: lsn_before_rollover,
            };
        }

        let Action::Append { lsn } = action else {
            unreachable!("rollover branch handled above");
        };

        record.lsn = Lsn(lsn);

        // Phase C: short borrow — enqueue. Drop borrow before await.
        let handle = {
            let inner = self.inner.borrow();
            let committer = inner
                .committer
                .as_ref()
                .expect("committer present between rollovers");
            committer.append(record.clone())?
        };

        // Phase D: await durability without holding any borrow.
        let durable_lsn = handle.wait().await?;
        debug_assert_eq!(durable_lsn, lsn, "committer ack returned wrong LSN");

        // Phase E: short borrow — bump counters.
        {
            let mut inner = self.inner.borrow_mut();
            inner.bytes_in_active_segment += record_bytes;
            inner.next_lsn = lsn + 1;
        }
        Ok(Lsn(lsn))
    }

    async fn rollover(&self) -> Result<(), WalError> {
        // Phase A: take the old committer + capture state under a short borrow.
        let (old_committer, dir, shard_uuid, new_seq, new_starting_lsn, group_commit_cfg) = {
            let mut inner = self.inner.borrow_mut();
            let old_committer = inner
                .committer
                .take()
                .expect("committer present at rollover entry");
            (
                old_committer,
                inner.dir.clone(),
                inner.shard_uuid,
                inner.active_segment_seq + 1,
                inner.next_lsn,
                inner.config.group_commit,
            )
        };

        // Phase B: shutdown the old committer (await) without any borrow held.
        let old_segment = old_committer.shutdown().await?;
        old_segment.close().await?;

        // Phase C: create the new segment, fsync the directory.
        let new_path = segment_path(&dir, new_seq);
        let new_segment =
            WalSegment::create_new(&new_path, new_seq, new_starting_lsn, shard_uuid).await?;
        fsync_dir(&dir)?;

        // Phase D: re-install the new committer + state under a short borrow.
        {
            let mut inner = self.inner.borrow_mut();
            inner.committer = Some(GroupCommitter::start(new_segment, group_commit_cfg));
            inner.active_segment_seq = new_seq;
            inner.bytes_in_active_segment = 0;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Wal::reader.
// ---------------------------------------------------------------------------

impl Wal {
    pub fn reader(&self) -> Result<WalReader, WalError> {
        let inner = self.inner.borrow();
        Ok(WalReader::open(&inner.dir, inner.shard_uuid)?)
    }
}

// ---------------------------------------------------------------------------
// Wal::shutdown / Drop.
// ---------------------------------------------------------------------------

impl Wal {
    pub async fn shutdown(self) -> Result<(), WalError> {
        self.shutdown_in_place().await
    }

    /// Drain the committer + close the active segment, leaving `self` in a
    /// post-shutdown state. Idempotent — calling twice is a no-op on the
    /// second call.
    ///
    /// Exists alongside `shutdown(self)` for callers that hold `&mut self`
    /// or `&self` and can't move out. Brain-server's shard main loop uses
    /// this on the cleanup path because the Shard owns the Wal by value.
    pub async fn shutdown_in_place(&self) -> Result<(), WalError> {
        let committer = {
            let mut inner = self.inner.borrow_mut();
            inner.committer.take()
        };
        if let Some(committer) = committer {
            let seg = committer.shutdown().await?;
            seg.close().await?;
        }
        Ok(())
    }
}

impl Drop for Wal {
    fn drop(&mut self) {
        // Best-effort drop. We can't await here, so the committer's detached
        // task winds down on its own. The WalSegment file descriptor closes
        // via its Drop impl. Tests and the connection layer's graceful
        // shutdown path should call `shutdown().await` explicitly to avoid
        // leaving in-flight records unflushed.
        let _ = self.inner.borrow_mut().committer.take();
    }
}

impl core::fmt::Debug for Wal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let inner = self.inner.borrow();
        f.debug_struct("Wal")
            .field("dir", &inner.dir)
            .field("shard_uuid", &inner.shard_uuid)
            .field("next_lsn", &inner.next_lsn)
            .field("active_segment_seq", &inner.active_segment_seq)
            .field("bytes_in_active_segment", &inner.bytes_in_active_segment)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn segment_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{:010}.wal", seq))
}

/// `fsync` the parent directory so a recently-created segment file's
/// directory entry is durable (spec §05/06 §7 step 4). Stays sync because
/// it's a brief metadata sync, and we don't have an io_uring directory-sync
/// path in Glommio's typed API.
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

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::wal::kinds::WalRecordKind;
    use crate::wal::segment::glommio_run;

    fn uuid(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn record_with_payload_size(payload_bytes: usize) -> WalRecord {
        WalRecord {
            lsn: Lsn(0),
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
        let path = dir.path().to_owned();
        let path_clone = path.clone();
        glommio_run(move || async move {
            let wal = Wal::create(&path_clone, uuid(1)).await.unwrap();
            assert_eq!(wal.next_lsn(), 1);
            assert_eq!(wal.active_segment_seq(), 0);
            assert_eq!(wal.shard_uuid(), uuid(1));
            wal.shutdown().await.unwrap();
        });
        assert!(path.join("0000000000.wal").exists());
    }

    #[test]
    fn create_on_dir_with_existing_wal_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let p1 = path.clone();
        glommio_run(move || async move {
            let wal = Wal::create(&p1, uuid(1)).await.unwrap();
            wal.shutdown().await.unwrap();
        });
        let p2 = path.clone();
        glommio_run(move || async move {
            let err = Wal::create(&p2, uuid(1)).await.unwrap_err();
            assert!(
                matches!(err, WalError::DirectoryNotEmpty { .. }),
                "got {err:?}"
            );
        });
    }

    #[test]
    fn create_creates_dir_if_absent() {
        let parent = tempfile::tempdir().unwrap();
        let nested = parent.path().join("nested/wal");
        let nested_clone = nested.clone();
        glommio_run(move || async move {
            let wal = Wal::create(&nested_clone, uuid(1)).await.unwrap();
            wal.shutdown().await.unwrap();
        });
        assert!(nested.is_dir());
    }

    // ----- LSN allocation ----------------------------------------------

    #[test]
    fn five_appends_have_lsns_one_through_five() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        glommio_run(move || async move {
            let wal = Wal::create(&path, uuid(1)).await.unwrap();
            let mut got = Vec::new();
            for i in 1..=5 {
                let lsn = wal.append(record(0)).await.unwrap();
                got.push(lsn.raw());
                assert_eq!(wal.next_lsn(), i + 1);
            }
            assert_eq!(got, vec![1, 2, 3, 4, 5]);
            wal.shutdown().await.unwrap();
        });
    }

    #[test]
    fn caller_supplied_lsn_is_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let p = path.clone();
        glommio_run(move || async move {
            let wal = Wal::create(&p, uuid(1)).await.unwrap();
            let lsn = wal.append(record(99)).await.unwrap();
            assert_eq!(lsn, Lsn(1));
            wal.shutdown().await.unwrap();
        });

        let reader = WalReader::open(&path, uuid(1)).unwrap();
        let r = reader.into_iter().next().unwrap().unwrap();
        assert_eq!(r.lsn, Lsn(1));
    }

    // ----- End-to-end round-trip --------------------------------------

    #[test]
    fn hundred_records_round_trip_through_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let p = path.clone();
        glommio_run(move || async move {
            let wal = Wal::create(&p, uuid(2)).await.unwrap();
            for _ in 1..=100u64 {
                let _ = wal.append(record(0)).await.unwrap();
            }
            let reader = wal.reader().unwrap();
            let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
            assert_eq!(lsns, (1..=100).collect::<Vec<_>>());
            wal.shutdown().await.unwrap();
        });
    }

    // ----- Rollover -----------------------------------------------------

    #[test]
    fn rollover_when_segment_fills() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let p = path.clone();
        let small_cap = WAL_SEGMENT_HEADER_LEN + 200;
        let cfg = WalConfig {
            group_commit: GroupCommitConfig::default(),
            max_segment_bytes: small_cap,
        };
        glommio_run(move || async move {
            let wal = Wal::create_with_config(&p, uuid(3), cfg).await.unwrap();
            let _lsn1 = wal.append(record_with_payload_size(64)).await.unwrap();
            assert_eq!(wal.active_segment_seq(), 0);
            let _lsn2 = wal.append(record_with_payload_size(64)).await.unwrap();
            assert!(wal.active_segment_seq() >= 1);
            let lsns: Vec<u64> = wal
                .reader()
                .unwrap()
                .map(|r| r.unwrap().lsn.raw())
                .collect();
            assert_eq!(lsns, vec![1, 2]);
            wal.shutdown().await.unwrap();
        });
        assert!(path.join("0000000000.wal").exists());
        assert!(path.join("0000000001.wal").exists());
    }

    #[test]
    fn many_rollovers_keep_lsns_contiguous() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let small_cap = WAL_SEGMENT_HEADER_LEN + 200;
        let cfg = WalConfig {
            group_commit: GroupCommitConfig::default(),
            max_segment_bytes: small_cap,
        };
        glommio_run(move || async move {
            let wal = Wal::create_with_config(&path, uuid(4), cfg).await.unwrap();
            for _ in 0..20 {
                let _ = wal.append(record_with_payload_size(64)).await.unwrap();
            }
            assert!(wal.active_segment_seq() >= 10, "expected several rollovers");
            let lsns: Vec<u64> = wal
                .reader()
                .unwrap()
                .map(|r| r.unwrap().lsn.raw())
                .collect();
            assert_eq!(lsns, (1..=20).collect::<Vec<_>>());
            wal.shutdown().await.unwrap();
        });
    }

    #[test]
    fn record_larger_than_segment_cap_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let small_cap = WAL_SEGMENT_HEADER_LEN + 100;
        let cfg = WalConfig {
            group_commit: GroupCommitConfig::default(),
            max_segment_bytes: small_cap,
        };
        glommio_run(move || async move {
            let wal = Wal::create_with_config(&path, uuid(5), cfg).await.unwrap();
            let too_big = record_with_payload_size(200);
            let err = wal.append(too_big).await.unwrap_err();
            assert!(
                matches!(err, WalError::RecordExceedsSegmentLimit { .. }),
                "got {err:?}"
            );
            assert_eq!(wal.next_lsn(), 1);
            wal.shutdown().await.unwrap();
        });
    }

    // ----- Reader -------------------------------------------------------

    #[test]
    fn reader_sees_durably_appended_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        glommio_run(move || async move {
            let wal = Wal::create(&path, uuid(6)).await.unwrap();
            wal.append(record(0)).await.unwrap();
            wal.append(record(0)).await.unwrap();
            wal.append(record(0)).await.unwrap();
            let lsns: Vec<u64> = wal
                .reader()
                .unwrap()
                .map(|r| r.unwrap().lsn.raw())
                .collect();
            assert_eq!(lsns, vec![1, 2, 3]);
            wal.shutdown().await.unwrap();
        });
    }

    // ----- Shutdown -----------------------------------------------------

    #[test]
    fn shutdown_leaves_consistent_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let p = path.clone();
        glommio_run(move || async move {
            let wal = Wal::create(&p, uuid(7)).await.unwrap();
            for _ in 0..3 {
                wal.append(record(0)).await.unwrap();
            }
            wal.shutdown().await.unwrap();
        });
        let reader = WalReader::open(&path, uuid(7)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, vec![1, 2, 3]);
    }

    // ----- open_existing ------------------------------------------------

    #[test]
    fn open_existing_resumes_after_clean_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let p1 = path.clone();
        glommio_run(move || async move {
            let wal = Wal::create(&p1, uuid(20)).await.unwrap();
            for _ in 0..3 {
                wal.append(record(0)).await.unwrap();
            }
            wal.shutdown().await.unwrap();
        });
        // Reopen, append more, verify LSN sequence continues.
        let p2 = path.clone();
        glommio_run(move || async move {
            let wal = Wal::open_existing(&p2, uuid(20), 4, WalConfig::default())
                .await
                .expect("open existing");
            assert_eq!(wal.next_lsn(), 4);
            assert_eq!(wal.active_segment_seq(), 0);
            let lsn = wal.append(record(0)).await.unwrap();
            assert_eq!(lsn, Lsn(4));
            wal.shutdown().await.unwrap();
        });
        // The WAL now has records 1..=4.
        let reader = WalReader::open(&path, uuid(20)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, vec![1, 2, 3, 4]);
    }

    #[test]
    fn open_existing_on_empty_dir_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        glommio_run(move || async move {
            let err = Wal::open_existing(&path, uuid(21), 1, WalConfig::default())
                .await
                .expect_err("must fail on empty dir");
            assert!(
                matches!(err, WalError::NoSegmentsFound { .. } | WalError::Read(_)),
                "got {err:?}"
            );
        });
    }

    #[test]
    fn open_existing_rejects_wrong_shard_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let p1 = path.clone();
        glommio_run(move || async move {
            let wal = Wal::create(&p1, uuid(22)).await.unwrap();
            wal.append(record(0)).await.unwrap();
            wal.shutdown().await.unwrap();
        });
        let p2 = path.clone();
        glommio_run(move || async move {
            let err = Wal::open_existing(&p2, uuid(99), 2, WalConfig::default())
                .await
                .expect_err("uuid mismatch must fail");
            // WalReader catches the uuid mismatch before we get to
            // open_for_append, so the error surfaces as Read(_).
            assert!(matches!(err, WalError::Read(_)), "got {err:?}");
        });
    }

    // ----- Executor responsiveness during commit bursts -----------------
    //
    // Sibling task increments a counter every 100 µs while we burst-append
    // 200 records. Asserts the counter advanced — i.e. the executor was
    // NOT stalled inside fsync (which would be the SD-2.8-2 v1 behavior).

    #[test]
    fn wal_append_does_not_block_executor() {
        use std::cell::Cell;
        use std::rc::Rc;
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        glommio_run(move || async move {
            let wal = Wal::create(&path, uuid(9)).await.unwrap();
            let wal = Rc::new(wal);
            let counter = Rc::new(Cell::new(0u32));

            let stop = Rc::new(Cell::new(false));
            let counter_c = counter.clone();
            let stop_c = stop.clone();
            let ticker = glommio::spawn_local(async move {
                while !stop_c.get() {
                    glommio::timer::sleep(Duration::from_micros(100)).await;
                    counter_c.set(counter_c.get() + 1);
                }
            });

            for _ in 0..200u32 {
                wal.append(record(0)).await.unwrap();
            }
            stop.set(true);
            ticker.await;
            // 200 records × ~150 µs each ≈ 30 ms; with 100 µs ticker that's
            // ~300 ticks. Allow wide variance — we just want non-zero ticks
            // to prove the executor wasn't monopolised by sync syscalls.
            assert!(
                counter.get() >= 10,
                "executor stalled? ticker fired only {} times",
                counter.get()
            );
            let wal = match Rc::try_unwrap(wal) {
                Ok(w) => w,
                Err(_) => panic!("only one Rc remaining"),
            };
            wal.shutdown().await.unwrap();
        });
    }
}
