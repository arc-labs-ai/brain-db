//! SUBSCRIBE — change-feed for new memories matching a filter.
//!
//! Spec: `spec/09_cognitive_operations/09_subscribe.md` and
//! `spec/03_wire_protocol/07_subscribe.md` (frame shape).
//!
//! ## v1 scope (sub-task 7.10)
//!
//! - **EventBus**: in-process [`tokio::sync::broadcast`] of
//!   [`EventEnvelope`]. The writer publishes one envelope per
//!   successful committed mutation (single-op encode/forget +
//!   each encode/forget inside a TXN_COMMIT batch). The bus owns
//!   a monotonic LSN allocator — stand-in until Phase 9 wires the
//!   WAL LSN.
//! - **SubscriptionRegistry**: tracks active subscriptions by
//!   `target_stream_id`, caches the parsed filter, and remembers the
//!   `started_at_lsn` and the last-delivered `final_lsn` per stream.
//! - **Dispatcher**: [`handle_subscribe`] registers + awaits the
//!   first matching event (bounded poll, default 5s). The wire
//!   response shape is a single event today; the long-lived push
//!   path lives in Phase 9, which will call
//!   [`SubscriptionRegistry::register`] directly and frame events
//!   out of the returned receiver.
//! - **Backpressure**: a lagged subscriber returns
//!   [`broadcast::error::RecvError::Lagged`], which is surfaced as
//!   `OpError::Overloaded` from the dispatcher path; the registry's
//!   `final_lsn` for that stream stays frozen.
//!
//! ## v1 gaps (Phase 9 closes)
//!
//! - No WAL-tail history replay; `from_lsn = Some(_)` is rejected as
//!   `LsnTooOld`-equivalent (currently surfaced as `NotFound { what:
//!   "wal_segment", ... }`).
//! - No `EdgeAdded` / `EdgeRemoved` events — wire `EventType` enum
//!   today is `{Encoded, Forgotten, Reclaimed, KindChanged}`. LINK
//!   / UNLINK commits write to redb but do **not** emit events.
//! - `Reclaimed` / `KindChanged` are background-worker concerns
//!   (Phase 8); the writer never produces them.
//! - `SimilarityFilter` is rejected with `NotYetImplemented`.
//! - `ack_required` flow-control protocol is out of scope.
//! - `min_salience` filter slot is reserved but not populated — the
//!   wire `SubscriptionFilter` doesn't carry the field today.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use brain_core::{ContextId, MemoryId, MemoryKind};
use brain_protocol::request::{SubscribeRequest, UnsubscribeRequest};
use brain_protocol::response::{EventType, SubscriptionEvent, UnsubscribeResponse};
use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::context::OpsContext;
use crate::error::OpError;

/// Default broadcast channel capacity. A subscriber that lags by more
/// than this many envelopes will receive
/// [`broadcast::error::RecvError::Lagged`].
pub const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 1024;

// ---------------------------------------------------------------------------
// LSN allocator + envelope.
// ---------------------------------------------------------------------------

/// Strictly-increasing per-process LSN. v1 stand-in until Phase 9
/// wires the WAL LSN. Single shard ⇒ a single allocator gives spec
/// §10/4's "delivered in WAL order (per shard)" property by
/// construction.
#[derive(Debug, Default)]
pub struct LsnAllocator(AtomicU64);

impl LsnAllocator {
    /// Reserve the next LSN. Returns a value strictly greater than any
    /// previously returned value.
    pub fn next_lsn(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Read the highest-allocated LSN without consuming one. Used by
    /// [`SubscriptionRegistry::register`] to snapshot the "started at"
    /// LSN for a fresh subscription.
    pub fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Internal event payload pushed onto the [`EventBus`]. Carries the
/// raw `brain-core` types so per-subscriber filter evaluation is
/// cheap (no wire-type conversion until we serialise the matched
/// event).
#[derive(Clone, Debug)]
pub struct EventEnvelope {
    pub lsn: u64,
    pub event_type: EventType,
    pub memory_id: MemoryId,
    pub context_id: ContextId,
    pub kind: MemoryKind,
    pub salience: f32,
    pub timestamp_unix_nanos: u64,
    /// Memory text — `Some` only if the publisher carries it
    /// (encode publishes the text; forget does not).
    pub text: Option<String>,
    /// Typed knowledge-layer payload — `None` for substrate events,
    /// `Some(_)` for the 14 knowledge event variants (phase 16.7+).
    pub knowledge_payload: Option<brain_protocol::knowledge::KnowledgeEventPayload>,
}

impl EventEnvelope {
    /// Convert to the wire [`SubscriptionEvent`].
    #[must_use]
    pub fn to_wire(&self) -> SubscriptionEvent {
        SubscriptionEvent {
            event_type: self.event_type,
            memory_id: self.memory_id.into(),
            context_id: self.context_id.into(),
            text: self.text.clone().unwrap_or_default(),
            kind: self.kind.into(),
            salience: self.salience,
            timestamp_unix_nanos: self.timestamp_unix_nanos,
            lsn: self.lsn,
            knowledge_payload: self.knowledge_payload.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// EventBus.
// ---------------------------------------------------------------------------

/// In-process broadcast bus owning the per-shard LSN allocator. One
/// instance per `OpsContext`. Cloning is cheap (Arc inside).
pub struct EventBus {
    sender: broadcast::Sender<EventEnvelope>,
    lsn: LsnAllocator,
}

impl EventBus {
    #[must_use]
    pub fn new(channel_capacity: usize) -> Self {
        let (sender, _rx) = broadcast::channel(channel_capacity);
        Self {
            sender,
            lsn: LsnAllocator::default(),
        }
    }

    /// Highest-allocated LSN. New subscribers anchor on this value.
    pub fn current_lsn(&self) -> u64 {
        self.lsn.current()
    }

    /// Allocate a fresh LSN, stamp it on the envelope, and publish to
    /// all active subscribers. Returns the assigned LSN.
    ///
    /// `send` returns `Err` when there are no receivers; that's not
    /// a failure for us — events are dropped on the floor (spec
    /// §10/4: "delivered at-least-once" applies only to *active*
    /// subscribers).
    pub fn publish(&self, mut env: EventEnvelope) -> u64 {
        env.lsn = self.lsn.next_lsn();
        let lsn = env.lsn;
        let _ = self.sender.send(env);
        lsn
    }

    /// Get a fresh receiver. Only events sent *after* this call are
    /// delivered.
    pub fn receiver(&self) -> broadcast::Receiver<EventEnvelope> {
        self.sender.subscribe()
    }

    /// Active subscriber count (useful for tests).
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_EVENT_CHANNEL_CAPACITY)
    }
}

// ---------------------------------------------------------------------------
// ParsedFilter.
// ---------------------------------------------------------------------------

/// Registry-side filter representation. Built once at `register` time
/// so per-event matching is cheap (set lookups, no wire conversions).
#[derive(Clone, Debug, Default)]
pub struct ParsedFilter {
    pub contexts: Option<HashSet<ContextId>>,
    pub kinds: Option<HashSet<MemoryKind>>,
    /// Reserved slot. Wire `SubscriptionFilter` doesn't carry
    /// `min_salience` today; spec §2 lists it as desirable. Always
    /// `None` in v1.
    pub min_salience: Option<f32>,
}

impl ParsedFilter {
    #[must_use]
    pub fn matches(&self, env: &EventEnvelope) -> bool {
        if let Some(ctxs) = &self.contexts {
            if !ctxs.contains(&env.context_id) {
                return false;
            }
        }
        if let Some(ks) = &self.kinds {
            if !ks.contains(&env.kind) {
                return false;
            }
        }
        if let Some(t) = self.min_salience {
            if env.salience < t {
                return false;
            }
        }
        true
    }
}

/// Parse the wire `SubscribeRequest` into a registry-side
/// [`ParsedFilter`]. Made public in 9.11 so `brain-server`'s
/// connection-layer registry can reuse the same shape.
pub fn parse_filter(req: &SubscribeRequest) -> Result<ParsedFilter, OpError> {
    if req.filter.similar_to.is_some() {
        return Err(OpError::NotYetImplemented(
            "SUBSCRIBE: similar_to filter — Phase 9",
        ));
    }
    let contexts = req
        .filter
        .contexts
        .as_ref()
        .map(|v| v.iter().copied().map(ContextId).collect::<HashSet<_>>());
    let kinds = req.filter.kinds.as_ref().map(|v| {
        v.iter()
            .copied()
            .map(MemoryKind::from)
            .collect::<HashSet<_>>()
    });
    Ok(ParsedFilter {
        contexts,
        kinds,
        min_salience: None,
    })
}

// ---------------------------------------------------------------------------
// SubscriptionRegistry.
// ---------------------------------------------------------------------------

struct SubEntry {
    /// Cached filter — used by Phase 9's pump task. The dispatcher
    /// path doesn't consult it (it clones the filter onto the
    /// `SubscriptionHandle` instead), so the field is dead for v1.
    #[allow(dead_code)]
    filter: ParsedFilter,
    /// Snapshot of `EventBus::current_lsn()` at register time.
    /// Surfaced via [`SubscriptionHandle::started_at_lsn`] for the
    /// caller; the registry uses it as the initial `final_lsn`.
    #[allow(dead_code)]
    started_at_lsn: u64,
    final_lsn: AtomicU64,
}

struct RegistryInner {
    next_stream_id: u32,
    streams: HashMap<u32, SubEntry>,
}

/// Tracks active subscriptions. Phase 9's connection task calls
/// [`Self::register`] to get a receiver + handle and frames events
/// directly; the dispatcher path uses the same surface but returns
/// only the first matching event.
pub struct SubscriptionRegistry {
    bus: Arc<EventBus>,
    inner: Mutex<RegistryInner>,
}

/// Per-subscription handle returned to callers. Holds the receiver
/// the caller pumps to deliver events.
pub struct SubscriptionHandle {
    pub target_stream_id: u32,
    pub started_at_lsn: u64,
    pub filter: ParsedFilter,
    pub receiver: broadcast::Receiver<EventEnvelope>,
}

impl SubscriptionRegistry {
    #[must_use]
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self {
            bus,
            inner: Mutex::new(RegistryInner {
                next_stream_id: 1,
                streams: HashMap::new(),
            }),
        }
    }

    /// Validate the request, allocate a stream id, install the entry,
    /// and return a receiver primed at the bus's current tail.
    pub fn register(&self, req: &SubscribeRequest) -> Result<SubscriptionHandle, OpError> {
        if req.from_lsn.is_some() {
            // Spec §17 — LsnTooOld until Phase 9 wires WAL replay. We
            // surface it as `NotFound { what: "wal_segment", ... }`
            // which maps to the same wire `NotFound` family.
            return Err(OpError::NotFound {
                what: "wal_segment",
                detail: "subscribe: from_lsn replay requires WAL (Phase 9) — \
                         LsnTooOld for any non-LatestOnly start"
                    .into(),
            });
        }
        let filter = parse_filter(req)?;

        // Subscribe *first*, then snapshot the LSN. That ordering
        // means an event published between these two lines lands in
        // the receiver buffer (we'll see it) and may have lsn >
        // started_at_lsn (we won't miss it). The reverse ordering
        // would race the other way and could lose an event.
        let receiver = self.bus.receiver();
        let started_at_lsn = self.bus.current_lsn();

        let mut inner = self.inner.lock();
        let stream_id = inner.next_stream_id;
        inner.next_stream_id = inner
            .next_stream_id
            .checked_add(1)
            .ok_or_else(|| OpError::Overloaded("subscribe: out of stream ids".into()))?;
        inner.streams.insert(
            stream_id,
            SubEntry {
                filter: filter.clone(),
                started_at_lsn,
                final_lsn: AtomicU64::new(started_at_lsn),
            },
        );
        Ok(SubscriptionHandle {
            target_stream_id: stream_id,
            started_at_lsn,
            filter,
            receiver,
        })
    }

    /// Drop a subscription and return its last-delivered LSN. The
    /// matching wire response is `UnsubscribeResponse { stream_id,
    /// final_lsn }`.
    pub fn unregister(&self, stream_id: u32) -> Result<u64, OpError> {
        let mut inner = self.inner.lock();
        match inner.streams.remove(&stream_id) {
            Some(entry) => Ok(entry.final_lsn.load(Ordering::SeqCst)),
            None => Err(OpError::NotFound {
                what: "subscription",
                detail: format!("stream_id={stream_id}"),
            }),
        }
    }

    /// Advance the recorded `final_lsn` for a stream. Phase 9's pump
    /// task calls this after each event it frames; the v1 dispatcher
    /// calls it once after the first matching event.
    pub fn update_final_lsn(&self, stream_id: u32, lsn: u64) {
        let inner = self.inner.lock();
        if let Some(entry) = inner.streams.get(&stream_id) {
            entry.final_lsn.store(lsn, Ordering::SeqCst);
        }
    }

    /// Number of active streams.
    pub fn active_count(&self) -> usize {
        self.inner.lock().streams.len()
    }

    /// Inspect a stream's recorded `final_lsn` (used by tests).
    pub fn final_lsn(&self, stream_id: u32) -> Option<u64> {
        self.inner
            .lock()
            .streams
            .get(&stream_id)
            .map(|e| e.final_lsn.load(Ordering::SeqCst))
    }
}

// ---------------------------------------------------------------------------
// Dispatcher handlers.
// ---------------------------------------------------------------------------

/// Spec §09/09 — one-shot dispatcher contract.
///
/// 1. Reject `from_lsn = Some(_)` (no WAL replay yet).
/// 2. Reject `similar_to` filter (no per-event vector lookup yet).
/// 3. Register the subscription, get a receiver.
/// 4. Poll the receiver with a bounded window (default 5 s;
///    configured per-context via
///    [`OpsContext::with_subscribe_poll_window`]) for the first event
///    that matches the filter.
/// 5. On match → update `final_lsn`, return the wire event.
///    On `Lagged` → return `Overloaded`.
///    On timeout → return `Overloaded` ("retry / use the streaming
///    path that Phase 9 wires").
pub async fn handle_subscribe(
    req: SubscribeRequest,
    ctx: &OpsContext,
) -> Result<SubscriptionEvent, OpError> {
    let handle = ctx.subscriptions.register(&req)?;
    let stream_id = handle.target_stream_id;
    let filter = handle.filter.clone();
    let mut receiver = handle.receiver;

    let deadline = tokio::time::Instant::now() + ctx.subscribe_poll_window;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(OpError::Overloaded(
                "subscribe: no matching event within poll window — \
                 Phase 9 enables long-lived streaming"
                    .into(),
            ));
        }
        match tokio::time::timeout(remaining, receiver.recv()).await {
            Ok(Ok(env)) => {
                if filter.matches(&env) {
                    ctx.subscriptions.update_final_lsn(stream_id, env.lsn);
                    return Ok(env.to_wire());
                }
                // Non-matching event — keep waiting.
                continue;
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                // Backpressure. `final_lsn` stays frozen at the
                // started_at_lsn; the registry entry survives so the
                // client can UNSUBSCRIBE and observe the freeze.
                return Err(OpError::Overloaded(
                    "subscribe: subscriber lagged — Phase 9's long-lived \
                     stream tolerates lag without dropping the subscription"
                        .into(),
                ));
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Err(OpError::Internal("subscribe: event bus closed".into()));
            }
            Err(_) => {
                return Err(OpError::Overloaded(
                    "subscribe: no matching event within poll window — \
                     Phase 9 enables long-lived streaming"
                        .into(),
                ));
            }
        }
    }
}

/// Spec §09/09 §8 — drop the subscription, return final LSN.
pub async fn handle_unsubscribe(
    req: UnsubscribeRequest,
    ctx: &OpsContext,
) -> Result<UnsubscribeResponse, OpError> {
    let final_lsn = ctx.subscriptions.unregister(req.target_stream_id)?;
    Ok(UnsubscribeResponse {
        target_stream_id: req.target_stream_id,
        final_lsn,
    })
}

// ---------------------------------------------------------------------------
// Send/Sync guards.
// ---------------------------------------------------------------------------
