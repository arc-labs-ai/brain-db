//! SUBSCRIBE / UNSUBSCRIBE responses + event frames.

use rkyv::{Archive, Deserialize, Serialize};

use super::types::EventType;
use crate::request::{MemoryKindWire, WireContextId, WireMemoryId};

/// Spec §08 §7 — push event for a subscription.
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
}

/// Spec §08 §8.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnsubscribeResponse {
    pub target_stream_id: u32,
    pub final_lsn: u64,
}
