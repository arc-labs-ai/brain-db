//! Foundational wire-domain enums shared by multiple request bodies.

// `PlanState` and `ObservationInput` use `By*` variant naming that mirrors
// the spec's discriminator phrasing — see request.rs for the historical note.
#![allow(clippy::enum_variant_names)]

use rkyv::{Archive, Deserialize, Serialize};

use crate::request::WireMemoryId;

/// Spec §02/02 — three durable kinds.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum MemoryKindWire {
    Episodic = 0,
    Semantic = 1,
    Consolidated = 2,
}

/// Spec §02/06 — eight built-in edge kinds.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum EdgeKindWire {
    Caused = 0,
    FollowedBy = 1,
    DerivedFrom = 2,
    SimilarTo = 3,
    Contradicts = 4,
    Supports = 5,
    References = 6,
    PartOf = 7,
}

/// Client-side recall strategy selector.
///
/// Hybrid retrieval (semantic + lexical + graph + RRF fusion) is
/// the default for every deployment. Schemas no longer gate
/// retrieval; they only constrain typed knowledge ops. This
/// selector is the client-side escape hatch.
///
/// - `Auto` — server default. Today: hybrid unless inside a txn,
///   in which case it falls back to substrate so read-your-writes
///   stays consistent with the buffered ops.
/// - `SubstrateOnly` — force the raw substrate vector path. Used
///   by benchmarks measuring HNSW-only latency and by clients
///   that explicitly want no lexical/graph contribution.
/// - `HybridOnly` — force hybrid; if a required retriever slot
///   (semantic, lexical) is missing on this shard, the server
///   returns `HybridUnavailable` instead of silently falling back.
///   Lets callers fail loud rather than receive a degraded answer.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Default, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum RecallStrategy {
    #[default]
    Auto = 0,
    SubstrateOnly = 1,
    HybridOnly = 2,
}

/// Spec §07/4 — plan-strategy hint.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum PlanStrategy {
    Auto = 0,
    AStar = 1,
    Mcts = 2,
    AttractorRollout = 3,
}

/// Spec §07/4 — plan endpoint specification. Variant names mirror the
/// spec's `ByMemoryId` / `ByText` / `ByVector` discriminator naming.
/// (See the crate-level `#![allow(clippy::enum_variant_names)]` for why
/// the per-item allow isn't enough.)
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum PlanState {
    ByMemoryId(WireMemoryId),
    ByText(String),
    ByVector { offset: u32, dim: u16 },
}

/// Spec §07/5 — what to reason about.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum ObservationInput {
    ByMemoryId(WireMemoryId),
    ByText(String),
}

/// Spec §07/6 — soft tombstone vs. hard erase.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum ForgetMode {
    Soft = 0,
    Hard = 1,
}

/// Spec §07/12 — cancellation reason.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum CancellationReason {
    ClientUnneeded,
    Timeout,
    Other(String),
}

/// Spec §07/16 — admin stats verbosity.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum StatsDetail {
    Summary = 0,
    PerShard = 1,
    PerContext = 2,
    Full = 3,
}

/// Spec §07/19 — integrity-check scope.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum CheckScope {
    QuickSample,
    PerShard(Vec<u8>),
    Full,
}
