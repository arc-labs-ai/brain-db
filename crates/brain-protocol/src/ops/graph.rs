//! `GRAPH_FETCH` — paginated export of the caller's typed graph.
//!
//! A read-only projection over the typed-graph state a `(namespace, agent)`
//! already owns: entities, the relations and entity-object facts that link
//! them, and — behind opt-in layers — value-object statements and the
//! memories that mention entities. It is the "see my whole memory as a
//! graph" read: no cue, no ranking, no relevance suppression.
//!
//! Pagination spines on the subject-anchored statement index (the one
//! typed-graph index that is `(namespace, agent)`-prefixed), so the entity
//! set is *derived from traversal* rather than a dedicated entity index.
//! Because an entity can be reached on more than one page, the response
//! contract is **completeness, not disjointness**: every node and edge
//! appears in at least one page, may repeat across pages, and carries a
//! stable id so the client accumulates them into a set. This is the
//! standard graph-render pattern and is what lets the cursor stay a single
//! keyset position instead of a growing "seen" set.

use super::memory::ActAs;

// ============================================================
// GRAPH_FETCH — full-agent graph export
// ============================================================

/// Kind of a graph node. The id width is uniform (16 bytes) across kinds;
/// this byte is what disambiguates which id-space it belongs to.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum GraphNodeKindWire {
    /// A canonical entity. `label` is its canonical name, `type_qname` its
    /// declared type.
    Entity = 0,
    /// A reified value-object statement (an attribute/preference/event whose
    /// object is a literal, not another entity). Only emitted when the
    /// request sets `include_statements`. `label` is `predicate: value`.
    Statement = 1,
    /// A source memory that mentions an entity. Only emitted when the
    /// request sets `include_memories`. `label` is a text snippet.
    Memory = 2,
}

/// Kind of a graph edge.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum GraphEdgeKindWire {
    /// Entity→entity link from the relations table (a `Relation`-kind fact).
    /// `label` is the relation-type's canonical name.
    Relation = 0,
    /// Entity→entity link derived from a statement whose object is an
    /// entity (a `Fact`/`Event`/`Preference`-kind statement). `label` is the
    /// predicate's canonical name.
    Fact = 1,
    /// Entity→statement link from a mentioned entity to a value-object
    /// statement node. Only present with `include_statements`. `label` is
    /// the predicate.
    HasStatement = 2,
    /// Memory→entity provenance link. Only present with `include_memories`.
    /// `label` is empty.
    Mentions = 3,
}

/// One graph node. `id` is the 16-byte entity / statement / memory id; the
/// `kind` byte says which id-space it is.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GraphNode {
    #[serde(with = "serde_bytes")]
    pub id: [u8; 16],
    /// 0 = Entity, 1 = Statement, 2 = Memory (see [`GraphNodeKindWire`]).
    pub kind: u8,
    /// Human-facing label: entity canonical name, `predicate: value` for a
    /// statement, or a memory text snippet.
    pub label: String,
    /// Entity type qname (e.g. `brain:Person`); empty for non-entity nodes.
    pub type_qname: String,
}

/// One graph edge between two nodes, identified by their 16-byte ids.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GraphEdge {
    #[serde(with = "serde_bytes")]
    pub from_id: [u8; 16],
    #[serde(with = "serde_bytes")]
    pub to_id: [u8; 16],
    /// 0 = Relation, 1 = Fact, 2 = HasStatement, 3 = Mentions (see
    /// [`GraphEdgeKindWire`]).
    pub kind: u8,
    /// Predicate / relation-type label; empty for `Mentions`.
    pub label: String,
}

/// `GRAPH_FETCH` (`0x0163`) — a paginated export of the caller's
/// `(namespace, agent)` typed graph.
///
/// The default layer is the *concept map*: entity nodes plus the
/// `Relation` and `Fact` edges that link them. `include_statements` adds
/// value-object statement nodes (attributes / preferences with literal
/// objects) and their `HasStatement` edges; `include_memories` adds source
/// memory nodes and their `Mentions` edges.
///
/// The cursor is opaque and signed: it encodes the active layer toggles and
/// the last statement-index key seen. Echoing a cursor back after changing a
/// toggle is rejected (`stale_cursor`), because the resumed page would
/// otherwise belong to a differently-shaped export.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GraphFetchRequest {
    /// Page size (nodes are unbounded per statement, but the statement spine
    /// is capped): validated server-side to `1..=500`.
    pub limit: u32,
    /// Empty on the first page; otherwise the opaque `next_cursor` from a
    /// previous response.
    pub cursor: Vec<u8>,
    /// Emit value-object statement nodes + their `HasStatement` edges.
    pub include_statements: bool,
    /// Emit source memory nodes + their `Mentions` edges.
    pub include_memories: bool,
    /// Include tombstoned statements/relations in the export. Default false.
    pub include_tombstoned: bool,
    /// Effective identity this export runs as, on behalf of the
    /// authenticated connection principal. `None` (the common case, omitted
    /// on the wire) runs as the connection's own key-bound identity. The
    /// export is scoped to the effective `(namespace, agent)`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

/// Response body for `GRAPH_FETCH` (`0x01E3`). One frame carries a page of
/// nodes + edges. Empty `next_cursor` means the export is exhausted; a
/// non-empty `next_cursor` is the opaque token to resume from. Nodes/edges
/// may repeat across pages (completeness, not disjointness) — dedup by id.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GraphFetchResponseFrame {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Empty when exhausted; otherwise the keyset token to resume with.
    pub next_cursor: Vec<u8>,
    pub is_final: bool,
}

impl GraphFetchResponseFrame {
    /// True for the final tail frame. Mirrors the body-side `is_final`
    /// signal used by the other streaming list responses.
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.is_final
    }
}
