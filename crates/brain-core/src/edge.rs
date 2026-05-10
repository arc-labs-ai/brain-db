//! Edge types for the memory graph.
//!
//! See `spec/02_data_model/06_edges.md`.

use serde::{Deserialize, Serialize};

use crate::ids::MemoryId;

/// The eight built-in edge kinds (spec §02/06 §2).
///
/// Some kinds are inherently asymmetric (`Caused`, `FollowedBy`), some
/// are symmetric (`SimilarTo`, `Contradicts`). The substrate stores all
/// edges directionally; symmetric kinds are stored both ways.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum EdgeKind {
    /// `source` caused `target`.
    Caused = 0,
    /// `source` happened before `target`.
    FollowedBy = 1,
    /// `target` was derived from `source` (e.g. consolidated, summarised).
    DerivedFrom = 2,
    /// Symmetric: `source` and `target` are similar.
    SimilarTo = 3,
    /// Symmetric: `source` and `target` contradict.
    Contradicts = 4,
    /// `source` provides evidence for `target`.
    Supports = 5,
    /// `source` references `target` (citation, link).
    References = 6,
    /// `source` is part of `target`.
    PartOf = 7,
}

impl EdgeKind {
    /// Whether this edge kind is stored bidirectionally.
    #[must_use]
    pub const fn is_symmetric(self) -> bool {
        matches!(self, EdgeKind::SimilarTo | EdgeKind::Contradicts)
    }
}

/// Where an edge came from (spec §02/06 §1).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum EdgeOrigin {
    /// Explicitly created by an agent (e.g. supplied with `ENCODE_REQ.edges`).
    Explicit = 0,
    /// Auto-derived by the substrate (e.g. similarity edges added at encode time).
    AutoDerived = 1,
}

/// A directed edge between two memories (spec §02/06 §1).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    /// Edge weight in `[0.0, 1.0]` (spec §02/06 §1.2).
    pub weight: f32,
    pub origin: EdgeOrigin,
    pub created_at_unix_nanos: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_edges_are_marked_correctly() {
        assert!(EdgeKind::SimilarTo.is_symmetric());
        assert!(EdgeKind::Contradicts.is_symmetric());
        assert!(!EdgeKind::Caused.is_symmetric());
        assert!(!EdgeKind::FollowedBy.is_symmetric());
    }

    #[test]
    fn edge_constructs_with_required_fields() {
        let a = MemoryId::pack(1, 1, 1);
        let b = MemoryId::pack(1, 2, 1);
        let edge = Edge {
            source: a,
            target: b,
            kind: EdgeKind::Caused,
            weight: 0.75,
            origin: EdgeOrigin::Explicit,
            created_at_unix_nanos: 1_700_000_000_000_000_000,
        };
        assert_eq!(edge.source, a);
        assert_eq!(edge.target, b);
        assert!((edge.weight - 0.75).abs() < f32::EPSILON);
        assert_eq!(edge.origin, EdgeOrigin::Explicit);
    }
}
