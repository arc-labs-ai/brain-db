//! Graph retriever trait + value types.
//!
//! The production impl (`BrainGraphRetriever`) lives in
//! `brain-ops::ops::graph_retriever` for the same reason as
//! `SemanticRetriever`: it needs `brain-metadata` (which
//! transitively pulls Linux-only `glommio`), and we keep
//! `brain-index` native-buildable on macOS.

use brain_core::{EntityId, MemoryId, NodeRef, RelationTypeId};

/// Default top-k.
pub const DEFAULT_TOP_K: usize = 64;

/// Default per-hop depth.
pub const DEFAULT_DEPTH: u8 = 3;

/// Hard cap on traversal depth.
pub const MAX_DEPTH_HARD_CAP: u8 = 5;

/// Default per-node child cap.
pub const DEFAULT_MAX_BRANCHING: u32 = 200;

/// Default per-query timeout.
pub const DEFAULT_TIMEOUT_MS: u32 = 50;

/// The graph-retrieval trait. Object-safe; consumers hold an
/// `Arc<dyn GraphRetriever>`.
pub trait GraphRetriever: Send + Sync {
    fn retrieve(
        &self,
        query: &GraphQuery,
        config: &GraphRetrieverConfig,
    ) -> Result<Vec<crate::RankedItem>, GraphError>;
}

/// Dual-mode anchor for graph traversal.
///
/// The graph retriever walks two different physical tables
/// depending on which node it's anchored at:
///
/// - [`GraphAnchor::Entity`] — the typed graph
///   (relations, predicates). Requires a declared schema for
///   useful results; on schemaless deployments this anchor will
///   simply find no entities.
/// - [`GraphAnchor::Memory`] — the substrate memory graph
///   (edges_out / edges_in). Works on every deployment; lights
///   up the hybrid path's graph contribution even without a
///   schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphAnchor {
    Memory(MemoryId),
    Entity(EntityId),
}

impl From<GraphAnchor> for NodeRef {
    fn from(a: GraphAnchor) -> Self {
        match a {
            GraphAnchor::Memory(m) => NodeRef::Memory(m),
            GraphAnchor::Entity(e) => NodeRef::Entity(e),
        }
    }
}

/// Per-query traversal spec.
#[derive(Debug, Clone)]
pub enum GraphQuery {
    /// BFS from `anchor` outward up to `depth`. Anchor may be
    /// either a memory or an entity.
    Star {
        anchor: GraphAnchor,
        depth: u8,
        direction: Direction,
        relation_types: Option<Vec<RelationTypeId>>,
        include_statements: bool,
    },
    /// Find paths from `from` to `to` up to `max_depth`.
    Path {
        from: EntityId,
        to: EntityId,
        max_depth: u8,
    },
    /// Closed k-hop neighbourhood of `anchor`.
    Subgraph { anchor: GraphAnchor, depth: u8 },
}

impl GraphQuery {
    /// Effective depth — accessor used by the planner cost
    /// estimate and the hard-cap check.
    #[must_use]
    pub fn depth(&self) -> u8 {
        match self {
            Self::Star { depth, .. } | Self::Subgraph { depth, .. } => *depth,
            Self::Path { max_depth, .. } => *max_depth,
        }
    }

    /// Anchor (or `from` for `Path`) — used by the router to
    /// resolve `merged_into` redirects on entity anchors. `Path`
    /// is entity-only in v1 so wraps its `from` as
    /// `GraphAnchor::Entity`.
    #[must_use]
    pub fn anchor(&self) -> GraphAnchor {
        match self {
            Self::Star { anchor, .. } | Self::Subgraph { anchor, .. } => *anchor,
            Self::Path { from, .. } => GraphAnchor::Entity(*from),
        }
    }
}

/// Direction of relation traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow `from_entity → to_entity` edges only.
    Outgoing,
    /// Follow `to_entity → from_entity` edges only.
    Incoming,
    /// Both directions (symmetric edges + heterogeneous).
    Both,
}

/// Search config.
#[derive(Debug, Clone, Copy)]
pub struct GraphRetrieverConfig {
    pub top_k: usize,
    pub max_depth: u8,
    pub max_branching: u32,
    pub timeout_ms: u32,
}

impl Default for GraphRetrieverConfig {
    fn default() -> Self {
        Self {
            top_k: DEFAULT_TOP_K,
            max_depth: DEFAULT_DEPTH,
            max_branching: DEFAULT_MAX_BRANCHING,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// Error taxonomy.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("anchor entity not found: {0:?}")]
    AnchorNotFound(EntityId),
    /// Memory-anchored graph walk was asked to start from a
    /// `MemoryId` that doesn't exist (or has been tombstoned).
    /// Distinct from `AnchorNotFound` so callers — particularly
    /// the auto router that materialises anchors from semantic
    /// top-K — can tell entity vs memory misses apart.
    #[error("anchor memory not found: {0:?}")]
    MemoryAnchorNotFound(MemoryId),
    #[error("max depth {got} exceeds hard cap {MAX_DEPTH_HARD_CAP}")]
    MaxDepthExceeded { got: u8 },
    #[error("index unavailable: {0}")]
    IndexUnavailable(String),
    #[error("query timed out after {0} ms")]
    Timeout(u32),
    #[error("internal: {0}")]
    Internal(String),
}

/// Proximity score: `1 / (hop_distance + 1)`.
#[must_use]
pub fn proximity_score(hop_distance: u8) -> f32 {
    1.0 / (f32::from(hop_distance) + 1.0)
}

/// Validate depth caps. Returns early
/// `MaxDepthExceeded` for any of the three modes.
pub fn validate_depth(query: &GraphQuery, config: &GraphRetrieverConfig) -> Result<(), GraphError> {
    if query.depth() > MAX_DEPTH_HARD_CAP {
        return Err(GraphError::MaxDepthExceeded { got: query.depth() });
    }
    if config.max_depth > MAX_DEPTH_HARD_CAP {
        return Err(GraphError::MaxDepthExceeded {
            got: config.max_depth,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proximity_score_decays_with_distance() {
        assert!((proximity_score(0) - 1.0).abs() < 1e-6);
        assert!((proximity_score(1) - 0.5).abs() < 1e-6);
        assert!((proximity_score(2) - 1.0 / 3.0).abs() < 1e-6);
        assert!((proximity_score(4) - 0.2).abs() < 1e-6);
    }

    #[test]
    fn validate_rejects_depth_above_cap() {
        let q = GraphQuery::Star {
            anchor: GraphAnchor::Entity(EntityId::new()),
            depth: 6,
            direction: Direction::Outgoing,
            relation_types: None,
            include_statements: false,
        };
        let err = validate_depth(&q, &GraphRetrieverConfig::default()).expect_err("rejects");
        assert!(matches!(err, GraphError::MaxDepthExceeded { got: 6 }));
    }

    #[test]
    fn validate_accepts_at_cap() {
        let q = GraphQuery::Star {
            anchor: GraphAnchor::Entity(EntityId::new()),
            depth: 5,
            direction: Direction::Outgoing,
            relation_types: None,
            include_statements: false,
        };
        validate_depth(&q, &GraphRetrieverConfig::default()).expect("at-cap is ok");
    }

    #[test]
    fn anchor_accessor_returns_from_for_path() {
        let from = EntityId::new();
        let to = EntityId::new();
        let q = GraphQuery::Path {
            from,
            to,
            max_depth: 3,
        };
        assert_eq!(q.anchor(), GraphAnchor::Entity(from));
        assert_eq!(q.depth(), 3);
    }
}
