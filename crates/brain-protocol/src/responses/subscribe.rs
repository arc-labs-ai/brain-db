//! SUBSCRIBE / UNSUBSCRIBE responses + event frames.

use rkyv::{Archive, Deserialize, Serialize};

use super::types::EventType;
use crate::knowledge::KnowledgeEventPayload;
use crate::request::{MemoryKindWire, WireContextId, WireMemoryId};

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
}

/// Spec §08 §8.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnsubscribeResponse {
    pub target_stream_id: u32,
    pub final_lsn: u64,
}
