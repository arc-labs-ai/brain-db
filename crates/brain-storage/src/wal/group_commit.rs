//! Group commit: batches concurrent `WalRecord` appends into a single
//! `write_at` + `fdatasync` (both io_uring) for amortized fsync cost.
//!
//! ## Architecture
//!
//! [`GroupCommitter::start`] spawns a Glommio **task** (`Task::local`) on
//! the current executor that owns the [`WalSegment`]. Appenders call
//! [`GroupCommitter::append`], which enqueues a record on a `flume::Sender`
//! and returns an [`AppendHandle`] backed by an oneshot ack channel
//! (`flume::bounded(1)`). The committer task:
//!
//! 1. Awaits the *first* record of each batch on the submission receiver.
//! 2. Once a record arrives, waits up to [`GroupCommitConfig::commit_window`]
//!    (default 100 µs) for more records — or until the buffer reaches
//!    [`GroupCommitConfig::max_batch_bytes`] (default 60 KiB).
//! 3. Calls [`WalSegment::flush_durable`] once for the whole batch.
//! 4. Signals every pending ack with `Ok(lsn)` (or `Err(WalBroken)` on
//!    failure).
//!
//! This design lives on the shard's single Glommio executor; no
//! cross-thread synchronization on the hot path.
//!
//! Errors are sticky for the process lifetime: after a failed flush the WAL is
//! "broken"; all in-flight handles and subsequent appends receive
//! `Err(WalBroken)` until the shard process restarts.
//! The committer always returns its segment so shutdown can release the file
//! descriptor; the shared fault latch carries terminal durability failure.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use flume::{Receiver, Sender};
use futures_lite::FutureExt;
use glommio::spawn_local;
use glommio::timer::sleep;

use crate::wal::record::WalRecord;
use crate::wal::segment::WalSegment;

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// Configuration for a [`GroupCommitter`].
#[derive(Debug, Clone, Copy)]
pub struct GroupCommitConfig {
    /// Maximum time the committer waits for additional records once the
    /// first record of a batch has arrived. Default: 100 µs.
    pub commit_window: Duration,
    /// Buffer-size threshold that triggers an early flush. Default:
    /// 60 KiB.
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

/// Handle returned by [`GroupCommitter::append`]. Await [`Self::wait`]
/// until the record's batch is on stable storage.
#[must_use = "an AppendHandle must be awaited; otherwise durability isn't observed"]
pub struct AppendHandle {
    ack_rx: Receiver<AckMessage>,
}

impl AppendHandle {
    /// Await durable completion. Returns the record's LSN on success.
    pub async fn wait(self) -> Result<u64, CommitError> {
        match self.ack_rx.recv_async().await {
            Ok(Ok(lsn)) => Ok(lsn),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(CommitError::AckChannelClosed),
        }
    }
}

/// Errors returned by [`GroupCommitter`] / [`AppendHandle`].
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    /// The WAL has entered a broken state after an I/O failure.
    #[error("WAL is broken: {0}")]
    WalBroken(String),

    /// The committer task has shut down; new appends are no longer accepted.
    #[error("committer has shut down")]
    ShutDown,

    /// The ack channel was dropped before the flush completed.
    #[error("ack channel was dropped before the flush completed")]
    AckChannelClosed,
}

/// The shard's terminal write-ahead-log fault.
///
/// A durability failure means the shard can no longer prove any write
/// reached stable storage, so it must refuse every later write rather than
/// ack something it cannot log. The latch is shared by every committer the
/// shard ever starts — including the one a segment rollover installs — so the
/// refusal cannot be lost by replacing the committer task. It is never
/// cleared: only restarting the shard process, which re-derives the WAL tail
/// from the bytes actually on disk, clears it. No marker is written to disk:
/// recovery derives `next_lsn` and the physical tail from those bytes, so the
/// dangerous divergence cannot survive a restart; if the underlying cause
/// persists, the first flush after restart latches immediately. A sticky
/// on-disk marker would turn a transient fault into a shard that refuses to
/// boot until an operator deletes an undocumented file.
pub struct WalFault {
    reason: RefCell<Option<String>>,
}

impl WalFault {
    pub fn new() -> Self {
        Self {
            reason: RefCell::new(None),
        }
    }

    pub fn latch(&self, reason: String) {
        let mut latched = self.reason.borrow_mut();
        if latched.is_none() {
            tracing::error!(
                reason = %reason,
                "the shard refuses WAL writes until the process is restarted"
            );
            *latched = Some(reason);
        }
    }

    pub fn reason(&self) -> Option<String> {
        self.reason.borrow().clone()
    }

    pub fn is_latched(&self) -> bool {
        self.reason.borrow().is_some()
    }
}

impl Default for WalFault {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-shard group commit coordinator. Owns one [`WalSegment`] and one
/// committer task running on the local Glommio executor.
pub struct GroupCommitter {
    submission_tx: Option<Sender<Submission>>,
    shutdown_tx: Sender<()>,
    /// Single-shot completion channel from the committer task. Receives the
    /// reclaimed `WalSegment` on clean shutdown, or `CommitError` on failure.
    completion_rx: Option<Receiver<Result<WalSegment, CommitError>>>,
    /// Detach handle for the committer task — kept so the task doesn't get
    /// cancelled when `GroupCommitter` drops the explicit reference.
    /// `Task::local` returns a `Task<T>` which cancels on drop unless
    /// `detach()` is called.
    _task: Option<glommio::TaskQueueHandle>,
}

impl GroupCommitter {
    /// Spawn the committer task on the current executor. Takes ownership
    /// of `segment`. **Must be called from inside a `glommio::LocalExecutor`.**
    pub fn start(segment: WalSegment, config: GroupCommitConfig, fault: Rc<WalFault>) -> Self {
        let (submission_tx, submission_rx) = flume::unbounded::<Submission>();
        let (shutdown_tx, shutdown_rx) = flume::bounded::<()>(1);
        let (completion_tx, completion_rx) = flume::bounded::<Result<WalSegment, CommitError>>(1);

        spawn_local(async move {
            let result = committer_loop(segment, submission_rx, shutdown_rx, config, fault).await;
            let _ = completion_tx.send(result);
        })
        .detach();

        Self {
            submission_tx: Some(submission_tx),
            shutdown_tx,
            completion_rx: Some(completion_rx),
            _task: None,
        }
    }

    /// Enqueue a record for durable write. Returns immediately with an
    /// [`AppendHandle`]; await the handle to observe durability.
    ///
    /// Stays synchronous because the underlying `flume::Sender::send` on an
    /// unbounded channel never blocks (it's an `AtomicVec` push). Awaiting
    /// happens on the handle.
    pub fn append(&self, record: WalRecord) -> Result<AppendHandle, CommitError> {
        let (ack_tx, ack_rx) = flume::bounded::<AckMessage>(1);
        let sender = self.submission_tx.as_ref().ok_or(CommitError::ShutDown)?;
        sender
            .send(Submission::Append { record, ack_tx })
            .map_err(|_| CommitError::ShutDown)?;
        Ok(AppendHandle { ack_rx })
    }

    /// Drain the queue, flush the final batch durably, and reclaim the
    /// owned [`WalSegment`]. Consumes `self`. The segment always returns so
    /// callers can close its descriptor; terminal durability failure lives in
    /// the shared [`WalFault`] latch.
    pub async fn shutdown(mut self) -> Result<WalSegment, CommitError> {
        // Close the submission channel so the committer sees disconnection
        // *after* it drains anything already in flight.
        self.submission_tx = None;
        // Signal explicit shutdown. Ignore error (task may have already exited).
        let _ = self.shutdown_tx.send(());
        let rx = self
            .completion_rx
            .take()
            .ok_or(CommitError::AckChannelClosed)?;
        match rx.recv_async().await {
            Ok(result) => result,
            Err(_) => Err(CommitError::WalBroken(
                "committer task did not signal completion".into(),
            )),
        }
    }
}

impl Drop for GroupCommitter {
    fn drop(&mut self) {
        // Best-effort: cause the committer to wind down. Without awaiting
        // completion we may leak the WalSegment briefly, but the task is
        // detached so it'll run to completion on its own executor.
        self.submission_tx = None;
        let _ = self.shutdown_tx.send(());
        // Drop completion_rx without awaiting — the committer's send-on-completion
        // will fail silently. Acceptable: callers that need a clean shutdown
        // call `shutdown().await` explicitly.
    }
}

// ---------------------------------------------------------------------------
// Internal types.
// ---------------------------------------------------------------------------

enum Submission {
    Append {
        record: WalRecord,
        ack_tx: Sender<AckMessage>,
    },
}

type AckMessage = Result<u64, CommitError>;

struct PendingAck {
    ack_tx: Sender<AckMessage>,
    lsn: u64,
}

// ---------------------------------------------------------------------------
// Committer task loop.
// ---------------------------------------------------------------------------

async fn committer_loop(
    mut segment: WalSegment,
    submission_rx: Receiver<Submission>,
    shutdown_rx: Receiver<()>,
    config: GroupCommitConfig,
    fault: Rc<WalFault>,
) -> Result<WalSegment, CommitError> {
    let mut pending: Vec<PendingAck> = Vec::new();
    let mut shutdown_requested = false;

    loop {
        // -----------------------------------------------------------------
        // Step 1: receive the first submission (or shutdown).
        // -----------------------------------------------------------------
        let first = if shutdown_requested {
            submission_rx.try_recv().ok()
        } else {
            // Race the submission queue against the shutdown signal.
            let sub_fut = submission_rx.recv_async();
            let shut_fut = shutdown_rx.recv_async();
            match sub_fut
                .or(async {
                    let _ = shut_fut.await;
                    Err(flume::RecvError::Disconnected)
                })
                .await
            {
                Ok(s) => Some(s),
                Err(_) => {
                    shutdown_requested = true;
                    None
                }
            }
        };

        let mut got_any = false;
        if let Some(first) = first {
            handle_submission(
                first,
                &mut segment,
                &mut pending,
                fault.reason().as_ref(),
                &fault,
            );
            got_any = true;
        }

        // -----------------------------------------------------------------
        // Step 2: gather more records up to commit_window or size threshold.
        // -----------------------------------------------------------------
        if got_any && fault.reason().is_none() && !shutdown_requested {
            // Drain submissions that are already waiting before considering
            // the timer. An empty queue is a point-in-time fact: a sender
            // that races this check remains queued and is handled by the next
            // loop iteration, so it cannot be lost.
            let mut found_waiting_submission = false;
            while let Ok(sub) = submission_rx.try_recv() {
                found_waiting_submission = true;
                handle_submission(
                    sub,
                    &mut segment,
                    &mut pending,
                    fault.reason().as_ref(),
                    &fault,
                );
                if segment.write_buf_len() >= config.max_batch_bytes {
                    break;
                }
            }

            // A quiet queue means there is no overlap to wait for. When
            // overlap is present, retain the window so later submissions can
            // join the same durable flush.
            if found_waiting_submission && segment.write_buf_len() < config.max_batch_bytes {
                // Race the timer against the submission stream. We re-arm the
                // timer once per iteration relative to when we entered step 2,
                // not per-record.
                let timer_fut = sleep(config.commit_window);
                futures_lite::pin!(timer_fut);

                loop {
                    if segment.write_buf_len() >= config.max_batch_bytes {
                        break;
                    }
                    let sub_fut = submission_rx.recv_async();
                    // futures_lite::FutureExt::or returns whichever future
                    // completes first. The unit Future returned by sleep is
                    // wrapped so its result type matches the submission recv.
                    let timer_signal = async {
                        (&mut timer_fut).await;
                        Err::<Submission, flume::RecvError>(flume::RecvError::Disconnected)
                    };
                    match sub_fut.or(timer_signal).await {
                        Ok(sub) => {
                            handle_submission(
                                sub,
                                &mut segment,
                                &mut pending,
                                fault.reason().as_ref(),
                                &fault,
                            );
                        }
                        Err(_) => break, // timer fired or submission closed
                    }
                }
            }
        }

        // -----------------------------------------------------------------
        // Step 3: drain any remaining queued submissions on shutdown.
        // -----------------------------------------------------------------
        if shutdown_requested {
            while let Ok(sub) = submission_rx.try_recv() {
                handle_submission(
                    sub,
                    &mut segment,
                    &mut pending,
                    fault.reason().as_ref(),
                    &fault,
                );
            }
        }

        // -----------------------------------------------------------------
        // Step 4: flush + signal.
        // -----------------------------------------------------------------
        if !pending.is_empty() {
            if let Some(reason) = fault.reason() {
                for p in pending.drain(..) {
                    let _ = p.ack_tx.send(Err(CommitError::WalBroken(reason.clone())));
                }
            } else {
                match segment.flush_durable().await {
                    Ok(()) => {
                        for p in pending.drain(..) {
                            let _ = p.ack_tx.send(Ok(p.lsn));
                        }
                    }
                    Err(e) => {
                        let reason = e.to_string();
                        fault.latch(reason.clone());
                        for p in pending.drain(..) {
                            let _ = p.ack_tx.send(Err(CommitError::WalBroken(reason.clone())));
                        }
                    }
                }
            }
        }

        // -----------------------------------------------------------------
        // Step 5: exit on shutdown when the queue is fully drained.
        // -----------------------------------------------------------------
        if shutdown_requested && submission_rx.is_empty() {
            // The segment always returns so callers can close its descriptor;
            // the shared fault latch carries terminal durability failure.
            return Ok(segment);
        }
    }
}

fn handle_submission(
    sub: Submission,
    segment: &mut WalSegment,
    pending: &mut Vec<PendingAck>,
    broken: Option<&String>,
    fault: &WalFault,
) {
    let Submission::Append { record, ack_tx } = sub;
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
            let reason = e.to_string();
            fault.latch(reason.clone());
            let _ = ack_tx.send(Err(CommitError::WalBroken(reason)));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::wal::kinds::WalRecordKind;
    use crate::wal::reader::WalReader;
    use crate::wal::record::Lsn;
    use crate::wal::segment::{glommio_run, FAIL_NEXT_FLUSH_DURABLE, FLUSH_DURABLE_CALLS};
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
            space_id_lo64: 0x1234_5678_9ABC_DEF0,
            payload: vec![(lsn & 0xFF) as u8; 32],
        }
    }

    async fn fresh_segment(path: PathBuf, seq: u64, starting_lsn: u64) -> WalSegment {
        WalSegment::create_new(path, seq, starting_lsn, uuid(1))
            .await
            .unwrap()
    }

    // ----- WalSegment::flush_durable -----------------------------------

    #[test]
    fn flush_durable_round_trips_one_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        let p = path.clone();
        glommio_run(move || async move {
            let mut seg = fresh_segment(p, 0, 1).await;
            seg.append_record(&record(1)).unwrap();
            seg.flush_durable().await.unwrap();
            seg.close().await.unwrap();
        });

        let mut reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let r = reader.next().unwrap().unwrap();
        assert_eq!(r.lsn.raw(), 1);
        assert!(reader.next().is_none());
    }

    #[test]
    fn flush_durable_on_empty_buffer_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        glommio_run(move || async move {
            let mut seg = fresh_segment(path, 0, 1).await;
            seg.flush_durable().await.unwrap();
            seg.flush_durable().await.unwrap();
            seg.close().await.unwrap();
        });
    }

    #[test]
    fn flush_durable_advances_bytes_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        let r = record(1);
        let len = r.encoded_len();
        glommio_run(move || async move {
            let mut seg = fresh_segment(path, 0, 1).await;
            seg.append_record(&r).unwrap();
            seg.flush_durable().await.unwrap();
            assert_eq!(
                seg.size_bytes(),
                crate::wal::segment::WAL_SEGMENT_HEADER_LEN + len
            );
            seg.close().await.unwrap();
        });
    }

    // ----- GroupCommitter sequential durability ------------------------

    #[test]
    fn one_record_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        glommio_run(move || async move {
            let seg = fresh_segment(path, 0, 1).await;
            let committer = GroupCommitter::start(
                seg,
                GroupCommitConfig::default(),
                std::rc::Rc::new(WalFault::new()),
            );
            let handle = committer.append(record(1)).unwrap();
            let lsn = handle.wait().await.unwrap();
            assert_eq!(lsn, 1);
            let seg = committer.shutdown().await.unwrap();
            seg.close().await.unwrap();
        });

        let mut reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let r = reader.next().unwrap().unwrap();
        assert_eq!(r.lsn.raw(), 1);
        assert!(reader.next().is_none());
    }

    #[test]
    fn ten_sequential_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        glommio_run(move || async move {
            let seg = fresh_segment(path, 0, 1).await;
            let committer = GroupCommitter::start(
                seg,
                GroupCommitConfig::default(),
                std::rc::Rc::new(WalFault::new()),
            );
            for i in 1..=10u64 {
                let h = committer.append(record(i)).unwrap();
                assert_eq!(h.wait().await.unwrap(), i);
            }
            let seg = committer.shutdown().await.unwrap();
            seg.close().await.unwrap();
        });

        let reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, (1..=10).collect::<Vec<_>>());
    }

    // ----- GroupCommitter batching --------------------------------------

    #[test]
    fn concurrent_records_serialise_correctly_within_executor() {
        // On a single Glommio executor, concurrent Task::local appends share
        // the executor cooperatively. We assign LSNs deterministically via a
        // single counter local to the test future (no Mutex needed).
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        let before = FLUSH_DURABLE_CALLS.load(Ordering::SeqCst);
        glommio_run(move || async move {
            let seg = fresh_segment(path, 0, 1).await;
            let committer = std::rc::Rc::new(GroupCommitter::start(
                seg,
                GroupCommitConfig::default(),
                std::rc::Rc::new(WalFault::new()),
            ));

            const N: u64 = 50;
            let mut tasks = Vec::new();
            for lsn in 1..=N {
                let c = committer.clone();
                tasks.push(spawn_local(async move {
                    let h = c.append(record(lsn)).unwrap();
                    h.wait().await.unwrap()
                }));
            }
            let mut acked = Vec::with_capacity(N as usize);
            for t in tasks {
                acked.push(t.await);
            }
            acked.sort();
            assert_eq!(acked, (1..=N).collect::<Vec<_>>());

            let committer = match std::rc::Rc::try_unwrap(committer) {
                Ok(c) => c,
                Err(_) => panic!("only one Rc reference remains"),
            };
            let seg = committer.shutdown().await.unwrap();
            seg.close().await.unwrap();
        });

        let reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, (1..=50).collect::<Vec<_>>());

        // Group-commit invariant: 50 concurrent appends produce far fewer
        // fsyncs than 50.
        let after = FLUSH_DURABLE_CALLS.load(Ordering::SeqCst);
        assert!(
            after - before < 50,
            "50 concurrent appends produced {} flush_durable calls; expected < 50",
            after - before
        );
    }

    #[test]
    fn queued_overlapping_records_share_one_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        let before = FLUSH_DURABLE_CALLS.load(Ordering::SeqCst);
        glommio_run(move || async move {
            let seg = fresh_segment(path, 0, 1).await;
            let committer = GroupCommitter::start(
                seg,
                GroupCommitConfig::default(),
                std::rc::Rc::new(WalFault::new()),
            );
            let handles: Vec<_> = (1..=8)
                .map(|lsn| committer.append(record(lsn)).unwrap())
                .collect();
            for (expected, handle) in (1..=8).zip(handles) {
                assert_eq!(handle.wait().await.unwrap(), expected);
            }
            let seg = committer.shutdown().await.unwrap();
            seg.close().await.unwrap();
        });
        let after = FLUSH_DURABLE_CALLS.load(Ordering::SeqCst);
        assert_eq!(after - before, 1);
    }

    // ----- Shutdown / Drop ----------------------------------------------

    #[test]
    fn shutdown_drains_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        glommio_run(move || async move {
            let seg = fresh_segment(path, 0, 1).await;
            let committer = GroupCommitter::start(
                seg,
                GroupCommitConfig::default(),
                std::rc::Rc::new(WalFault::new()),
            );
            // Enqueue without awaiting; shutdown drains.
            let _h1 = committer.append(record(1)).unwrap();
            let _h2 = committer.append(record(2)).unwrap();
            let seg = committer.shutdown().await.unwrap();
            seg.close().await.unwrap();
        });

        let reader = WalReader::open(dir.path(), uuid(1)).unwrap();
        let lsns: Vec<u64> = reader.map(|r| r.unwrap().lsn.raw()).collect();
        assert_eq!(lsns, vec![1, 2]);
    }

    #[test]
    fn fault_latch_survives_a_fresh_committer() {
        let dir = tempfile::tempdir().unwrap();
        let path0 = segment_path(dir.path(), 0);
        let path1 = segment_path(dir.path(), 1);
        glommio_run(move || async move {
            let fault = std::rc::Rc::new(WalFault::new());
            let seg0 = fresh_segment(path0, 0, 1).await;
            let committer0 =
                GroupCommitter::start(seg0, GroupCommitConfig::default(), fault.clone());
            FAIL_NEXT_FLUSH_DURABLE.with(|fail| fail.set(true));
            let first = committer0.append(record(1)).unwrap().wait().await;
            assert!(matches!(first, Err(CommitError::WalBroken(_))));
            let seg0 = committer0.shutdown().await.unwrap();
            seg0.close().await.unwrap();

            let seg1 = fresh_segment(path1, 1, 2).await;
            let committer1 =
                GroupCommitter::start(seg1, GroupCommitConfig::default(), fault.clone());
            let second = committer1.append(record(2)).unwrap().wait().await;
            assert!(matches!(second, Err(CommitError::WalBroken(_))));
            assert!(fault.is_latched());
            let seg1 = committer1.shutdown().await.unwrap();
            seg1.close().await.unwrap();
        });
    }

    #[test]
    fn append_after_shutdown_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        glommio_run(move || async move {
            let seg = fresh_segment(path, 0, 1).await;
            let mut committer = GroupCommitter::start(
                seg,
                GroupCommitConfig::default(),
                std::rc::Rc::new(WalFault::new()),
            );
            // Manually drop the submission sender by setting to None — mimic
            // the post-shutdown state from inside append().
            committer.submission_tx = None;
            match committer.append(record(1)) {
                Ok(_) => panic!("append after shutdown should fail"),
                Err(CommitError::ShutDown) => {}
                Err(other) => panic!("expected ShutDown, got {other:?}"),
            }
        });
    }
}
