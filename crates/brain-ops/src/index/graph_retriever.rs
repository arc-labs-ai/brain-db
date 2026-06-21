//! Production `GraphRetriever` impl.
//!
//! The trait + value types live in `brain-index::graph_retriever`; the
//! impl that reads redb tables lives here so `brain-index` stays free
//! of the `brain-metadata` → `brain-storage` → `glommio` Linux-only
//! transitive dep.
//!
//! ## One walk to rule them all
//!
//! Before Wave 3a the retriever had two BFS modes: a memory-anchored
//! walk over the substrate edge tables and an entity-anchored walk
//! over the typed-relation tables. With the unified edge table
//! (`EDGES_TABLE` / `EDGES_REVERSE_TABLE` keyed by
//! `(NodeRef, EdgeKindRef, NodeRef, disambiguator)`), every edge —
//! substrate `Builtin`, future `Mentions`, and typed `Typed(rel_id)`
//! — lives in the same physical pair. The retriever no longer
//! dispatches on anchor kind: it BFS's over `NodeRef` and emits
//! whatever neighbours and relations the edges produce.
//!
//! ## Per-edge emission
//!
//! - `EdgeKindRef::Builtin(_)` — neighbour emitted (Memory or Entity
//!   per the neighbour's `NodeRef` variant). No relation id exists,
//!   so no `RankedItemId::Relation` is produced.
//! - `EdgeKindRef::Mentions` — same shape as Builtin; reserved tag
//!   that future mention edges will populate.
//! - `EdgeKindRef::Typed(rt)` — neighbour emitted **and** the
//!   `RelationId` (recovered from the edge-key disambiguator) emitted
//!   as a separate `RankedItemId::Relation` hit. Superseded
//!   (non-current) relations are dropped via the sidecar lookup so
//!   the walk reflects the live graph.
//!
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use brain_core::SubjectRef;
use brain_core::{
    EdgeKind, EdgeKindRef, EntityId, MemoryId, NodeRef, RelationId, RelationTypeId, StatementId,
};
use brain_index::{
    proximity_score, validate_graph_depth, Direction, GraphError, GraphQuery,
    GraphRetriever, GraphRetrieverConfig, RankedItem, RankedItemId,
};
use brain_metadata::statement::{statement_list, StatementListFilter, StatementOpError};
use brain_metadata::tables::edge::{walk_incoming, walk_outgoing, EdgeOpError, EdgeRow};
use brain_metadata::tables::entity::ENTITIES_TABLE;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::relation::RELATION_METADATA_TABLE;
use brain_metadata::MetadataDb;
use redb::ReadTransaction;

/// Production `GraphRetriever` impl. Cheap to clone — single
/// `Arc<MetadataDb>` field; redb MVCC handles read concurrency.
#[derive(Clone)]
pub struct BrainGraphRetriever {
    metadata: Arc<MetadataDb>,
}

impl BrainGraphRetriever {
    #[must_use]
    pub fn new(metadata: Arc<MetadataDb>) -> Self {
        Self { metadata }
    }
}

impl GraphRetriever for BrainGraphRetriever {
    fn retrieve(
        &self,
        query: &GraphQuery,
        config: &GraphRetrieverConfig,
    ) -> Result<Vec<RankedItem>, GraphError> {
        validate_graph_depth(query, config)?;

        let rtxn = self
            .metadata
            .read_txn()
            .map_err(|e| GraphError::IndexUnavailable(format!("read_txn: {e}")))?;

        // One always-on graph algorithm: the proximity-decay BFS walk below
        // (no flag, no PPR alternative). Its structural output is
        // cue-conditioned by the recall layer — each candidate is scaled by
        // its cosine to the query — so the entity-graph lane is always lit
        // without the subject-dump flood, while the walk keeps the
        // direction / relation-type-filter / closed-neighbourhood semantics
        // typed-graph QUERY relies on.
        match query {
            GraphQuery::Star {
                anchor,
                depth,
                direction,
                relation_types,
                include_statements,
            } => walk(
                &rtxn,
                NodeRef::from(*anchor),
                *depth,
                *direction,
                relation_types.as_deref(),
                *include_statements,
                config,
            ),
            GraphQuery::Path {
                from,
                to,
                max_depth,
            } => run_path(&rtxn, *from, *to, *max_depth, config),
            GraphQuery::Subgraph { anchor, depth } => walk(
                &rtxn,
                NodeRef::from(*anchor),
                *depth,
                Direction::Both,
                None,
                // Subgraph traditionally pulls evidence statements
                // for entity anchors; for memory anchors there are
                // no statement subjects to pivot from so the flag
                // is harmless.
                true,
                config,
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Unified BFS.
// ---------------------------------------------------------------------------

/// Per-hit accumulator carrying enough information to rank and
/// dedupe. We push raw entries and sort once at the end — sorting
/// per-frontier wastes work because later hops can only score lower
/// at equal weights.
#[derive(Clone, Copy, Debug)]
struct EmittedNode {
    id: RankedItemId,
    score: f32,
}

#[allow(clippy::too_many_arguments)]
fn walk(
    rtxn: &ReadTransaction,
    anchor: NodeRef,
    depth: u8,
    direction: Direction,
    relation_type_filter: Option<&[RelationTypeId]>,
    include_statements: bool,
    config: &GraphRetrieverConfig,
) -> Result<Vec<RankedItem>, GraphError> {
    // Anchor existence guard. Picks the matching `*AnchorNotFound`
    // variant so the router can tell entity vs memory misses apart.
    check_anchor_exists(rtxn, anchor)?;

    let mut visited: HashSet<NodeRef> = HashSet::new();
    // `(node, hop, edge_weight_into_node)`. Anchor enters with
    // weight 1.0 (unused — it's never emitted).
    let mut frontier: VecDeque<(NodeRef, u8, f32)> = VecDeque::new();
    frontier.push_back((anchor, 0, 1.0));

    let mut emitted: Vec<EmittedNode> = Vec::new();
    let mut emitted_relations: HashSet<RelationId> = HashSet::new();
    let mut emitted_statements: HashSet<StatementId> = HashSet::new();
    let max_branching = config.max_branching as usize;

    while let Some((node, d, weight_in)) = frontier.pop_front() {
        if !visited.insert(node) {
            continue;
        }

        if d > 0 {
            // Anchor itself is omitted from the result set. Score
            // folds the incoming edge weight in so heavier edges at
            // equal depth outrank lighter ones — preserves the
            // substrate retriever's pre-Wave-3a contract while
            // collapsing the entity-side score (weight = 1.0 for
            // typed relations) to plain `proximity_score(d)`.
            emitted.push(EmittedNode {
                id: node_to_id(node),
                score: proximity_score(d) * weight_in,
            });
        }

        // Statement pivot — only meaningful for entity nodes.
        if include_statements {
            if let NodeRef::Entity(e) = node {
                push_statements(
                    rtxn,
                    e,
                    d,
                    max_branching,
                    &mut emitted,
                    &mut emitted_statements,
                )?;
            }
        }

        if d >= depth {
            continue;
        }

        let neighbours = collect_neighbours(rtxn, node, direction)?;

        // Per-hop branching cap. Stable ordering by `(kind bytes, to
        // bytes, disambiguator)` is what `collect_neighbours` returns
        // already, so the truncation is reproducible across runs.
        let mut count = 0usize;
        for (kind, neighbour, disamb, data) in neighbours {
            // Temporal edges record session order, not relevance — they
            // are a filter signal, never candidate-producing. Skip them
            // (without consuming the per-hop branching budget) so the
            // graph lane can't propagate the query-independent
            // FollowedBy session chain into retrieval.
            if matches!(kind, EdgeKindRef::Builtin(EdgeKind::FollowedBy)) {
                continue;
            }
            if count >= max_branching {
                tracing::warn!(
                    target: "brain_ops::graph_retriever",
                    anchor = ?node,
                    cap = config.max_branching,
                    "graph walk branching truncated at per-hop cap",
                );
                break;
            }

            if !kind_matches_filter(kind, relation_type_filter) {
                continue;
            }

            // Typed edges go through the sidecar to drop superseded
            // relations and to emit the relation id as a separate hit.
            if let EdgeKindRef::Typed(_) = kind {
                let rel_id = RelationId::from(disamb);
                if !typed_edge_is_current(rtxn, rel_id)? {
                    continue;
                }
                if emitted_relations.insert(rel_id) {
                    // Relations sit at the same hop as the edge that
                    // introduced them — `d` (the parent's hop), not
                    // `d + 1`. Matches the pre-Wave-3a behaviour.
                    emitted.push(EmittedNode {
                        id: RankedItemId::Relation(rel_id),
                        score: proximity_score(d),
                    });
                }
            }

            count += 1;
            if !visited.contains(&neighbour) {
                frontier.push_back((neighbour, d + 1, data.weight));
            }
        }
    }

    Ok(rank_emitted(emitted, config.top_k))
}
fn collect_neighbours(
    rtxn: &ReadTransaction,
    node: NodeRef,
    direction: Direction,
) -> Result<Vec<EdgeRow>, GraphError> {
    let outgoing =
        || -> Result<_, GraphError> { walk_outgoing(rtxn, node, None).map_err(map_edge_err) };
    let incoming =
        || -> Result<_, GraphError> { walk_incoming(rtxn, node, None).map_err(map_edge_err) };

    let mut out = Vec::new();
    match direction {
        Direction::Outgoing => out.extend(outgoing()?),
        Direction::Incoming => out.extend(incoming()?),
        Direction::Both => {
            out.extend(outgoing()?);
            out.extend(incoming()?);
        }
    }

    // Both-direction walks can yield the same physical edge twice
    // when a symmetric `Builtin` kind has mirrored rows. Dedupe on
    // `(kind, neighbour, disambiguator)` so the per-hop branching
    // cap counts unique edges only.
    out.sort_by(|a, b| {
        a.0.to_bytes()
            .cmp(&b.0.to_bytes())
            .then_with(|| a.1.to_bytes().cmp(&b.1.to_bytes()))
            .then_with(|| a.2.cmp(&b.2))
    });
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);
    Ok(out)
}

fn kind_matches_filter(kind: EdgeKindRef, filter: Option<&[RelationTypeId]>) -> bool {
    match (kind, filter) {
        // `None` means "include every kind", so substrate and typed
        // edges both pass.
        (_, None) => true,
        // A non-empty allow-list restricts to typed edges with the
        // listed relation types. Substrate `Builtin` / `Mentions`
        // edges have no relation type, so they're excluded — the
        // filter is asking specifically about typed relations.
        (EdgeKindRef::Typed(rt), Some(types)) => types.contains(&rt),
        (EdgeKindRef::Builtin(_) | EdgeKindRef::Mentions, Some(_)) => false,
    }
}

fn typed_edge_is_current(rtxn: &ReadTransaction, rel_id: RelationId) -> Result<bool, GraphError> {
    let sidecar = rtxn
        .open_table(RELATION_METADATA_TABLE)
        .map_err(|e| GraphError::IndexUnavailable(format!("sidecar open: {e}")))?;
    let row = sidecar
        .get(&rel_id.to_bytes())
        .map_err(|e| GraphError::IndexUnavailable(format!("sidecar get: {e}")))?;
    Ok(row.map(|g| g.value().is_current()).unwrap_or(false))
}

fn check_anchor_exists(rtxn: &ReadTransaction, anchor: NodeRef) -> Result<(), GraphError> {
    match anchor {
        NodeRef::Memory(m) => check_memory_anchor(rtxn, m),
        NodeRef::Entity(e) => check_entity_anchor(rtxn, e),
    }
}

fn check_memory_anchor(rtxn: &ReadTransaction, anchor: MemoryId) -> Result<(), GraphError> {
    let memories = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| GraphError::IndexUnavailable(format!("open memories table: {e}")))?;
    let row = memories
        .get(&anchor.to_be_bytes())
        .map_err(|e| GraphError::IndexUnavailable(format!("memories.get: {e}")))?;
    let Some(row) = row else {
        return Err(GraphError::MemoryAnchorNotFound(anchor));
    };
    if row.value().is_tombstoned() {
        return Err(GraphError::MemoryAnchorNotFound(anchor));
    }
    Ok(())
}

fn check_entity_anchor(rtxn: &ReadTransaction, anchor: EntityId) -> Result<(), GraphError> {
    let entities = rtxn
        .open_table(ENTITIES_TABLE)
        .map_err(|e| GraphError::IndexUnavailable(format!("open entities table: {e}")))?;
    let row = entities
        .get(&anchor.to_bytes())
        .map_err(|e| GraphError::IndexUnavailable(format!("entities.get: {e}")))?;
    if row.is_none() {
        return Err(GraphError::AnchorNotFound(anchor));
    }
    Ok(())
}

fn node_to_id(n: NodeRef) -> RankedItemId {
    match n {
        NodeRef::Memory(m) => RankedItemId::Memory(m),
        NodeRef::Entity(e) => RankedItemId::Entity(e),
    }
}

// ---------------------------------------------------------------------------
// Path (entity-only in v1).
// ---------------------------------------------------------------------------

fn run_path(
    rtxn: &ReadTransaction,
    from: EntityId,
    to: EntityId,
    max_depth: u8,
    config: &GraphRetrieverConfig,
) -> Result<Vec<RankedItem>, GraphError> {
    check_entity_anchor(rtxn, from)?;
    check_entity_anchor(rtxn, to)?;

    // Single-source BFS from `from`. Tracks parent for each
    // discovered entity so we can reconstruct the path once `to`
    // is dequeued.
    let mut visited: HashSet<EntityId> = HashSet::new();
    visited.insert(from);
    let mut parents: HashMap<EntityId, (EntityId, RelationId)> = HashMap::new();
    let mut frontier: VecDeque<(EntityId, u8)> = VecDeque::new();
    frontier.push_back((from, 0));

    let mut found = false;
    while let Some((current, hop)) = frontier.pop_front() {
        if current == to {
            found = true;
            break;
        }
        if hop >= max_depth {
            continue;
        }

        let neighbours = collect_neighbours(rtxn, NodeRef::Entity(current), Direction::Both)?;
        let mut count = 0usize;
        let cap = config.max_branching as usize;
        for (kind, neighbour, disamb, _data) in neighbours {
            if count >= cap {
                break;
            }
            // Path is the typed graph; substrate edges
            // never form a relation-chain so they're not eligible.
            let EdgeKindRef::Typed(_) = kind else {
                continue;
            };
            let rel_id = RelationId::from(disamb);
            if !typed_edge_is_current(rtxn, rel_id)? {
                continue;
            }
            let NodeRef::Entity(neighbour) = neighbour else {
                continue;
            };
            if visited.insert(neighbour) {
                parents.insert(neighbour, (current, rel_id));
                frontier.push_back((neighbour, hop + 1));
            }
            count += 1;
        }
    }

    let mut entities: Vec<(EntityId, u8)> = Vec::new();
    let mut relations: Vec<(RelationId, u8)> = Vec::new();
    if found {
        // Walk back from `to` via `parents`. Both endpoints emit at
        // hop 0 (score 1.0); intermediates carry the distance from
        // the target node.
        entities.push((from, 0));
        entities.push((to, 0));
        let mut cursor = to;
        let mut depth_from_target: u8 = 1;
        while let Some((parent, rel_id)) = parents.get(&cursor) {
            relations.push((*rel_id, depth_from_target.saturating_sub(1)));
            if *parent != from {
                entities.push((*parent, depth_from_target));
            }
            cursor = *parent;
            depth_from_target = depth_from_target.saturating_add(1);
            if cursor == from {
                break;
            }
        }
    } else {
        // No path; emit both endpoints with the proximity-1 score so
        // the planner can still highlight the disconnected pair.
        entities.push((from, 1));
        entities.push((to, 1));
    }

    let mut items: Vec<EmittedNode> = Vec::with_capacity(entities.len() + relations.len());
    for (id, d) in entities {
        items.push(EmittedNode {
            id: RankedItemId::Entity(id),
            score: proximity_score(d),
        });
    }
    for (id, d) in relations {
        items.push(EmittedNode {
            id: RankedItemId::Relation(id),
            score: proximity_score(d),
        });
    }
    Ok(rank_emitted(items, config.top_k))
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn push_statements(
    rtxn: &ReadTransaction,
    subject: EntityId,
    hop: u8,
    cap: usize,
    out: &mut Vec<EmittedNode>,
    seen: &mut HashSet<StatementId>,
) -> Result<(), GraphError> {
    let filter = StatementListFilter {
        subject: Some(subject),
        predicate: None,
        kind: None,
        current_only: true,
        min_confidence: None,
        limit: cap,
    };
    let rows = statement_list(rtxn, &filter).map_err(map_statement_err)?;
    for s in rows {
        if !matches!(s.subject, SubjectRef::Entity(_)) {
            continue;
        }
        if !seen.insert(s.id) {
            continue;
        }
        out.push(EmittedNode {
            id: RankedItemId::Statement(s.id),
            score: proximity_score(hop),
        });
    }
    Ok(())
}

fn rank_emitted(mut items: Vec<EmittedNode>, top_k: usize) -> Vec<RankedItem> {
    // Score-descending; ties broken on the same id_sort_key the
    // pre-unification retriever used so RRF stays deterministic
    // across runs.
    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| id_sort_key(&a.id).cmp(&id_sort_key(&b.id)))
    });
    items.truncate(top_k);
    items
        .into_iter()
        .enumerate()
        .map(|(i, it)| RankedItem {
            id: it.id,
            rank: (i as u32) + 1,
            score: it.score,
            snippet: None,
        })
        .collect()
}

fn id_sort_key(id: &RankedItemId) -> [u8; 17] {
    let mut key = [0u8; 17];
    match id {
        RankedItemId::Memory(m) => {
            key[0] = 0;
            key[1..].copy_from_slice(&m.raw().to_be_bytes());
        }
        RankedItemId::Statement(s) => {
            key[0] = 1;
            key[1..].copy_from_slice(&s.to_bytes());
        }
        RankedItemId::Entity(e) => {
            key[0] = 2;
            key[1..].copy_from_slice(&e.to_bytes());
        }
        RankedItemId::Relation(r) => {
            key[0] = 3;
            key[1..].copy_from_slice(&r.to_bytes());
        }
    }
    key
}

fn map_edge_err(e: EdgeOpError) -> GraphError {
    GraphError::IndexUnavailable(format!("edges scan: {e}"))
}

fn map_statement_err(e: StatementOpError) -> GraphError {
    GraphError::IndexUnavailable(format!("statement_list: {e}"))
}

#[cfg(test)]
mod tests;
