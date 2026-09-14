//! Connection-layer SUBSCRIBE infrastructure.
//!
//! Implements this topology:
//!
//! ```text
//!   Shard 0 Glommio                  Connection layer Tokio
//!   ┌─────────────────────┐          ┌────────────────────────────────┐
//!   │ writer.publish(env) │          │ ShardEventHub (per-listener)   │
//!   │       │             │ flume    │   per_shard_bus: Vec<broadcast>│
//!   │       ▼             │ bounded  │       ▲                        │
//!   │ EventBus (broadcast)├─►        │       │  bridge_task (×shards) │
//!   │ → fanout_task       │          │       │  drains flume → bcast  │
//!   └─────────────────────┘          │                                │
//!                                     │ SubscriptionRegistry (per-conn)│
//!                                     │   HashMap<StreamId, State>     │
//!                                     │       │                        │
//!                                     │   per-sub tokio task           │
//!                                     │   drains broadcast →           │
//!                                     │   filters → frames →           │
//!                                     │   outgoing queue               │
//!                                     └────────────────────────────────┘
//! ```
//!
//! - **`ShardEventHub`**: built once per listener at bind time.
//!   Spawns N `bridge_task`s (one per shard) that drain the shard's
//!   per-process flume Receiver (from `ShardHandle::events()`) and
//!   republish into a `tokio::sync::broadcast::Sender<EventEnvelope>`.
//!   Per-connection subscriptions clone receivers off these senders.
//! - **`SubscriptionRegistry`**: per-connection. Owns the
//!   `HashMap<StreamId, SubscriptionState>`. Each entry has a cancel
//!   watch + a final-LSN counter.
//! - **per-subscription task**: spawned on SUBSCRIBE_REQ. Drains
//!   broadcast receivers (one per relevant shard — typically the
//!   space's bound shard) and pushes filtered events to the per-conn
//!   outgoing-frame queue.

#![cfg(target_os = "linux")]
// `num_shards` + `active_count` are diagnostic / test-only surfaces;
// production code paths don't call them yet (admin endpoints
// will). Avoid churning the surface for now.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// SubscriptionMetrics — lightweight per-process counters surfaced via
// the admin exposition path. Bumped from `run_subscription_task` when
// the broadcast channel returns `Lagged`, and from `start` when a new
// subscription registers.
// ---------------------------------------------------------------------------

/// Process-wide subscription counters. Cheap to clone (Arc inside).
#[derive(Default, Clone)]
pub struct SubscriptionMetrics {
    inner: Arc<SubscriptionMetricsInner>,
}

#[derive(Default)]
struct SubscriptionMetricsInner {
    /// Subscriptions started since process boot.
    started: AtomicU64,
    /// Subscriptions terminated because the broadcast Receiver fell
    /// behind the per-shard capacity. A sustained non-zero rate
    /// means operators should raise `subscription_broadcast_capacity`
    /// (or look at why subscribers are slow).
    dropped_due_to_lag: AtomicU64,
    /// Per-shard "skipped events at the moment of lag" — sum of all
    /// `Lagged(n).0` reported by tokio broadcast. Useful for sizing
    /// the buffer ("we lost N events across M lag events").
    skipped_events_on_lag: AtomicU64,
}

impl SubscriptionMetrics {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    pub fn started(&self) -> u64 {
        self.inner.started.load(Ordering::Relaxed)
    }
    pub fn dropped_due_to_lag(&self) -> u64 {
        self.inner.dropped_due_to_lag.load(Ordering::Relaxed)
    }
    pub fn skipped_events_on_lag(&self) -> u64 {
        self.inner.skipped_events_on_lag.load(Ordering::Relaxed)
    }
    fn record_start(&self) {
        self.inner.started.fetch_add(1, Ordering::Relaxed);
    }
    fn record_lag(&self, skipped: u64) {
        self.inner
            .dropped_due_to_lag
            .fetch_add(1, Ordering::Relaxed);
        self.inner
            .skipped_events_on_lag
            .fetch_add(skipped, Ordering::Relaxed);
    }
}

use brain_core::{MemoryId, ShardId};
use brain_ops::{parse_filter, EventEnvelope, ParsedFilter};
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::envelope::request::{
    CancelStreamRequest, SubscribeRequest, UnsubscribeRequest,
};
use brain_protocol::envelope::response::{
    CancelStreamAck, ErrorCategoryWire, ErrorCodeWire, ErrorResponse, ResponseBody,
    SubscriptionEvent, UnsubscribeResponse,
};
use brain_protocol::error::ErrorCode;
use brain_protocol::Frame;
use parking_lot::Mutex;
use tokio::sync::{broadcast, watch};
use tracing::{debug, warn};

use crate::shard::ShardHandle;

/// Default per-shard broadcast buffer size. Holds ~100 ms of events
/// at a 10K-events/sec write rate; subscribers that can't drain that
/// fast are dropped with `Overloaded`. Operators override via
/// [`ShardEventHub::spawn_with_capacity`] / the matching server
/// config knob.
pub const DEFAULT_SUBSCRIPTION_BROADCAST_CAPACITY: usize = 1024;

/// Default cap on concurrent WAL replays across all subscribers on
/// this hub. A reconnect storm (every client subscribes from_lsn=1
/// at the same time) queues at this limit instead of saturating the
/// tokio blocking pool — keeping the rest of the server responsive
/// while replays drain in waves. Tunable via
/// [`ShardEventHub::spawn_with_capacity_and_replay_limit`].
pub const DEFAULT_REPLAY_CONCURRENCY: usize = 64;
const FLAG_EOS: u8 = 1 << 7;

// ---------------------------------------------------------------------------
// ShardEventHub
// ---------------------------------------------------------------------------

/// Per-shard WAL locator captured at hub-construction time. Lets the
/// connection-layer subscribe-replay open a `WalReader` without
/// round-tripping through the shard executor for every record.
#[derive(Clone, Debug)]
struct WalLocator {
    dir: std::path::PathBuf,
    shard_uuid: [u8; 16],
}

/// One bridge per shard: drains the shard's flume event Receiver and
/// republishes into a `broadcast::Sender`. Built at listener startup
/// and shared across all connections.
#[derive(Clone)]
pub struct ShardEventHub {
    per_shard_bus: Arc<Vec<broadcast::Sender<EventEnvelope>>>,
    /// Same length + ordering as `per_shard_bus`. `wal_locators[i]`
    /// gives the WAL directory + UUID for shard `i`.
    wal_locators: Arc<Vec<WalLocator>>,
    /// Capacity used when constructing each shard's broadcast
    /// channel. Stored for admin/metrics exposition.
    broadcast_capacity: usize,
    /// Global cap on concurrent WAL replays. Each
    /// `run_subscription_task` acquires a permit BEFORE spawning the
    /// blocking WAL scan; a reconnect storm queues here instead of
    /// thundering against the tokio blocking pool.
    replay_semaphore: Arc<tokio::sync::Semaphore>,
}

impl ShardEventHub {
    /// Construct the hub from a slice of `ShardHandle`s and spawn one
    /// `bridge_task` per shard. The returned handles can be discarded;
    /// the bridges shut down when the shard's flume Receiver returns
    /// `Err` (which happens when every shard sender drops — i.e., on
    /// shard shutdown).
    ///
    /// Uses [`DEFAULT_SUBSCRIPTION_BROADCAST_CAPACITY`] for the
    /// per-shard broadcast channel. Call
    /// [`Self::spawn_with_capacity`] to override (e.g., for a
    /// write-heavy deployment with many subscribers).
    pub fn spawn(shards: &[ShardHandle]) -> Self {
        Self::spawn_with_capacity(shards, DEFAULT_SUBSCRIPTION_BROADCAST_CAPACITY)
    }

    /// Same as [`Self::spawn`] with an explicit per-shard broadcast
    /// buffer size. Larger capacity = more tolerance for slow
    /// subscribers; cost is memory (capacity × per-event size,
    /// shared via Arc internally so the multiplier is small).
    pub fn spawn_with_capacity(shards: &[ShardHandle], capacity: usize) -> Self {
        Self::spawn_with_capacity_and_replay_limit(shards, capacity, DEFAULT_REPLAY_CONCURRENCY)
    }

    /// Full constructor — both broadcast capacity AND the replay
    /// concurrency cap are explicit. Use this when you know your
    /// expected reconnect-storm size and want to size the
    /// blocking-pool budget accordingly.
    pub fn spawn_with_capacity_and_replay_limit(
        shards: &[ShardHandle],
        capacity: usize,
        replay_concurrency: usize,
    ) -> Self {
        let mut per_shard_bus = Vec::with_capacity(shards.len());
        let mut wal_locators = Vec::with_capacity(shards.len());
        for shard in shards {
            let (tx, _rx) = broadcast::channel::<EventEnvelope>(capacity);
            let events_rx = shard.events();
            let tx_for_task = tx.clone();
            let shard_id = shard.shard_id();
            tokio::spawn(async move {
                bridge_task(shard_id, events_rx, tx_for_task).await;
            });
            per_shard_bus.push(tx);
            wal_locators.push(WalLocator {
                dir: shard.wal_dir(),
                shard_uuid: shard.shard_uuid(),
            });
        }
        Self {
            per_shard_bus: Arc::new(per_shard_bus),
            wal_locators: Arc::new(wal_locators),
            broadcast_capacity: capacity,
            replay_semaphore: Arc::new(tokio::sync::Semaphore::new(replay_concurrency.max(1))),
        }
    }

    fn replay_semaphore(&self) -> Arc<tokio::sync::Semaphore> {
        self.replay_semaphore.clone()
    }

    /// Per-shard broadcast buffer size in effect for this hub.
    /// Exposed for admin/metrics views.
    pub fn broadcast_capacity(&self) -> usize {
        self.broadcast_capacity
    }

    /// Subscribe to one shard's event stream.
    pub fn subscribe_shard(&self, shard_id: ShardId) -> Option<broadcast::Receiver<EventEnvelope>> {
        self.per_shard_bus
            .get(shard_id as usize)
            .map(|tx| tx.subscribe())
    }

    fn wal_locator(&self, shard_id: ShardId) -> Option<WalLocator> {
        self.wal_locators.get(shard_id as usize).cloned()
    }

    pub fn num_shards(&self) -> usize {
        self.per_shard_bus.len()
    }
}

async fn bridge_task(
    shard_id: ShardId,
    events_rx: flume::Receiver<EventEnvelope>,
    bcast_tx: broadcast::Sender<EventEnvelope>,
) {
    loop {
        match events_rx.recv_async().await {
            Ok(env) => {
                // `send` returns Err only when no receivers exist;
                // that's fine — drop on the floor.
                let _ = bcast_tx.send(env);
            }
            Err(_) => {
                debug!(shard_id, "event bridge exiting (shard disconnected)");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SubscriptionRegistry (per-connection)
// ---------------------------------------------------------------------------

pub struct SubscriptionRegistry {
    hub: ShardEventHub,
    inner: Arc<Mutex<RegistryInner>>,
    next_stream_id: AtomicU32,
    metrics: SubscriptionMetrics,
    /// Per-connection cap on concurrently-active subscriptions — the
    /// advertised `max_concurrent_streams`. Over the cap, `start` returns
    /// `StreamLimitExceeded` rather than registering an unbounded number of
    /// per-subscription tasks + broadcast receivers on one connection.
    max_streams: usize,
}

struct RegistryInner {
    streams: HashMap<u32, SubscriptionState>,
}

struct SubscriptionState {
    final_lsn: Arc<AtomicU64>,
    cancel: watch::Sender<bool>,
}

/// RAII slot reclaimer for a per-connection subscription entry.
///
/// Held by the per-subscription task; its `Drop` removes the
/// `stream_id` entry from `RegistryInner::streams` when the task
/// returns by *any* path — natural end, broadcast `Closed`, lag
/// `Overloaded`, WAL-replay error, or client disconnect. Without it,
/// only the explicit `cancel()` path frees the slot, so a churning
/// connection would leak entries until it hit `StreamLimitExceeded`
/// with zero live subscriptions.
///
/// The reference is a [`Weak`](std::sync::Weak) so a task that outlives its
/// (per-connection) registry does not resurrect the freed map: if the
/// registry is gone, the whole `streams` map is gone and the drop is a
/// no-op. Removing an already-absent key (the `cancel()` raced ahead)
/// is likewise a harmless no-op.
struct StreamSlotGuard {
    inner: std::sync::Weak<Mutex<RegistryInner>>,
    stream_id: u32,
}

impl Drop for StreamSlotGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.lock().streams.remove(&self.stream_id);
        }
    }
}

impl SubscriptionRegistry {
    pub fn new(hub: ShardEventHub, max_streams: usize) -> Self {
        Self::with_metrics(hub, SubscriptionMetrics::default(), max_streams)
    }

    /// Construct with an externally-owned `SubscriptionMetrics` so
    /// the admin exposition path can read the same counters the
    /// registry bumps from `start` / `run_subscription_task`.
    pub fn with_metrics(
        hub: ShardEventHub,
        metrics: SubscriptionMetrics,
        max_streams: usize,
    ) -> Self {
        Self {
            hub,
            inner: Arc::new(Mutex::new(RegistryInner {
                streams: HashMap::new(),
            })),
            next_stream_id: AtomicU32::new(0),
            max_streams: max_streams.max(1),
            metrics,
        }
    }

    /// Process-wide subscription counters. Clone-cheap; safe to read
    /// from any thread.
    pub fn metrics(&self) -> SubscriptionMetrics {
        self.metrics.clone()
    }

    /// Start a subscription. Spawns the per-sub task that drains one
    /// shard's broadcast Receiver, filters events, and pushes
    /// SUBSCRIPTION_EVENT frames to `frame_tx`.
    ///
    /// Returns the stream id the client should observe events on. We
    /// use the client's request `stream_id` directly (so the
    /// returned id == `client_stream_id`); future per-listener stream
    /// allocation may reuse the internal `next_stream_id`.
    pub fn start(
        &self,
        client_stream_id: u32,
        target_shard: ShardId,
        req: &SubscribeRequest,
        frame_tx: flume::Sender<crate::connection::OutgoingFrame>,
        similarity_reference: Option<[f32; brain_embed::VECTOR_DIM]>,
    ) -> Result<u32, OpError> {
        let mut filter = parse_filter(req).map_err(OpError::Ops)?;
        // Inject the reference vector resolved by the caller
        // (`handle_subscribe_start`) via the one-time shard round-trip.
        // The threshold was already validated by `parse_filter`. A
        // similarity filter with no resolved reference cannot match
        // meaningfully, so treat it as a hard rejection rather than a
        // silent all-pass.
        if let Some(sim) = req.filter.similar_to {
            match similarity_reference {
                Some(reference) => filter.set_similarity_reference(reference, sim.threshold),
                None => {
                    return Err(OpError::Ops(brain_ops::OpError::InvalidRequest(
                        "subscribe: similar_to reference vector was not resolved".into(),
                    )))
                }
            }
        }
        // Subscribe live FIRST, then replay the WAL — this is the
        // cutover discipline:
        // taking the broadcast Receiver before we read the WAL
        // guarantees no event in `[from_lsn, current_tail)` slips
        // through the gap between "WAL tail snapshot" and "live
        // drain start." Duplicate events between replay + live are
        // filtered by LSN inside `run_subscription_task`.
        let rx = self
            .hub
            .subscribe_shard(target_shard)
            .ok_or(OpError::ShardOutOfRange(target_shard))?;

        // `from_lsn` is the precise resume mechanism ("continue from
        // where I left off"); `include_history` is the coarse "replay
        // the history still retained, then go live" flag. An explicit
        // `from_lsn` always wins; when the client only asked for
        // `include_history` (no `from_lsn`), replay everything still in
        // the WAL by anchoring at LSN 0 — the retained-history-then-live
        // cutover the WAL-tail semantic already implements. Without this
        // mapping `include_history` would be silently ignored.
        let effective_from_lsn = req.from_lsn.or({
            if req.include_history {
                Some(0)
            } else {
                None
            }
        });
        // Optional replay info — `None` when the client subscribed
        // to the live tail only.
        let replay = if let Some(from_lsn) = effective_from_lsn {
            let locator = self
                .hub
                .wal_locator(target_shard)
                .ok_or(OpError::ShardOutOfRange(target_shard))?;
            // Validate `from_lsn` against the oldest available
            // segment up front so we surface `LsnTooOld` in the
            // SUBSCRIBE_RESP-equivalent path instead of after the
            // task has started streaming.
            //
            // `from_lsn == 0` means "everything still in the WAL" —
            // not an error.
            let reader =
                brain_storage::wal::reader::WalReader::open(&locator.dir, locator.shard_uuid)
                    .map_err(|e| OpError::WalOpen(format!("{e}")))?;
            let oldest = reader.segments().first().map_or(1, |s| s.starting_lsn);
            if from_lsn > 0 && from_lsn < oldest {
                return Err(OpError::LsnTooOld { oldest });
            }
            Some(ReplayParams { from_lsn, locator })
        } else {
            None
        };

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let final_lsn = Arc::new(AtomicU64::new(0));

        {
            let mut inner = self.inner.lock();
            if inner.streams.contains_key(&client_stream_id) {
                return Err(OpError::StreamIdInUse);
            }
            if inner.streams.len() >= self.max_streams {
                return Err(OpError::StreamLimitExceeded);
            }
            inner.streams.insert(
                client_stream_id,
                SubscriptionState {
                    final_lsn: final_lsn.clone(),
                    cancel: cancel_tx,
                },
            );
        }
        let _ = self
            .next_stream_id
            .fetch_max(client_stream_id, Ordering::SeqCst);

        self.metrics.record_start();
        let stream_id = client_stream_id;
        let final_lsn_for_task = final_lsn;
        let frame_tx_for_task = frame_tx;
        let metrics_for_task = self.metrics.clone();
        let replay_semaphore = self.hub.replay_semaphore();
        // Free the registry slot on EVERY task exit, not just explicit
        // cancel(). The guard is moved into the task and drops when
        // `run_subscription_task` returns by any path.
        let slot_guard = StreamSlotGuard {
            inner: Arc::downgrade(&self.inner),
            stream_id,
        };
        tokio::spawn(async move {
            let _slot_guard = slot_guard;
            run_subscription_task(
                stream_id,
                target_shard,
                filter,
                rx,
                cancel_rx,
                final_lsn_for_task,
                frame_tx_for_task,
                replay,
                metrics_for_task,
                replay_semaphore,
            )
            .await;
        });

        Ok(stream_id)
    }

    /// Cancel a subscription (UNSUBSCRIBE_REQ or CANCEL_STREAM path).
    /// Returns the last-delivered LSN, or None if the stream id isn't
    /// registered.
    pub fn cancel(&self, stream_id: u32) -> Option<u64> {
        let entry = {
            let mut inner = self.inner.lock();
            inner.streams.remove(&stream_id)?
        };
        let lsn = entry.final_lsn.load(Ordering::SeqCst);
        let _ = entry.cancel.send(true);
        Some(lsn)
    }

    pub fn active_count(&self) -> usize {
        self.inner.lock().streams.len()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error("subscribe: from_lsn below oldest available LSN {oldest}")]
    LsnTooOld { oldest: u64 },
    #[error("subscribe: stream_id already in use")]
    StreamIdInUse,
    #[error("subscribe: per-connection concurrent stream limit reached")]
    StreamLimitExceeded,
    #[error("subscribe: target shard {0} out of range")]
    ShardOutOfRange(u16),
    #[error("subscribe: WAL open: {0}")]
    WalOpen(String),
    #[error("subscribe: filter parse: {0}")]
    Ops(#[from] brain_ops::OpError),
}

impl OpError {
    pub fn to_error_frame(&self, stream_id: u32) -> Frame {
        let (code, message) = match self {
            Self::LsnTooOld { oldest } => (
                ErrorCode::SubscriptionLsnTooOld,
                format!(
                    "from_lsn is below the oldest available LSN ({oldest}); WAL retention has GC'd that range"
                ),
            ),
            Self::StreamIdInUse => (ErrorCode::StreamIdInUse, self.to_string()),
            Self::StreamLimitExceeded => (ErrorCode::StreamLimitExceeded, self.to_string()),
            Self::ShardOutOfRange(_) => (ErrorCode::ShardUnavailable, self.to_string()),
            Self::WalOpen(_) => (ErrorCode::Internal, self.to_string()),
            Self::Ops(e) => (ErrorCode::InvalidArgument, format!("subscribe filter: {e}")),
        };
        let body = ResponseBody::Error(ErrorResponse {
            code: ErrorCodeWire::from(code),
            category: ErrorCategoryWire::from(code.category()),
            message,
            details: None,
            retry_after_ms: None,
        });
        Frame::new(Opcode::Error.as_u16(), FLAG_EOS, stream_id, body.encode())
    }
}

/// Replay state captured at SUBSCRIBE-time and handed to the per-sub
/// task. The locator + from_lsn are everything the prologue needs to
/// stream historical events before the live tail kicks in.
struct ReplayParams {
    from_lsn: u64,
    locator: WalLocator,
}

// ---------------------------------------------------------------------------
// Per-subscription task
// ---------------------------------------------------------------------------

// Subscription task argument list mirrors the per-subscription owned
// state. Bundling into a struct would just shadow the same fields.
#[allow(clippy::too_many_arguments)]
async fn run_subscription_task(
    stream_id: u32,
    target_shard: ShardId,
    filter: ParsedFilter,
    mut rx: broadcast::Receiver<EventEnvelope>,
    mut cancel_rx: watch::Receiver<bool>,
    final_lsn: Arc<AtomicU64>,
    frame_tx: flume::Sender<crate::connection::OutgoingFrame>,
    replay: Option<ReplayParams>,
    metrics: SubscriptionMetrics,
    replay_semaphore: Arc<tokio::sync::Semaphore>,
) {
    // ── Replay prologue: history-then-live with no
    // gap and no dupes. Walk the WAL from `from_lsn` to the first
    // LSN we observe on the live channel, emitting matching events.
    // The cutover key is `replay_high_water`: any live event with
    // `lsn <= replay_high_water` is a dupe — we already emitted it
    // from the WAL — and is suppressed. ────────────────────────────
    let mut replay_high_water: u64 = 0;
    if let Some(params) = replay {
        // Acquire a replay permit — bounds concurrent WAL scans
        // process-wide so a 10K-subscriber reconnect storm queues
        // here instead of thrashing the tokio blocking pool. The
        // permit is dropped at the end of this scope, AFTER the
        // blocking scan completes.
        let _replay_permit = match replay_semaphore.acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                // Semaphore closed — server is shutting down.
                return;
            }
        };
        match replay_wal_segment(
            stream_id,
            &params.locator,
            params.from_lsn,
            &filter,
            &final_lsn,
            &frame_tx,
        )
        .await
        {
            Ok(last_lsn) => {
                replay_high_water = last_lsn;
            }
            Err(e) => {
                tracing::warn!(stream_id, target_shard, error = %e, "WAL replay failed mid-stream");
                let f = error_frame(
                    stream_id,
                    ErrorCode::Internal,
                    &format!("subscribe replay error: {e}"),
                );
                let _ = frame_tx
                    .send_async(crate::connection::OutgoingFrame {
                        bytes: f.encode(),
                        close_after: false,
                    })
                    .await;
                return;
            }
        }
    }

    loop {
        tokio::select! {
            biased;
            res = cancel_rx.changed() => {
                if res.is_err() || *cancel_rx.borrow() {
                    break;
                }
            }
            res = rx.recv() => {
                match res {
                    Ok(env) => {
                        if env.lsn != 0 && env.lsn <= replay_high_water {
                            // Already delivered via WAL replay.
                            continue;
                        }
                        if !filter.matches(&env) {
                            continue;
                        }
                        let frame = build_subscription_event_frame(stream_id, &env);
                        final_lsn.store(env.lsn, Ordering::SeqCst);
                        if frame_tx
                            .send_async(crate::connection::OutgoingFrame {
                                bytes: frame.encode(),
                                close_after: false,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // Slow subscriber — drop the subscription with
                        // an ERROR(Overloaded).
                        metrics.record_lag(skipped);
                        warn!(stream_id, target_shard, skipped, "subscription lagged");
                        let f = error_frame(
                            stream_id,
                            ErrorCode::Overloaded,
                            "subscription lagged; reconnect with a fresh from_lsn",
                        );
                        let _ = frame_tx
                            .send_async(crate::connection::OutgoingFrame {
                                bytes: f.encode(),
                                close_after: false,
                            })
                            .await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    // Final EOS frame on this subscription's stream.
    let eos = empty_subscription_event_frame(stream_id, final_lsn.load(Ordering::SeqCst));
    let _ = frame_tx
        .send_async(crate::connection::OutgoingFrame {
            bytes: eos.encode(),
            close_after: false,
        })
        .await;
}

/// Walk the WAL from `from_lsn` to its current tail, projecting each
/// record into an `EventEnvelope` via [`EventEnvelope::from_wal_record`]
/// and writing matching events as SUBSCRIBE_EVENT frames. Returns the
/// highest LSN written (or 0 if nothing matched). Synchronous WAL
/// reads happen on a spawn_blocking pool to keep the tokio runtime
/// healthy.
async fn replay_wal_segment(
    stream_id: u32,
    locator: &WalLocator,
    from_lsn: u64,
    filter: &ParsedFilter,
    final_lsn: &Arc<AtomicU64>,
    frame_tx: &flume::Sender<crate::connection::OutgoingFrame>,
) -> Result<u64, String> {
    let locator_dir = locator.dir.clone();
    let locator_uuid = locator.shard_uuid;
    let filter = filter.clone();
    let final_lsn = final_lsn.clone();
    let frame_tx = frame_tx.clone();
    // Run the WAL scan + frame emission on a blocking thread so the
    // tokio runtime doesn't stall on synchronous fs reads. The WAL
    // reader is sync; the only async edge is the frame send.
    let last_lsn = tokio::task::spawn_blocking(move || -> Result<u64, String> {
        let iter =
            brain_storage::wal::reader::WalReader::iter_from(&locator_dir, locator_uuid, from_lsn)
                .map_err(|e| format!("WalReader::iter_from: {e}"))?;
        let mut last = 0u64;
        for item in iter {
            let record = item.map_err(|e| format!("WAL read: {e}"))?;
            let lsn = record.lsn.raw();
            // One WAL record may surface multiple envelopes (e.g. an
            // ENCODE with N edges emits Encoded + N EdgeAdded events).
            for env in EventEnvelope::from_wal_record(&record) {
                if !filter.matches(&env) {
                    continue;
                }
                let frame = build_subscription_event_frame(stream_id, &env);
                final_lsn.store(lsn, Ordering::SeqCst);
                // Blocking send into the frame_tx; we're on a blocking
                // pool, so this is fine. The async send variant
                // requires a tokio context.
                frame_tx
                    .send(crate::connection::OutgoingFrame {
                        bytes: frame.encode(),
                        close_after: false,
                    })
                    .map_err(|_| "frame_tx dropped".to_string())?;
                last = lsn;
            }
        }
        Ok(last)
    })
    .await
    .map_err(|e| format!("spawn_blocking: {e}"))??;
    Ok(last_lsn)
}

fn build_subscription_event_frame(stream_id: u32, env: &EventEnvelope) -> Frame {
    let payload = ResponseBody::SubscribeEvent(env.to_wire()).encode();
    // Intermediate frames in an open-ended subscription don't carry
    // EOS.
    Frame::new(Opcode::SubscribeEvent.as_u16(), 0, stream_id, payload)
}

fn empty_subscription_event_frame(stream_id: u32, last_lsn: u64) -> Frame {
    // Terminal EOS frame for a cancelled / lagged subscription. The
    // body carries the last-delivered LSN so the client can resume.
    let payload = ResponseBody::SubscribeEvent(SubscriptionEvent {
        event_type: brain_protocol::envelope::response::EventType::Forgotten,
        memory_id: MemoryId::NULL.raw(),
        session_id: 0,
        text: String::new(),
        kind: brain_protocol::envelope::request::MemoryKindWire::Episodic,
        salience: 0.0,
        timestamp_unix_nanos: 0,
        lsn: last_lsn,
        graph_payload: None,
        edge_payload: None,
        stage_kind: None,
        stage_outcome: None,
        stage_payload: None,
    })
    .encode();
    Frame::new(
        Opcode::SubscribeEvent.as_u16(),
        FLAG_EOS,
        stream_id,
        payload,
    )
}

fn error_frame(stream_id: u32, code: ErrorCode, message: &str) -> Frame {
    let body = ResponseBody::Error(ErrorResponse {
        code: ErrorCodeWire::from(code),
        category: ErrorCategoryWire::from(code.category()),
        message: message.to_owned(),
        details: None,
        retry_after_ms: None,
    });
    Frame::new(Opcode::Error.as_u16(), FLAG_EOS, stream_id, body.encode())
}

// ---------------------------------------------------------------------------
// UNSUBSCRIBE / CANCEL_STREAM frame builders
// ---------------------------------------------------------------------------

pub fn build_unsubscribe_response_frame(
    request_stream_id: u32,
    req: &UnsubscribeRequest,
    final_lsn: u64,
) -> Frame {
    let body = ResponseBody::Unsubscribe(UnsubscribeResponse {
        target_stream_id: req.target_stream_id,
        final_lsn,
    });
    Frame::new(
        Opcode::UnsubscribeResp.as_u16(),
        FLAG_EOS,
        request_stream_id,
        body.encode(),
    )
}

pub fn build_cancel_stream_ack_frame(
    request_stream_id: u32,
    req: &CancelStreamRequest,
    now_unix_nanos: u64,
) -> Frame {
    let body = ResponseBody::CancelStreamAck(CancelStreamAck {
        target_stream_id: req.target_stream_id,
        cancelled_at_unix_nanos: now_unix_nanos,
    });
    Frame::new(
        Opcode::CancelStreamAck.as_u16(),
        FLAG_EOS,
        request_stream_id,
        body.encode(),
    )
}

#[cfg(test)]
mod slot_reclaim_tests {
    use super::*;
    use brain_core::{MemoryKind, SessionId, SpaceId};
    use brain_protocol::envelope::response::EventType;
    use std::time::Duration;

    /// A hub with one broadcast bus and no real shard behind it. The
    /// caller keeps the returned `Sender` to publish synthetic events
    /// onto the single shard's channel.
    fn test_hub() -> (ShardEventHub, broadcast::Sender<EventEnvelope>) {
        let (tx, _keep) = broadcast::channel::<EventEnvelope>(16);
        let hub = ShardEventHub {
            per_shard_bus: Arc::new(vec![tx.clone()]),
            wal_locators: Arc::new(vec![WalLocator {
                dir: std::path::PathBuf::from("/nonexistent"),
                shard_uuid: [0u8; 16],
            }]),
            broadcast_capacity: 16,
            replay_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        };
        (hub, tx)
    }

    /// An all-pass live-tail subscribe request (no filter, no replay).
    fn all_pass_request() -> SubscribeRequest {
        SubscribeRequest {
            filter: brain_protocol::envelope::request::SubscriptionFilter {
                session_filter: None,
                kinds: None,
                similar_to: None,
                spaces: None,
                memory_ids: None,
            },
            include_history: false,
            from_lsn: None,
            max_inflight: 0,
            act_as: None,
        }
    }

    fn synthetic_event(lsn: u64) -> EventEnvelope {
        EventEnvelope {
            lsn,
            event_type: EventType::Encoded,
            memory_id: MemoryId::from(1u128),
            session_id: SessionId::default(),
            kind: MemoryKind::Episodic,
            salience: 0.5,
            timestamp_unix_nanos: 0,
            text: None,
            graph_payload: None,
            edge_payload: None,
            stage_kind: None,
            stage_outcome: None,
            stage_payload: None,
            space_id: SpaceId::default(),
            vector: None,
        }
    }

    async fn wait_for_count(registry: &SubscriptionRegistry, want: usize) {
        for _ in 0..200 {
            if registry.active_count() == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "active_count never reached {want}; stuck at {}",
            registry.active_count()
        );
    }

    /// A subscription that exits *naturally* (client gone — the
    /// outgoing frame channel's receiver is dropped, so the task's
    /// frame send fails and it returns) must free its registry slot.
    /// Before the RAII guard, only `cancel()` freed the slot, so this
    /// path leaked one of `max_concurrent_streams` forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn natural_exit_frees_slot() {
        let (hub, event_tx) = test_hub();
        let registry = SubscriptionRegistry::new(hub, 4);

        let (frame_tx, frame_rx) = flume::bounded(16);
        // Model a disconnected client: nothing will ever read frames,
        // and the sender sees the channel as closed.
        drop(frame_rx);

        let req = all_pass_request();
        registry
            .start(1, 0, &req, frame_tx, None)
            .expect("start ok");
        // Entry is registered synchronously on `start`.
        assert_eq!(registry.active_count(), 1);

        // Publish an event; the task wakes, tries to forward it, the
        // send fails (receiver gone), the task returns, and the guard
        // reclaims the slot.
        event_tx.send(synthetic_event(1)).expect("broadcast send");

        wait_for_count(&registry, 0).await;
        assert_eq!(registry.active_count(), 0);
    }

    /// After a natural exit reclaims the slot, the same `stream_id`
    /// can be reused for a fresh subscription — proof the entry is
    /// truly gone and not merely dead-but-present (which would trip
    /// `StreamIdInUse`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_id_reusable_after_natural_exit() {
        let (hub, event_tx) = test_hub();
        let registry = SubscriptionRegistry::new(hub, 4);

        let (frame_tx, frame_rx) = flume::bounded(16);
        drop(frame_rx);
        registry
            .start(7, 0, &all_pass_request(), frame_tx, None)
            .expect("first start ok");
        event_tx.send(synthetic_event(1)).expect("broadcast send");
        wait_for_count(&registry, 0).await;

        // Same stream_id, fresh live receiver — must not be
        // StreamIdInUse.
        let (frame_tx2, _frame_rx2) = flume::bounded(16);
        registry
            .start(7, 0, &all_pass_request(), frame_tx2, None)
            .expect("reuse of freed stream_id must succeed");
        assert_eq!(registry.active_count(), 1);
        // Clean shutdown of the live subscription.
        assert!(registry.cancel(7).is_some());
    }

    /// Churn `max_streams` subscriptions to natural exit, one at a
    /// time, then confirm a fresh subscribe still succeeds. Before the
    /// fix, leaked entries would exhaust the per-connection cap and
    /// this final subscribe would fail with `StreamLimitExceeded`
    /// despite zero live subscriptions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn churn_does_not_exhaust_cap() {
        let (hub, event_tx) = test_hub();
        let max = 3;
        let registry = SubscriptionRegistry::new(hub, max);

        for id in 0..(max as u32 * 3) {
            let (frame_tx, frame_rx) = flume::bounded(16);
            drop(frame_rx);
            registry
                .start(id, 0, &all_pass_request(), frame_tx, None)
                .expect("start within reclaimed cap");
            event_tx.send(synthetic_event(1)).expect("broadcast send");
            wait_for_count(&registry, 0).await;
        }

        // Cap is fully reclaimed: a live subscription still fits.
        let (frame_tx, _frame_rx) = flume::bounded(16);
        registry
            .start(999, 0, &all_pass_request(), frame_tx, None)
            .expect("fresh subscribe after churn must succeed");
        assert_eq!(registry.active_count(), 1);
    }
}
