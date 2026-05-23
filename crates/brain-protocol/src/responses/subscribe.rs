//! SUBSCRIBE / UNSUBSCRIBE responses + event frames.

use rkyv::{Archive, Deserialize, Serialize};

use super::types::{EventType, StageKind, StageOutcome, StagePayload};
use crate::responses::KnowledgeEventPayload;
use crate::request::{MemoryKindWire, WireContextId, WireMemoryId, WireUuid};

/// Push event for a subscription.
///
/// Body carries `knowledge_payload`, an optional typed sidecar with
/// typed-graph event data. For cognitive events (`Encoded`,
/// `Forgotten`, `Reclaimed`, `KindChanged`) the field is `None`. For
/// typed-graph events the cognitive fields (`memory_id`, `context_id`,
/// `kind`, `salience`, `text`) are zero-filled and `knowledge_payload`
/// carries the data.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SubscriptionEvent {
    pub event_type: EventType,
    pub memory_id: WireMemoryId,
    pub context_id: WireContextId,
    pub text: String,
    pub kind: MemoryKindWire,
    pub salience: f32,
    pub timestamp_unix_nanos: u64,
    pub lsn: u64,
    /// `None` for cognitive events; `Some(_)` for typed-graph events.
    pub knowledge_payload: Option<KnowledgeEventPayload>,
    /// `Some(_)` when `event_type` is `EdgeAdded`, `EdgeRemoved` or
    /// `EdgeSuperseded` — unified-edge change-feed events. LINK /
    /// UNLINK, typed-relation create / supersede / tombstone all
    /// surface here. `None` for every other event.
    pub edge_payload: Option<EdgeEventPayload>,
    /// `Some(_)` when `event_type == StageCompleted` — one background
    /// stage of a write's pipeline finished. The triple
    /// `(memory_id, stage_kind, outcome)` is the wait-helper's
    /// match-key; `payload` carries the per-stage detail. `None` for
    /// every other event.
    pub stage_kind: Option<StageKind>,
    pub stage_outcome: Option<StageOutcome>,
    pub stage_payload: Option<StagePayload>,
}

/// Side-channel payload carried on an `EdgeAdded` / `EdgeRemoved` /
/// `EdgeSuperseded` subscription event. The same shape covers
/// memory-graph edges and typed-graph relations — kind discriminator
/// and optional `relation_id` distinguish them.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EdgeEventPayload {
    /// `0` = Memory, `1` = Entity — matches the `NodeRef::tag()` byte.
    pub from_kind: u8,
    pub from_id: WireUuid,
    pub to_kind: u8,
    pub to_id: WireUuid,
    /// `0` = Builtin memory-graph kind, `1` = Mentions, `2` = Typed
    /// relation. Matches `EdgeKindRef` discriminator.
    pub edge_kind_tag: u8,
    /// Discriminator-specific payload byte:
    /// - `Builtin(EdgeKind)` → the memory-graph `EdgeKind` u8.
    /// - `Mentions` → 0.
    /// - `Typed(RelationTypeId)` → low byte; full id in
    ///   `relation_type_id`.
    pub edge_kind_byte: u8,
    /// `Some(_)` for typed-relation events (`Typed(RelationTypeId)`).
    /// `None` for memory-graph / mentions edges.
    pub relation_type_id: Option<u32>,
    /// Per-edge weight from `EdgeData`. Typed-relation rows write
    /// `1.0` (sidecar carries `confidence`).
    pub weight: f32,
    /// `Some(_)` for typed-relation events — the per-relation
    /// disambiguator id. `None` for memory-graph / mentions edges.
    pub relation_id: Option<WireUuid>,
    /// Only populated for `EdgeSuperseded` — the prior relation that
    /// got replaced.
    pub superseded_relation_id: Option<WireUuid>,
    /// Origin discriminator copied from
    /// `brain_metadata::tables::edge::origin::*`:
    /// `0` = `EXPLICIT` (LINK / RELATION_LINK / WAL replay of either),
    /// `1` = `AUTO_DERIVED` (worker-inferred, e.g. AutoEdgeWorker's
    /// `SimilarTo`).
    /// Agents driving on the change feed filter by this so they can
    /// distinguish edges they wrote from edges the server inferred.
    pub origin: u8,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnsubscribeResponse {
    pub target_stream_id: u32,
    pub final_lsn: u64,
}
