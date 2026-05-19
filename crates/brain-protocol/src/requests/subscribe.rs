//! SUBSCRIBE / UNSUBSCRIBE plus filter sub-structs.

use rkyv::{Archive, Deserialize, Serialize};

use super::types::MemoryKindWire;
use crate::request::{WireContextId, WireMemoryId, WireUuid};

/// Spec §07/7.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SubscribeRequest {
    pub filter: SubscriptionFilter,
    pub include_history: bool,
    pub from_lsn: Option<u64>,
    pub max_inflight: u32,
}

/// Spec §07/7.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SubscriptionFilter {
    pub contexts: Option<Vec<WireContextId>>,
    pub kinds: Option<Vec<MemoryKindWire>>,
    pub similar_to: Option<SimilarityFilter>,
    /// Subset of agent ids whose events the subscriber wants. `None`
    /// or empty = all agents (server-wide / shard-wide). The single
    /// most useful filter on a multi-tenant shard — without it, a
    /// subscriber sees every other agent's events that happen to
    /// route to the same shard. Server-side matching is a
    /// `HashSet::contains` per event.
    pub agents: Option<Vec<WireUuid>>,
}

/// Spec §07/7.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SimilarityFilter {
    pub reference_memory_id: WireMemoryId,
    pub threshold: f32,
}

/// Spec §07/8.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnsubscribeRequest {
    pub target_stream_id: u32,
}
