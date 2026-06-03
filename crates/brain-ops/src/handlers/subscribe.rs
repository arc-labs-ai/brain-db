//! SUBSCRIBE — change-feed for new memories matching a filter.
//!
//! ## v1 scope
//!
//! - **EventBus**: in-process [`tokio::sync::broadcast`] of
//!   [`EventEnvelope`]. The writer publishes one envelope per
//!   successful committed mutation (single-op encode/forget +
//!   each encode/forget inside a TXN_COMMIT batch). The bus owns
//!   a monotonic LSN allocator — stand-in until the WAL LSN is
//!   wired.
//! - **SubscriptionRegistry**: tracks active subscriptions by
//!   `target_stream_id`, caches the parsed filter, and remembers the
//!   `started_at_lsn` and the last-delivered `final_lsn` per stream.
//! - **Dispatcher**: [`handle_subscribe`] registers + awaits the
//!   first matching event (bounded poll, default 5s). The wire
//!   response shape is a single event today; the long-lived push
//!   path lands later, calling
//!   [`SubscriptionRegistry::register`] directly and framing events
//!   out of the returned receiver.
//! - **Backpressure**: a lagged subscriber returns
//!   [`broadcast::error::RecvError::Lagged`], which is surfaced as
//!   `OpError::Overloaded` from the dispatcher path; the registry's
//!   `final_lsn` for that stream stays frozen.
//!
//! ## v1 gaps
//!
//! - No WAL-tail history replay; `from_lsn = Some(_)` is rejected as
//!   `LsnTooOld`-equivalent (currently surfaced as `NotFound { what:
//!   "wal_segment", ... }`).
//! - No `EdgeAdded` / `EdgeRemoved` events — wire `EventType` enum
//!   today is `{Encoded, Forgotten, Reclaimed, KindChanged}`. LINK
//!   / UNLINK commits write to redb but do **not** emit events.
//! - `Reclaimed` / `KindChanged` are background-worker concerns;
//!   the writer never produces them.
//! - `SimilarityFilter` is rejected with `NotYetImplemented`.
//! - `ack_required` flow-control protocol is out of scope.
//! - `min_salience` filter slot is reserved but not populated — the
//!   wire `SubscriptionFilter` doesn't carry the field today.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use brain_core::{ContextId, MemoryId, MemoryKind};
use brain_protocol::envelope::request::{SubscribeRequest, UnsubscribeRequest};
use brain_protocol::envelope::response::{
    EdgeEventPayload, EventType, SubscriptionEvent, UnsubscribeResponse,
};
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

/// Strictly-increasing per-process LSN. v1 stand-in until the WAL
/// LSN is wired. Single shard ⇒ a single allocator gives the
/// "delivered in WAL order (per shard)" property by
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

    /// Advance the watermark to at least `floor`. Used by the WAL-
    /// stamped publish path so events that already carry a durable
    /// LSN keep the local allocator monotonic with respect to them.
    pub fn bump_to(&self, floor: u64) {
        let mut cur = self.0.load(Ordering::SeqCst);
        while floor > cur {
            match self
                .0
                .compare_exchange_weak(cur, floor, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
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
    /// Typed opaque-body payload — `None` for substrate events,
    /// `Some(_)` for the typed-graph event variants.
    pub graph_payload: Option<brain_protocol::GraphEventPayload>,
    /// Unified-edge change-feed payload — `Some(_)` when `event_type`
    /// is `EdgeAdded`, `EdgeRemoved` or `EdgeSuperseded`.
    pub edge_payload: Option<EdgeEventPayload>,
    /// Stage triple — `Some(_)` when `event_type == StageCompleted`.
    /// All three are populated together; the helper publishers in
    /// brain-workers fill all three on the same envelope. `None` on
    /// every non-stage event.
    pub stage_kind: Option<brain_protocol::StageKind>,
    pub stage_outcome: Option<brain_protocol::StageOutcome>,
    pub stage_payload: Option<brain_protocol::StagePayload>,
    /// Agent the event was attributed to. Substrate writers stamp
    /// their bound agent; typed-graph handlers stamp the auth-time
    /// agent the request ran under. Default (nil) for tests +
    /// events synthesized from WAL records that didn't capture an
    /// agent (none today — every WAL payload carries agent_id).
    pub agent_id: brain_core::AgentId,
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
            graph_payload: self.graph_payload.clone(),
            edge_payload: self.edge_payload.clone(),
            stage_kind: self.stage_kind,
            stage_outcome: self.stage_outcome,
            stage_payload: self.stage_payload.clone(),
        }
    }

    /// Project a durable WAL record back into zero-or-more in-memory
    /// event envelopes. Subscribe-replay calls this for every record
    /// in the `[from_lsn, current_tail)` range, applies the
    /// subscriber's filter, and writes any matches as
    /// `SUBSCRIBE_EVENT` frames.
    ///
    /// One WAL record can produce more than one envelope: an `Encode`
    /// with N attached edges emits one `Encoded` event plus N
    /// `EdgeAdded` events at the same LSN. Replay frames each as a
    /// separate `SUBSCRIBE_EVENT` so per-edge filters see them.
    ///
    /// Returns an empty `Vec` for records that don't surface as
    /// subscribe events (CheckpointBegin/End, TxnBegin/Commit/Abort,
    /// MigrateEmbedding, UpdateSalience, Reclaim, Consolidate). The
    /// caller skips those LSNs silently.
    #[must_use]
    pub fn from_wal_record(record: &brain_storage::wal::record::WalRecord) -> Vec<Self> {
        use brain_protocol::GraphEventPayload;
        use brain_storage::wal::payload::WalPayload;

        let lsn = record.lsn.raw();
        let timestamp_unix_nanos = record.timestamp_ns;
        let is_subscribe_event = record.flags
            & brain_storage::wal::record::FLAG_SUBSCRIBE_EVENT
            != 0;
        let Ok(payload) = record.typed_payload() else {
            return Vec::new();
        };
        match payload {
            WalPayload::Encode(p) => {
                let mut out = Vec::with_capacity(1 + p.edges.len());
                let agent_id = p.agent_id;
                let context_id = p.context_id;
                let kind = p.kind;
                out.push(Self {
                    lsn,
                    event_type: EventType::Encoded,
                    memory_id: p.memory_id,
                    context_id,
                    kind,
                    salience: p.salience_initial,
                    timestamp_unix_nanos,
                    text: Some(p.text),
                    graph_payload: None,
                    edge_payload: None,
                    stage_kind: None,
                    stage_outcome: None,
                    stage_payload: None,
                    agent_id,
                });
                for e in p.edges {
                    out.push(Self {
                        lsn,
                        event_type: EventType::EdgeAdded,
                        memory_id: MemoryId::NULL,
                        context_id,
                        kind: MemoryKind::Episodic,
                        salience: 0.0,
                        timestamp_unix_nanos,
                        text: None,
                        graph_payload: None,
                        edge_payload: Some(edge_payload_to_event(
                            e.source,
                            e.target,
                            e.kind,
                            e.weight,
                            None,
                            None,
                            brain_metadata::tables::edge::origin::EXPLICIT,
                        )),
                        stage_kind: None,
                        stage_outcome: None,
                        stage_payload: None,
                        agent_id,
                    });
                }
                out
            }
            WalPayload::Forget(p) => vec![Self {
                lsn,
                event_type: EventType::Forgotten,
                memory_id: p.memory_id,
                // Forget payload doesn't carry context/kind/salience.
                // Replay synthesises zero-fills; the substrate fields
                // are still useful (memory_id + event_type), and a
                // subscriber that needs richer metadata can resolve
                // via RECALL after observing the event.
                context_id: ContextId::default(),
                kind: MemoryKind::Episodic,
                salience: 0.0,
                timestamp_unix_nanos,
                text: None,
                graph_payload: None,
                edge_payload: None,
                // ForgetPayload doesn't carry agent_id today; replay
                // can't route through the per-agent allowlist for
                // forgets. Live forgets stamp it via `writer.agent_id`.
                stage_kind: None,
                stage_outcome: None,
                stage_payload: None,
                agent_id: brain_core::AgentId::default(),
            }],
            WalPayload::Link(p) => vec![Self {
                lsn,
                event_type: EventType::EdgeAdded,
                memory_id: MemoryId::NULL,
                context_id: ContextId::default(),
                kind: MemoryKind::Episodic,
                salience: 0.0,
                timestamp_unix_nanos,
                text: None,
                graph_payload: None,
                edge_payload: Some(edge_payload_to_event(
                    p.source,
                    p.target,
                    p.edge_kind,
                    p.weight,
                    None,
                    None,
                    brain_metadata::tables::edge::origin::EXPLICIT,
                )),
                // LinkPayload has no agent_id today; replay can't
                // route to a per-agent allowlist. Live writes stamp
                // via WalSink.
                stage_kind: None,
                stage_outcome: None,
                stage_payload: None,
                agent_id: brain_core::AgentId::default(),
            }],
            WalPayload::Unlink(p) => vec![Self {
                lsn,
                event_type: EventType::EdgeRemoved,
                memory_id: MemoryId::NULL,
                context_id: ContextId::default(),
                kind: MemoryKind::Episodic,
                salience: 0.0,
                timestamp_unix_nanos,
                text: None,
                graph_payload: None,
                edge_payload: Some(edge_payload_to_event(
                    p.source,
                    p.target,
                    p.edge_kind,
                    0.0,
                    None,
                    None,
                    brain_metadata::tables::edge::origin::EXPLICIT,
                )),
                stage_kind: None,
                stage_outcome: None,
                stage_payload: None,
                agent_id: brain_core::AgentId::default(),
            }],
            WalPayload::RelationLink(p) => vec![Self {
                lsn,
                event_type: EventType::EdgeAdded,
                memory_id: MemoryId::NULL,
                context_id: ContextId::default(),
                kind: MemoryKind::Episodic,
                salience: 0.0,
                timestamp_unix_nanos,
                text: None,
                graph_payload: None,
                edge_payload: Some(edge_payload_to_event(
                    p.from,
                    p.to,
                    brain_core::EdgeKindRef::Typed(p.relation_type_id),
                    1.0,
                    Some(p.relation_id),
                    None,
                    brain_metadata::tables::edge::origin::EXPLICIT,
                )),
                stage_kind: None,
                stage_outcome: None,
                stage_payload: None,
                agent_id: p.agent_id,
            }],
            WalPayload::RelationSupersede(p) => vec![Self {
                lsn,
                event_type: EventType::EdgeSuperseded,
                memory_id: MemoryId::NULL,
                context_id: ContextId::default(),
                kind: MemoryKind::Episodic,
                salience: 0.0,
                timestamp_unix_nanos,
                text: None,
                graph_payload: None,
                edge_payload: Some(edge_payload_to_event(
                    p.new.from,
                    p.new.to,
                    brain_core::EdgeKindRef::Typed(p.new.relation_type_id),
                    1.0,
                    Some(p.new.relation_id),
                    Some(p.old_relation_id),
                    brain_metadata::tables::edge::origin::EXPLICIT,
                )),
                stage_kind: None,
                stage_outcome: None,
                stage_payload: None,
                agent_id: p.new.agent_id,
            }],
            WalPayload::RelationTombstone(p) => vec![Self {
                lsn,
                event_type: EventType::EdgeRemoved,
                memory_id: MemoryId::NULL,
                context_id: ContextId::default(),
                kind: MemoryKind::Episodic,
                salience: 0.0,
                timestamp_unix_nanos,
                text: None,
                graph_payload: None,
                // Tombstone replay doesn't reconstruct (from, to)
                // endpoints — the WAL record only carries the
                // relation_id; the sidecar lookup needed to recover
                // the pair is out of scope for from_wal_record (no
                // metadata handle available here). Subscribers that
                // need the endpoints resolve via RECALL on the
                // relation_id.
                edge_payload: Some(EdgeEventPayload {
                    from_kind: 0,
                    from_id: [0u8; 16],
                    to_kind: 0,
                    to_id: [0u8; 16],
                    edge_kind_tag: 2,
                    edge_kind_byte: 0,
                    relation_type_id: None,
                    weight: 0.0,
                    relation_id: Some(p.relation_id.to_bytes()),
                    superseded_relation_id: None,
                    origin: brain_metadata::tables::edge::origin::EXPLICIT,
                }),
                stage_kind: None,
                stage_outcome: None,
                stage_payload: None,
                agent_id: p.agent_id,
            }],
            WalPayload::PhaseBody(body_record) => {
                // Only the subscribe-event records carry a CBOR
                // `GraphEventPayload` body; the durable write records share
                // these kinds but hold an rkyv row instead. Project only the
                // flagged change-feed records — the durable ones are
                // reconstructed by recovery, not surfaced as subscribe
                // events here. Pair with `wal_kind_for_event` /
                // `publish_graph` (which sets the flag).
                if !is_subscribe_event {
                    return Vec::new();
                }
                // Decode the CBOR body back into the typed-graph
                // event so subscribers see the same shape as a live
                // publish. Pair with `wal_kind_for_event` in
                // `crate::handlers::entity`.
                let Ok(payload) =
                    ciborium::from_reader::<GraphEventPayload, _>(&body_record.body[..])
                else {
                    return Vec::new();
                };
                let event_type = match &payload {
                    GraphEventPayload::EntityCreated(_) => EventType::EntityCreated,
                    GraphEventPayload::EntityUpdated(_) => EventType::EntityUpdated,
                    GraphEventPayload::EntityRenamed(_) => EventType::EntityRenamed,
                    GraphEventPayload::EntityMerged(_) => EventType::EntityMerged,
                    GraphEventPayload::EntityUnmerged(_) => EventType::EntityUnmerged,
                    GraphEventPayload::EntityTombstoned(_) => EventType::EntityTombstoned,
                    GraphEventPayload::StatementCreated(_) => EventType::StatementCreated,
                    GraphEventPayload::StatementSuperseded(_) => EventType::StatementSuperseded,
                    GraphEventPayload::StatementTombstoned(_) => EventType::StatementTombstoned,
                    GraphEventPayload::RelationCreated(_) => EventType::RelationCreated,
                    GraphEventPayload::RelationSuperseded(_) => EventType::RelationSuperseded,
                    GraphEventPayload::RelationTombstoned(_) => EventType::RelationTombstoned,
                    GraphEventPayload::SchemaUpdated(_) => EventType::SchemaUpdated,
                };
                vec![Self {
                    lsn,
                    event_type,
                    memory_id: MemoryId::NULL,
                    context_id: ContextId::default(),
                    kind: MemoryKind::Episodic,
                    salience: 0.0,
                    timestamp_unix_nanos,
                    text: None,
                    graph_payload: Some(payload),
                    edge_payload: None,
                    stage_kind: None,
                    stage_outcome: None,
                    stage_payload: None,
                    agent_id: brain_core::AgentId::default(),
                }]
            }
            // TXN brackets, checkpoints, salience updates, reclaims,
            // consolidations, embedding migrations, kind/context
            // updates are durable-only — no subscriber event today.
            _ => Vec::new(),
        }
    }
}

/// Project a [`brain_core::NodeRef`] + [`brain_core::EdgeKindRef`]
/// pair plus optional relation ids into a wire [`EdgeEventPayload`].
///
/// `origin` mirrors `EdgeData.origin` so subscribers can filter
/// explicit vs auto-derived edges (`brain_metadata::tables::edge::origin::*`).
pub(crate) fn edge_payload_to_event(
    from: brain_core::NodeRef,
    to: brain_core::NodeRef,
    kind: brain_core::EdgeKindRef,
    weight: f32,
    relation_id: Option<brain_core::RelationId>,
    superseded: Option<brain_core::RelationId>,
    origin: u8,
) -> EdgeEventPayload {
    let (edge_kind_tag, edge_kind_byte, relation_type_id) = match kind {
        brain_core::EdgeKindRef::Builtin(k) => (0u8, k as u8, None),
        brain_core::EdgeKindRef::Mentions => (1u8, 0u8, None),
        brain_core::EdgeKindRef::Typed(rt) => {
            let raw = rt.raw();
            // Stash the low byte in `edge_kind_byte` for cheap filter
            // checks; the full id lives in `relation_type_id`.
            #[allow(clippy::cast_possible_truncation)]
            let low = (raw & 0xFF) as u8;
            (2u8, low, Some(raw))
        }
    };
    EdgeEventPayload {
        from_kind: from.tag(),
        from_id: from.id_bytes(),
        to_kind: to.tag(),
        to_id: to.id_bytes(),
        edge_kind_tag,
        edge_kind_byte,
        relation_type_id,
        weight,
        relation_id: relation_id.map(|r| r.to_bytes()),
        superseded_relation_id: superseded.map(|r| r.to_bytes()),
        origin,
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
    /// a failure for us — events are dropped on the floor
    /// ("delivered at-least-once" applies only to *active*
    /// subscribers).
    pub fn publish(&self, mut env: EventEnvelope) -> u64 {
        env.lsn = self.lsn.next_lsn();
        let lsn = env.lsn;
        let _ = self.sender.send(env);
        lsn
    }

    /// Publish without minting an LSN — the envelope already carries
    /// one (typically assigned by [`crate::writer::WalSink`]).
    /// The internal allocator is still advanced so future bus-only
    /// publishes (workers that don't go through the WAL) stay
    /// monotonic.
    pub fn publish_prestamped(&self, env: EventEnvelope) {
        self.lsn.bump_to(env.lsn.saturating_add(1));
        let _ = self.sender.send(env);
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
    /// Subset of agent ids the subscriber wants events for. `None`
    /// = all agents (substrate-wide). On a shared shard this is
    /// the difference between "I see only my agent" and "I see
    /// every agent on this shard.".
    pub agents: Option<HashSet<brain_core::AgentId>>,
    /// Reserved slot. Wire `SubscriptionFilter` doesn't carry
    /// `min_salience` today lists it as desirable. Always
    /// `None` in v1.
    pub min_salience: Option<f32>,
}

impl ParsedFilter {
    #[must_use]
    pub fn matches(&self, env: &EventEnvelope) -> bool {
        if let Some(agents) = &self.agents {
            if !agents.contains(&env.agent_id) {
                return false;
            }
        }
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
/// [`ParsedFilter`]. Public so `brain-server`'s
/// connection-layer registry can reuse the same shape.
pub fn parse_filter(req: &SubscribeRequest) -> Result<ParsedFilter, OpError> {
    if req.filter.similar_to.is_some() {
        return Err(OpError::NotYetImplemented(
            "subscribe: similarity-based filtering (similar_to) is not yet supported",
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
    // An empty agent list is "no filter" (same as None) — the wire
    // encoding can't tell them apart cleanly, and an empty allowlist
    // would silently drop every event, which is rarely what a
    // subscriber means.
    let agents = req.filter.agents.as_ref().and_then(|v| {
        if v.is_empty() {
            None
        } else {
            Some(
                v.iter()
                    .copied()
                    .map(brain_core::AgentId::from)
                    .collect::<HashSet<_>>(),
            )
        }
    });
    Ok(ParsedFilter {
        contexts,
        kinds,
        agents,
        min_salience: None,
    })
}

// ---------------------------------------------------------------------------
// SubscriptionRegistry.
// ---------------------------------------------------------------------------

struct SubEntry {
    /// Cached filter — used by the long-lived pump task. The dispatcher
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

/// Tracks active subscriptions. The connection task calls
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
            // LsnTooOld until WAL replay is wired. We
            // surface it as `NotFound { what: "wal_segment", ... }`
            // which maps to the same wire `NotFound` family.
            return Err(OpError::NotFound {
                what: "wal_segment",
                detail: "subscribe: historical replay (from_lsn) is not yet \
                         supported. Omit from_lsn to subscribe to the live tail."
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

    /// Advance the recorded `final_lsn` for a stream. The pump
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

/// — one-shot dispatcher contract.
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
///    path").
///
/// The deadline race uses `glommio::timer::sleep` because this
/// function runs entirely inside the per-shard Glommio executor.
/// Using `tokio::time` here panicked at the first SUBSCRIBE_REQ in
/// production. The non-Linux stub returns `NotYetImplemented` —
/// Brain is Linux-only at runtime.
#[cfg(target_os = "linux")]
pub async fn handle_subscribe(
    req: SubscribeRequest,
    ctx: &OpsContext,
) -> Result<SubscriptionEvent, OpError> {
    use std::time::Instant;

    use futures_lite::FutureExt;

    let handle = ctx.subscriptions.register(&req)?;
    let stream_id = handle.target_stream_id;
    let filter = handle.filter.clone();
    let mut receiver = handle.receiver;

    let deadline = Instant::now() + ctx.subscribe_poll_window;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(OpError::Overloaded(
                "subscribe: no matching event within poll window — \
                 Phase 9 enables long-lived streaming"
                    .into(),
            ));
        }
        // Race recv against the remaining-deadline timer. `Some` =
        // event arrived; `None` = timer fired first.
        let recv_arm = async { Some(receiver.recv().await) };
        let timer_arm = async {
            glommio::timer::sleep(remaining).await;
            None
        };
        match recv_arm.or(timer_arm).await {
            Some(Ok(env)) => {
                if filter.matches(&env) {
                    ctx.subscriptions.update_final_lsn(stream_id, env.lsn);
                    return Ok(env.to_wire());
                }
                // Non-matching event — keep waiting.
                continue;
            }
            Some(Err(broadcast::error::RecvError::Lagged(_))) => {
                // Backpressure. `final_lsn` stays frozen at the
                // started_at_lsn; the registry entry survives so the
                // client can UNSUBSCRIBE and observe the freeze.
                return Err(OpError::Overloaded(
                    "subscribe: subscriber lagged — Phase 9's long-lived \
                     stream tolerates lag without dropping the subscription"
                        .into(),
                ));
            }
            Some(Err(broadcast::error::RecvError::Closed)) => {
                return Err(OpError::Internal("subscribe: event bus closed".into()));
            }
            None => {
                return Err(OpError::Overloaded(
                    "subscribe: no matching event within poll window — \
                     Phase 9 enables long-lived streaming"
                        .into(),
                ));
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn handle_subscribe(
    _req: SubscribeRequest,
    _ctx: &OpsContext,
) -> Result<SubscriptionEvent, OpError> {
    Err(OpError::NotYetImplemented(
        "subscribe requires Linux (Glommio timer)",
    ))
}

/// — drop the subscription, return final LSN.
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
