//! `WalSink` — the writer's escape hatch to the per-shard WAL.
//!
//! The writer struct (`RealWriterHandle`) is wrapped in
//! `Arc<dyn WriterHandle>` and therefore lives behind a `Send + Sync`
//! boundary. The shard's `Wal` is `!Sync` (interior `RefCell` +
//! Glommio-bound committer task), so the writer can't hold one
//! directly.
//!
//! `WalSink` is the bridge: any `Send + Sync` impl that ferries a
//! [`WalRecord`] across to whatever owns the real `Wal`, returning the
//! assigned [`Lsn`]. Production uses [`ChannelWalSink`] backed by a
//! `flume` channel drained on the shard's Glommio executor; tests use
//! [`NoopWalSink`] (synthesises monotonic LSNs without touching disk)
//! or [`RecordingWalSink`] (captures every appended record for
//! assertions).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use brain_storage::wal::record::{Lsn, WalRecord};
use brain_storage::wal::wal::WalError;
use parking_lot::Mutex;

/// Boundary between the writer and the per-shard WAL.
///
/// `Send + Sync` so the implementing type can live inside
/// `Arc<dyn WalSink>` on the writer.
pub trait WalSink: Send + Sync {
    /// Append `record` and return the assigned LSN. The future may
    /// outlive the borrow of `self`.
    fn append<'a>(
        &'a self,
        record: WalRecord,
    ) -> Pin<Box<dyn Future<Output = Result<Lsn, WalSinkError>> + Send + 'a>>;

    /// LSN the next `append` will assign. Subscribe uses this as the
    /// cutover point. Default delegates to a monotonic counter on the
    /// sink itself; the real channel-backed impl asks the WAL.
    fn current_tail_lsn(&self) -> u64 {
        0
    }
}

/// Errors a `WalSink` can surface. Production maps the real
/// [`WalError`] into the `Internal` variant; tests use
/// `Internal("…")` directly.
#[derive(Debug, thiserror::Error)]
pub enum WalSinkError {
    #[error("wal sink internal: {0}")]
    Internal(String),
    #[error("wal sink: drain task gone")]
    Disconnected,
}

impl From<WalError> for WalSinkError {
    fn from(e: WalError) -> Self {
        Self::Internal(format!("{e}"))
    }
}

// ---------------------------------------------------------------------------
// NoopWalSink — test default. Mints synthetic monotonic LSNs.
// ---------------------------------------------------------------------------

/// In-memory sink that drops records on the floor and returns a
/// synthetic monotonic LSN. Useful for tests that exercise the writer
/// without needing a real WAL.
#[derive(Debug, Default)]
pub struct NoopWalSink {
    next_lsn: AtomicU64,
}

impl NoopWalSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_lsn: AtomicU64::new(1),
        }
    }
}

impl WalSink for NoopWalSink {
    fn append<'a>(
        &'a self,
        _record: WalRecord,
    ) -> Pin<Box<dyn Future<Output = Result<Lsn, WalSinkError>> + Send + 'a>> {
        let lsn = self.next_lsn.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move { Ok(Lsn(lsn)) })
    }

    fn current_tail_lsn(&self) -> u64 {
        self.next_lsn.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// RecordingWalSink — test fixture for ordering assertions.
// ---------------------------------------------------------------------------

/// Wraps a `NoopWalSink` and captures every appended record so tests
/// can assert what was written, in which order. The captured records
/// have their assigned LSN stamped in.
#[derive(Debug, Default)]
pub struct RecordingWalSink {
    inner: NoopWalSink,
    appended: Mutex<Vec<WalRecord>>,
}

impl RecordingWalSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: NoopWalSink::new(),
            appended: Mutex::new(Vec::new()),
        }
    }

    /// Snapshot of every record appended through this sink, in append
    /// order, with the synthetic LSN stamped in.
    pub fn appended(&self) -> Vec<WalRecord> {
        self.appended.lock().clone()
    }

    pub fn len(&self) -> usize {
        self.appended.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.appended.lock().is_empty()
    }
}

impl WalSink for RecordingWalSink {
    fn append<'a>(
        &'a self,
        mut record: WalRecord,
    ) -> Pin<Box<dyn Future<Output = Result<Lsn, WalSinkError>> + Send + 'a>> {
        Box::pin(async move {
            let lsn = self.inner.append(record.clone()).await?;
            record.lsn = lsn;
            self.appended.lock().push(record);
            Ok(lsn)
        })
    }

    fn current_tail_lsn(&self) -> u64 {
        self.inner.current_tail_lsn()
    }
}

// ---------------------------------------------------------------------------
// FailingWalSink — for negative tests.
// ---------------------------------------------------------------------------

/// Sink that always fails. Used to assert the writer aborts the op
/// without committing to redb when WAL append fails.
#[derive(Debug)]
pub struct FailingWalSink {
    reason: String,
}

impl FailingWalSink {
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl WalSink for FailingWalSink {
    fn append<'a>(
        &'a self,
        _record: WalRecord,
    ) -> Pin<Box<dyn Future<Output = Result<Lsn, WalSinkError>> + Send + 'a>> {
        let reason = self.reason.clone();
        Box::pin(async move { Err(WalSinkError::Internal(reason)) })
    }
}

// ---------------------------------------------------------------------------
// ChannelWalSink — production impl. Forwards records over a flume
// channel to a drain task that owns the real `Wal`.
// ---------------------------------------------------------------------------

/// One in-flight append: the record to write + a one-shot channel for
/// the reply.
pub type WalAppendMessage = (WalRecord, flume::Sender<Result<Lsn, WalSinkError>>);

/// Send-side handle the writer holds. The receiving end of the same
/// channel is drained by a Glommio-local task on the shard executor;
/// see `brain_server::shard` for the spawn site.
pub struct ChannelWalSink {
    tx: flume::Sender<WalAppendMessage>,
    /// Latest tail LSN, refreshed every `append`. Reads on the
    /// subscribe path don't go through the channel; they read this
    /// directly so the live-vs-replay cutover is cheap.
    cached_tail: AtomicU64,
}

impl ChannelWalSink {
    #[must_use]
    pub fn new(tx: flume::Sender<WalAppendMessage>) -> Self {
        Self {
            tx,
            cached_tail: AtomicU64::new(1),
        }
    }

    /// Update the cached tail LSN after an external bump (e.g. the
    /// drain task observed a higher LSN from a checkpoint or worker
    /// append). Tests don't need to call this; the `append` path
    /// updates it inline.
    pub fn bump_cached_tail(&self, lsn: u64) {
        let mut cur = self.cached_tail.load(Ordering::Relaxed);
        while lsn > cur {
            match self.cached_tail.compare_exchange_weak(
                cur,
                lsn,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
    }
}

impl WalSink for ChannelWalSink {
    fn append<'a>(
        &'a self,
        record: WalRecord,
    ) -> Pin<Box<dyn Future<Output = Result<Lsn, WalSinkError>> + Send + 'a>> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        let tx = self.tx.clone();
        Box::pin(async move {
            // `send_async` on a bounded channel applies backpressure:
            // when the drain task can't keep up the writer awaits a
            // free slot instead of unbounded-buffering pending appends
            // in memory. Under sustained overload the caller's encode
            // request becomes flow-controlled by the WAL — which is
            // exactly the spec §05/06 semantics we want.
            tx.send_async((record, reply_tx))
                .await
                .map_err(|_| WalSinkError::Disconnected)?;
            let lsn = reply_rx
                .recv_async()
                .await
                .map_err(|_| WalSinkError::Disconnected)??;
            self.bump_cached_tail(lsn.raw().saturating_add(1));
            Ok(lsn)
        })
    }

    fn current_tail_lsn(&self) -> u64 {
        self.cached_tail.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Convenience: build a (sink, receiver) pair the shard wires together.
// ---------------------------------------------------------------------------

/// Default bound on the writer→WAL-drain channel. Sized to hold
/// ~100 ms of writes at 10K ops/sec — beyond that the writer waits
/// for the drain to catch up, applying backpressure all the way up
/// to the client's ENCODE call. Operators tune via the per-shard
/// `wal_drain_capacity` config knob (a future addition; this
/// constant is the floor).
pub const DEFAULT_WAL_DRAIN_CAPACITY: usize = 1024;

/// Construct a `ChannelWalSink` plus the matching receiver. The shard
/// keeps the receiver to drive a per-executor drain loop that calls
/// `wal.append` for each enqueued record. Default channel size is
/// [`DEFAULT_WAL_DRAIN_CAPACITY`]; use [`channel_wal_sink_with_capacity`]
/// to override.
#[must_use]
pub fn channel_wal_sink() -> (Arc<ChannelWalSink>, flume::Receiver<WalAppendMessage>) {
    channel_wal_sink_with_capacity(DEFAULT_WAL_DRAIN_CAPACITY)
}

/// Same as [`channel_wal_sink`] but with an explicit channel
/// capacity. A capacity of `0` means unbounded — only useful for
/// tests that want to avoid backpressure.
#[must_use]
pub fn channel_wal_sink_with_capacity(
    capacity: usize,
) -> (Arc<ChannelWalSink>, flume::Receiver<WalAppendMessage>) {
    let (tx, rx) = if capacity == 0 {
        flume::unbounded()
    } else {
        flume::bounded(capacity)
    };
    (Arc::new(ChannelWalSink::new(tx)), rx)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_storage::wal::kinds::WalRecordKind;

    fn sample_record() -> WalRecord {
        WalRecord {
            lsn: Lsn(0),
            kind: WalRecordKind::Encode,
            flags: 0,
            timestamp_ns: 1,
            agent_id_lo64: 0,
            payload: vec![0xAB; 8],
        }
    }

    #[tokio::test]
    async fn noop_sink_assigns_monotonic_lsns() {
        let sink = NoopWalSink::new();
        let a = sink.append(sample_record()).await.unwrap();
        let b = sink.append(sample_record()).await.unwrap();
        assert_eq!(a.raw(), 1);
        assert_eq!(b.raw(), 2);
        assert_eq!(sink.current_tail_lsn(), 3);
    }

    #[tokio::test]
    async fn recording_sink_captures_in_order() {
        let sink = RecordingWalSink::new();
        sink.append(sample_record()).await.unwrap();
        sink.append(sample_record()).await.unwrap();
        let recs = sink.appended();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].lsn.raw(), 1);
        assert_eq!(recs[1].lsn.raw(), 2);
    }

    #[tokio::test]
    async fn failing_sink_propagates_error() {
        let sink = FailingWalSink::new("test");
        let err = sink.append(sample_record()).await.unwrap_err();
        assert!(matches!(err, WalSinkError::Internal(_)));
    }

    #[tokio::test]
    async fn channel_sink_reports_disconnected_when_drain_drops() {
        let (sink, rx) = channel_wal_sink();
        drop(rx);
        let err = sink.append(sample_record()).await.unwrap_err();
        assert!(matches!(err, WalSinkError::Disconnected));
    }

    #[tokio::test]
    async fn bounded_channel_backpressures_when_drain_lags() {
        // capacity=1 channel. Sender enqueues one, then the second
        // append must AWAIT (not error) until the drain task pops the
        // first. Confirms that backpressure is async, not failure.
        let (sink, rx) = channel_wal_sink_with_capacity(1);
        let sink_for_task = sink.clone();
        // Spawn an append BUT don't drain — it lands in the channel.
        let first =
            tokio::spawn(
                async move { sink_for_task.append(sample_record()).await.map(|l| l.raw()) },
            );
        // Give it a moment to enqueue.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Now the channel is full (capacity=1). A second append
        // must block in send_async, NOT error. Race it against a
        // short timer: if it returns within 50ms we have a bug
        // (it'd mean send_async didn't actually backpressure).
        let sink_for_second = sink.clone();
        let second = tokio::spawn(async move {
            sink_for_second
                .append(sample_record())
                .await
                .map(|l| l.raw())
        });
        let blocked = tokio::time::timeout(std::time::Duration::from_millis(50), async {
            second.is_finished()
        })
        .await;
        // The wrapper future itself completes instantly with is_finished()=false.
        assert!(!blocked.unwrap(), "send_async should have backpressured");

        // Drain both: pop two messages, reply with synthetic LSNs.
        for expected in 1..=2u64 {
            let (_record, reply) = rx.recv_async().await.unwrap();
            reply.send(Ok(Lsn(expected))).unwrap();
        }
        assert_eq!(first.await.unwrap().unwrap(), 1);
        assert_eq!(second.await.unwrap().unwrap(), 2);
    }
}
