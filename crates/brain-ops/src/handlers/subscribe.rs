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
//!   first matching event (bounded poll, default 5s), then returns.
//!   This single-event, bounded-poll shape is **not** what real
//!   client connections experience: `brain-server`'s connection
//!   layer (`SubscriptionRegistry` / `run_subscription_task`) calls
//!   [`SubscriptionRegistry::register`] directly and bypasses this
//!   dispatcher entirely, framing a genuine long-lived event stream
//!   out of the returned receiver, with WAL-tail replay-then-live
//!   cutover and proper `UNSUBSCRIBE`/cancel handling — that is the
//!   real, live-today implementation. This handler exists only to
//!   satisfy the shared `brain-ops` dispatch surface and shares its
//!   filter parsing (`ParsedFilter`) with the real path; it is not
//!   itself reachable from a real client connection. See
//!   `spec/05_operations/05_subscribe.md` §21 for the full picture.
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

use brain_core::{MemoryId, MemoryKind, SessionId};
use brain_embed::VECTOR_DIM;
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

/// Upper bound on the entry count of any one subscription filter list
/// (`session_filter`, `kinds`, `spaces`). Otherwise bounded only by the 16 MiB
/// payload cap; an explicit cap rejects a crafted oversized filter with
/// a clear `InvalidRequest` instead of building a large `HashSet`. The
/// bound is generous — far above any legitimate subscription scope.
pub const MAX_SUBSCRIBE_FILTER_ENTRIES: usize = 1024;

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
    pub session_id: SessionId,
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
    /// Space the event was attributed to. Substrate writers stamp
    /// their bound space; typed-graph handlers stamp the auth-time
    /// space the request ran under. Default (nil) for tests +
    /// events synthesized from WAL records that didn't capture an
    /// space (none today — every WAL payload carries space_id).
    pub space_id: brain_core::SpaceId,
    /// Embedding vector of the memory this event is about — `Some`
    /// only on memory/encode events (the writer has the freshly-
    /// embedded vector at publish time), `None` for forget / graph /
    /// edge / stage events. Carried so a similarity subscription
    /// (`SubscriptionFilter.similar_to`) can be evaluated network-side
    /// without a per-event shard lookup. `Arc` keeps the broadcast
    /// clone cheap (one refcount bump per receiver, not a 1536-byte
    /// copy).
    pub vector: Option<Arc<[f32; VECTOR_DIM]>>,
}

impl EventEnvelope {
    /// Convert to the wire [`SubscriptionEvent`].
    #[must_use]
    pub fn to_wire(&self) -> SubscriptionEvent {
        SubscriptionEvent {
            event_type: self.event_type,
            memory_id: self.memory_id.into(),
            session_id: self.session_id.into(),
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
        let is_subscribe_event =
            record.flags & brain_storage::wal::record::FLAG_SUBSCRIBE_EVENT != 0;
        let Ok(payload) = record.typed_payload() else {
            return Vec::new();
        };
        match payload {
            WalPayload::Encode(p) => {
                let mut out = Vec::with_capacity(1 + p.edges.len());
                let space_id = p.space_id;
                let session_id = p.session_id;
                let kind = p.kind;
                out.push(Self {
                    lsn,
                    event_type: EventType::Encoded,
                    memory_id: p.memory_id,
                    session_id,
                    kind,
                    salience: p.salience_initial,
                    timestamp_unix_nanos,
                    text: Some(p.text),
                    graph_payload: None,
                    edge_payload: None,
                    stage_kind: None,
                    stage_outcome: None,
                    stage_payload: None,
                    space_id,
                    vector: None,
                });
                for e in p.edges {
                    out.push(Self {
                        lsn,
                        event_type: EventType::EdgeAdded,
                        memory_id: MemoryId::NULL,
                        session_id,
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
                        space_id,
                        vector: None,
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
                session_id: SessionId::default(),
                kind: MemoryKind::Episodic,
                salience: 0.0,
                timestamp_unix_nanos,
                text: None,
                graph_payload: None,
                edge_payload: None,
                // ForgetPayload doesn't carry space_id today; replay
                // can't route through the per-space allowlist for
                // forgets. Live forgets stamp it via `writer.space_id`.
                stage_kind: None,
                stage_outcome: None,
                stage_payload: None,
                space_id: brain_core::SpaceId::default(),
                vector: None,
            }],
            WalPayload::Link(p) => vec![Self {
                lsn,
                event_type: EventType::EdgeAdded,
                memory_id: MemoryId::NULL,
                session_id: SessionId::default(),
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
                // LinkPayload has no space_id today; replay can't
                // route to a per-space allowlist. Live writes stamp
                // via WalSink.
                stage_kind: None,
                stage_outcome: None,
                stage_payload: None,
                space_id: brain_core::SpaceId::default(),
                vector: None,
            }],
            WalPayload::Unlink(p) => vec![Self {
                lsn,
                event_type: EventType::EdgeRemoved,
                memory_id: MemoryId::NULL,
                session_id: SessionId::default(),
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
                space_id: brain_core::SpaceId::default(),
                vector: None,
            }],
            WalPayload::RelationLink(p) => vec![Self {
                lsn,
                event_type: EventType::EdgeAdded,
                memory_id: MemoryId::NULL,
                // The relation-link record now carries the per-utterance
                // session; deliver the event scoped to it (Stage 2 filled 0).
                session_id: p.session_id,
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
                space_id: p.space_id,
                vector: None,
            }],
            WalPayload::RelationSupersede(p) => vec![Self {
                lsn,
                event_type: EventType::EdgeSuperseded,
                memory_id: MemoryId::NULL,
                // The new relation row carries the per-utterance session.
                session_id: p.new.session_id,
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
                space_id: p.new.space_id,
                vector: None,
            }],
            WalPayload::RelationTombstone(p) => vec![Self {
                lsn,
                event_type: EventType::EdgeRemoved,
                memory_id: MemoryId::NULL,
                session_id: SessionId::default(),
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
                space_id: p.space_id,
                vector: None,
            }],
            WalPayload::PhaseBody(body_record) => {
                // Only the subscribe-event records carry a CBOR
                // `GraphEventPayload` / `StageCompletedEventBody` body; the
                // durable write records share these kinds but hold an rkyv
                // row instead. Project only the flagged change-feed
                // records — the durable ones (where they exist) are
                // reconstructed by recovery, not surfaced as subscribe
                // events here. Pair with `wal_kind_for_event` /
                // `OpsContext::publish_notification` (which sets the flag).
                if !is_subscribe_event {
                    return Vec::new();
                }
                // `StageCompleted` has no durable write-record counterpart
                // (unlike the typed-graph kinds below) — the flagged
                // notification record decoded here is the sole durable
                // trace of the event. Its body shape differs from
                // `GraphEventPayload` (it carries `memory_id` directly), so
                // it gets its own decode arm ahead of the generic one.
                if body_record.kind == brain_storage::wal::kinds::WalRecordKind::StageCompleted {
                    let Ok(stage_body) = ciborium::from_reader::<
                        brain_protocol::StageCompletedEventBody,
                        _,
                    >(&body_record.body[..]) else {
                        return Vec::new();
                    };
                    return vec![Self {
                        lsn,
                        event_type: EventType::StageCompleted,
                        memory_id: MemoryId::from(stage_body.memory_id),
                        session_id: SessionId::default(),
                        kind: MemoryKind::Episodic,
                        salience: 0.0,
                        timestamp_unix_nanos,
                        text: None,
                        graph_payload: None,
                        edge_payload: None,
                        stage_kind: Some(stage_body.stage_kind),
                        stage_outcome: Some(stage_body.stage_outcome),
                        stage_payload: Some(stage_body.stage_payload),
                        // Real space, unlike the typed-graph arm below —
                        // the notification record carries it in the same
                        // 16-byte prefix `publish_notification` writes, and
                        // `PhaseBodyRecord::space_id` is already populated
                        // from that prefix by `WalPayload::decode`.
                        space_id: body_record.space_id,
                        vector: None,
                    }];
                }
                // Decode the CBOR body back into the typed-graph
                // event so subscribers see the same shape as a live
                // publish. Pair with `wal_kind_for_event` in
                // `crate::handlers::entity` and `OpsContext::publish_notification`.
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
                    session_id: SessionId::default(),
                    kind: MemoryKind::Episodic,
                    salience: 0.0,
                    timestamp_unix_nanos,
                    text: None,
                    graph_payload: Some(payload),
                    edge_payload: None,
                    stage_kind: None,
                    stage_outcome: None,
                    stage_payload: None,
                    space_id: brain_core::SpaceId::default(),
                    vector: None,
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
    pub session_filter: Option<HashSet<SessionId>>,
    pub kinds: Option<HashSet<MemoryKind>>,
    /// Subset of space ids the subscriber wants events for. `None`
    /// = all spaces (substrate-wide). On a shared shard this is
    /// the difference between "I see only my space" and "I see
    /// every space on this shard.".
    pub spaces: Option<HashSet<brain_core::SpaceId>>,
    /// Subset of memory ids the subscriber wants events for. `None`
    /// = all memories. Lets a client scope a subscription to a
    /// single in-flight write (e.g. to watch that write's async
    /// derivation stages complete) without seeing unrelated traffic
    /// on a busy shard.
    pub memory_ids: Option<HashSet<MemoryId>>,
    /// Reserved slot. Wire `SubscriptionFilter` doesn't carry
    /// `min_salience` today lists it as desirable. Always
    /// `None` in v1.
    pub min_salience: Option<f32>,
    /// Similarity gate. `Some` only after the connection layer has
    /// resolved the reference memory's vector (once, at registration
    /// time — see [`ParsedFilter::set_similarity_reference`]). When set,
    /// only events whose envelope carries a vector cosine-similar to the
    /// reference at or above the threshold match; vector-less events
    /// (forget / graph / edge / stage) never match. [`parse_filter`]
    /// validates the wire threshold but leaves this `None`; the
    /// reference vector is injected shard-side because the network-layer
    /// registry has no direct access to shard vectors.
    pub similar_to: Option<SimilarityMatch>,
}

/// Resolved similarity filter: the reference memory's embedding vector
/// plus the cosine threshold. Cheap to `Copy` (a fixed array + one f32),
/// so per-event matching is a single dot-product with no allocation.
#[derive(Clone, Copy, Debug)]
pub struct SimilarityMatch {
    pub reference: [f32; VECTOR_DIM],
    pub threshold: f32,
}

impl ParsedFilter {
    /// Inject the resolved reference vector for a similarity
    /// subscription. Called by the connection layer after it has fetched
    /// the reference memory's vector from the owning shard (once, at
    /// registration). `threshold` was already validated by
    /// [`parse_filter`].
    pub fn set_similarity_reference(&mut self, reference: [f32; VECTOR_DIM], threshold: f32) {
        self.similar_to = Some(SimilarityMatch {
            reference,
            threshold,
        });
    }

    #[must_use]
    pub fn matches(&self, env: &EventEnvelope) -> bool {
        if let Some(spaces) = &self.spaces {
            if !spaces.contains(&env.space_id) {
                return false;
            }
        }
        if let Some(sessions) = &self.session_filter {
            // Typed-graph events are stamped with the unscoped default
            // session (`SessionId(0)`) until per-event session tagging
            // lands. Deliver those to any session-filtered subscriber
            // rather than dropping them, so a filter never silently
            // swallows graph events.
            if env.session_id != SessionId(0) && !sessions.contains(&env.session_id) {
                return false;
            }
        }
        if let Some(ks) = &self.kinds {
            if !ks.contains(&env.kind) {
                return false;
            }
        }
        if let Some(ids) = &self.memory_ids {
            if !ids.contains(&env.memory_id) {
                return false;
            }
        }
        if let Some(t) = self.min_salience {
            if env.salience < t {
                return false;
            }
        }
        if let Some(sim) = &self.similar_to {
            // Similarity is about memory content: an event without a
            // vector (forget / graph / edge / stage) can never satisfy a
            // similarity subscription, so it drops rather than passing
            // the gate vacuously.
            match &env.vector {
                Some(v)
                    if crate::grounded::cosine(v.as_slice(), sim.reference.as_slice())
                        >= sim.threshold => {}
                _ => return false,
            }
        }
        true
    }
}

/// Parse the wire `SubscribeRequest` into a registry-side
/// [`ParsedFilter`]. Public so `brain-server`'s
/// connection-layer registry can reuse the same shape.
pub fn parse_filter(req: &SubscribeRequest) -> Result<ParsedFilter, OpError> {
    // Validate the similarity threshold up front. The reference *vector*
    // is resolved later, shard-side (the network registry has no direct
    // vector access), so `similar_to` is left `None` here and injected by
    // `ParsedFilter::set_similarity_reference` after that round-trip.
    if let Some(sim) = req.filter.similar_to {
        if !sim.threshold.is_finite() || !(-1.0..=1.0).contains(&sim.threshold) {
            return Err(OpError::InvalidRequest(format!(
                "subscribe: filter.similar_to.threshold must be a finite cosine in [-1.0, 1.0], got {}",
                sim.threshold
            )));
        }
    }
    if let Some(ref v) = req.filter.session_filter {
        if v.len() > MAX_SUBSCRIBE_FILTER_ENTRIES {
            return Err(OpError::InvalidRequest(format!(
                "subscribe: filter.session_filter must have <= {MAX_SUBSCRIBE_FILTER_ENTRIES} entries"
            )));
        }
    }
    if let Some(ref v) = req.filter.kinds {
        if v.len() > MAX_SUBSCRIBE_FILTER_ENTRIES {
            return Err(OpError::InvalidRequest(format!(
                "subscribe: filter.kinds must have <= {MAX_SUBSCRIBE_FILTER_ENTRIES} entries"
            )));
        }
    }
    if let Some(ref v) = req.filter.spaces {
        if v.len() > MAX_SUBSCRIBE_FILTER_ENTRIES {
            return Err(OpError::InvalidRequest(format!(
                "subscribe: filter.spaces must have <= {MAX_SUBSCRIBE_FILTER_ENTRIES} entries"
            )));
        }
    }
    if let Some(ref v) = req.filter.memory_ids {
        if v.len() > MAX_SUBSCRIBE_FILTER_ENTRIES {
            return Err(OpError::InvalidRequest(format!(
                "subscribe: filter.memory_ids must have <= {MAX_SUBSCRIBE_FILTER_ENTRIES} entries"
            )));
        }
    }
    let session_filter = req
        .filter
        .session_filter
        .as_ref()
        .map(|v| v.iter().copied().map(SessionId).collect::<HashSet<_>>());
    let kinds = req.filter.kinds.as_ref().map(|v| {
        v.iter()
            .copied()
            .map(MemoryKind::from)
            .collect::<HashSet<_>>()
    });
    // An empty space list is "no filter" (same as None) — the wire
    // encoding can't tell them apart cleanly, and an empty allowlist
    // would silently drop every event, which is rarely what a
    // subscriber means.
    let spaces = req.filter.spaces.as_ref().and_then(|v| {
        if v.is_empty() {
            None
        } else {
            Some(
                v.iter()
                    .copied()
                    .map(brain_core::SpaceId::from)
                    .collect::<HashSet<_>>(),
            )
        }
    });
    // An empty memory_ids list is "no filter" (same as None), mirroring
    // the `spaces` handling above — an empty allowlist would silently
    // drop every event.
    let memory_ids = req.filter.memory_ids.as_ref().and_then(|v| {
        if v.is_empty() {
            None
        } else {
            Some(
                v.iter()
                    .copied()
                    .map(MemoryId::from)
                    .collect::<HashSet<_>>(),
            )
        }
    });
    Ok(ParsedFilter {
        session_filter,
        kinds,
        spaces,
        memory_ids,
        min_salience: None,
        // Resolved by the connection layer after the one-time
        // reference-vector round-trip; see `set_similarity_reference`.
        similar_to: None,
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
        if req.from_lsn.is_some() || req.include_history {
            // This one-shot poller only tails live events; it has no
            // WAL-replay machinery (that lives in the connection-layer
            // path, see the module doc §21). Both `from_lsn` (resume)
            // and `include_history` (replay retained history) ask for
            // history, so reject them here rather than silently
            // ignoring the flag and returning live-only events. We
            // surface it as `NotFound { what: "wal_segment", ... }`
            // which maps to the same wire `NotFound` family.
            return Err(OpError::NotFound {
                what: "wal_segment",
                detail: "subscribe: historical replay (from_lsn / include_history) is not yet \
                         supported on this path. Omit both to subscribe to the live tail."
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

#[cfg(test)]
mod similarity_tests {
    use super::*;
    use brain_protocol::envelope::request::SubscribeRequest;
    use brain_protocol::ops::subscribe::{SimilarityFilter, SubscriptionFilter};

    fn envelope_with_vector(vector: Option<[f32; VECTOR_DIM]>) -> EventEnvelope {
        EventEnvelope {
            lsn: 1,
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
            space_id: brain_core::SpaceId::default(),
            vector: vector.map(Arc::new),
        }
    }

    fn unit(index: usize) -> [f32; VECTOR_DIM] {
        let mut v = [0.0_f32; VECTOR_DIM];
        v[index] = 1.0;
        v
    }

    fn filter_with_similarity(reference: [f32; VECTOR_DIM], threshold: f32) -> ParsedFilter {
        let mut f = ParsedFilter::default();
        f.set_similarity_reference(reference, threshold);
        f
    }

    #[test]
    fn matches_when_cosine_at_or_above_threshold() {
        let filter = filter_with_similarity(unit(0), 0.9);
        // Identical vector → cosine 1.0 ≥ 0.9.
        assert!(filter.matches(&envelope_with_vector(Some(unit(0)))));
    }

    #[test]
    fn drops_when_cosine_below_threshold() {
        let filter = filter_with_similarity(unit(0), 0.5);
        // Orthogonal vector → cosine 0.0 < 0.5.
        assert!(!filter.matches(&envelope_with_vector(Some(unit(1)))));
    }

    #[test]
    fn drops_vector_less_events() {
        let filter = filter_with_similarity(unit(0), -1.0);
        // Even at the most permissive threshold, a vector-less event
        // never satisfies a similarity subscription.
        assert!(!filter.matches(&envelope_with_vector(None)));
    }

    fn request_with_threshold(threshold: f32) -> SubscribeRequest {
        SubscribeRequest {
            filter: SubscriptionFilter {
                session_filter: None,
                kinds: None,
                similar_to: Some(SimilarityFilter {
                    reference_memory_id: MemoryId::from(1u128).into(),
                    threshold,
                }),
                spaces: None,
                memory_ids: None,
            },
            include_history: false,
            from_lsn: None,
            max_inflight: 0,
            act_as: None,
        }
    }

    #[test]
    fn parse_filter_rejects_nan_threshold() {
        let err = parse_filter(&request_with_threshold(f32::NAN));
        assert!(matches!(err, Err(OpError::InvalidRequest(_))));
    }

    #[test]
    fn parse_filter_rejects_out_of_range_threshold() {
        assert!(matches!(
            parse_filter(&request_with_threshold(1.5)),
            Err(OpError::InvalidRequest(_))
        ));
        assert!(matches!(
            parse_filter(&request_with_threshold(-2.0)),
            Err(OpError::InvalidRequest(_))
        ));
    }

    #[test]
    fn parse_filter_accepts_valid_threshold_but_leaves_reference_unresolved() {
        let parsed = parse_filter(&request_with_threshold(0.5)).expect("valid threshold");
        // The vector is resolved shard-side, so parse_filter alone
        // leaves the gate inert.
        assert!(parsed.similar_to.is_none());
    }
}
