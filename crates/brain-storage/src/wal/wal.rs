//! `Wal` — the public per-shard WAL handle.
//!
//! Composes [`WalSegment`], [`GroupCommitter`], and [`WalReader`] into one type
//! that:
//!
//! - Allocates monotonic LSNs (LSN 0 reserved; first
//!   record after fresh creation is LSN 1).
//! - Owns the active segment via the committer task.
//! - Triggers segment rollover when the active segment plus the next record
//!   would exceed `max_segment_bytes`. Rollover follows:
//!   drain current commit → close old segment → create new segment → fsync
//!   directory → restart committer.
//!
//! ## Async on `&self`
//!
//! After 9.6a, `Wal::append` is `async fn(&self, ...)`. Single-writer-per-shard
//! is enforced by living on a single Glommio executor: there's
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

use crate::wal::group_commit::{AppendHandle, CommitError, GroupCommitConfig, GroupCommitter};
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
    /// Serializes segment rollover. A task performing rollover holds this
    /// permit for the whole (drain old → create new → install new) sequence.
    /// A concurrent appender that also chose rollover blocks on this permit
    /// and, on acquiring it, observes the advanced segment sequence and
    /// retries against the fresh committer instead of tearing down a
    /// committer twice. Single unit, local (`!Send`) — lives on the shard
    /// executor alongside the rest of the WAL state.
    rollover_lock: glommio::sync::Semaphore,
}

struct WalInner {
    dir: PathBuf,
    shard_uuid: [u8; 16],
    next_lsn: u64,
    active_segment_seq: u64,
    segment: SegmentState,
    config: WalConfig,
}

/// The active-segment slot.
///
/// `Open` carries the live committer plus the byte count written to the
/// current segment. `RollingOver` is the transient state held only while a
/// rollover task (holding [`Wal::rollover_lock`]) is draining the old
/// segment and creating the next one.
///
/// Modeling this as a sum type — rather than `Option<GroupCommitter>`
/// paired with a separate byte counter — makes the "no committer during
/// rollover" window *unrepresentable* on the append enqueue path: a
/// `RollingOver` slot exposes no committer, so an appender cannot enqueue
/// against it and there is no `Option::expect` to panic. The one place that
/// transitions `Open → RollingOver → Open` is [`Wal::rollover`], guarded by
/// the rollover permit.
enum SegmentState {
    Open {
        committer: GroupCommitter,
        /// Bytes of records written to the current segment (excludes the
        /// segment header).
        bytes: usize,
    },
    RollingOver,
}

#[derive(thiserror::Error, Debug)]
pub enum WalError {
    #[error("directory {dir:?} already contains *.wal files; use the recovery driver to reopen")]
    DirectoryNotEmpty { dir: PathBuf },

    #[error("directory {dir:?} contains no *.wal segment files; cannot open_existing")]
    NoSegmentsFound { dir: PathBuf },

    #[error(
        "recovered tail offset {offset} is out of range for active segment {path:?} \
         (header {header}, on-disk size {file_size})"
    )]
    RecoveredOffsetOutOfRange {
        path: PathBuf,
        offset: u64,
        header: usize,
        file_size: u64,
    },

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
                segment: SegmentState::Open {
                    committer,
                    bytes: 0,
                },
                config,
            }),
            rollover_lock: glommio::sync::Semaphore::new(1),
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

    /// The LSN the next `append` will assign. Subscribe uses this as the
    /// cutover point `T`: records `[from_lsn, T-1]` are replayed from the
    /// WAL, records `[T, ∞)` arrive via the live event bus.
    #[must_use]
    pub fn current_tail_lsn(&self) -> u64 {
        self.inner.borrow().next_lsn
    }

    /// The lowest LSN still readable from disk — `starting_lsn` of the
    /// oldest segment under retention. If the WAL is empty (no segments
    /// at all), returns `next_lsn` so callers see a coherent "nothing
    /// before this point" answer.
    ///
    /// Subscribe rejects `from_lsn < oldest_available_lsn()` with
    /// `SubscriptionLsnTooOld`.
    pub fn oldest_available_lsn(&self) -> Result<u64, WalError> {
        let inner = self.inner.borrow();
        let reader = WalReader::open(&inner.dir, inner.shard_uuid)?;
        Ok(reader
            .segments()
            .first()
            .map_or(inner.next_lsn, |s| s.starting_lsn))
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
    /// both `next_lsn` and `recovered_tail_offset`
    /// ([`crate::recovery::RecoveryReport::active_tail_offset`]) — supplying a
    /// wrong value risks LSN reuse, which the WAL reader will then reject as
    /// corruption on the next recovery.
    ///
    /// Selects the highest-`segment_seq` segment as the active one. Before
    /// positioning the append cursor it **physically truncates** that
    /// segment to `recovered_tail_offset` — the byte position after the last
    /// durably-good, fully-applied record recovery validated. This removes
    /// any torn-tail bytes or never-committed dangling-transaction prefix a
    /// crash left on disk, so subsequent appends overwrite the garbage
    /// rather than following it. Without this, a later recovery would treat
    /// the buried garbage as a clean end and silently drop everything
    /// appended after it (and LSNs would be reused).
    ///
    /// Subsequent appends extend the truncated segment (or roll over to a
    /// new one per the usual capacity rule).
    pub async fn open_existing(
        dir: impl AsRef<Path>,
        shard_uuid: [u8; 16],
        next_lsn: u64,
        recovered_tail_offset: u64,
        config: WalConfig,
    ) -> Result<Self, WalError> {
        let dir_path = dir.as_ref().to_path_buf();

        // Enumerate segments via WalReader (it validates every segment's
        // 4 KB header against shard_uuid + format version + CRC). Pull out
        // only what we need, then drop the reader before opening the
        // segment for async append.
        let (active_segment_seq, active_starting_lsn, file_size) = {
            let reader = WalReader::open(&dir_path, shard_uuid)?;
            let last = reader
                .segments()
                .last()
                .ok_or_else(|| WalError::NoSegmentsFound {
                    dir: dir_path.clone(),
                })?;
            (last.segment_seq, last.starting_lsn, last.file_size)
        };
        let active_path = segment_path(&dir_path, active_segment_seq);

        // Validate the recovered offset lies within [header, on-disk size].
        // Recovery derives it from the very bytes on disk, so a value past
        // EOF (or below the header) signals a caller bug or a mismatched
        // recover()/open_existing() pairing — fail loud rather than corrupt.
        if recovered_tail_offset < WAL_SEGMENT_HEADER_LEN as u64
            || recovered_tail_offset > file_size
        {
            return Err(WalError::RecoveredOffsetOutOfRange {
                path: active_path.clone(),
                offset: recovered_tail_offset,
                header: WAL_SEGMENT_HEADER_LEN,
                file_size,
            });
        }

        // Physically drop torn / uncommitted-tail bytes past the validated
        // logical tail. A committed record can never sit past this offset
        // (recovery only advances the tail at a commit boundary), so this
        // truncation is loss-free. `sync_all` makes the shorter length
        // durable before we resume appending.
        if recovered_tail_offset < file_size {
            let f = std::fs::OpenOptions::new().write(true).open(&active_path)?;
            f.set_len(recovered_tail_offset)?;
            f.sync_all()?;
        }
        let bytes_on_disk_pre = (recovered_tail_offset as usize) - WAL_SEGMENT_HEADER_LEN;

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
                segment: SegmentState::Open {
                    committer,
                    bytes: bytes_on_disk_pre,
                },
                config,
            }),
            rollover_lock: glommio::sync::Semaphore::new(1),
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

        // LSN assignment + committer enqueue + counter bump happen in ONE
        // synchronous `borrow_mut`, with no `.await` between reading
        // `next_lsn` and bumping it. This is the critical invariant: the
        // committer's `append` is a synchronous flume push, so the borrow
        // window never spans a yield. Without it, two appends racing on the
        // same single-threaded executor (e.g. the writer's WAL-drain task
        // and the snapshot worker's CHECKPOINT records) could both read the
        // same `next_lsn` across the await and stamp duplicate LSNs —
        // recovery then trips `WalReadError::LsnGap`. Mirrors `append_many`.
        //
        // Bumping `next_lsn` before durability is safe: a flush failure
        // makes the WAL "broken" and every subsequent append errors, so no
        // LSN-reuse window opens. Rollover is the one async step; it runs
        // outside the critical section and the loop re-checks afterward.
        //
        // A concurrent appender may observe the segment mid-rollover
        // (`SegmentState::RollingOver`). It cannot enqueue against it (the
        // slot exposes no committer), so it routes to `rollover`, which
        // blocks on the rollover permit until the in-flight rollover
        // completes and then no-ops (the segment sequence has advanced),
        // after which the loop retries against the fresh committer.
        loop {
            enum Action {
                Enqueued { lsn: u64, handle: AppendHandle },
                Rollover { observed_seq: u64 },
            }
            let action = {
                let mut inner = self.inner.borrow_mut();
                let max_segment_bytes = inner.config.max_segment_bytes;
                let segment_capacity_bytes =
                    max_segment_bytes.saturating_sub(WAL_SEGMENT_HEADER_LEN);
                if record_bytes > segment_capacity_bytes {
                    return Err(WalError::RecordExceedsSegmentLimit {
                        record_bytes,
                        segment_max: max_segment_bytes,
                    });
                }
                let WalInner {
                    next_lsn,
                    active_segment_seq,
                    segment,
                    ..
                } = &mut *inner;
                match segment {
                    SegmentState::RollingOver => Action::Rollover {
                        observed_seq: *active_segment_seq,
                    },
                    SegmentState::Open { committer, bytes } => {
                        let projected = WAL_SEGMENT_HEADER_LEN + *bytes + record_bytes;
                        if projected > max_segment_bytes {
                            Action::Rollover {
                                observed_seq: *active_segment_seq,
                            }
                        } else {
                            let lsn = *next_lsn;
                            record.lsn = Lsn(lsn);
                            let handle = committer.append(record.clone())?;
                            *bytes += record_bytes;
                            *next_lsn = lsn + 1;
                            Action::Enqueued { lsn, handle }
                        }
                    }
                }
            };

            match action {
                Action::Rollover { observed_seq } => {
                    self.rollover(observed_seq).await?;
                    continue;
                }
                Action::Enqueued { lsn, handle } => {
                    // Await durability with no borrow held — other appends
                    // may reserve their (later) LSNs and enqueue meanwhile.
                    let durable_lsn = handle.wait().await?;
                    debug_assert_eq!(durable_lsn, lsn, "committer ack returned wrong LSN");
                    return Ok(Lsn(lsn));
                }
            }
        }
    }

    /// Append a batch of records under one logical "submit". All records
    /// hit the committer back-to-back without an `.await` between
    /// submissions, so the group-commit task accumulates the entire batch
    /// into a single `fdatasync`. Returns the assigned LSNs in input
    /// order.
    ///
    /// Empty input is a no-op that returns `Ok(vec![])`.
    ///
    /// Rollovers mid-batch are handled: when the next record would exceed
    /// the active segment, the in-flight handles are drained durably
    /// against the *current* segment first, then rollover proceeds, then
    /// the remaining records target the new segment. A batch that
    /// straddles a rollover therefore costs one extra fsync (the
    /// drain-before-rollover one) — still strictly better than the
    /// per-record path.
    pub async fn append_many(&self, records: Vec<WalRecord>) -> Result<Vec<Lsn>, WalError> {
        if records.is_empty() {
            return Ok(Vec::new());
        }

        let mut assigned: Vec<Lsn> = Vec::with_capacity(records.len());
        let mut handles: Vec<AppendHandle> = Vec::with_capacity(records.len());

        let mut iter = records.into_iter();
        let mut pending: Option<WalRecord> = None;
        while let Some(mut record) = pending.take().or_else(|| iter.next()) {
            let record_bytes = record.encoded_len();

            // Step A: short borrow — validate size, decide rollover, assign LSN.
            enum Action {
                Append { lsn: u64 },
                Rollover { observed_seq: u64 },
            }
            let action = {
                let inner = self.inner.borrow();
                let max_segment_bytes = inner.config.max_segment_bytes;
                let segment_capacity_bytes =
                    max_segment_bytes.saturating_sub(WAL_SEGMENT_HEADER_LEN);
                if record_bytes > segment_capacity_bytes {
                    return Err(WalError::RecordExceedsSegmentLimit {
                        record_bytes,
                        segment_max: max_segment_bytes,
                    });
                }
                match &inner.segment {
                    SegmentState::RollingOver => Action::Rollover {
                        observed_seq: inner.active_segment_seq,
                    },
                    SegmentState::Open { bytes, .. } => {
                        let projected = WAL_SEGMENT_HEADER_LEN + *bytes + record_bytes;
                        if projected > max_segment_bytes {
                            Action::Rollover {
                                observed_seq: inner.active_segment_seq,
                            }
                        } else {
                            Action::Append {
                                lsn: inner.next_lsn,
                            }
                        }
                    }
                }
            };

            match action {
                Action::Rollover { observed_seq } => {
                    // Make sure everything we already submitted lands on
                    // the old segment durably before we tear it down.
                    for h in handles.drain(..) {
                        let durable_lsn = h.wait().await?;
                        let _ = durable_lsn;
                    }
                    self.rollover(observed_seq).await?;
                    pending = Some(record);
                    continue;
                }
                Action::Append { lsn } => {
                    record.lsn = Lsn(lsn);
                    // Single borrow_mut: enqueue + bump counters. The
                    // committer's `append` is synchronous (flume push),
                    // so the borrow window doesn't span an await.
                    // Bumping `next_lsn` early (before durability) is
                    // safe because a flush failure makes the WAL "broken"
                    // and all subsequent appends error — no LSN reuse
                    // window opens.
                    //
                    // No `.await` separates step A from this borrow, so on
                    // this single-threaded executor the slot is still the
                    // `Open` state step A observed. The `RollingOver` arm is
                    // therefore unreachable in practice; if it ever fires we
                    // re-queue this record (without consuming its LSN) rather
                    // than panic.
                    let handle = {
                        let mut inner = self.inner.borrow_mut();
                        let WalInner {
                            next_lsn, segment, ..
                        } = &mut *inner;
                        match segment {
                            SegmentState::Open { committer, bytes } => {
                                let h = committer.append(record.clone())?;
                                *bytes += record_bytes;
                                *next_lsn = lsn + 1;
                                Some(h)
                            }
                            SegmentState::RollingOver => None,
                        }
                    };
                    match handle {
                        Some(h) => {
                            handles.push(h);
                            assigned.push(Lsn(lsn));
                        }
                        None => {
                            pending = Some(record);
                            continue;
                        }
                    }
                }
            }
        }

        // Await durability for the whole batch. GroupCommitter coalesces
        // the entire vector into one fsync as long as the records reach
        // the committer task within the commit_window.
        for h in handles {
            let durable_lsn = h.wait().await?;
            let _ = durable_lsn;
        }

        Ok(assigned)
    }

    /// Roll the active segment over to the next one.
    ///
    /// `observed_seq` is the `active_segment_seq` the caller saw when it
    /// decided a rollover was needed. This makes rollover safe under
    /// concurrent append on the single shard executor:
    ///
    /// 1. Acquire the exclusive rollover permit. A second appender that also
    ///    chose rollover blocks here until the first finishes.
    /// 2. Re-check `active_segment_seq`. If it advanced past `observed_seq`,
    ///    another task already rolled the segment we saw full — this call is
    ///    a no-op and the caller retries its append against the fresh
    ///    committer. This prevents a spurious second rollover that would tear
    ///    down a freshly-installed (empty) committer and create an empty
    ///    segment.
    /// 3. Otherwise transition the slot to `RollingOver` (taking the old
    ///    committer out), drain + close the old segment durably, create the
    ///    new segment, fsync the directory, then install the fresh committer
    ///    and bump the sequence — all before releasing the permit.
    ///
    /// The old segment is drained and closed *before* the new segment is
    /// created, preserving the invariant that every record in a lower-seq
    /// segment is durable before any record lands in a higher-seq one (so
    /// recovery never sees an LSN gap straddling a segment boundary).
    async fn rollover(&self, observed_seq: u64) -> Result<(), WalError> {
        // Step 1: serialize — only one task transitions the segment at a time.
        let _permit = self
            .rollover_lock
            .acquire_permit(1)
            .await
            .expect("invariant: rollover semaphore is never closed");

        // Step 2 + 3a: under a short borrow, bail if the segment already
        // rolled, else take the old committer and mark the slot RollingOver.
        let (old_committer, dir, shard_uuid, new_seq, new_starting_lsn, group_commit_cfg) = {
            let mut inner = self.inner.borrow_mut();
            if inner.active_segment_seq != observed_seq {
                // Someone rolled past the segment we saw full. Nothing to do.
                return Ok(());
            }
            let new_seq = inner.active_segment_seq + 1;
            let new_starting_lsn = inner.next_lsn;
            let dir = inner.dir.clone();
            let shard_uuid = inner.shard_uuid;
            let group_commit_cfg = inner.config.group_commit;
            // We hold the exclusive permit and the sequence matched, so the
            // slot must be `Open` — no other task can be mid-rollover. A
            // `RollingOver` slot here can only mean a *previous* rollover
            // failed after taking the committer; treat the WAL as broken
            // rather than tear down twice.
            let old_committer =
                match std::mem::replace(&mut inner.segment, SegmentState::RollingOver) {
                    SegmentState::Open { committer, .. } => committer,
                    SegmentState::RollingOver => {
                        return Err(WalError::Commit(CommitError::WalBroken(
                            "segment rollover previously failed; WAL is broken".into(),
                        )));
                    }
                };
            (
                old_committer,
                dir,
                shard_uuid,
                new_seq,
                new_starting_lsn,
                group_commit_cfg,
            )
        };

        // Step 3b: shutdown the old committer (await) without any borrow held.
        let old_segment = old_committer.shutdown().await?;
        old_segment.close().await?;

        // Step 3c: create the new segment, fsync the directory.
        let new_path = segment_path(&dir, new_seq);
        let new_segment =
            WalSegment::create_new(&new_path, new_seq, new_starting_lsn, shard_uuid).await?;
        fsync_dir(&dir)?;

        // Step 3d: install the new committer + advance the sequence under a
        // short borrow. Resetting bytes to 0 is implicit in the fresh
        // `Open`; a concurrent appender that was blocked on the permit now
        // observes the advanced sequence and retries against this committer.
        {
            let mut inner = self.inner.borrow_mut();
            inner.segment = SegmentState::Open {
                committer: GroupCommitter::start(new_segment, group_commit_cfg),
                bytes: 0,
            };
            inner.active_segment_seq = new_seq;
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
            match std::mem::replace(&mut inner.segment, SegmentState::RollingOver) {
                SegmentState::Open { committer, .. } => Some(committer),
                SegmentState::RollingOver => None,
            }
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
        //
        // Overwriting the slot drops any live `Open` committer, signaling its
        // detached task to wind down.
        self.inner.borrow_mut().segment = SegmentState::RollingOver;
    }
}

impl core::fmt::Debug for Wal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let inner = self.inner.borrow();
        let (rolling_over, bytes_in_active_segment) = match &inner.segment {
            SegmentState::Open { bytes, .. } => (false, *bytes),
            SegmentState::RollingOver => (true, 0),
        };
        f.debug_struct("Wal")
            .field("dir", &inner.dir)
            .field("shard_uuid", &inner.shard_uuid)
            .field("next_lsn", &inner.next_lsn)
            .field("active_segment_seq", &inner.active_segment_seq)
            .field("rolling_over", &rolling_over)
            .field("bytes_in_active_segment", &bytes_in_active_segment)
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
/// directory entry is durable (step 4). Stays sync because
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
            space_id_lo64: 0xCAFE_BABE_DEAD_BEEF,
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

    #[test]
    fn concurrent_appends_assign_distinct_contiguous_lsns() {
        // Regression for the append() LSN race. Two appends running on
        // the same single-threaded executor interleave at the
        // `handle.wait().await` durability point. The committer enqueue
        // and the `next_lsn` bump now happen in one synchronous borrow
        // *before* that await — so a task that yields can never leave a
        // stale `next_lsn` for the other task to re-read. The historical
        // bug bumped after the await: task B read task A's un-bumped LSN,
        // stamped a duplicate, and recovery tripped LsnGap.
        use std::rc::Rc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let p = path.clone();
        const PER_TASK: u64 = 50;
        glommio_run(move || async move {
            let wal = Rc::new(Wal::create(&p, uuid(7)).await.unwrap());

            let w1 = wal.clone();
            let t1 = glommio::spawn_local(async move {
                let mut got = Vec::new();
                for _ in 0..PER_TASK {
                    got.push(w1.append(record(0)).await.unwrap().raw());
                }
                got
            });
            let w2 = wal.clone();
            let t2 = glommio::spawn_local(async move {
                let mut got = Vec::new();
                for _ in 0..PER_TASK {
                    got.push(w2.append(record(0)).await.unwrap().raw());
                }
                got
            });

            let mut all: Vec<u64> = t1.await;
            all.extend(t2.await);
            all.sort_unstable();

            // Every LSN distinct and exactly 1..=2*PER_TASK — no dup, no gap.
            assert_eq!(
                all,
                (1..=2 * PER_TASK).collect::<Vec<_>>(),
                "concurrent appends must assign distinct, contiguous LSNs"
            );

            wal.shutdown_in_place().await.unwrap();
        });

        // Recovery enforces sequential LSNs; a dup would surface as LsnGap.
        let reader = WalReader::open(&path, uuid(7)).unwrap();
        let lsns: Vec<u64> = reader.into_iter().map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, (1..=2 * PER_TASK).collect::<Vec<_>>());
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
    fn concurrent_appends_across_rollover_stay_contiguous() {
        // Regression for the rollover concurrency panic. With a SMALL segment
        // cap, two tasks appending concurrently on the same single-threaded
        // executor repeatedly straddle segment boundaries. The historical bug:
        // task A entered rollover, `take()`d the committer, and parked on
        // `shutdown().await`; while parked, task B saw the still-full byte
        // counter, also chose rollover, and hit `take().expect(...)` on the
        // now-absent committer → shard-crashing panic.
        //
        // The fix serializes rollover behind a permit and models the slot as
        // a sum type, so a concurrent appender that observes the mid-rollover
        // slot blocks on the permit, then finds the segment already advanced
        // and retries against the fresh committer — no double teardown, no
        // panic. This test asserts: no panic, distinct+contiguous LSNs, the
        // segment sequence actually advanced (rollovers happened), and every
        // record readable back in order after recovery with no dup/loss.
        use std::rc::Rc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let p = path.clone();
        // ~1–2 records per segment forces heavy concurrent rollover contention.
        let small_cap = WAL_SEGMENT_HEADER_LEN + 200;
        let cfg = WalConfig {
            group_commit: GroupCommitConfig::default(),
            max_segment_bytes: small_cap,
        };
        const PER_TASK: u64 = 50;
        let final_seq = glommio_run(move || async move {
            let wal = Rc::new(Wal::create_with_config(&p, uuid(8), cfg).await.unwrap());

            let w1 = wal.clone();
            let t1 = glommio::spawn_local(async move {
                let mut got = Vec::new();
                for _ in 0..PER_TASK {
                    got.push(w1.append(record_with_payload_size(64)).await.unwrap().raw());
                }
                got
            });
            let w2 = wal.clone();
            let t2 = glommio::spawn_local(async move {
                let mut got = Vec::new();
                for _ in 0..PER_TASK {
                    got.push(w2.append(record_with_payload_size(64)).await.unwrap().raw());
                }
                got
            });

            let mut all: Vec<u64> = t1.await;
            all.extend(t2.await);
            all.sort_unstable();

            // Distinct + contiguous: no dup, no gap.
            assert_eq!(
                all,
                (1..=2 * PER_TASK).collect::<Vec<_>>(),
                "concurrent appends across rollover must assign distinct, contiguous LSNs"
            );

            let seq = wal.active_segment_seq();
            wal.shutdown_in_place().await.unwrap();
            seq
        });

        // Rollovers actually happened (many, given ~1–2 records/segment) and
        // the sequence advanced monotonically — not the spurious double
        // rollovers the old race would have produced.
        assert!(
            final_seq >= 10,
            "expected many rollovers under contention, got seq {final_seq}"
        );

        // Recovery reads every segment in order; a dup or a straddling gap
        // would surface as LsnGap. All records durable, in order, none lost.
        let reader = WalReader::open(&path, uuid(8)).unwrap();
        let lsns: Vec<u64> = reader.into_iter().map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, (1..=2 * PER_TASK).collect::<Vec<_>>());
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
        // Reopen, append more, verify LSN sequence continues. A clean
        // shutdown left no torn tail, so the recovered offset is the full
        // on-disk size (no truncation).
        let p2 = path.clone();
        let clean_size = std::fs::metadata(path.join("0000000000.wal"))
            .unwrap()
            .len();
        glommio_run(move || async move {
            let wal = Wal::open_existing(&p2, uuid(20), 4, clean_size, WalConfig::default())
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
            let err = Wal::open_existing(
                &path,
                uuid(21),
                1,
                WAL_SEGMENT_HEADER_LEN as u64,
                WalConfig::default(),
            )
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
            let err = Wal::open_existing(
                &p2,
                uuid(99),
                2,
                WAL_SEGMENT_HEADER_LEN as u64,
                WalConfig::default(),
            )
            .await
            .expect_err("uuid mismatch must fail");
            // WalReader catches the uuid mismatch before we get to
            // open_for_append, so the error surfaces as Read(_).
            assert!(matches!(err, WalError::Read(_)), "got {err:?}");
        });
    }

    // ----- Subscribe-replay accessors ----------------------------------

    #[test]
    fn oldest_available_lsn_empty_wal_returns_next_lsn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        glommio_run(move || async move {
            let wal = Wal::create(&path, uuid(30)).await.unwrap();
            // Empty WAL: zero records, segment 0 with starting_lsn=1.
            // The first segment's starting_lsn IS 1, so oldest reads as 1.
            // Subscribe treats this as "everything still in the WAL."
            assert_eq!(wal.oldest_available_lsn().unwrap(), 1);
            wal.shutdown().await.unwrap();
        });
    }

    #[test]
    fn oldest_available_lsn_after_rollover_reports_first_segment_start() {
        // After rollover, segments 0 and 1 both exist on disk; retention
        // hasn't GC'd yet, so the first segment's starting_lsn (1) is
        // still the oldest available.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let small_cap = WAL_SEGMENT_HEADER_LEN + 200;
        let cfg = WalConfig {
            group_commit: GroupCommitConfig::default(),
            max_segment_bytes: small_cap,
        };
        glommio_run(move || async move {
            let wal = Wal::create_with_config(&path, uuid(31), cfg).await.unwrap();
            wal.append(record_with_payload_size(64)).await.unwrap();
            wal.append(record_with_payload_size(64)).await.unwrap();
            assert!(wal.active_segment_seq() >= 1);
            // Both segments still on disk; first one's starting_lsn = 1.
            assert_eq!(wal.oldest_available_lsn().unwrap(), 1);
            wal.shutdown().await.unwrap();
        });
    }

    #[test]
    fn current_tail_lsn_increments_with_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        glommio_run(move || async move {
            let wal = Wal::create(&path, uuid(32)).await.unwrap();
            assert_eq!(wal.current_tail_lsn(), 1);
            wal.append(record(0)).await.unwrap();
            assert_eq!(wal.current_tail_lsn(), 2);
            wal.append(record(0)).await.unwrap();
            wal.append(record(0)).await.unwrap();
            assert_eq!(wal.current_tail_lsn(), 4);
            wal.shutdown().await.unwrap();
        });
    }

    // ----- Executor responsiveness during commit bursts -----------------
    //
    // Sibling task increments a counter every 100 µs while we burst-append
    // 200 records. Asserts the counter advanced — i.e. the executor was
    // NOT stalled inside fsync.

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

    // ===================================================================
    // Rollover stress / chaos: recovery + durability under segment rollover.
    //
    // These push the segment-rollover concurrency fix hard: many
    // concurrent appenders on one Glommio executor, segment caps tiny
    // enough to force a rollover every 1–4 records, driven by a
    // deterministic seeded RNG so a failure reproduces from its seed.
    // ===================================================================

    /// Deterministic splitmix64 — no wall-clock, no `rand` crate, so a
    /// failing scenario reproduces verbatim from its seed.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// Uniform-ish integer in `[lo, hi]` inclusive.
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            debug_assert!(hi >= lo);
            lo + self.next_u64() % (hi - lo + 1)
        }
    }

    fn record_with_kind(kind: WalRecordKind, payload_bytes: usize) -> WalRecord {
        let mut r = record_with_payload_size(payload_bytes);
        r.kind = kind;
        r
    }

    /// One appender task's plan: which record kind, payload size, and how
    /// many records it will append.
    #[derive(Clone, Copy)]
    struct TaskSpec {
        kind: WalRecordKind,
        payload: usize,
        count: u64,
    }

    /// Spawn one `spawn_local` appender per `TaskSpec` against a shared
    /// `Wal`, run them concurrently to completion, then `shutdown_in_place`.
    /// Returns every assigned LSN (sorted) and the final `active_segment_seq`.
    ///
    /// All appenders share one single-threaded executor, so they interleave
    /// only at `.await` points — exactly the window the rollover fix
    /// serializes. The returned LSN multiset is the ground truth for the
    /// distinct/contiguous invariant; `final_seq` is checked against the
    /// deterministic packing to catch spurious (empty-segment) rollovers.
    fn run_concurrent_appenders(
        dir: &std::path::Path,
        shard: [u8; 16],
        cfg: WalConfig,
        specs: Vec<TaskSpec>,
    ) -> (Vec<u64>, u64) {
        use std::rc::Rc;
        let dir = dir.to_owned();
        glommio_run(move || async move {
            let wal = Rc::new(Wal::create_with_config(&dir, shard, cfg).await.unwrap());

            let mut tasks = Vec::new();
            for spec in specs {
                let w = wal.clone();
                tasks.push(glommio::spawn_local(async move {
                    let mut got = Vec::with_capacity(spec.count as usize);
                    for _ in 0..spec.count {
                        let lsn = w
                            .append(record_with_kind(spec.kind, spec.payload))
                            .await
                            .unwrap();
                        got.push(lsn.raw());
                    }
                    got
                }));
            }

            let mut all = Vec::new();
            for t in tasks {
                all.extend(t.await);
            }
            all.sort_unstable();

            let final_seq = wal.active_segment_seq();
            wal.shutdown_in_place().await.unwrap();
            (all, final_seq)
        })
    }

    /// **Stress 1 — high concurrency + tiny segments, seeded scenarios.**
    ///
    /// For each seed: 4–8 appenders, each writing a random record count,
    /// against a segment cap sized to hold only `per_seg` (1–4) records.
    /// Asserts, per scenario:
    ///   - no panic (rollover races don't hit the old `expect()` on a
    ///     taken committer),
    ///   - every LSN `1..=N` assigned exactly once — no dup, no gap,
    ///   - the segment count equals the deterministic packing
    ///     `ceil(N / per_seg)` — real rollovers only, no spurious
    ///     empty-segment rollovers and none skipped,
    ///   - recovery (`WalReader`) reads back `1..=N` in order with no
    ///     straddling boundary gap.
    #[test]
    fn rollover_stress_high_concurrency_tiny_segments() {
        // 12 seeded scenarios. Seeds are arbitrary but fixed; a failure
        // prints the seed so it reproduces exactly.
        for scenario in 0..12u64 {
            let seed = 0xA5A5_0000_0000_0001u64
                .wrapping_mul(scenario.wrapping_add(1))
                .rotate_left(scenario as u32 & 63);
            let mut rng = SplitMix64::new(seed);

            let payload = [32usize, 48, 64][(rng.range(0, 2)) as usize];
            let rb = record_with_payload_size(payload).encoded_len();
            let per_seg = rng.range(1, 4); // records that fit per segment
                                           // capacity_bytes must hold exactly `per_seg` records:
                                           // per_seg*rb <= capacity < (per_seg+1)*rb.
            let capacity_bytes = (per_seg * rb as u64) + rng.range(0, rb as u64 - 1);
            let cap = WAL_SEGMENT_HEADER_LEN + capacity_bytes as usize;
            let cfg = WalConfig {
                group_commit: GroupCommitConfig::default(),
                max_segment_bytes: cap,
            };

            let task_count = rng.range(4, 8);
            let mut specs = Vec::new();
            let mut n: u64 = 0;
            for _ in 0..task_count {
                let count = rng.range(15, 30);
                n += count;
                specs.push(TaskSpec {
                    kind: WalRecordKind::Encode,
                    payload,
                    count,
                });
            }

            let dir = tempfile::tempdir().unwrap();
            let (lsns, final_seq) = run_concurrent_appenders(dir.path(), uuid(40), cfg, specs);

            // Distinct + contiguous: exactly 1..=N once each.
            assert_eq!(
                lsns,
                (1..=n).collect::<Vec<_>>(),
                "seed {seed:#x}: LSNs not distinct+contiguous \
                 (payload={payload} rb={rb} per_seg={per_seg} cap={cap} N={n})"
            );

            // Deterministic packing: one segment per `per_seg` records, no
            // spurious rollovers (which would leave empty segments and push
            // final_seq higher), none skipped (which would pack too many).
            let expected_segments = n.div_ceil(per_seg);
            assert_eq!(
                final_seq + 1,
                expected_segments,
                "seed {seed:#x}: segment count {} != expected {} \
                 (spurious or missed rollover) per_seg={per_seg} N={n}",
                final_seq + 1,
                expected_segments
            );
            assert!(
                final_seq >= 8,
                "seed {seed:#x}: only {final_seq} rollovers — scenario not stressful"
            );

            // Recovery reads every record back in order across all
            // boundaries; a dup or straddling gap surfaces as LsnGap.
            let reader = WalReader::open(dir.path(), uuid(40)).unwrap();
            let recovered: Vec<u64> = reader.into_iter().map(|r| r.unwrap().lsn.raw()).collect();
            assert_eq!(
                recovered,
                (1..=n).collect::<Vec<_>>(),
                "seed {seed:#x}: recovery did not yield 1..=N in order"
            );
        }
    }

    /// **Stress 2 — rollover interleaved with a checkpoint-shaped appender.**
    ///
    /// The original race was the writer's drain and the snapshot worker's
    /// CHECKPOINT records both appending across a rollover boundary. Here a
    /// dedicated task appends `CheckpointBegin`/`CheckpointEnd`-kind records
    /// concurrently with several data appenders, all straddling tiny-segment
    /// boundaries. The wal layer stamps LSNs identically regardless of kind,
    /// so the same distinct/contiguous/recovery invariants must hold.
    ///
    /// Limitation: these are checkpoint-*kind* records with opaque payloads,
    /// not fully-formed CHECKPOINT payloads (the snapshot worker lives above
    /// the wal layer). What is faithfully reproduced is the concurrency shape
    /// — a second independent appender racing the writer through rollovers.
    #[test]
    fn rollover_interleaved_with_checkpoint_appender() {
        for scenario in 0..8u64 {
            let seed = 0xC4EC_C001_0000_0001u64
                .wrapping_mul(scenario.wrapping_add(1))
                .rotate_left((scenario as u32).wrapping_mul(7) & 63);
            let mut rng = SplitMix64::new(seed);

            let payload = [32usize, 40, 56][(rng.range(0, 2)) as usize];
            let rb = record_with_payload_size(payload).encoded_len();
            let per_seg = rng.range(1, 3);
            let capacity_bytes = (per_seg * rb as u64) + rng.range(0, rb as u64 - 1);
            let cap = WAL_SEGMENT_HEADER_LEN + capacity_bytes as usize;
            let cfg = WalConfig {
                group_commit: GroupCommitConfig::default(),
                max_segment_bytes: cap,
            };

            // 3–5 data appenders + one checkpoint-shaped appender that
            // alternates begin/end via its own kind (payload fixed so it
            // packs with the same `per_seg`).
            let data_tasks = rng.range(3, 5);
            let mut specs = Vec::new();
            let mut n: u64 = 0;
            for _ in 0..data_tasks {
                let count = rng.range(12, 24);
                n += count;
                specs.push(TaskSpec {
                    kind: WalRecordKind::Encode,
                    payload,
                    count,
                });
            }
            let ckpt_count = rng.range(10, 20);
            n += ckpt_count;
            // CheckpointBegin and CheckpointEnd are both substrate kinds; a
            // single kind is enough to exercise the concurrent-appender race
            // and keeps `per_seg` packing uniform.
            specs.push(TaskSpec {
                kind: WalRecordKind::CheckpointEnd,
                payload,
                count: ckpt_count,
            });

            let dir = tempfile::tempdir().unwrap();
            let (lsns, final_seq) = run_concurrent_appenders(dir.path(), uuid(41), cfg, specs);

            assert_eq!(
                lsns,
                (1..=n).collect::<Vec<_>>(),
                "seed {seed:#x}: data+checkpoint appenders must assign \
                 distinct, contiguous LSNs (per_seg={per_seg} N={n})"
            );
            let expected_segments = n.div_ceil(per_seg);
            assert_eq!(
                final_seq + 1,
                expected_segments,
                "seed {seed:#x}: segment count {} != expected {}",
                final_seq + 1,
                expected_segments
            );

            let reader = WalReader::open(dir.path(), uuid(41)).unwrap();
            let recovered: Vec<u64> = reader.into_iter().map(|r| r.unwrap().lsn.raw()).collect();
            assert_eq!(
                recovered,
                (1..=n).collect::<Vec<_>>(),
                "seed {seed:#x}: recovery did not yield 1..=N in order"
            );
        }
    }

    /// **Stress 3 — recovery after rollover under load, incl. torn tail.**
    ///
    /// Runs a concurrent-rollover scenario, then exercises the reopen /
    /// recovery path three ways:
    ///   1. clean reopen: `WalReader` yields `1..=N` in order (full record
    ///      recovery across every boundary),
    ///   2. torn tail: a partial trailing write is appended to the active
    ///      (highest-seq) segment; the reader treats it as a clean end and
    ///      still yields exactly `1..=N` (`last_decoded_lsn == N`),
    ///   3. truncating reopen: `Wal::open_existing` at the pre-torn offset
    ///      physically drops the garbage and resumes appends at `N+1`, and a
    ///      subsequent read yields `1..=N+1` with no gap.
    #[test]
    fn recovery_after_rollover_under_load_handles_torn_tail() {
        let seed = 0xD00D_FEED_1234_5678u64;
        let mut rng = SplitMix64::new(seed);

        let payload = 48usize;
        let rb = record_with_payload_size(payload).encoded_len();
        let per_seg = rng.range(1, 3);
        let capacity_bytes = (per_seg * rb as u64) + rng.range(0, rb as u64 - 1);
        let cap = WAL_SEGMENT_HEADER_LEN + capacity_bytes as usize;
        let cfg = WalConfig {
            group_commit: GroupCommitConfig::default(),
            max_segment_bytes: cap,
        };

        let mut specs = Vec::new();
        let mut n: u64 = 0;
        for _ in 0..rng.range(4, 6) {
            let count = rng.range(15, 25);
            n += count;
            specs.push(TaskSpec {
                kind: WalRecordKind::Encode,
                payload,
                count,
            });
        }

        let dir = tempfile::tempdir().unwrap();
        let (lsns, final_seq) = run_concurrent_appenders(dir.path(), uuid(42), cfg, specs);
        assert_eq!(lsns, (1..=n).collect::<Vec<_>>(), "seed {seed:#x}");
        assert!(final_seq >= 8, "seed {seed:#x}: expected many rollovers");

        // (1) Clean reopen: full record recovery across every boundary.
        {
            let reader = WalReader::open(dir.path(), uuid(42)).unwrap();
            let recovered: Vec<u64> = reader.into_iter().map(|r| r.unwrap().lsn.raw()).collect();
            assert_eq!(recovered, (1..=n).collect::<Vec<_>>(), "clean recovery");
        }

        // Capture the active segment's clean, durable size before we
        // simulate a crash torn tail on it.
        let active_path = segment_path(dir.path(), final_seq);
        let clean_size = std::fs::metadata(&active_path).unwrap().len();
        assert!(clean_size >= WAL_SEGMENT_HEADER_LEN as u64);

        // (2) Append a partial trailing write (shorter than a record header)
        // to mimic a crash mid-append. On the last segment this is a torn
        // final write — the reader drops it and stops cleanly at N.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&active_path)
                .unwrap();
            f.write_all(&[0xFFu8; 16]).unwrap();
            f.sync_all().unwrap();
        }
        {
            let mut reader = WalReader::open(dir.path(), uuid(42)).unwrap();
            let recovered: Vec<u64> = reader.by_ref().map(|r| r.unwrap().lsn.raw()).collect();
            assert_eq!(
                recovered,
                (1..=n).collect::<Vec<_>>(),
                "torn tail must be dropped, not misread"
            );
            assert_eq!(reader.last_decoded_lsn(), Some(n));
        }

        // (3) Truncating reopen: open_existing physically drops the garbage
        // past the recovered tail, resumes appends at N+1.
        let dir_path = dir.path().to_owned();
        let ap = active_path.clone();
        glommio_run(move || async move {
            let wal = Wal::open_existing(
                &dir_path,
                uuid(42),
                n + 1,      // next_lsn recovery would compute
                clean_size, // recovered_tail_offset: before the torn bytes
                cfg,
            )
            .await
            .expect("reopen after torn tail");
            assert_eq!(wal.next_lsn(), n + 1);
            // The active segment shrank back to its clean size — the torn
            // bytes are gone before any new append follows them.
            assert_eq!(std::fs::metadata(&ap).unwrap().len(), clean_size);

            let lsn = wal.append(record_with_payload_size(payload)).await.unwrap();
            assert_eq!(lsn, Lsn(n + 1));
            wal.shutdown_in_place().await.unwrap();
        });

        // Final recovery: 1..=N+1 contiguous, torn bytes never resurfaced.
        let reader = WalReader::open(dir.path(), uuid(42)).unwrap();
        let recovered: Vec<u64> = reader.into_iter().map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(
            recovered,
            (1..=n + 1).collect::<Vec<_>>(),
            "post-truncation recovery must yield 1..=N+1"
        );
    }
}
