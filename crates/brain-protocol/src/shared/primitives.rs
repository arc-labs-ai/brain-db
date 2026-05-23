//! Foundational wire-domain enums shared by multiple request bodies.

// `PlanState` and `ObservationInput` use `By*` variant naming that mirrors
// the spec's discriminator phrasing — see request.rs for the historical note.
#![allow(clippy::enum_variant_names)]

use rkyv::{Archive, Deserialize, Serialize};

use crate::envelope::request::WireMemoryId;

/// — three durable kinds.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum MemoryKindWire {
    Episodic = 0,
    Semantic = 1,
    Consolidated = 2,
}

/// — eight built-in edge kinds.
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

/// — plan-strategy hint.
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

/// — plan endpoint specification. Variant names mirror the
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

/// — what to reason about.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum ObservationInput {
    ByMemoryId(WireMemoryId),
    ByText(String),
}

/// — soft tombstone vs. hard erase.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum ForgetMode {
    Soft = 0,
    Hard = 1,
}

/// — cancellation reason.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum CancellationReason {
    ClientUnneeded,
    Timeout,
    Other(String),
}

/// — admin stats verbosity.
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

/// — integrity-check scope.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum CheckScope {
    QuickSample,
    PerShard(Vec<u8>),
    Full,
}
