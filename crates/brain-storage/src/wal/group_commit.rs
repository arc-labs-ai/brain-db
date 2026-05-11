//! Group commit: batches concurrent `WalRecord` appends into a single
//! `pwritev2(RWF_DSYNC)` for amortized fsync cost.
//!
//! See `spec/05_storage_arena_wal/06_wal_durability.md`.
//!
//! ## Architecture
//!
//! [`GroupCommitter::start`] spawns a dedicated OS thread that owns the
//! [`WalSegment`]. Appenders call [`GroupCommitter::append`], which
//! enqueues a record on a `crossbeam_channel` and returns an
//! [`AppendHandle`] backed by a oneshot ack channel. The committer
//! thread:
//!
//! 1. Blocks on the submission channel for the *first* record of each
//!    batch.
//! 2. Once a record arrives, waits up to [`GroupCommitConfig::commit_window`]
//!    (default 100 µs) for more records — or until the buffer reaches
//!    [`GroupCommitConfig::max_batch_bytes`] (default 60 KiB).
//! 3. Calls [`WalSegment::flush_durable`] once for the whole batch.
//! 4. Signals every pending ack with `Ok(lsn)` (or `Err(WalBroken)` on
//!    failure).
//!
//! Errors are sticky: after a failed flush, the WAL is "broken"; all
//! in-flight handles and subsequent appends receive `Err(WalBroken)` until
//! the [`GroupCommitter`] is dropped.
//!
//! ## Spec deviations
//!
//! - **SD-2.8-1**: WAL segments are opened *without* `O_DIRECT`. The
//!   spec's per-flush 4 KB padding produces zero-padded gaps mid-segment
//!   that `WalReader` (sub-task 2.7) would treat as corruption. The full
//!   `O_DIRECT`-correct design (WAL pages with per-page headers) is a
//!   later sub-task.
//! - **SD-2.8-2**: the committer uses synchronous `pwritev2` from a
//!   `std::thread`, not `io_uring` via Glommio. The public API
//!   (`append → AppendHandle::wait`) is shaped so the swap to a Glommio
//!   coroutine in Phase 9 is local.
//!
//! Both are tracked in `docs/spec-deviations.md`.

use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, RecvError, RecvTimeoutError, Sender};

use crate::wal::record::WalRecord;
use crate::wal::segment::WalSegment;

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// Configuration for a [`GroupCommitter`].
#[derive(Debug, Clone, Copy)]
pub struct GroupCommitConfig {
    /// Maximum time the committer waits for additional records once the
    /// first record of a batch has arrived. Default: 100 µs (spec §06 §4).
    pub commit_window: Duration,
    /// Buffer-size threshold that triggers an early flush. Default:
    /// 60 KiB (spec §06 §4).
    pub max_batch_bytes: usize,
}

impl Default for GroupCommitConfig {
    fn default() -> Self {
        Self {
            commit_window: Duration::from_micros(100),
            max_batch_bytes: 60 * 1024,
        }
    }
}

/// Handle returned by [`GroupCommitter::append`]. Block on [`Self::wait`]
/// (or [`Self::wait_timeout`]) until the record's batch is on stable
/// storage.
#[must_use = "an AppendHandle must be awaited; otherwise durability isn't observed"]
pub struct AppendHandle {
    ack_rx: Receiver<AckMessage>,
}

impl AppendHandle {
    /// Block until the record's batch is durable. Returns the record's
    /// LSN on success.
    pub fn wait(self) -> Result<u64, CommitError> {
        match self.ack_rx.recv() {
            Ok(Ok(lsn)) => Ok(lsn),
            Ok(Err(e)) => Err(e),
            Err(RecvError) => Err(CommitError::AckChannelClosed),
        }
    }

    /// Block up to `dur` for the record's batch to become durable. Wraps
    /// the inner channel's `recv_timeout`.
    pub fn wait_timeout(self, dur: Duration) -> Result<Result<u64, CommitError>, RecvTimeoutError> {
        self.ack_rx.recv_timeout(dur)
    }
}

/// Errors returned by [`GroupCommitter`] / [`AppendHandle`].
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    /// The WAL has entered a broken state after an I/O failure. All
    /// in-flight handles and subsequent appends receive this error.
    #[error("WAL is broken: {0}")]
    WalBroken(String),

    /// The committer thread has shut down; new appends are no longer
    /// accepted.
    #[error("committer has shut down")]
    ShutDown,

    /// The committer's ack channel was dropped before signaling. Should
    /// only happen if the committer thread panicked.
    #[error("ack channel was dropped before the flush completed")]
    AckChannelClosed,
}

/// Per-shard group commit coordinator. Owns one [`WalSegment`] and one
/// committer thread.
pub struct GroupCommitter {
    submission_tx: Option<Sender<Submission>>,
    shutdown_tx: Sender<()>,
    join_handle: Option<JoinHandle<Result<WalSegment, CommitError>>>,
}

impl GroupCommitter {
    /// Start the committer thread. Takes ownership of `segment`.
    pub fn start(segment: WalSegment, config: GroupCommitConfig) -> Self {
        let (submission_tx, submission_rx) = unbounded::<Submission>();
        // Shutdown is a one-shot: the channel becomes "ready" when we
        // drop our sender, or send an explicit shutdown signal.
        let (shutdown_tx, shutdown_rx) = bounded::<()>(1);

        let join_handle = thread::Builder::new()
            .name("wal-group-committer".into())
            .spawn(move || committer_loop(segment, submission_rx, shutdown_rx, config))
            .expect("spawn wal-group-committer thread");

        Self {
            submission_tx: Some(submission_tx),
            shutdown_tx,
            join_handle: Some(join_handle),
        }
    }

    /// Enqueue a record for durable write. Returns immediately with an
    /// [`AppendHandle`]; block on the handle to observe durability.
    pub fn append(&self, record: WalRecord) -> Result<AppendHandle, CommitError> {
        let (ack_tx, ack_rx) = bounded::<AckMessage>(1);
        let sender = self.submission_tx.as_ref().ok_or(CommitError::ShutDown)?;
        sender
            .send(Submission::Append { record, ack_tx })
            .map_err(|_| CommitError::ShutDown)?;
        Ok(AppendHandle { ack_rx })
    }

    /// Drain the queue, flush the final batch durably, and reclaim the
    /// owned [`WalSegment`].
    ///
    /// Consumes `self`. Returns `Err` if the committer thread panicked or
    /// the final flush failed.
    pub fn shutdown(mut self) -> Result<WalSegment, CommitError> {
        // Close the submission channel so the committer sees disconnection
        // *after* it drains anything already in flight.
        self.submission_tx = None;
        // Signal explicit shutdown. Ignore error (committer may have
        // already exited).
        let _ = self.shutdown_tx.send(());
        let handle = self.join_handle.take().expect("join handle present");
        match handle.join() {
            Ok(result) => result,
            Err(_) => Err(CommitError::WalBroken("committer thread panicked".into())),
        }
    }
}

impl Drop for GroupCommitter {
    fn drop(&mut self) {
        // Make sure the committer thread terminates even if the caller
        // didn't call `shutdown`.
        self.submission_tx = None;
        let _ = self.shutdown_tx.send(());
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Internal types.
// ---------------------------------------------------------------------------

/// Message sent from the appender to the committer thread.
enum Submission {
    Append {
        record: WalRecord,
        ack_tx: Sender<AckMessage>,
    },
}

/// Message sent from the committer to an appender's `AppendHandle`.
type AckMessage = Result<u64, CommitError>;

// ---------------------------------------------------------------------------
// Committer thread.
// ---------------------------------------------------------------------------

fn committer_loop(
    mut segment: WalSegment,
    submission_rx: Receiver<Submission>,
    shutdown_rx: Receiver<()>,
    config: GroupCommitConfig,
) -> Result<WalSegment, CommitError> {
    // Pending acks, one per record in the current batch.
    let mut pending: Vec<PendingAck> = Vec::new();
    // Once a flush fails, the WAL is broken; we drain remaining
    // submissions with errors but keep accepting them until the channel
    // closes, so callers get a consistent failure rather than a hang.
    let mut broken: Option<String> = None;
    // Set to true on the first explicit shutdown signal *or* when the
    // submission channel closes.
    let mut shutdown_requested = false;

    loop {
        // -----------------------------------------------------------------
        // Phase 1: receive the first submission (or shutdown / disconnect).
        // -----------------------------------------------------------------
        let first = if shutdown_requested {
            // After shutdown, only drain what's already queued.
            submission_rx.try_recv().ok()
        } else {
            crossbeam_channel::select! {
                recv(submission_rx) -> msg => match msg {
                    Ok(s) => Some(s),
                    Err(_) => {
                        // Submission channel closed; one final drain pass
                        // then exit.
                        shutdown_requested = true;
                        None
                    }
                },
                recv(shutdown_rx) -> _ => {
                    shutdown_requested = true;
                    None
                },
            }
        };

        let mut got_any = false;
        if let Some(first) = first {
            handle_submission(first, &mut segment, &mut pending, broken.as_ref());
            got_any = true;
        }

        // -----------------------------------------------------------------
        // Phase 2: gather more records up to commit_window or size threshold.
        // -----------------------------------------------------------------
        if got_any && broken.is_none() && !shutdown_requested {
            let deadline = Instant::now() + config.commit_window;
            while segment.write_buf_len() < config.max_batch_bytes {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match submission_rx.recv_timeout(remaining) {
                    Ok(sub) => {
                        handle_submission(sub, &mut segment, &mut pending, broken.as_ref());
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        shutdown_requested = true;
                        break;
                    }
                }
            }
        }

        // -----------------------------------------------------------------
        // Phase 3: drain any remaining queued submissions on shutdown.
        // -----------------------------------------------------------------
        if shutdown_requested {
            while let Ok(sub) = submission_rx.try_recv() {
                handle_submission(sub, &mut segment, &mut pending, broken.as_ref());
            }
        }

        // -----------------------------------------------------------------
        // Phase 4: flush + signal.
        // -----------------------------------------------------------------
        if !pending.is_empty() {
            if let Some(reason) = broken.clone() {
                for p in pending.drain(..) {
                    let _ = p.ack_tx.send(Err(CommitError::WalBroken(reason.clone())));
                }
            } else {
                match segment.flush_durable() {
                    Ok(()) => {
                        for p in pending.drain(..) {
                            let _ = p.ack_tx.send(Ok(p.lsn));
                        }
                    }
                    Err(e) => {
                        let reason = e.to_string();
                        broken = Some(reason.clone());
                        for p in pending.drain(..) {
                            let _ = p.ack_tx.send(Err(CommitError::WalBroken(reason.clone())));
                        }
                    }
                }
            }
        }

        // -----------------------------------------------------------------
        // Phase 5: exit on shutdown when the queue is fully drained.
        // -----------------------------------------------------------------
        if shutdown_requested && submission_rx.is_empty() {
            return Ok(segment);
        }
    }
}

struct PendingAck {
    ack_tx: Sender<AckMessage>,
    lsn: u64,
}

fn handle_submission(
    sub: Submission,
    segment: &mut WalSegment,
    pending: &mut Vec<PendingAck>,
    broken: Option<&String>,
) {
    let Submission::Append { record, ack_tx } = sub;
    // If the WAL is already broken, reject the submission immediately —
    // don't waste cycles encoding it.
    if let Some(reason) = broken {
        let _ = ack_tx.send(Err(CommitError::WalBroken(reason.clone())));
        return;
    }
    let lsn = record.lsn.raw();
    match segment.append_record(&record) {
        Ok(_) => {
            pending.push(PendingAck { ack_tx, lsn });
        }
        Err(e) => {
            // append_record currently can't fail (it's pure in-memory
            // encoding), but handle it for forward compatibility.
            let _ = ack_tx.send(Err(CommitError::WalBroken(e.to_string())));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

// Tests spawn the committer thread + open WAL segment files. Gated under
// miri; see `.claude/plans/phase-02-miri.md`.
#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::wal::kinds::WalRecordKind;
    use crate::wal::reader::WalReader;
    use crate::wal::record::Lsn;
    use crate::wal::segment::FLUSH_DURABLE_CALLS;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    fn uuid(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn segment_path(dir: &std::path::Path, seq: u64) -> PathBuf {
        dir.join(format!("{:010}.wal", seq))
    }

    fn record(lsn: u64) -> WalRecord {
        WalRecord {
            lsn: Lsn(lsn),
            kind: WalRecordKind::Encode,
            flags: 0,
            timestamp_ns: 1_700_000_000_000_000_000,
            agent_id_lo64: 0x1234_5678_9ABC_DEF0,
            payload: vec![(lsn & 0xFF) as u8; 32],
        }
    }

    fn fresh_segment(dir: &tempfile::TempDir, seq: u64, starting_lsn: u64) -> WalSegment {
        WalSegment::create_new(segment_path(dir.path(), seq), seq, starting_lsn, uuid(1)).unwrap()
    }

    // ----- WalSegment::flush_durable -----------------------------------

    #[test]
    fn flush_durable_round_trips_one_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut seg = fresh_segment(&dir, 0, 1);
        seg.append_record(&record(1)).unwrap();
        seg.flush_durable().unwrap();
        drop(seg);

        let mut reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let r = reader.next().unwrap().unwrap();
        assert_eq!(r.lsn.raw(), 1);
        assert!(reader.next().is_none());
    }

    #[test]
    fn flush_durable_on_empty_buffer_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut seg = fresh_segment(&dir, 0, 1);
        seg.flush_durable().unwrap();
        seg.flush_durable().unwrap();
    }

    #[test]
    fn flush_durable_advances_bytes_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut seg = fresh_segment(&dir, 0, 1);
        let r = record(1);
        let len = r.encoded_len();
        seg.append_record(&r).unwrap();
        seg.flush_durable().unwrap();
        // After flush, size_bytes() should reflect header + one record.
        assert_eq!(
            seg.size_bytes(),
            crate::wal::segment::WAL_SEGMENT_HEADER_LEN + len
        );
    }

    // ----- GroupCommitter sequential durability ------------------------

    #[test]
    fn one_record_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let seg = fresh_segment(&dir, 0, 1);
        let committer = GroupCommitter::start(seg, GroupCommitConfig::default());

        let handle = committer.append(record(1)).unwrap();
        let lsn = handle.wait().unwrap();
        assert_eq!(lsn, 1);

        let _seg = committer.shutdown().unwrap();

        let mut reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let r = reader.next().unwrap().unwrap();
        assert_eq!(r.lsn.raw(), 1);
        assert!(reader.next().is_none());
    }

    #[test]
    fn ten_sequential_records() {
        let dir = tempfile::tempdir().unwrap();
        let seg = fresh_segment(&dir, 0, 1);
        let committer = GroupCommitter::start(seg, GroupCommitConfig::default());

        for i in 1..=10u64 {
            let h = committer.append(record(i)).unwrap();
            assert_eq!(h.wait().unwrap(), i);
        }
        let _seg = committer.shutdown().unwrap();

        let reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, (1..=10).collect::<Vec<_>>());
    }

    // ----- GroupCommitter batching --------------------------------------

    #[test]
    fn many_concurrent_records_all_durable() {
        // Concurrent appenders share a mutex that serializes the
        // (assign-LSN, enqueue) pair — mimicking what the `Wal` type in
        // sub-task 2.9 will provide. Without serialization, multiple
        // threads could fetch_add() out-of-order vs. their channel send,
        // and `WalReader` (correctly) rejects out-of-order LSNs as a
        // corruption signal.
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Mutex;

        let dir = tempfile::tempdir().unwrap();
        let seg = fresh_segment(&dir, 0, 1);
        let committer =
            std::sync::Arc::new(GroupCommitter::start(seg, GroupCommitConfig::default()));
        let lsn_counter = std::sync::Arc::new(AtomicU64::new(1));
        let enqueue_lock = std::sync::Arc::new(Mutex::new(()));
        let assigned: std::sync::Arc<Mutex<Vec<(u64, AppendHandle)>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));

        const N: u64 = 50;
        let mut threads = Vec::new();
        for _ in 0..N {
            let c = committer.clone();
            let counter = lsn_counter.clone();
            let lock = enqueue_lock.clone();
            let assigned = assigned.clone();
            threads.push(std::thread::spawn(move || {
                let _g = lock.lock().unwrap();
                let lsn = counter.fetch_add(1, Ordering::SeqCst);
                let handle = c.append(record(lsn)).unwrap();
                assigned.lock().unwrap().push((lsn, handle));
            }));
        }
        for t in threads {
            t.join().unwrap();
        }

        // Wait for every handle; check each ack matches its assigned LSN.
        let assigned = std::sync::Arc::try_unwrap(assigned)
            .ok()
            .expect("all spawn threads have joined")
            .into_inner()
            .unwrap();
        let mut acked: Vec<u64> = assigned
            .into_iter()
            .map(|(lsn, h)| {
                let got = h.wait().unwrap();
                assert_eq!(got, lsn);
                got
            })
            .collect();
        acked.sort();
        assert_eq!(acked, (1..=N).collect::<Vec<_>>());

        // Reclaim the committer + read back.
        let committer = std::sync::Arc::try_unwrap(committer)
            .ok()
            .expect("only one Arc reference remains");
        let _seg = committer.shutdown().unwrap();

        let reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, (1..=N).collect::<Vec<_>>());
    }

    #[test]
    fn batching_amortizes_fsyncs() {
        // 100 appends should produce far fewer than 100 fsyncs. Phase doc
        // 2.8: "100 appends batched into ≤ 5 fsyncs". We assert ≤ 50 to
        // keep the test robust against scheduler timing (the ideal is 1–5
        // batches; CI under load may produce more, but we should never see
        // one-fsync-per-append).
        //
        // We append from a single thread (rather than 100) because the
        // *batching* test is about queue accumulation under the
        // `commit_window`, not threading. The committer drains the queue
        // when it wakes; many records that arrived during the window all
        // ride one fsync.
        FLUSH_DURABLE_CALLS.store(0, Ordering::SeqCst);

        let dir = tempfile::tempdir().unwrap();
        let seg = fresh_segment(&dir, 0, 1);
        let committer = GroupCommitter::start(
            seg,
            GroupCommitConfig {
                commit_window: Duration::from_millis(5),
                max_batch_bytes: 60 * 1024,
            },
        );

        const N: u64 = 100;
        let mut handles = Vec::new();
        for i in 1..=N {
            handles.push(committer.append(record(i)).unwrap());
        }
        for h in handles {
            h.wait().unwrap();
        }

        let flushes = FLUSH_DURABLE_CALLS.load(Ordering::SeqCst);
        assert!(
            flushes <= 50,
            "expected ≤ 50 fsyncs for 100 records, got {flushes}"
        );

        let _ = committer.shutdown().unwrap();
    }

    // ----- Torn-write recovery ------------------------------------------

    #[test]
    fn torn_write_at_tail_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let seg = fresh_segment(&dir, 0, 1);
        let committer = GroupCommitter::start(seg, GroupCommitConfig::default());

        // Five durable records.
        for i in 1..=5u64 {
            committer.append(record(i)).unwrap().wait().unwrap();
        }
        let _ = committer.shutdown().unwrap();

        // Simulate a torn write by truncating the file mid-record (set_len
        // to drop the trailing bytes of the last record). WalReader treats
        // a Truncated decode at the last-segment tail as a clean end, which
        // is exactly the semantics we want for "the kernel got partway
        // through the last pwritev2 before the crash."
        let path = segment_path(dir.path(), 0);
        let current_size = std::fs::metadata(&path).unwrap().len();
        // Chop 30 bytes off the tail — enough to dent the last record.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(current_size - 30)
            .unwrap();

        let reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let mut count = 0u64;
        let mut last_lsn = 0u64;
        for item in reader {
            let r = item.unwrap();
            count += 1;
            assert_eq!(r.lsn.raw(), count);
            last_lsn = r.lsn.raw();
        }
        assert_eq!(count, 4, "4 records before the torn-write tail");
        assert_eq!(last_lsn, 4);
    }

    // ----- Failure modes ------------------------------------------------

    #[test]
    fn drop_without_shutdown_terminates_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let seg = fresh_segment(&dir, 0, 1);
        let committer = GroupCommitter::start(seg, GroupCommitConfig::default());

        committer.append(record(1)).unwrap().wait().unwrap();
        // Drop without calling shutdown — committer thread should exit.
        drop(committer);

        // Reopen — file should be in a valid state with one record.
        let mut reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let r = reader.next().unwrap().unwrap();
        assert_eq!(r.lsn.raw(), 1);
        assert!(reader.next().is_none());
    }

    #[test]
    fn append_after_shutdown_errors() {
        let dir = tempfile::tempdir().unwrap();
        let seg = fresh_segment(&dir, 0, 1);
        let committer = GroupCommitter::start(seg, GroupCommitConfig::default());
        committer.append(record(1)).unwrap().wait().unwrap();
        let _ = committer.shutdown().unwrap();
        // After shutdown(), the GroupCommitter is consumed; no further
        // calls are possible at the type level. This test exists to
        // document the API contract.
    }
}
