//! Edge types for the memory graph.
//!
//! See `spec/02_data_model/06_edges.md`.

use serde::{Deserialize, Serialize};

use crate::ids::MemoryId;

/// The eight built-in edge kinds. Per `spec/02_data_model/06_edges.md`.
///
/// Some kinds are inherently asymmetric (`Caused`, `FollowedBy`), some are
/// symmetric (`SimilarTo`, `Contradicts`). The substrate stores all edges
/// directionally; symmetric kinds are stored both ways.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum EdgeKind {
    /// `a` caused `b`.
    Caused,
    /// `a` happened before `b`.
    FollowedBy,
    /// `b` was derived from `a` (e.g. consolidated, summarised).
    DerivedFrom,
    /// Symmetric: `a` and `b` are similar.
    SimilarTo,
    /// Symmetric: `a` and `b` contradict.
    Contradicts,
    /// `a` provides evidence for `b`.
    Supports,
    /// `a` references `b` (citation, link).
    References,
    /// `a` is part of `b`.
    PartOf,
}

impl EdgeKind {
    /// Whether this edge kind is stored bidirectionally.
    #[must_use]
    pub const fn is_symmetric(self) -> bool {
        matches!(self, EdgeKind::SimilarTo | EdgeKind::Contradicts)
    }
}

/// An edge between two memories.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: MemoryId,
    pub to: MemoryId,
    pub kind: EdgeKind,
    pub created_at_unix_ms: u64,
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
}
