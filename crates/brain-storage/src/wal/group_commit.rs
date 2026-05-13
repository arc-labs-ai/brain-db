//! Group commit: batches concurrent `WalRecord` appends into a single
//! `write_at` + `fdatasync` (both io_uring) for amortized fsync cost.
//!
//! See `spec/05_storage_arena_wal/06_wal_durability.md`.
//!
//! ## Architecture (sub-task 9.6a port)
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
//! Pre-port (SD-2.8-2) used a dedicated OS `std::thread` calling synchronous
//! `pwritev2(RWF_DSYNC)` from a `crossbeam_channel::select!` loop. The new
//! design lives on the shard's single Glommio executor; no cross-thread
//! synchronization on the hot path.
//!
//! Errors are sticky: after a failed flush the WAL is "broken"; all
//! in-flight handles and subsequent appends receive `Err(WalBroken)` until
//! the [`GroupCommitter`] is dropped.

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
    pub fn start(segment: WalSegment, config: GroupCommitConfig) -> Self {
        let (submission_tx, submission_rx) = flume::unbounded::<Submission>();
        let (shutdown_tx, shutdown_rx) = flume::bounded::<()>(1);
        let (completion_tx, completion_rx) = flume::bounded::<Result<WalSegment, CommitError>>(1);

        spawn_local(async move {
            let result = committer_loop(segment, submission_rx, shutdown_rx, config).await;
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
    /// owned [`WalSegment`]. Consumes `self`. Returns `Err` if the task
    /// panicked or the final flush failed.
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
) -> Result<WalSegment, CommitError> {
    let mut pending: Vec<PendingAck> = Vec::new();
    let mut broken: Option<String> = None;
    let mut shutdown_requested = false;

    loop {
        // -----------------------------------------------------------------
        // Phase 1: receive the first submission (or shutdown).
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
            handle_submission(first, &mut segment, &mut pending, broken.as_ref());
            got_any = true;
        }

        // -----------------------------------------------------------------
        // Phase 2: gather more records up to commit_window or size threshold.
        // -----------------------------------------------------------------
        if got_any && broken.is_none() && !shutdown_requested {
            // Race the timer against the submission stream. We re-arm the
            // timer once per iteration relative to when we entered Phase 2,
            // not per-record — matches the pre-port crossbeam behavior.
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
                        handle_submission(sub, &mut segment, &mut pending, broken.as_ref());
                    }
                    Err(_) => break, // timer fired or submission closed
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
                match segment.flush_durable().await {
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

fn handle_submission(
    sub: Submission,
    segment: &mut WalSegment,
    pending: &mut Vec<PendingAck>,
    broken: Option<&String>,
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
            let _ = ack_tx.send(Err(CommitError::WalBroken(e.to_string())));
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
    use crate::wal::segment::{glommio_run, FLUSH_DURABLE_CALLS};
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
            let committer = GroupCommitter::start(seg, GroupCommitConfig::default());
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
            let committer = GroupCommitter::start(seg, GroupCommitConfig::default());
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
            let committer =
                std::rc::Rc::new(GroupCommitter::start(seg, GroupCommitConfig::default()));

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

    // ----- Shutdown / Drop ----------------------------------------------

    #[test]
    fn shutdown_drains_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        glommio_run(move || async move {
            let seg = fresh_segment(path, 0, 1).await;
            let committer = GroupCommitter::start(seg, GroupCommitConfig::default());
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
    fn append_after_shutdown_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(dir.path(), 0);
        glommio_run(move || async move {
            let seg = fresh_segment(path, 0, 1).await;
            let mut committer = GroupCommitter::start(seg, GroupCommitConfig::default());
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
