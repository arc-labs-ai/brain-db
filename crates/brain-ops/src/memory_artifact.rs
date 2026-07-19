//! Durable per-memory write-artifact bundle.
//!
//! A memory's write produces derived artifacts across several stages — the
//! embedding vector (sync), the stored metadata row (sync), the typed-graph
//! rows (async extractor), the analyzed lexical terms (async text-indexer),
//! and the hypothetical questions (async HyPE). Only the vector's *embedding*
//! and the graph *rows* survive in their native tables; the analyzed terms and
//! the HyPE question text are otherwise discarded once indexed/embedded. This
//! module keeps a friendly, denormalized copy of the whole set in one redb
//! table (`MEMORY_ARTIFACTS`, value = JSON of [`EncodeStageArtifact`]) so
//! `MEMORY_INSPECT` can show any memory's full write story later, not just the
//! one just written via the live ENCODE trace.
//!
//! ## Incremental population, no lost update
//!
//! The bundle is built up by several producers at different times:
//! - **sync** (apply, on the ack txn): vector + record, piggybacked on the
//!   memory-row write so it costs no extra transaction on the ack path.
//! - **async** (background workers, off the ack path): the extractor merges
//!   the graph, the text-indexer merges the analyzed terms, HyPE merges the
//!   generated questions.
//!
//! Every producer uses [`merge_memory_artifact`] — a read-modify-write inside
//! a single redb write txn. redb's exclusive write lock serializes concurrent
//! producers on the shard, so a later merge never clobbers an earlier one's
//! fields; each touches only its own portion of the bundle.

use brain_core::MemoryId;
use brain_metadata::tables::memory_artifacts::MEMORY_ARTIFACTS_TABLE;
use brain_metadata::MetadataDb;
use brain_protocol::envelope::response::{
    EncodeGraphEdge, EncodeGraphNode, EncodeStageArtifact, EncodeStageGraph, EncodeStageKeywordField,
    EncodeStageRecord,
};
use redb::{ReadableTable, WriteTransaction};

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
    Ok(())
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
    wtxn.commit().map_err(|e| format!("hype merge commit: {e}"))?;
    Ok(())
}

/// Assemble the typed-graph portion of the bundle by reading back the
/// committed graph for `memory_id`, then merge it in. Called by the extractor
/// worker **after** its graph commit, so the read sees the entities /
/// statements / relations it just wrote. Uses the same enrichment resolver
/// RECALL uses, so the content is real (canonical names, predicates,
/// confidences) rather than raw counts. Derives the memory's `(namespace,
/// agent)` scope from its own row so the enrichment stays tenant-scoped.
///
/// Best-effort: a read or merge failure returns `Err` for the caller to log,
/// never blocks the durable graph write (which already committed).
pub fn merge_graph_from_committed(metadata: &MetadataDb, memory_id: MemoryId) -> Result<(), String> {
    let graph = {
        let rtxn = metadata
            .read_txn()
            .map_err(|e| format!("graph merge read_txn: {e}"))?;
        let Some(scope) = memory_scope(&rtxn, memory_id)? else {
            // Memory row gone (e.g. hard-forgotten between commit and merge):
            // nothing to enrich, and the bundle was purged with the row.
            return Ok(());
        };
        let enr = crate::handlers::recall::fetch_enrichment_for(&[memory_id], scope, &rtxn)
            .map_err(|e| format!("graph enrichment: {e}"))?;
        enrichment_to_graph(enr.into_iter().next())
    };

    let wtxn = metadata
        .write_txn()
        .map_err(|e| format!("graph merge write_txn: {e}"))?;
    merge_memory_artifact(&wtxn, memory_id.to_be_bytes(), |bundle| {
        bundle.graph = Some(graph);
    })?;
    wtxn.commit().map_err(|e| format!("graph merge commit: {e}"))?;
    Ok(())
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

/// Read a memory's `(namespace, agent)` scope from its metadata row.
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
            brain_metadata::RowScope::from_bytes(m.namespace_id, m.agent_id_bytes)
        }))
}

/// Convert one memory's [`GraphEnrichment`](brain_protocol::envelope::response::GraphEnrichment)
/// into the bundle's [`EncodeStageGraph`]. Entities become nodes (they carry
/// ids); statement objects and relation endpoints are matched back to those
/// node ids by canonical name so edges reference real nodes when possible. An
/// endpoint the enrichment didn't surface as an entity (a literal object, or
/// an entity beyond the enrichment cap) resolves to the zero id — the renderer
/// still has the name via the edge's predicate context.
fn enrichment_to_graph(
    enr: Option<brain_protocol::envelope::response::GraphEnrichment>,
) -> EncodeStageGraph {
    let Some(enr) = enr else {
        return EncodeStageGraph::default();
    };

    let nodes: Vec<EncodeGraphNode> = enr
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
    let id_by_name: std::collections::HashMap<&str, [u8; 16]> =
        enr.entities.iter().map(|e| (e.name.as_str(), e.id)).collect();
    let lookup = |name: &str| -> [u8; 16] { id_by_name.get(name).copied().unwrap_or([0u8; 16]) };

    let mut edges: Vec<EncodeGraphEdge> = Vec::new();
    // Entity-object statements (subject → object via predicate).
    for s in &enr.statements {
        edges.push(EncodeGraphEdge {
            source: lookup(&s.subject_name),
            target: lookup(&s.object_label),
            predicate: s.predicate.clone(),
            kind: "statement".to_string(),
            confidence: s.confidence,
        });
    }
    // Typed relations (from → to via relation-type predicate).
    for r in &enr.relations {
        edges.push(EncodeGraphEdge {
            source: lookup(&r.from_name),
            target: lookup(&r.to_name),
            predicate: r.predicate.clone(),
            kind: "relation".to_string(),
            confidence: 1.0,
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
    use brain_protocol::envelope::response::EncodeStageArtifact;
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
}
