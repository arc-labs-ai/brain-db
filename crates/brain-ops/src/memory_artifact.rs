//! Durable per-memory write-artifact bundle.
//!
//! A memory's write produces derived artifacts across several stages — the
//! embedding vector (sync), the stored metadata row (sync), the typed-graph
//! rows (async extractor), the derived `SimilarTo` / `FollowedBy` edges
//! (async auto_edge / temporal_edge), the analyzed lexical terms (async
//! text-indexer), and the hypothetical questions (async HyPE). Only the
//! vector's *embedding* and the graph/edge *rows* survive in their native
//! tables; the analyzed terms and the HyPE question text are otherwise
//! discarded once indexed/embedded. This module keeps a friendly,
//! denormalized copy of the whole set in one redb table (`MEMORY_ARTIFACTS`,
//! value = JSON of [`EncodeStageArtifact`]) so `MEMORY_INSPECT` can show any
//! memory's full write story later, not just the one just written via the
//! live ENCODE trace.
//!
//! ## Incremental population, no lost update
//!
//! The bundle is built up by several producers at different times:
//! - **sync** (apply, on the ack txn): vector + record, piggybacked on the
//!   memory-row write so it costs no extra transaction on the ack path.
//! - **async** (background workers, off the ack path): the extractor merges
//!   the graph, `auto_edge` / `temporal_edge` merge the derived edges, the
//!   text-indexer merges the analyzed terms, HyPE merges the generated
//!   questions.
//!
//! Every producer uses [`merge_memory_artifact`] — a read-modify-write inside
//! a single redb write txn. redb's exclusive write lock serializes concurrent
//! producers on the shard, so a later merge never clobbers an earlier one's
//! fields; each touches only its own portion of the bundle. The extractor and
//! the two edge workers additionally share one field (`graph`) rather than
//! each getting their own — see [`merge_edge_links`] for how they avoid
//! clobbering each other there.

use brain_core::MemoryId;
use brain_metadata::tables::memory_artifacts::MEMORY_ARTIFACTS_TABLE;
use brain_metadata::tables::memory_vector::MEMORY_VECTORS_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_metadata::MetadataDb;
use brain_protocol::envelope::response::{
    EncodeGraphEdge, EncodeGraphNode, EncodeStageArtifact, EncodeStageGraph,
    EncodeStageKeywordField, EncodeStageRecord,
};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

/// Edge kinds the extractor stage owns. It always recomputes the *full*
/// current committed entity/statement/relation graph on every call (via
/// [`crate::handlers::recall::fetch_enrichment_for`]), so its merge fully
/// replaces edges of these kinds — but must leave edges of every other kind
/// (the `auto_edge` / `temporal_edge` derived edges, which share the same
/// bundle field) untouched.
const EXTRACTOR_EDGE_KINDS: [&str; 2] = ["statement", "relation"];

/// Read-modify-write one memory's artifact bundle inside `wtxn`. Reads the
/// current bundle (default when absent), applies `f`, writes it back. Commits
/// atomically with whatever else the caller's txn is doing; concurrent
/// producers are serialized by redb's write lock, so no field is lost.
pub fn merge_memory_artifact<F>(
    wtxn: &WriteTransaction,
    memory_id: [u8; 16],
    f: F,
) -> Result<(), String>
where
    F: FnOnce(&mut EncodeStageArtifact),
{
    let mut table = wtxn
        .open_table(MEMORY_ARTIFACTS_TABLE)
        .map_err(|e| format!("open memory_artifacts: {e}"))?;
    // The get-guard is a temporary bound only for this expression, so it is
    // dropped before `table.insert` needs exclusive access below.
    let mut bundle: EncodeStageArtifact = table
        .get(&memory_id)
        .map_err(|e| format!("artifact read: {e}"))?
        .and_then(|g| serde_json::from_str::<EncodeStageArtifact>(g.value()).ok())
        .unwrap_or_default();
    f(&mut bundle);
    let json = serde_json::to_string(&bundle).map_err(|e| format!("artifact serialize: {e}"))?;
    table
        .insert(&memory_id, json.as_str())
        .map_err(|e| format!("artifact write: {e}"))?;
    Ok(())
}

/// Delete a memory's artifact bundle (FORGET cascade). A missing row is a
/// no-op success — FORGET is lenient, and a memory that never produced a
/// bundle is indistinguishable from one whose bundle is already gone.
pub fn delete_memory_artifact(wtxn: &WriteTransaction, memory_id: [u8; 16]) -> Result<(), String> {
    let mut table = wtxn
        .open_table(MEMORY_ARTIFACTS_TABLE)
        .map_err(|e| format!("open memory_artifacts: {e}"))?;
    table
        .remove(&memory_id)
        .map_err(|e| format!("artifact remove: {e}"))?;
    // The raw vector is embedding-at-rest too — drop it alongside the
    // bundle so a reclaim / hard-forget leaves nothing behind.
    let mut vt = wtxn
        .open_table(MEMORY_VECTORS_TABLE)
        .map_err(|e| format!("open memory_vectors: {e}"))?;
    vt.remove(&memory_id)
        .map_err(|e| format!("vector remove: {e}"))?;
    Ok(())
}

/// Encode an embedding as the flat little-endian `f32` byte run the
/// `memory_vectors` table stores.
fn vector_to_le_bytes(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for f in vector {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    bytes
}

/// Write the **sync** portion (vector + record + analyzed keyword terms) into
/// the bundle inside the apply write txn. Called from
/// [`crate::apply::memory::apply_upsert_memory`] so these fields commit
/// atomically with the memory row at zero extra transaction cost on the ack
/// path. The async producers fill the rest (graph / hype) later via
/// [`merge_memory_artifact`].
pub fn put_sync_artifact(
    wtxn: &WriteTransaction,
    memory_id: [u8; 16],
    vector: Vec<f32>,
    record: EncodeStageRecord,
    keyword_fields: Vec<EncodeStageKeywordField>,
) -> Result<(), String> {
    // Fast by-id vector store: the raw LE-`f32` run, resolved on the hot
    // recall path (`get_artifact_vector`) with a single point lookup +
    // decode — no JSON parse of the whole (graph-bearing) bundle.
    {
        let mut vt = wtxn
            .open_table(MEMORY_VECTORS_TABLE)
            .map_err(|e| format!("open memory_vectors: {e}"))?;
        vt.insert(&memory_id, vector_to_le_bytes(&vector).as_slice())
            .map_err(|e| format!("vector write: {e}"))?;
    }
    merge_memory_artifact(wtxn, memory_id, |bundle| {
        bundle.vector = vector;
        bundle.record = Some(record);
        bundle.keyword_fields = keyword_fields;
    })
}

/// Analyze `text` into the exact lexical terms the `memory_text` index will
/// match on, using the same [`build_analyzer`](brain_index::build_analyzer)
/// the shard registers on that index — so the inspection view shows the real
/// analyzed tokens (lowercased, stemmed), not a re-tokenized approximation.
/// Deduplicated in first-seen order. Empty text yields no field.
#[must_use]
pub fn analyze_memory_keywords(text: &str) -> Vec<EncodeStageKeywordField> {
    use tantivy::tokenizer::TokenStream;

    let mut analyzer = brain_index::build_analyzer();
    let mut stream = analyzer.token_stream(text);
    let mut seen = std::collections::HashSet::new();
    let mut terms: Vec<String> = Vec::new();
    while stream.advance() {
        let t = &stream.token().text;
        if seen.insert(t.clone()) {
            terms.push(t.clone());
        }
    }
    if terms.is_empty() {
        return Vec::new();
    }
    vec![EncodeStageKeywordField {
        field: "memory_text".to_string(),
        terms,
    }]
}

/// Merge the generated hypothetical questions produced by the HyPE worker.
/// Opens its own write txn (HyPE runs off the ack path).
pub fn merge_hype_questions(
    metadata: &MetadataDb,
    memory_id: MemoryId,
    questions: Vec<String>,
) -> Result<(), String> {
    let wtxn = metadata
        .write_txn()
        .map_err(|e| format!("hype merge write_txn: {e}"))?;
    merge_memory_artifact(&wtxn, memory_id.to_be_bytes(), |bundle| {
        bundle.hype_questions = questions;
    })?;
    wtxn.commit()
        .map_err(|e| format!("hype merge commit: {e}"))?;
    Ok(())
}

/// Assemble the typed-graph portion of the bundle by reading back the
/// committed graph for `memory_id`, then merge it in. Called by the extractor
/// worker **after** its graph commit, so the read sees the entities /
/// statements / relations it just wrote. Uses the same enrichment resolver
/// RECALL uses, so the content is real (canonical names, predicates,
/// confidences) rather than raw counts. Derives the memory's `(namespace,
/// space)` scope from its own row so the enrichment stays tenant-scoped.
///
/// Best-effort: a read or merge failure returns `Err` for the caller to log,
/// never blocks the durable graph write (which already committed).
pub fn merge_graph_from_committed(
    metadata: &MetadataDb,
    memory_id: MemoryId,
) -> Result<(), String> {
    let graph = {
        let rtxn = metadata
            .read_txn()
            .map_err(|e| format!("graph merge read_txn: {e}"))?;
        let Some(scope) = memory_scope(&rtxn, memory_id)? else {
            // Memory row gone (e.g. hard-forgotten between commit and merge):
            // nothing to enrich, and the bundle was purged with the row.
            return Ok(());
        };
        let enr = crate::handlers::recall::fetch_enrichment_for(&[memory_id], scope, None, &rtxn)
            .map_err(|e| format!("graph enrichment: {e}"))?;
        enrichment_to_graph(enr.into_iter().next())
    };

    let wtxn = metadata
        .write_txn()
        .map_err(|e| format!("graph merge write_txn: {e}"))?;
    merge_memory_artifact(&wtxn, memory_id.to_be_bytes(), |bundle| {
        let existing = bundle.graph.take().unwrap_or_default();
        bundle.graph = Some(replace_owned_edges(
            existing,
            &EXTRACTOR_EDGE_KINDS,
            graph.nodes,
            graph.edges,
        ));
    })?;
    wtxn.commit()
        .map_err(|e| format!("graph merge commit: {e}"))?;
    Ok(())
}

/// Merge derived memory↔memory edges (`SimilarTo` from `auto_edge`,
/// `FollowedBy` from `temporal_edge`) into the bundle's shared `graph`
/// field, resolving each linked memory's text into a `"memory"`-kind
/// [`EncodeGraphNode`] so the bundle is self-contained (matching how
/// [`merge_graph_from_committed`] resolves canonical entity names inline
/// rather than deferring to a later read-time enrichment call). `memory_id`
/// is the memory this bundle belongs to (the source for `auto_edge`, the
/// successor for `temporal_edge`); `links` are `(from, to, weight)`
/// triples oriented to match the real edge direction the cycle wrote, and
/// `edge_kind` is `"similar_to"` or `"followed_by"`.
///
/// Unlike `merge_graph_from_committed` (which recomputes and replaces the
/// *entire* entity/statement/relation graph on every call, since it always
/// re-reads the full committed set), an `auto_edge`/`temporal_edge` cycle
/// only ever sees one cycle's worth of new links — a memory's out-edge
/// budget accumulates across many cycles — so this merge **appends**
/// rather than replaces, deduping on `(source, target, kind)` so a retried
/// cycle (or a repeated merge call for the same edge) doesn't grow the
/// bundle without bound. Edges of every other kind are always left
/// untouched.
///
/// Best-effort like its siblings: opens its own read+write txn pair (these
/// workers run off the ack path); a failure here never blocks the edge
/// write itself, which already committed via `submit(Write)`. A no-op
/// (`Ok(())`, no txn opened) when `links` is empty.
pub fn merge_edge_links(
    metadata: &MetadataDb,
    memory_id: MemoryId,
    edge_kind: &str,
    links: &[(MemoryId, MemoryId, f32)],
) -> Result<(), String> {
    if links.is_empty() {
        return Ok(());
    }

    let (nodes, edges) = {
        let rtxn = metadata
            .read_txn()
            .map_err(|e| format!("edge merge read_txn: {e}"))?;
        let texts = rtxn
            .open_table(TEXTS_TABLE)
            .map_err(|e| format!("open texts: {e}"))?;

        let mut seen: std::collections::HashSet<[u8; 16]> = std::collections::HashSet::new();
        let mut nodes = Vec::new();
        for (from, to, _weight) in links {
            for id in [*from, *to] {
                let key = id.to_be_bytes();
                if seen.insert(key) {
                    let name = texts
                        .get(&key)
                        .ok()
                        .flatten()
                        .and_then(|g| std::str::from_utf8(g.value()).ok().map(str::to_string))
                        .map(|s| memory_text_preview(&s))
                        .unwrap_or_default();
                    nodes.push(EncodeGraphNode {
                        id: key,
                        name,
                        kind: "memory".to_string(),
                        type_qname: String::new(),
                    });
                }
            }
        }
        let edges: Vec<EncodeGraphEdge> = links
            .iter()
            .map(|(from, to, weight)| EncodeGraphEdge {
                source: from.to_be_bytes(),
                target: to.to_be_bytes(),
                predicate: edge_kind.to_string(),
                kind: edge_kind.to_string(),
                confidence: *weight,
                // A derived similarity / adjacency edge records no event.
                event_at_unix_nanos: None,
            })
            .collect();
        (nodes, edges)
    };

    let wtxn = metadata
        .write_txn()
        .map_err(|e| format!("edge merge write_txn: {e}"))?;
    merge_memory_artifact(&wtxn, memory_id.to_be_bytes(), |bundle| {
        let existing = bundle.graph.take().unwrap_or_default();
        bundle.graph = Some(append_owned_edges(existing, nodes, edges));
    })?;
    wtxn.commit()
        .map_err(|e| format!("edge merge commit: {e}"))?;
    Ok(())
}

/// Replace every edge whose `kind` is in `owned_kinds` with `new_edges`,
/// leaving edges of every other kind untouched. Nodes are unioned, deduped
/// by id (first write wins — a node's display name shouldn't change
/// depending on which producer happened to add it first).
fn replace_owned_edges(
    mut graph: EncodeStageGraph,
    owned_kinds: &[&str],
    new_nodes: Vec<EncodeGraphNode>,
    new_edges: Vec<EncodeGraphEdge>,
) -> EncodeStageGraph {
    graph
        .edges
        .retain(|e| !owned_kinds.contains(&e.kind.as_str()));
    for node in new_nodes {
        if !graph.nodes.iter().any(|n| n.id == node.id) {
            graph.nodes.push(node);
        }
    }
    graph.edges.extend(new_edges);
    graph
}

/// Append `new_edges` to `graph`, deduping against edges already present
/// with the same `(source, target, kind)` triple. Never removes anything —
/// safe to call incrementally across many worker cycles. Nodes are unioned,
/// deduped by id.
fn append_owned_edges(
    mut graph: EncodeStageGraph,
    new_nodes: Vec<EncodeGraphNode>,
    new_edges: Vec<EncodeGraphEdge>,
) -> EncodeStageGraph {
    for node in new_nodes {
        if !graph.nodes.iter().any(|n| n.id == node.id) {
            graph.nodes.push(node);
        }
    }
    for edge in new_edges {
        let dup = graph
            .edges
            .iter()
            .any(|e| e.source == edge.source && e.target == edge.target && e.kind == edge.kind);
        if !dup {
            graph.edges.push(edge);
        }
    }
    graph
}

/// Truncate a memory's text to a short preview for a graph-node label — the
/// bundle need not carry the linked memory's entire (possibly long) text,
/// just enough to identify it in a renderer. Truncates on a char boundary.
fn memory_text_preview(text: &str) -> String {
    const MAX_PREVIEW_CHARS: usize = 160;
    if text.chars().count() <= MAX_PREVIEW_CHARS {
        return text.to_string();
    }
    let mut preview: String = text.chars().take(MAX_PREVIEW_CHARS).collect();
    preview.push('…');
    preview
}

/// Read a memory's durable write-artifact bundle, if one exists. Used by the
/// traced ENCODE path to fold the durable keyword terms + HyPE questions + the
/// settled graph back into the live trace, so `trace = true` returns the same
/// complete picture `MEMORY_INSPECT` would (at the cost of waiting for the
/// async producers — the caller polls this until the bundle is complete).
pub fn read_memory_artifact(
    metadata: &MetadataDb,
    memory_id: MemoryId,
) -> Result<Option<EncodeStageArtifact>, String> {
    let rtxn = metadata
        .read_txn()
        .map_err(|e| format!("artifact read_txn: {e}"))?;
    let table = rtxn
        .open_table(MEMORY_ARTIFACTS_TABLE)
        .map_err(|e| format!("open memory_artifacts: {e}"))?;
    Ok(table
        .get(&memory_id.to_be_bytes())
        .map_err(|e| format!("artifact read: {e}"))?
        .and_then(|g| serde_json::from_str::<EncodeStageArtifact>(g.value()).ok()))
}

/// Read just a memory's stored write-time embedding vector by id, under a
/// caller-provided read txn. Returns `None` when the row is absent or the
/// stored vector isn't the expected dimension.
///
/// This is the LIVE by-id vector store, written on the ENCODE ack path by
/// [`put_sync_artifact`] — unlike the memory-mapped arena (populated only
/// by WAL recovery on shard restart), it is present for a memory encoded
/// in the current run. Consumers needing a memory's vector by id
/// (single-space brute-force recall, consolidation clustering, rebuild
/// sources) must resolve it here, not from the arena.
///
/// Fast path: the dedicated `memory_vectors` table — a single point
/// lookup + little-endian decode, no JSON. Fallback: the JSON artifact
/// bundle, for rows written before the raw table existed.
#[must_use]
pub fn get_artifact_vector(
    rtxn: &ReadTransaction,
    memory_id: [u8; 16],
) -> Option<[f32; brain_embed::VECTOR_DIM]> {
    // Fast path — raw LE-f32 bytes, no bundle parse.
    if let Ok(vt) = rtxn.open_table(MEMORY_VECTORS_TABLE) {
        if let Some(g) = vt.get(&memory_id).ok().flatten() {
            let bytes = g.value();
            if bytes.len() == brain_embed::VECTOR_DIM * 4 {
                let mut v = [0.0_f32; brain_embed::VECTOR_DIM];
                for (slot, chunk) in v.iter_mut().zip(bytes.as_chunks::<4>().0.iter()) {
                    *slot = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                }
                return Some(v);
            }
        }
    }
    // Fallback — older rows whose vector lives only in the JSON bundle.
    let table = rtxn.open_table(MEMORY_ARTIFACTS_TABLE).ok()?;
    let bundle: EncodeStageArtifact = table
        .get(&memory_id)
        .ok()
        .flatten()
        .and_then(|g| serde_json::from_str(g.value()).ok())?;
    if bundle.vector.len() != brain_embed::VECTOR_DIM {
        return None;
    }
    let mut v = [0.0_f32; brain_embed::VECTOR_DIM];
    v.copy_from_slice(&bundle.vector);
    Some(v)
}

/// Read a memory's `(namespace, space)` scope from its metadata row.
/// `None` when the row is absent (forgotten / never existed).
fn memory_scope(
    rtxn: &redb::ReadTransaction,
    memory_id: MemoryId,
) -> Result<Option<brain_metadata::RowScope>, String> {
    use brain_metadata::tables::memory::MEMORIES_TABLE;
    let table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| format!("open memories: {e}"))?;
    Ok(table
        .get(&memory_id.to_be_bytes())
        .map_err(|e| format!("memory read: {e}"))?
        .map(|g| {
            let m = g.value();
            brain_metadata::RowScope::from_bytes(m.namespace_id, m.space_id_bytes)
        }))
}

/// Deterministic synthetic node id for a literal-object statement edge:
/// `blake3("encode_graph_literal:" || source_entity_id || predicate ||
/// literal_text)` truncated to the leading 16 bytes. The same fact —
/// same subject, predicate, and literal text — always hashes to the same
/// id, so inspecting the same write's graph twice (or the same literal
/// value appearing on more than one statement in one write) converges on
/// one node rather than minting a fresh one each time.
///
/// Shared by both graph renderers — the live ENCODE trace
/// (`encode_artifacts_to_graph`) and the durable bundle
/// ([`enrichment_to_graph`]) — so one fact carries one literal node id no
/// matter which path surfaced it.
pub(crate) fn literal_node_id(source: &[u8; 16], predicate: &str, literal_text: &str) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"encode_graph_literal:");
    hasher.update(source);
    hasher.update(b"\0");
    hasher.update(predicate.as_bytes());
    hasher.update(b"\0");
    hasher.update(literal_text.as_bytes());
    let digest = hasher.finalize();
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest.as_bytes()[..16]);
    id
}

/// Convert one memory's [`GraphEnrichment`](brain_protocol::envelope::response::GraphEnrichment)
/// into the bundle's [`EncodeStageGraph`]. Entities become nodes (they carry
/// ids); statement objects and relation endpoints are matched back to those
/// node ids by canonical name so edges reference real nodes when possible. A
/// statement object the enrichment didn't surface as an entity is a literal
/// value (e.g. "favorite color is **blue**") — it gets a synthetic
/// `"literal"` node (deduped by id within this call) carrying the real text,
/// so the bundle's edge points at a real node instead of the all-zero
/// placeholder. A relation endpoint still falls back to the zero id —
/// relations are entity-to-entity by schema, so an unresolved endpoint there
/// is an enrichment-cap miss, not a literal.
pub(crate) fn enrichment_to_graph(
    enr: Option<brain_protocol::envelope::response::GraphEnrichment>,
) -> EncodeStageGraph {
    let Some(enr) = enr else {
        return EncodeStageGraph::default();
    };

    let mut nodes: Vec<EncodeGraphNode> = enr
        .entities
        .iter()
        .map(|e| EncodeGraphNode {
            id: e.id,
            name: e.name.clone(),
            kind: "entity".to_string(),
            type_qname: e.type_qname.clone(),
        })
        .collect();

    // Canonical-name → node id, for wiring edge endpoints back to nodes.
    let id_by_name: std::collections::HashMap<&str, [u8; 16]> = enr
        .entities
        .iter()
        .map(|e| (e.name.as_str(), e.id))
        .collect();
    let lookup = |name: &str| -> Option<[u8; 16]> { id_by_name.get(name).copied() };

    let mut seen_literals: std::collections::HashSet<[u8; 16]> = std::collections::HashSet::new();
    let mut edges: Vec<EncodeGraphEdge> = Vec::new();
    // Statements (subject → object via predicate); a non-entity object is a
    // literal value and gets its own synthetic node.
    for s in &enr.statements {
        let source = lookup(&s.subject_name).unwrap_or([0u8; 16]);
        let target = match lookup(&s.object_label) {
            Some(id) => id,
            None => {
                let lit_id = literal_node_id(&source, &s.predicate, &s.object_label);
                if seen_literals.insert(lit_id) {
                    nodes.push(EncodeGraphNode {
                        id: lit_id,
                        name: s.object_label.clone(),
                        kind: "literal".to_string(),
                        type_qname: String::new(),
                    });
                }
                lit_id
            }
        };
        edges.push(EncodeGraphEdge {
            source,
            target,
            predicate: s.predicate.clone(),
            kind: "statement".to_string(),
            confidence: s.confidence,
            event_at_unix_nanos: s.event_at_unix_nanos,
        });
    }
    // Typed relations (from → to via relation-type predicate).
    for r in &enr.relations {
        edges.push(EncodeGraphEdge {
            source: lookup(&r.from_name).unwrap_or([0u8; 16]),
            target: lookup(&r.to_name).unwrap_or([0u8; 16]),
            predicate: r.predicate.clone(),
            kind: "relation".to_string(),
            confidence: 1.0,
            // A typed relation row carries no event time of its own.
            event_at_unix_nanos: None,
        });
    }

    EncodeStageGraph { nodes, edges }
}

/// Build the sync [`EncodeStageRecord`] from the fields in hand at apply time.
/// `lsn` is left `0` here — the durable log position isn't assigned until the
/// WAL append inside `submit`, and the live ENCODE trace carries the real lsn
/// on its `persist` stage. The record's other fields are authoritative.
#[must_use]
pub fn sync_record(
    memory_id: [u8; 16],
    kind_byte: u8,
    salience: f32,
    created_at_unix_nanos: u64,
    occurred_at_unix_nanos: u64,
    vector_dim: u32,
    text_len: u32,
) -> EncodeStageRecord {
    EncodeStageRecord {
        memory_id,
        kind: kind_byte,
        salience,
        created_at_unix_nanos,
        occurred_at_unix_nanos,
        vector_dim,
        text_len,
        lsn: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::envelope::response::{
        EncodeStageArtifact, EnrichedEntity, EnrichedStatement, GraphEnrichment,
    };
    use tempfile::TempDir;

    fn open_db() -> (TempDir, MetadataDb) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        (dir, db)
    }

    fn read_bundle(db: &MetadataDb, id: [u8; 16]) -> Option<EncodeStageArtifact> {
        let rtxn = db.read_txn().unwrap();
        let table = rtxn.open_table(MEMORY_ARTIFACTS_TABLE).unwrap();
        table
            .get(&id)
            .unwrap()
            .and_then(|g| serde_json::from_str::<EncodeStageArtifact>(g.value()).ok())
    }

    #[test]
    fn sync_write_then_read_roundtrips_vector_record_keywords() {
        let (_dir, db) = open_db();
        let id = [7u8; 16];
        let record = sync_record(id, 1, 0.5, 100, 0, 3, 12);
        let keywords = analyze_memory_keywords("The Quick Brown Fox");

        let wtxn = db.write_txn().unwrap();
        put_sync_artifact(&wtxn, id, vec![0.1, 0.2, 0.3], record, keywords).unwrap();
        wtxn.commit().unwrap();

        let b = read_bundle(&db, id).expect("bundle present");
        assert_eq!(b.vector, vec![0.1, 0.2, 0.3]);
        assert_eq!(b.record.as_ref().unwrap().kind, 1);
        assert_eq!(b.record.as_ref().unwrap().text_len, 12);
        assert_eq!(b.keyword_fields.len(), 1);
        assert!(!b.keyword_fields[0].terms.is_empty());
    }

    #[test]
    fn get_artifact_vector_reads_raw_table_and_delete_clears_it() {
        use brain_metadata::tables::memory_vector::MEMORY_VECTORS_TABLE;
        let (_dir, db) = open_db();
        let id = [9u8; 16];
        let mut vector = vec![0.0f32; brain_embed::VECTOR_DIM];
        vector[0] = 0.5;
        vector[brain_embed::VECTOR_DIM - 1] = -0.25;
        let record = sync_record(id, 0, 1.0, 1, 0, brain_embed::VECTOR_DIM as u32, 4);

        let wtxn = db.write_txn().unwrap();
        put_sync_artifact(&wtxn, id, vector.clone(), record, Vec::new()).unwrap();
        wtxn.commit().unwrap();

        // The raw table row exists and is exactly VECTOR_DIM * 4 bytes.
        {
            let rtxn = db.read_txn().unwrap();
            let vt = rtxn.open_table(MEMORY_VECTORS_TABLE).unwrap();
            let got = vt.get(&id).unwrap().expect("raw vector row present");
            assert_eq!(got.value().len(), brain_embed::VECTOR_DIM * 4);
        }
        // get_artifact_vector resolves it via the raw fast path.
        {
            let rtxn = db.read_txn().unwrap();
            let v = get_artifact_vector(&rtxn, id).expect("vector resolves");
            assert!((v[0] - 0.5).abs() < f32::EPSILON);
            assert!((v[brain_embed::VECTOR_DIM - 1] + 0.25).abs() < f32::EPSILON);
        }
        // delete_memory_artifact clears both the bundle and the raw row.
        let wtxn = db.write_txn().unwrap();
        delete_memory_artifact(&wtxn, id).unwrap();
        wtxn.commit().unwrap();
        {
            let rtxn = db.read_txn().unwrap();
            assert!(
                get_artifact_vector(&rtxn, id).is_none(),
                "vector gone after delete"
            );
            let vt = rtxn.open_table(MEMORY_VECTORS_TABLE).unwrap();
            assert!(vt.get(&id).unwrap().is_none(), "raw row removed");
        }
    }

    #[test]
    fn get_artifact_vector_falls_back_to_json_bundle_when_raw_absent() {
        // A row whose vector lives only in the JSON bundle (seeded via
        // merge_memory_artifact, or written before the raw table existed)
        // must still resolve — via the fallback path.
        let (_dir, db) = open_db();
        let id = [11u8; 16];
        let mut vector = vec![0.0f32; brain_embed::VECTOR_DIM];
        vector[1] = 0.75;
        let wtxn = db.write_txn().unwrap();
        merge_memory_artifact(&wtxn, id, |b| b.vector = vector.clone()).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let v = get_artifact_vector(&rtxn, id).expect("fallback resolves from bundle");
        assert!((v[1] - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn later_merge_preserves_earlier_fields() {
        // The incremental design: a graph merge must not clobber the sync
        // vector/record an earlier producer wrote.
        let (_dir, db) = open_db();
        let id = [9u8; 16];
        let record = sync_record(id, 0, 0.5, 1, 0, 2, 4);

        let wtxn = db.write_txn().unwrap();
        put_sync_artifact(&wtxn, id, vec![1.0, 2.0], record, Vec::new()).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        merge_memory_artifact(&wtxn, id, |bundle| {
            bundle.hype_questions = vec!["who?".to_string()];
        })
        .unwrap();
        wtxn.commit().unwrap();

        let b = read_bundle(&db, id).expect("bundle present");
        assert_eq!(b.vector, vec![1.0, 2.0], "sync vector survived the merge");
        assert!(b.record.is_some(), "sync record survived the merge");
        assert_eq!(b.hype_questions, vec!["who?".to_string()]);
    }

    #[test]
    fn delete_removes_the_bundle() {
        let (_dir, db) = open_db();
        let id = [3u8; 16];
        let record = sync_record(id, 0, 0.5, 1, 0, 1, 1);

        let wtxn = db.write_txn().unwrap();
        put_sync_artifact(&wtxn, id, vec![1.0], record, Vec::new()).unwrap();
        wtxn.commit().unwrap();
        assert!(read_bundle(&db, id).is_some());

        let wtxn = db.write_txn().unwrap();
        delete_memory_artifact(&wtxn, id).unwrap();
        wtxn.commit().unwrap();
        assert!(read_bundle(&db, id).is_none(), "bundle purged");

        // Deleting an absent bundle is a no-op success (FORGET leniency).
        let wtxn = db.write_txn().unwrap();
        delete_memory_artifact(&wtxn, id).unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn keyword_analysis_dedups_and_skips_empty() {
        assert!(analyze_memory_keywords("   ").is_empty());
        let fields = analyze_memory_keywords("fox fox FOX");
        assert_eq!(fields.len(), 1);
        // The shared analyzer lowercases, so all three collapse to one term.
        assert_eq!(fields[0].terms.len(), 1, "duplicate terms deduped");
    }

    fn put_text(db: &MetadataDb, id: [u8; 16], text: &str) {
        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&id, text.as_bytes()).unwrap();
        }
        wtxn.commit().unwrap();
    }

    #[test]
    fn merge_edge_links_writes_real_target_and_weight_not_just_a_count() {
        let (_dir, db) = open_db();
        let source = [1u8; 16];
        let target = [2u8; 16];
        put_text(&db, source, "Priya works at Stripe.");
        put_text(&db, target, "Priya joined Stripe last year.");

        let links = vec![(
            MemoryId::from_be_bytes(source),
            MemoryId::from_be_bytes(target),
            0.91_f32,
        )];
        merge_edge_links(&db, MemoryId::from_be_bytes(source), "similar_to", &links).unwrap();

        let b = read_bundle(&db, source).expect("bundle present");
        let graph = b.graph.expect("graph populated");
        assert_eq!(graph.nodes.len(), 2, "both endpoints resolved as nodes");
        let target_node = graph
            .nodes
            .iter()
            .find(|n| n.id == target)
            .expect("target node present");
        assert_eq!(target_node.kind, "memory");
        assert_eq!(target_node.name, "Priya joined Stripe last year.");

        assert_eq!(graph.edges.len(), 1);
        let edge = &graph.edges[0];
        assert_eq!(edge.source, source);
        assert_eq!(edge.target, target);
        assert_eq!(edge.kind, "similar_to");
        assert!(
            (edge.confidence - 0.91).abs() < 1e-6,
            "real similarity weight carried, not a bare count"
        );
    }

    #[test]
    fn merge_edge_links_accumulates_across_cycles_and_dedups_retries() {
        let (_dir, db) = open_db();
        let source = [3u8; 16];
        let n1 = [4u8; 16];
        let n2 = [5u8; 16];
        put_text(&db, source, "s");
        put_text(&db, n1, "n1");
        put_text(&db, n2, "n2");
        let id = MemoryId::from_be_bytes;

        // Cycle 1: one neighbour.
        merge_edge_links(&db, id(source), "similar_to", &[(id(source), id(n1), 0.9)]).unwrap();
        // Cycle 2: a different neighbour — must accumulate, not replace.
        merge_edge_links(&db, id(source), "similar_to", &[(id(source), id(n2), 0.86)]).unwrap();
        // A retry of cycle 1 (e.g. worker restart) must not duplicate.
        merge_edge_links(&db, id(source), "similar_to", &[(id(source), id(n1), 0.9)]).unwrap();

        let b = read_bundle(&db, source).expect("bundle present");
        let graph = b.graph.expect("graph populated");
        assert_eq!(graph.edges.len(), 2, "accumulated, deduped on retry");
        assert_eq!(graph.nodes.len(), 3, "source + two neighbours, deduped");
    }

    #[test]
    fn edge_merge_and_graph_merge_never_clobber_each_other() {
        // Simulates the extractor writing statement/relation edges into the
        // shared `graph` field, then auto_edge merging similar_to edges into
        // the SAME bundle — neither producer's edges may disappear.
        let (_dir, db) = open_db();
        let mid = [6u8; 16];
        let other = [7u8; 16];
        put_text(&db, mid, "m");
        put_text(&db, other, "o");

        // Extractor-shaped write: goes through the same replace-only-owned-
        // kinds path `merge_graph_from_committed` uses.
        let wtxn = db.write_txn().unwrap();
        merge_memory_artifact(&wtxn, mid, |bundle| {
            let existing = bundle.graph.take().unwrap_or_default();
            bundle.graph = Some(replace_owned_edges(
                existing,
                &EXTRACTOR_EDGE_KINDS,
                vec![EncodeGraphNode {
                    id: other,
                    name: "Acme Corp".to_string(),
                    kind: "entity".to_string(),
                    type_qname: "brain:Org".to_string(),
                }],
                vec![EncodeGraphEdge {
                    source: mid,
                    target: other,
                    predicate: "brain:worksAt".to_string(),
                    kind: "statement".to_string(),
                    confidence: 0.8,
                    event_at_unix_nanos: None,
                }],
            ));
        })
        .unwrap();
        wtxn.commit().unwrap();

        // auto_edge merges a similar_to edge into the same bundle.
        merge_edge_links(
            &db,
            MemoryId::from_be_bytes(mid),
            "similar_to",
            &[(
                MemoryId::from_be_bytes(mid),
                MemoryId::from_be_bytes(other),
                0.95,
            )],
        )
        .unwrap();

        let b = read_bundle(&db, mid).expect("bundle present");
        let graph = b.graph.expect("graph populated");
        let kinds: std::collections::HashSet<&str> =
            graph.edges.iter().map(|e| e.kind.as_str()).collect();
        assert!(
            kinds.contains("statement"),
            "extractor's edge survived auto_edge's merge"
        );
        assert!(
            kinds.contains("similar_to"),
            "auto_edge's edge is present alongside the extractor's"
        );
        assert_eq!(graph.edges.len(), 2);

        // Re-running the extractor merge (recomputing its full current
        // graph) must not drop the similar_to edge auto_edge added.
        let wtxn = db.write_txn().unwrap();
        merge_memory_artifact(&wtxn, mid, |bundle| {
            let existing = bundle.graph.take().unwrap_or_default();
            bundle.graph = Some(replace_owned_edges(
                existing,
                &EXTRACTOR_EDGE_KINDS,
                vec![EncodeGraphNode {
                    id: other,
                    name: "Acme Corp".to_string(),
                    kind: "entity".to_string(),
                    type_qname: "brain:Org".to_string(),
                }],
                vec![EncodeGraphEdge {
                    source: mid,
                    target: other,
                    predicate: "brain:worksAt".to_string(),
                    kind: "statement".to_string(),
                    confidence: 0.8,
                    event_at_unix_nanos: None,
                }],
            ));
        })
        .unwrap();
        wtxn.commit().unwrap();

        let b = read_bundle(&db, mid).expect("bundle present");
        let graph = b.graph.expect("graph populated");
        let kinds: std::collections::HashSet<&str> =
            graph.edges.iter().map(|e| e.kind.as_str()).collect();
        assert!(
            kinds.contains("similar_to"),
            "auto_edge's edge survives a re-run of the extractor's replace-own-kind merge"
        );
        assert_eq!(graph.edges.len(), 2, "no duplicate statement edge either");
    }

    fn priya_id() -> [u8; 16] {
        [1u8; 16]
    }

    /// "Priya's favorite color is blue." — the object is a literal with no
    /// same-named entity in the enrichment.
    fn literal_object_enrichment() -> GraphEnrichment {
        GraphEnrichment {
            entities: vec![EnrichedEntity {
                id: priya_id(),
                name: "Priya".into(),
                type_qname: "brain:person".into(),
            }],
            statements: vec![EnrichedStatement {
                id: [2u8; 16],
                subject_name: "Priya".into(),
                predicate: "favorite_color".into(),
                object_label: "blue".into(),
                confidence: 0.9,
                event_at_unix_nanos: None,
            }],
            relations: Vec::new(),
        }
    }

    #[test]
    fn literal_statement_object_renders_as_literal_node() {
        let graph = enrichment_to_graph(Some(literal_object_enrichment()));

        let literal_node = graph
            .nodes
            .iter()
            .find(|n| n.kind == "literal")
            .expect("a literal node must be synthesized for the literal-object statement");
        assert_eq!(literal_node.name, "blue");
        assert_eq!(literal_node.type_qname, "");
        assert_ne!(
            literal_node.id, [0u8; 16],
            "must not be the zero placeholder"
        );

        let edge = &graph.edges[0];
        assert_eq!(edge.kind, "statement");
        assert_eq!(edge.source, priya_id());
        assert_eq!(
            edge.target, literal_node.id,
            "edge target must point at the synthetic literal node, not the zero id"
        );
        assert_eq!(
            graph.nodes.len(),
            2,
            "one entity node plus one literal node"
        );
    }

    #[test]
    fn literal_node_id_is_deterministic() {
        let first = enrichment_to_graph(Some(literal_object_enrichment()));
        let second = enrichment_to_graph(Some(literal_object_enrichment()));
        let id_of = |g: &EncodeStageGraph| {
            g.nodes
                .iter()
                .find(|n| n.kind == "literal")
                .expect("literal node")
                .id
        };
        assert_eq!(
            id_of(&first),
            id_of(&second),
            "the same fact rendered twice must synthesize the same literal node id"
        );
    }

    #[test]
    fn repeated_literal_value_dedupes_to_one_node() {
        let mut enr = literal_object_enrichment();
        enr.statements.push(enr.statements[0].clone());
        let graph = enrichment_to_graph(Some(enr));

        let literal_nodes: Vec<_> = graph.nodes.iter().filter(|n| n.kind == "literal").collect();
        assert_eq!(
            literal_nodes.len(),
            1,
            "duplicate literal facts dedupe to one node"
        );
        assert_eq!(
            graph.edges.len(),
            2,
            "both statement edges are still emitted"
        );
        assert_eq!(graph.edges[0].target, graph.edges[1].target);
    }

    #[test]
    fn entity_statement_object_resolves_to_real_entity_node() {
        // A value object that DOES have a same-named entity (the "senior
        // engineer" role case) resolves to that entity, not a fresh literal.
        let enr = GraphEnrichment {
            entities: vec![
                EnrichedEntity {
                    id: priya_id(),
                    name: "Priya".into(),
                    type_qname: "brain:person".into(),
                },
                EnrichedEntity {
                    id: [3u8; 16],
                    name: "senior engineer".into(),
                    type_qname: "brain:role".into(),
                },
            ],
            statements: vec![EnrichedStatement {
                id: [2u8; 16],
                subject_name: "Priya".into(),
                predicate: "role".into(),
                object_label: "senior engineer".into(),
                confidence: 0.95,
                event_at_unix_nanos: None,
            }],
            relations: Vec::new(),
        };
        let graph = enrichment_to_graph(Some(enr));

        assert!(
            graph.nodes.iter().all(|n| n.kind != "literal"),
            "an entity-object statement must not synthesize a literal node"
        );
        assert_eq!(graph.edges[0].target, [3u8; 16]);
        assert_eq!(graph.nodes.len(), 2, "no duplicate node minted");
    }

    #[test]
    fn merge_edge_links_no_op_on_empty_links() {
        let (_dir, db) = open_db();
        let mid = [8u8; 16];
        merge_edge_links(&db, MemoryId::from_be_bytes(mid), "similar_to", &[]).unwrap();
        // No `MEMORY_ARTIFACTS` write txn was ever opened for an empty link
        // set, so the table itself doesn't exist yet — distinct from "table
        // exists but bundle absent" (which `read_bundle` would return `None`
        // for). Both mean "nothing was written," which is the contract here.
        let rtxn = db.read_txn().unwrap();
        assert!(
            rtxn.open_table(MEMORY_ARTIFACTS_TABLE).is_err(),
            "empty link set must not create the artifacts table"
        );
    }
}
