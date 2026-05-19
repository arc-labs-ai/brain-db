//! SUBSCRIBE / UNSUBSCRIBE responses + event frames.

use rkyv::{Archive, Deserialize, Serialize};

use super::types::EventType;
use crate::knowledge::KnowledgeEventPayload;
use crate::request::{MemoryKindWire, WireContextId, WireMemoryId, WireUuid};

/// Spec §03/08 §7 — push event for a subscription.
///
/// Phase 16.7 extended the body with `knowledge_payload`, an optional
/// typed sidecar carrying knowledge-layer event data. For substrate
/// events (`Encoded`, `Forgotten`, `Reclaimed`, `KindChanged`) the
/// field is `None`. For knowledge-layer events the substrate fields
/// (`memory_id`, `context_id`, `kind`, `salience`, `text`) are
/// zero-filled and `knowledge_payload` carries the data.
///
/// Wire-level extension is forward-compatible: pre-16.7 SDK builds
/// that don't decode `knowledge_payload` silently drop knowledge
/// events (or surface them as opaque `event_type` codes). Made under
/// the pre-v1.0 compatibility policy
/// (`spec/03_wire_protocol/12_versioning.md` §0).
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
    /// `None` for substrate events; `Some(_)` for knowledge events
    /// (see `spec/28_knowledge_wire_protocol/02_subscribe_events.md`).
    pub knowledge_payload: Option<KnowledgeEventPayload>,
    /// `Some(_)` when `event_type` is `EdgeAdded`, `EdgeRemoved` or
    /// `EdgeSuperseded` — Phase C unified-edge change-feed events.
    /// Substrate LINK / UNLINK, typed-relation create / supersede /
    /// tombstone all surface here. `None` for every other event.
    pub edge_payload: Option<EdgeEventPayload>,
}

/// Side-channel payload carried on an `EdgeAdded` / `EdgeRemoved` /
/// `EdgeSuperseded` subscription event. The same shape covers
/// substrate edges and typed knowledge relations — kind discriminator
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
    /// `0` = Builtin substrate kind, `1` = Mentions, `2` = Typed
    /// relation. Matches `EdgeKindRef` discriminator.
    pub edge_kind_tag: u8,
    /// Discriminator-specific payload byte:
    /// - `Builtin(EdgeKind)` → the substrate `EdgeKind` u8.
    /// - `Mentions` → 0.
    /// - `Typed(RelationTypeId)` → low byte; full id in
    ///   `relation_type_id`.
    pub edge_kind_byte: u8,
    /// `Some(_)` for typed-relation events (`Typed(RelationTypeId)`).
    /// `None` for substrate / mentions edges.
    pub relation_type_id: Option<u32>,
    /// Per-edge weight from `EdgeData`. Typed-relation rows write
    /// `1.0` (sidecar carries `confidence`).
    pub weight: f32,
    /// `Some(_)` for typed-relation events — the per-relation
    /// disambiguator id. `None` for substrate / mentions edges.
    pub relation_id: Option<WireUuid>,
    /// Only populated for `EdgeSuperseded` — the prior relation that
    /// got replaced.
    pub superseded_relation_id: Option<WireUuid>,
}

/// Spec §08 §8.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnsubscribeResponse {
    pub target_stream_id: u32,
    pub final_lsn: u64,
}
