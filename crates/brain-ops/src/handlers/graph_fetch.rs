//! `GRAPH_FETCH` handler — paginated export of the caller's typed graph.
//!
//! Not RECALL: no cue, no ranking, no relevance suppression. It walks the
//! caller's `(namespace, space)` typed-graph state and returns a page of
//! nodes + edges plus an opaque keyset cursor.
//!
//! ## Pagination spine
//!
//! The one typed-graph index that is `(namespace, space)`-prefixed is
//! `STATEMENTS_BY_SUBJECT_TABLE`, so that is the pagination spine: each page
//! ranges it forward from the cursor and takes up to `limit` statements. The
//! entity set is *derived from traversal* — subjects and entity-objects of
//! the statements on the page, plus the relation/mention neighbours of those
//! entities — rather than a dedicated entity index. A purely-relational
//! entity that is never a statement subject/object surfaces only if a
//! neighbouring statemented entity pulls it in; a fully-isolated entity does
//! not surface at all (it has nothing to render).
//!
//! ## Completeness, not disjointness
//!
//! An entity can be reached on more than one page, so its relation/mention
//! edges may be emitted on more than one page. Every node and edge carries a
//! stable id; the client accumulates them into a set and dedups by id. The
//! server guarantees each node/edge appears in *at least one* page, not that
//! pages are disjoint — this is what lets the cursor stay a single keyset
//! position instead of a growing "seen" set.
//!
//! ## Memory edges
//!
//! `include_memory_edges` adds the stored memory↔memory links (`SimilarTo`,
//! `FollowedBy`, …). Those hang off *memories*, which sit two hops off the
//! statement spine: page statements → subject/object entities → mentioning
//! memories. So the flag requires `include_memories` — that is the layer
//! that puts memory nodes on the page at all — and is rejected without it.
//!
//! An edge may point at a memory that mentions no entity on this page. Rather
//! than drop it (a lost edge violates completeness) or emit it dangling (the
//! client cannot render an unknown endpoint), the far memory is emitted as a
//! node too. That is exactly the completeness-not-disjointness contract: the
//! node set widens, the client dedups by id. The walk is one hop — far
//! memories are endpoints, not new seeds — so the page stays bounded.

use std::collections::HashSet;

use brain_core::{
    EdgeKind, EdgeKindRef, EntityId, MemoryId, NodeRef, StatementId, StatementObject,
    StatementValue, SubjectRef,
};
use brain_metadata::entity::ops::entity_get;
use brain_metadata::relation::types::relation_type_get;
use brain_metadata::schema::predicate::predicate_get;
use brain_metadata::statement::statement_get;
use brain_metadata::tables::edge::{walk_incoming, walk_outgoing};
use brain_metadata::tables::entity_type::ENTITY_TYPES_TABLE;
use brain_metadata::tables::statement::STATEMENTS_BY_SUBJECT_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_metadata::RowScope;
use brain_protocol::{
    GraphEdge, GraphEdgeKindWire, GraphFetchRequest, GraphFetchResponseFrame, GraphNode,
};

use crate::context::OpsContext;
use crate::error::OpError;

/// Cursor wire-format version. A mismatch means the client echoed a cursor
/// minted by an incompatible server build → treat as stale.
const CURSOR_VERSION: u8 = 1;

/// Serialized `STATEMENTS_BY_SUBJECT` key: `ns(4) + space(16) + subject(16)
/// + kind(1) + predicate(4) + is_current(1) + statement_id(16)`.
const STMT_KEY_LEN: usize = 4 + 16 + 16 + 1 + 4 + 1 + 16;

/// `[version(1)][flags(1)][stmt_key(58)]`.
const CURSOR_LEN: usize = 1 + 1 + STMT_KEY_LEN;

/// Max statements consumed off the spine per page. The wire caps `limit` at
/// this too; nodes/edges per page are bounded by it via the traversal.
const MAX_LIMIT: u32 = 500;

/// Per-entity cap on relation + mention edges walked in one page, so a single
/// hub entity (thousands of relations) can't explode a page. A hub whose
/// edges exceed this is truncated on the page it's reached; the rest surface
/// on later pages that re-reach it (completeness-not-disjointness).
const MAX_EDGES_PER_ENTITY: usize = 512;

/// Per-memory cap on builtin memory↔memory edges walked in one page — the
/// memory-side counterpart of [`MAX_EDGES_PER_ENTITY`], and deliberately the
/// same number so one hub memory can't outweigh a hub entity. Truncation is
/// safe for the same reason: a later page that re-reaches the memory emits
/// the rest.
const MAX_EDGES_PER_MEMORY: usize = MAX_EDGES_PER_ENTITY;

/// Node kind bytes (mirror `GraphNodeKindWire`).
const NODE_ENTITY: u8 = 0;
const NODE_STATEMENT: u8 = 1;
const NODE_MEMORY: u8 = 2;

/// Edge kind bytes (mirror `GraphEdgeKindWire`). The memory↔memory kinds are
/// derived from `EdgeKind` via `GraphEdgeKindWire::from`, not spelled here.
const EDGE_RELATION: u8 = 0;
const EDGE_FACT: u8 = 1;
const EDGE_HAS_STATEMENT: u8 = 2;
const EDGE_MENTIONS: u8 = 3;

const FLAG_STATEMENTS: u8 = 0b0001;
const FLAG_MEMORIES: u8 = 0b0010;
const FLAG_TOMBSTONED: u8 = 0b0100;
const FLAG_MEMORY_EDGES: u8 = 0b1000;

type StmtKey = (u32, [u8; 16], [u8; 16], u8, u32, u8, [u8; 16]);

pub fn handle_graph_fetch(
    req: GraphFetchRequest,
    ctx: &OpsContext,
) -> Result<GraphFetchResponseFrame, OpError> {
    if req.limit == 0 || req.limit > MAX_LIMIT {
        return Err(OpError::InvalidRequest(format!(
            "limit must be in 1..={MAX_LIMIT}"
        )));
    }
    // Memory edges hang off memory nodes. Without the layer that emits those
    // nodes there is nothing for the edges to attach to, so reject rather
    // than silently returning edges the client cannot place.
    if req.include_memory_edges && !req.include_memories {
        return Err(OpError::InvalidRequest(
            "include_memory_edges requires include_memories".into(),
        ));
    }

    let flags = req_flags(&req);
    let after_key: Option<StmtKey> = if req.cursor.is_empty() {
        None
    } else {
        Some(decode_cursor(&req.cursor, flags)?)
    };

    let scope = RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    let by_subject = rtxn
        .open_table(STATEMENTS_BY_SUBJECT_TABLE)
        .map_err(|e| OpError::Internal(format!("open statements_by_subject: {e}")))?;

    let ns = scope.namespace_id;
    let ag = scope.space_id_bytes;
    // Whole-space range over the subject-anchored index.
    let lo_key: StmtKey = (ns, ag, [0u8; 16], 0, 0, 0, [0u8; 16]);
    let hi_key: StmtKey = (
        ns,
        ag,
        [0xffu8; 16],
        u8::MAX,
        u32::MAX,
        u8::MAX,
        [0xffu8; 16],
    );
    let lower = match &after_key {
        // Resume strictly after the last key returned.
        Some(k) => std::ops::Bound::Excluded(*k),
        None => std::ops::Bound::Included(lo_key),
    };
    let range = by_subject
        .range((lower, std::ops::Bound::Included(hi_key)))
        .map_err(|e| OpError::Internal(format!("statement range: {e}")))?;

    let mut builder = GraphBuilder::new(
        req.include_statements,
        req.include_memories,
        req.include_memory_edges,
    );
    let mut consumed = 0u32;
    let mut last_key: Option<StmtKey> = None;
    let mut has_more = false;

    for entry in range {
        if consumed == req.limit {
            has_more = true;
            break;
        }
        let (k, v) = entry.map_err(|e| OpError::Internal(format!("statement row: {e}")))?;
        let key = k.value();
        let (_ns, _ag, _subj, _kind, _pred, is_current, _sid_bytes) = key;
        // Only current (non-superseded) statements form the live graph.
        if is_current == 0 {
            continue;
        }
        let sid = StatementId::from_bytes(v.value());
        let Some(stmt) = statement_get(&rtxn, sid)
            .map_err(|e| OpError::Internal(format!("statement_get: {e}")))?
        else {
            continue;
        };
        if stmt.tombstoned && !req.include_tombstoned {
            continue;
        }
        last_key = Some(key);
        consumed += 1;

        // Subject must be a concrete entity to place it on the graph.
        let SubjectRef::Entity(subject_id) = stmt.subject else {
            continue;
        };
        builder.emit_entity(&rtxn, subject_id)?;

        let predicate = predicate_get(&rtxn, stmt.predicate)
            .ok()
            .flatten()
            .map(|p| p.canonical())
            .unwrap_or_default();

        match &stmt.object {
            StatementObject::Entity(object_id) => {
                builder.emit_entity(&rtxn, *object_id)?;
                builder.emit_edge(
                    subject_id.to_bytes(),
                    object_id.to_bytes(),
                    EDGE_FACT,
                    predicate,
                );
            }
            StatementObject::Value(value) => {
                if builder.include_statements {
                    let label = format!("{predicate}: {}", render_value(value));
                    builder.emit_node(GraphNode {
                        id: sid.to_bytes(),
                        kind: NODE_STATEMENT,
                        label,
                        type_qname: String::new(),
                    });
                    builder.emit_edge(
                        subject_id.to_bytes(),
                        sid.to_bytes(),
                        EDGE_HAS_STATEMENT,
                        predicate,
                    );
                }
            }
            // Statement/Memory-referencing objects are provenance plumbing,
            // not entity-graph nodes; skip in the export.
            StatementObject::Statement(_) | StatementObject::Memory(_) => {}
        }
    }

    // Relation + mention edges for every entity discovered on this page.
    let page_entities: Vec<EntityId> = builder.entities_this_page.clone();
    for eid in page_entities {
        builder.emit_relations(&rtxn, eid)?;
        if builder.include_memories {
            builder.emit_mentions(&rtxn, eid)?;
        }
    }

    // Builtin memory↔memory edges for every memory the mention walk put on
    // the page. One hop only: `emit_memory_edges` may add far-endpoint memory
    // nodes, and those are not themselves walked.
    if builder.include_memory_edges {
        let page_memories: Vec<MemoryId> = builder.memories_this_page.clone();
        for mid in page_memories {
            builder.emit_memory_edges(&rtxn, mid)?;
        }
    }

    let next_cursor = if has_more {
        match last_key {
            Some(k) => encode_cursor(flags, &k),
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    Ok(GraphFetchResponseFrame {
        nodes: builder.nodes,
        edges: builder.edges,
        next_cursor,
        is_final: true,
    })
}

/// Accumulates the page's nodes + edges with per-page dedup so a node/edge
/// reached twice within one page is emitted once.
struct GraphBuilder {
    include_statements: bool,
    include_memories: bool,
    include_memory_edges: bool,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    seen_nodes: HashSet<[u8; 16]>,
    seen_edges: HashSet<([u8; 16], [u8; 16], u8)>,
    /// Entities emitted from the statement spine — the seeds for the
    /// relation/mention walk.
    entities_this_page: Vec<EntityId>,
    seen_entity_seeds: HashSet<[u8; 16]>,
    /// Memories emitted by the mention walk — the seeds for the memory-edge
    /// walk. Far endpoints pulled in *by* that walk are deliberately not
    /// added here, so the traversal stays one hop.
    memories_this_page: Vec<MemoryId>,
    seen_memory_seeds: HashSet<[u8; 16]>,
}

impl GraphBuilder {
    fn new(include_statements: bool, include_memories: bool, include_memory_edges: bool) -> Self {
        Self {
            include_statements,
            include_memories,
            include_memory_edges,
            nodes: Vec::new(),
            edges: Vec::new(),
            seen_nodes: HashSet::new(),
            seen_edges: HashSet::new(),
            entities_this_page: Vec::new(),
            seen_entity_seeds: HashSet::new(),
            memories_this_page: Vec::new(),
            seen_memory_seeds: HashSet::new(),
        }
    }

    fn emit_node(&mut self, node: GraphNode) {
        if self.seen_nodes.insert(node.id) {
            self.nodes.push(node);
        }
    }

    fn emit_edge(&mut self, from: [u8; 16], to: [u8; 16], kind: u8, label: String) {
        if self.seen_edges.insert((from, to, kind)) {
            self.edges.push(GraphEdge {
                from_id: from,
                to_id: to,
                kind,
                label,
            });
        }
    }

    /// Emit an entity node (deduped) and record it as a walk seed.
    fn emit_entity(&mut self, rtxn: &redb::ReadTransaction, eid: EntityId) -> Result<(), OpError> {
        if self.seen_entity_seeds.insert(eid.to_bytes()) {
            self.entities_this_page.push(eid);
        }
        if self.seen_nodes.contains(&eid.to_bytes()) {
            return Ok(());
        }
        let Some(ent) =
            entity_get(rtxn, eid).map_err(|e| OpError::Internal(format!("entity_get: {e}")))?
        else {
            return Ok(());
        };
        let type_qname = rtxn
            .open_table(ENTITY_TYPES_TABLE)
            .ok()
            .and_then(|t| t.get(&ent.entity_type.raw()).ok().flatten())
            .map(|g| g.value().name)
            .unwrap_or_default();
        self.emit_node(GraphNode {
            id: eid.to_bytes(),
            kind: NODE_ENTITY,
            label: ent.canonical_name,
            type_qname,
        });
        Ok(())
    }

    /// Walk the typed relations incident to `eid` (both directions), emitting
    /// a `Relation` edge and both endpoint entity nodes for each, deduped on
    /// the edge identity `(from, type, to)`. Bounded by
    /// [`MAX_EDGES_PER_ENTITY`].
    fn emit_relations(
        &mut self,
        rtxn: &redb::ReadTransaction,
        eid: EntityId,
    ) -> Result<(), OpError> {
        let mut walked = 0usize;
        for outgoing in [true, false] {
            let rows = if outgoing {
                walk_outgoing(rtxn, NodeRef::Entity(eid), None)
            } else {
                walk_incoming(rtxn, NodeRef::Entity(eid), None)
            }
            .map_err(|e| OpError::Internal(format!("walk relation: {e}")))?;
            for (kind, other, _disamb, _data) in rows {
                if walked >= MAX_EDGES_PER_ENTITY {
                    return Ok(());
                }
                let EdgeKindRef::Typed(rt_id) = kind else {
                    continue;
                };
                let NodeRef::Entity(other_id) = other else {
                    continue;
                };
                let Some(rt) = relation_type_get(rtxn, rt_id)
                    .map_err(|e| OpError::Internal(format!("relation_type_get: {e}")))?
                else {
                    continue;
                };
                let (from_id, to_id) = if outgoing {
                    (eid, other_id)
                } else {
                    (other_id, eid)
                };
                walked += 1;
                // The far endpoint may be an entity we haven't emitted yet.
                self.emit_entity(rtxn, other_id)?;
                self.emit_edge(
                    from_id.to_bytes(),
                    to_id.to_bytes(),
                    EDGE_RELATION,
                    rt.canonical(),
                );
            }
        }
        Ok(())
    }

    /// Emit memory nodes for the memories that mention `eid`, plus a
    /// `Mentions` edge each. Bounded by [`MAX_EDGES_PER_ENTITY`].
    fn emit_mentions(
        &mut self,
        rtxn: &redb::ReadTransaction,
        eid: EntityId,
    ) -> Result<(), OpError> {
        let rows = walk_incoming(rtxn, NodeRef::Entity(eid), Some(EdgeKindRef::Mentions))
            .map_err(|e| OpError::Internal(format!("walk mentions: {e}")))?;
        let texts = rtxn.open_table(TEXTS_TABLE).ok();
        for (idx, (_kind, from, _disamb, _data)) in rows.into_iter().enumerate() {
            if idx >= MAX_EDGES_PER_ENTITY {
                break;
            }
            let NodeRef::Memory(mem_id) = from else {
                continue;
            };
            let mem_bytes = mem_id.to_be_bytes();
            self.emit_memory_node(texts.as_ref(), mem_id);
            // Only mentioned memories seed the memory-edge walk.
            if self.seen_memory_seeds.insert(mem_bytes) {
                self.memories_this_page.push(mem_id);
            }
            self.emit_edge(mem_bytes, eid.to_bytes(), EDGE_MENTIONS, String::new());
        }
        Ok(())
    }

    /// Emit a memory node (deduped), labelled with a snippet of its stored
    /// text when the `texts` table is readable.
    fn emit_memory_node(&mut self, texts: Option<&MemoryTexts>, mem_id: MemoryId) {
        let mem_bytes = mem_id.to_be_bytes();
        if self.seen_nodes.contains(&mem_bytes) {
            return;
        }
        let label = texts
            .and_then(|t| t.get(&mem_bytes).ok().flatten())
            .and_then(|g| String::from_utf8(g.value().to_vec()).ok())
            .map(|s| snippet(&s))
            .unwrap_or_default();
        self.emit_node(GraphNode {
            id: mem_bytes,
            kind: NODE_MEMORY,
            label,
            type_qname: String::new(),
        });
    }

    /// Emit the stored memory↔memory edges incident to `mem_id` (both
    /// directions), plus the far-endpoint memory node for each so no edge
    /// dangles. Symmetric kinds are canonicalised to one direction per pair —
    /// the edge table stores those twice by design, and emitting both rows
    /// would render as a spurious bidirectional pair. Bounded by
    /// [`MAX_EDGES_PER_MEMORY`].
    fn emit_memory_edges(
        &mut self,
        rtxn: &redb::ReadTransaction,
        mem_id: MemoryId,
    ) -> Result<(), OpError> {
        let texts = rtxn.open_table(TEXTS_TABLE).ok();
        let mut walked = 0usize;
        for outgoing in [true, false] {
            let rows = if outgoing {
                walk_outgoing(rtxn, NodeRef::Memory(mem_id), None)
            } else {
                walk_incoming(rtxn, NodeRef::Memory(mem_id), None)
            }
            .map_err(|e| OpError::Internal(format!("walk memory edges: {e}")))?;
            for (kind, other, _disamb, _data) in rows {
                if walked >= MAX_EDGES_PER_MEMORY {
                    return Ok(());
                }
                // Builtin kinds are memory↔memory by construction; Mentions
                // and Typed rows on this anchor belong to other layers.
                let EdgeKindRef::Builtin(edge_kind) = kind else {
                    continue;
                };
                let NodeRef::Memory(other_id) = other else {
                    continue;
                };
                let (from, to) = memory_edge_endpoints(edge_kind, mem_id, other_id, outgoing);
                walked += 1;
                self.emit_memory_node(texts.as_ref(), other_id);
                self.emit_edge(
                    from.to_be_bytes(),
                    to.to_be_bytes(),
                    GraphEdgeKindWire::from(edge_kind) as u8,
                    builtin_edge_label(edge_kind).to_string(),
                );
            }
        }
        Ok(())
    }
}

/// The `texts` table as opened for label lookups.
type MemoryTexts = redb::ReadOnlyTable<[u8; 16], &'static [u8]>;

/// Orient one memory edge for the wire. Asymmetric kinds keep the stored
/// direction; symmetric kinds (`SimilarTo`, `Contradicts`) are collapsed onto
/// a single id-ordered representative so the pair yields one edge regardless
/// of which endpoint the page reached first.
fn memory_edge_endpoints(
    kind: EdgeKind,
    anchor: MemoryId,
    other: MemoryId,
    outgoing: bool,
) -> (MemoryId, MemoryId) {
    if kind.is_symmetric() {
        if anchor.to_be_bytes() <= other.to_be_bytes() {
            (anchor, other)
        } else {
            (other, anchor)
        }
    } else if outgoing {
        (anchor, other)
    } else {
        (other, anchor)
    }
}

/// Human-facing label for a builtin memory edge. The `kind` byte is already
/// authoritative; the label spares a generic renderer a mapping table, the
/// same way `Relation` / `Fact` edges carry their predicate name.
fn builtin_edge_label(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Caused => "caused",
        EdgeKind::FollowedBy => "followed_by",
        EdgeKind::DerivedFrom => "derived_from",
        EdgeKind::SimilarTo => "similar_to",
        EdgeKind::Contradicts => "contradicts",
        EdgeKind::Supports => "supports",
        EdgeKind::References => "references",
        EdgeKind::PartOf => "part_of",
    }
}

/// Render a statement value object as a short label (no `Debug` wrapper).
fn render_value(v: &StatementValue) -> String {
    match v {
        StatementValue::Text(s) => s.clone(),
        StatementValue::Integer(n) => n.to_string(),
        StatementValue::Float(f) => f.to_string(),
        StatementValue::Bool(b) => b.to_string(),
        StatementValue::UnixNanos(t) => t.to_string(),
        StatementValue::Blob(b) => format!("<{} bytes>", b.len()),
    }
}

/// First ~80 chars of memory text for a memory-node label.
fn snippet(text: &str) -> String {
    const MAX: usize = 80;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX).collect();
    out.push('…');
    out
}

fn req_flags(req: &GraphFetchRequest) -> u8 {
    (u8::from(req.include_statements) * FLAG_STATEMENTS)
        | (u8::from(req.include_memories) * FLAG_MEMORIES)
        | (u8::from(req.include_tombstoned) * FLAG_TOMBSTONED)
        | (u8::from(req.include_memory_edges) * FLAG_MEMORY_EDGES)
}

fn key_to_bytes(k: &StmtKey) -> [u8; STMT_KEY_LEN] {
    let (ns, ag, subj, kind, pred, is_current, sid) = k;
    let mut out = [0u8; STMT_KEY_LEN];
    out[0..4].copy_from_slice(&ns.to_be_bytes());
    out[4..20].copy_from_slice(ag);
    out[20..36].copy_from_slice(subj);
    out[36] = *kind;
    out[37..41].copy_from_slice(&pred.to_be_bytes());
    out[41] = *is_current;
    out[42..58].copy_from_slice(sid);
    out
}

fn bytes_to_key(b: &[u8]) -> Option<StmtKey> {
    if b.len() != STMT_KEY_LEN {
        return None;
    }
    let ns = u32::from_be_bytes(b[0..4].try_into().ok()?);
    let mut ag = [0u8; 16];
    ag.copy_from_slice(&b[4..20]);
    let mut subj = [0u8; 16];
    subj.copy_from_slice(&b[20..36]);
    let kind = b[36];
    let pred = u32::from_be_bytes(b[37..41].try_into().ok()?);
    let is_current = b[41];
    let mut sid = [0u8; 16];
    sid.copy_from_slice(&b[42..58]);
    Some((ns, ag, subj, kind, pred, is_current, sid))
}

fn encode_cursor(flags: u8, key: &StmtKey) -> Vec<u8> {
    let mut out = Vec::with_capacity(CURSOR_LEN);
    out.push(CURSOR_VERSION);
    out.push(flags);
    out.extend_from_slice(&key_to_bytes(key));
    out
}

fn decode_cursor(cursor: &[u8], flags: u8) -> Result<StmtKey, OpError> {
    let stale = || OpError::InvalidRequest("stale_cursor: layer toggles changed".into());
    if cursor.len() != CURSOR_LEN || cursor[0] != CURSOR_VERSION || cursor[1] != flags {
        return Err(stale());
    }
    bytes_to_key(&cursor[2..]).ok_or_else(stale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::GraphFetchRequest;

    fn key() -> StmtKey {
        (7, [1u8; 16], [0xABu8; 16], 3, 0x00C0_FFEE, 1, [0x42u8; 16])
    }

    #[test]
    fn key_bytes_round_trip() {
        let k = key();
        let bytes = key_to_bytes(&k);
        assert_eq!(bytes.len(), STMT_KEY_LEN);
        assert_eq!(bytes_to_key(&bytes), Some(k));
    }

    #[test]
    fn cursor_round_trips_and_binds_flags() {
        let flags = FLAG_STATEMENTS | FLAG_MEMORIES;
        let cur = encode_cursor(flags, &key());
        assert_eq!(cur.len(), CURSOR_LEN);
        assert_eq!(decode_cursor(&cur, flags).unwrap(), key());
    }

    #[test]
    fn cursor_rejects_changed_flags() {
        // A cursor minted with statements+memories must not resume a request
        // that dropped a layer — the derived result set would differ.
        let cur = encode_cursor(FLAG_STATEMENTS | FLAG_MEMORIES, &key());
        assert!(decode_cursor(&cur, FLAG_STATEMENTS).is_err());
        assert!(decode_cursor(&cur, 0).is_err());
    }

    #[test]
    fn cursor_rejects_wrong_version_and_length() {
        let mut cur = encode_cursor(0, &key());
        assert!(decode_cursor(&cur[..CURSOR_LEN - 1], 0).is_err());
        cur[0] = CURSOR_VERSION.wrapping_add(1);
        assert!(decode_cursor(&cur, 0).is_err());
    }

    #[test]
    fn req_flags_maps_each_toggle() {
        let base = GraphFetchRequest {
            limit: 10,
            cursor: Vec::new(),
            include_statements: false,
            include_memories: false,
            include_memory_edges: false,
            include_tombstoned: false,
            act_as: None,
        };
        assert_eq!(req_flags(&base), 0);
        assert_eq!(
            req_flags(&GraphFetchRequest {
                include_statements: true,
                ..base.clone()
            }),
            FLAG_STATEMENTS
        );
        assert_eq!(
            req_flags(&GraphFetchRequest {
                include_memories: true,
                include_tombstoned: true,
                ..base.clone()
            }),
            FLAG_MEMORIES | FLAG_TOMBSTONED
        );
        assert_eq!(
            req_flags(&GraphFetchRequest {
                include_memories: true,
                include_memory_edges: true,
                ..base
            }),
            FLAG_MEMORIES | FLAG_MEMORY_EDGES
        );
    }

    /// Toggling the memory-edge layer mid-scroll must invalidate the cursor
    /// for the same reason the other layers do: the resumed page would
    /// belong to a differently-shaped export.
    #[test]
    fn cursor_rejects_memory_edge_toggle_mid_scroll() {
        let with = FLAG_MEMORIES | FLAG_MEMORY_EDGES;
        let cur = encode_cursor(with, &key());
        assert_eq!(decode_cursor(&cur, with).unwrap(), key());
        assert!(decode_cursor(&cur, FLAG_MEMORIES).is_err());

        let without = encode_cursor(FLAG_MEMORIES, &key());
        assert!(decode_cursor(&without, with).is_err());
    }

    /// Every builtin kind must map to its own wire byte, and none may
    /// collide with the four typed-graph edge bytes emitted by the same
    /// export. Without this a client could not tell `SimilarTo` from
    /// `FollowedBy` — the whole point of the layer.
    #[test]
    fn builtin_edge_kinds_have_distinct_non_colliding_wire_bytes() {
        let mut seen = std::collections::HashSet::new();
        for k in ALL_EDGE_KINDS {
            let byte = GraphEdgeKindWire::from(k) as u8;
            assert!(seen.insert(byte), "{k:?} duplicate wire byte {byte}");
            assert!(
                ![EDGE_RELATION, EDGE_FACT, EDGE_HAS_STATEMENT, EDGE_MENTIONS].contains(&byte),
                "{k:?} collides with a typed-graph edge byte"
            );
            assert!(!builtin_edge_label(k).is_empty());
        }
        assert_eq!(seen.len(), 8);
    }

    /// Symmetric kinds collapse onto one id-ordered representative, so the
    /// pair yields a single edge whichever endpoint the page reached first.
    /// Asymmetric kinds keep the stored direction.
    #[test]
    fn memory_edge_endpoints_canonicalise_only_symmetric_kinds() {
        let lo = MemoryId::pack(0, 1, 1);
        let hi = MemoryId::pack(0, 2, 1);
        assert!(lo.to_be_bytes() < hi.to_be_bytes());

        for k in [EdgeKind::SimilarTo, EdgeKind::Contradicts] {
            assert_eq!(memory_edge_endpoints(k, lo, hi, true), (lo, hi));
            assert_eq!(memory_edge_endpoints(k, lo, hi, false), (lo, hi));
            assert_eq!(memory_edge_endpoints(k, hi, lo, true), (lo, hi));
            assert_eq!(memory_edge_endpoints(k, hi, lo, false), (lo, hi));
        }
        for k in ALL_EDGE_KINDS.into_iter().filter(|k| !k.is_symmetric()) {
            // Anchor is the source when the row came from the forward table,
            // the target when it came from the reverse one.
            assert_eq!(memory_edge_endpoints(k, hi, lo, true), (hi, lo));
            assert_eq!(memory_edge_endpoints(k, hi, lo, false), (lo, hi));
        }
    }

    const ALL_EDGE_KINDS: [EdgeKind; 8] = [
        EdgeKind::Caused,
        EdgeKind::FollowedBy,
        EdgeKind::DerivedFrom,
        EdgeKind::SimilarTo,
        EdgeKind::Contradicts,
        EdgeKind::Supports,
        EdgeKind::References,
        EdgeKind::PartOf,
    ];

    #[test]
    fn snippet_truncates_long_text() {
        let short = "Sarah Chen";
        assert_eq!(snippet(short), short);
        let long: String = "x".repeat(200);
        let s = snippet(&long);
        assert!(s.chars().count() <= 81); // 80 + ellipsis
        assert!(s.ends_with('…'));
    }
}
