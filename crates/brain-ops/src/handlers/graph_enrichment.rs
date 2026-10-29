//! The projection of a memory's extracted graph — its mentioned entities,
//! the statements it sourced and the typed relations around them.
//!
//! Three paths with nothing else in common need exactly this shape. RECALL
//! hangs it off each hit when `include_graph` is set, ENCODE builds its
//! post-write trace from it, and the memory-artifact merge folds it into the
//! stored bundle. Two of the three reached into `recall.rs` for it, because
//! `include_graph` is a RECALL request field and RECALL asked first. The
//! projection itself knows nothing about a cue, a plan or a fused result —
//! it takes memory ids and a read txn — and ENCODE has no recall in it at
//! all.
//!
//! The module boundary is "what a memory's extracted graph looks like to a
//! client", not "which verb first wanted to show it".

use std::collections::HashSet;

use brain_core::MemoryId;

use crate::error::OpError;

/// Per-hit opaque-body enrichment populated when the request carries
/// `include_graph = true`. One redb read txn serves all hits; per hit we
/// issue a small handful of point/range reads.
///
/// One `GraphEnrichment` per input id, in input order and without
/// deduplication — callers pair the two lists positionally, so a repeated
/// id yields a repeated entry and the two vectors always have the same
/// length. An id that resolves to nothing comes back as an entry whose
/// three lists are empty: "extracted, found nothing" and "not extracted"
/// are the same answer here, because every catalog table is materialized
/// at `MetadataDb::open`, so table presence proves nothing about whether
/// a memory went through the extractors.
///
/// Caps:
///   * entities  — first 16 mentioned (mention order)
///   * statements — top 5 by `confidence` desc, tombstoned skipped
///   * relations  — top 5 by `created_at_unix_nanos` desc, both
///     incoming and outgoing typed edges incident to mentioned
///     entities
pub(crate) fn fetch_enrichment_for(
    memory_ids: &[MemoryId],
    scope: brain_metadata::RowScope,
    session_filter: Option<&HashSet<u64>>,
    rtxn: &redb::ReadTransaction,
) -> Result<Vec<brain_protocol::envelope::response::GraphEnrichment>, OpError> {
    use brain_core::{EdgeKindRef, NodeRef};
    use brain_core::{EntityId, StatementId, SubjectRef};
    use brain_metadata::entity::ops::entity_get;
    use brain_metadata::relation::types::relation_type_get;
    use brain_metadata::schema::predicate::predicate_get;
    use brain_metadata::tables::edge::{walk_incoming, walk_outgoing};
    use brain_metadata::tables::entity_type::ENTITY_TYPES_TABLE;
    use brain_metadata::tables::relation::{RelationMetadata, RELATION_METADATA_TABLE};
    use brain_metadata::tables::statement::{
        statement_from_metadata, StatementMetadata, STATEMENTS_BY_EVIDENCE_TABLE, STATEMENTS_TABLE,
    };
    use brain_protocol::envelope::response::{
        EnrichedEntity, EnrichedRelation, EnrichedStatement, GraphEnrichment,
    };

    const ENTITY_CAP: usize = 16;
    const STATEMENT_CAP: usize = 5;
    const RELATION_CAP: usize = 5;
    let entity_types = rtxn.open_table(ENTITY_TYPES_TABLE).ok();
    let evidence_table = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).map_err(|e| {
        OpError::Internal(format!(
            "include_graph: open STATEMENTS_BY_EVIDENCE_TABLE: {e}"
        ))
    })?;
    // The statement/relation sidecars carry the per-utterance `session_id`;
    // read them directly so `session_filter` (already applied to memories)
    // also gates the graph rows — "session N" shows its memories AND its
    // graph. Session is a grouping column, never a key.
    let statements_table = rtxn
        .open_table(STATEMENTS_TABLE)
        .map_err(|e| OpError::Internal(format!("include_graph: open STATEMENTS_TABLE: {e}")))?;
    let relation_meta_table = rtxn.open_table(RELATION_METADATA_TABLE).map_err(|e| {
        OpError::Internal(format!("include_graph: open RELATION_METADATA_TABLE: {e}"))
    })?;

    let mut out: Vec<GraphEnrichment> = Vec::with_capacity(memory_ids.len());
    for &memory_id in memory_ids {
        // 1. Mentioned entities (walk Mentions edges from memory).
        let mention_rows = walk_outgoing(
            rtxn,
            NodeRef::Memory(memory_id),
            Some(EdgeKindRef::Mentions),
        )
        .map_err(|e| OpError::Internal(format!("include_graph: walk_outgoing(Mentions): {e}")))?;
        let entity_ids: Vec<EntityId> = mention_rows
            .iter()
            .filter_map(|(_, to, _, _)| match to {
                NodeRef::Entity(eid) => Some(*eid),
                _ => None,
            })
            .collect();

        let mut enriched_entities: Vec<EnrichedEntity> =
            Vec::with_capacity(entity_ids.len().min(ENTITY_CAP));
        for eid in entity_ids.iter().take(ENTITY_CAP) {
            let Some(ent) = entity_get(rtxn, *eid)
                .map_err(|e| OpError::Internal(format!("include_graph: entity_get: {e}")))?
            else {
                continue;
            };
            let type_name = entity_types
                .as_ref()
                .and_then(|t| t.get(&ent.entity_type.raw()).ok().flatten())
                .map(|g| g.value().name)
                .unwrap_or_default();
            enriched_entities.push(EnrichedEntity {
                id: eid.to_bytes(),
                name: ent.canonical_name,
                type_qname: type_name,
            });
        }

        // 2. Statements sourced by this memory. STATEMENTS_BY_EVIDENCE
        // keys are `(MemoryId.to_be_bytes(), StatementId.to_bytes())`.
        let mut enriched_statements: Vec<EnrichedStatement> = Vec::new();
        {
            let mid = memory_id.to_be_bytes();
            // STATEMENTS_BY_EVIDENCE is now scoped: the key is
            // `(namespace_id, space_id_bytes, MemoryId, StatementId)`.
            // Restrict the range to the caller's scope so the evidence
            // scan can never cross the tenant boundary.
            let lo = (scope.namespace_id, scope.space_id_bytes, mid, [0u8; 16]);
            let hi = (scope.namespace_id, scope.space_id_bytes, mid, [0xFFu8; 16]);
            let mut stmts: Vec<brain_core::Statement> = Vec::new();
            for entry in evidence_table
                .range(lo..=hi)
                .map_err(|e| OpError::Internal(format!("include_graph: evidence range: {e}")))?
            {
                let (k, _v) = entry
                    .map_err(|e| OpError::Internal(format!("include_graph: evidence row: {e}")))?;
                let (_ns, _space, _mem_bytes, sid_bytes) = k.value();
                let sid = StatementId::from_bytes(sid_bytes);
                let row: Option<StatementMetadata> = statements_table
                    .get(&sid.to_bytes())
                    .map_err(|e| OpError::Internal(format!("include_graph: statement row: {e}")))?
                    .map(|g| g.value());
                let Some(m) = row else {
                    continue;
                };
                // Session coherence: drop statements outside the requested
                // session(s) so a session-scoped read's graph matches its
                // memories.
                if let Some(sf) = session_filter {
                    if !sf.contains(&m.session_id) {
                        continue;
                    }
                }
                if let Some(stmt) = statement_from_metadata(&m) {
                    if !stmt.tombstoned {
                        stmts.push(stmt);
                    }
                }
            }
            stmts.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for stmt in stmts.into_iter().take(STATEMENT_CAP) {
                let subject_name = match stmt.subject {
                    SubjectRef::Entity(eid) => entity_get(rtxn, eid)
                        .ok()
                        .flatten()
                        .map(|e| e.canonical_name)
                        .unwrap_or_default(),
                    _ => "(ambiguous)".to_string(),
                };
                let predicate = predicate_get(rtxn, stmt.predicate)
                    .ok()
                    .flatten()
                    .map(|p| p.canonical())
                    .unwrap_or_default();
                let object_label = match &stmt.object {
                    brain_core::StatementObject::Entity(eid) => entity_get(rtxn, *eid)
                        .ok()
                        .flatten()
                        .map(|e| e.canonical_name)
                        .unwrap_or_default(),
                    // Render the inner value, not its Debug form: a bare
                    // `Text("acceptance")` wrapper must never leak into the
                    // client-facing enrichment label (or the eval report).
                    brain_core::StatementObject::Value(v) => match v {
                        brain_core::StatementValue::Text(s) => s.clone(),
                        brain_core::StatementValue::Integer(n) => n.to_string(),
                        brain_core::StatementValue::Float(f) => f.to_string(),
                        brain_core::StatementValue::Bool(b) => b.to_string(),
                        brain_core::StatementValue::UnixNanos(t) => t.to_string(),
                        brain_core::StatementValue::Blob(b) => format!("<{} bytes>", b.len()),
                    },
                    brain_core::StatementObject::Memory(mid) => {
                        format!("memory:{:x?}", mid.to_be_bytes())
                    }
                    brain_core::StatementObject::Statement(sid) => {
                        format!("statement:{:x?}", sid.to_bytes())
                    }
                };
                // The object's identity, not just its rendering. A literal
                // stays `None` even when its text equals an entity's name.
                let object_entity_id = match &stmt.object {
                    brain_core::StatementObject::Entity(eid) => Some(eid.to_bytes()),
                    _ => None,
                };
                enriched_statements.push(EnrichedStatement {
                    id: stmt.id.to_bytes(),
                    subject_name,
                    predicate,
                    object_label,
                    object_entity_id,
                    confidence: stmt.confidence,
                    event_at_unix_nanos: stmt.event_at_unix_nanos,
                });
            }
        }

        // 3. Typed relations incident to any mentioned entity. Both
        // directions; top RELATION_CAP by created_at desc across the
        // pool. A relation whose BOTH endpoints are mentioned by this
        // memory is reachable twice — once as an outgoing edge from one
        // endpoint, once as an incoming edge to the other — so dedup on
        // the edge identity `(from, type, to)` to emit each relation once.
        let mut all_rels: Vec<(u64, EnrichedRelation)> = Vec::new();
        let mut seen_rels: std::collections::HashSet<([u8; 16], u32, [u8; 16])> =
            std::collections::HashSet::new();
        for eid in &entity_ids {
            for outgoing in [true, false] {
                let rows = if outgoing {
                    walk_outgoing(rtxn, NodeRef::Entity(*eid), None)
                } else {
                    walk_incoming(rtxn, NodeRef::Entity(*eid), None)
                }
                .map_err(|e| OpError::Internal(format!("include_graph: walk relation: {e}")))?;
                for (kind, other, disamb, data) in rows {
                    let typed_id = match kind {
                        EdgeKindRef::Typed(rt_id) => rt_id,
                        _ => continue,
                    };
                    let other_entity = match other {
                        NodeRef::Entity(oid) => oid,
                        _ => continue,
                    };
                    // Session coherence: the typed-edge disambiguator is the
                    // relation id, keying its sidecar (which carries the
                    // per-utterance session). Drop relations outside the
                    // requested session(s) so a session-scoped read's graph
                    // matches its memories.
                    if let Some(sf) = session_filter {
                        let sidecar: Option<RelationMetadata> = relation_meta_table
                            .get(&disamb)
                            .map_err(|e| {
                                OpError::Internal(format!("include_graph: relation sidecar: {e}"))
                            })?
                            .map(|g| g.value());
                        match sidecar {
                            Some(rm) if sf.contains(&rm.session_id) => {}
                            // Drop both a foreign-session relation and one
                            // whose sidecar is missing (can't prove it belongs).
                            _ => continue,
                        }
                    }
                    let Some(rt) = relation_type_get(rtxn, typed_id).map_err(|e| {
                        OpError::Internal(format!("include_graph: relation_type_get: {e}"))
                    })?
                    else {
                        continue;
                    };
                    let (from_id, to_id) = if outgoing {
                        (*eid, other_entity)
                    } else {
                        (other_entity, *eid)
                    };
                    // Skip the mirror image of a relation already recorded
                    // from its other endpoint.
                    if !seen_rels.insert((from_id.to_bytes(), typed_id.raw(), to_id.to_bytes())) {
                        continue;
                    }
                    let from_name = entity_get(rtxn, from_id)
                        .ok()
                        .flatten()
                        .map(|e| e.canonical_name)
                        .unwrap_or_default();
                    let to_name = entity_get(rtxn, to_id)
                        .ok()
                        .flatten()
                        .map(|e| e.canonical_name)
                        .unwrap_or_default();
                    all_rels.push((
                        data.created_at_unix_nanos,
                        EnrichedRelation {
                            from_name,
                            predicate: rt.canonical(),
                            to_name,
                        },
                    ));
                }
            }
        }
        all_rels.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        let enriched_relations: Vec<EnrichedRelation> = all_rels
            .into_iter()
            .take(RELATION_CAP)
            .map(|(_, r)| r)
            .collect();

        out.push(GraphEnrichment {
            entities: enriched_entities,
            statements: enriched_statements,
            relations: enriched_relations,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::{Entity, EntityId, EntityType, NodeRef, SessionId};
    use brain_metadata::entity::ops::entity_put;
    use brain_metadata::tables::edge::{
        link, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
    };
    use brain_metadata::{MetadataDb, RowScope};
    use brain_protocol::envelope::response::GraphEnrichment;

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn scope() -> RowScope {
        RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
    }

    fn mem(n: u128) -> MemoryId {
        MemoryId::from(n)
    }

    /// A fresh metadata DB. `MetadataDb::open` seeds the system schema, so
    /// the Person entity type and every catalog table already exist.
    fn fresh_db(dir: &tempfile::TempDir) -> MetadataDb {
        MetadataDb::open(dir.path().join("meta.redb")).expect("open metadata")
    }

    /// Give `memory_id` a mentioned entity, both the edge and the entity
    /// row — the minimum that makes an id "resolve" to a non-empty
    /// enrichment.
    fn mention(db: &MetadataDb, memory_id: MemoryId, name: &str) -> EntityId {
        let entity = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            name.to_owned(),
            name.to_lowercase(),
            NOW,
        );
        let id = entity.id;
        let wtxn = db.write_txn().expect("write txn");
        entity_put(&wtxn, scope(), SessionId::DEFAULT, &entity).expect("entity_put");
        {
            let mut e = wtxn.open_table(EDGES_TABLE).expect("edges");
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).expect("edges_reverse");
            let data = EdgeData::new(
                1.0,
                brain_metadata::tables::edge::origin::AUTO_DERIVED,
                brain_metadata::tables::edge::derived_by::SIMILARITY_WORKER,
                NOW,
            );
            link(
                &mut e,
                &mut r,
                NodeRef::Memory(memory_id),
                brain_core::EdgeKindRef::Mentions,
                NodeRef::Entity(id),
                zero_disambiguator(),
                &data,
            )
            .expect("link");
        }
        wtxn.commit().expect("commit");
        id
    }

    /// A Mentions edge with no entity row behind it. The extractor writes
    /// the edge and the row in one txn, so this is a torn state — but a
    /// hard-forgotten entity leaves the same shape behind.
    fn dangling_mention(db: &MetadataDb, memory_id: MemoryId) {
        let wtxn = db.write_txn().expect("write txn");
        {
            let mut e = wtxn.open_table(EDGES_TABLE).expect("edges");
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).expect("edges_reverse");
            let data = EdgeData::new(
                1.0,
                brain_metadata::tables::edge::origin::AUTO_DERIVED,
                brain_metadata::tables::edge::derived_by::SIMILARITY_WORKER,
                NOW,
            );
            link(
                &mut e,
                &mut r,
                NodeRef::Memory(memory_id),
                brain_core::EdgeKindRef::Mentions,
                NodeRef::Entity(EntityId::new()),
                zero_disambiguator(),
                &data,
            )
            .expect("link");
        }
        wtxn.commit().expect("commit");
    }

    fn fetch(db: &MetadataDb, ids: &[MemoryId]) -> Vec<GraphEnrichment> {
        let rtxn = db.read_txn().expect("read txn");
        fetch_enrichment_for(ids, scope(), None, &rtxn).expect("enrichment")
    }

    fn is_empty(g: &GraphEnrichment) -> bool {
        g.entities.is_empty() && g.statements.is_empty() && g.relations.is_empty()
    }

    /// No ids is not an error. The helper still opens its three tables, so
    /// "nothing to enrich" and "the tables aren't there" would be the same
    /// call shape — RECALL with `include_graph` on and an empty result set
    /// takes this path on every miss.
    #[test]
    fn empty_id_list_yields_an_empty_vec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        assert!(fetch(&db, &[]).is_empty());
    }

    /// An id with nothing behind it comes back as an entry with three empty
    /// lists, NOT as a skipped entry. Dropping it would shorten the vector
    /// and silently shift every later caller's pairing by one.
    #[test]
    fn unresolvable_ids_get_an_empty_entry_each() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        let out = fetch(&db, &[mem(1), mem(2), mem(3)]);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(is_empty), "{out:?}");
    }

    /// The contract RECALL leans on: it zips its id list against this
    /// result, so entry `i` must describe input `i` whether or not input
    /// `i` resolved. A helper that only emitted entries for resolving ids
    /// would hand every later memory its neighbour's graph.
    #[test]
    fn a_mix_stays_positionally_aligned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        mention(&db, mem(2), "Priya");

        let out = fetch(&db, &[mem(1), mem(2), mem(3)]);
        assert_eq!(out.len(), 3);
        assert!(is_empty(&out[0]), "unknown id 1: {:?}", out[0]);
        assert_eq!(out[1].entities.len(), 1, "the one resolving id");
        assert_eq!(out[1].entities[0].name, "Priya");
        assert!(is_empty(&out[2]), "unknown id 3: {:?}", out[2]);
    }

    /// Input order is the output order — there is no sort. Swapping the
    /// inputs swaps the outputs, which is what makes positional pairing
    /// safe for a caller that built its id list in rank order.
    #[test]
    fn input_order_is_the_output_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        mention(&db, mem(1), "Priya");
        mention(&db, mem(2), "Sam");

        let forward = fetch(&db, &[mem(1), mem(2)]);
        assert_eq!(forward[0].entities[0].name, "Priya");
        assert_eq!(forward[1].entities[0].name, "Sam");

        let reversed = fetch(&db, &[mem(2), mem(1)]);
        assert_eq!(reversed[0].entities[0].name, "Sam");
        assert_eq!(reversed[1].entities[0].name, "Priya");
    }

    /// Repeated ids are NOT deduplicated — one entry per input slot, even
    /// when two slots name the same memory. A caller that wants unique
    /// memories has to dedupe its own id list; RECALL's `zip` into a
    /// HashMap collapses the repeat, which hides this from that call site
    /// but not from ENCODE or the artifact merge.
    #[test]
    fn repeated_ids_are_not_deduplicated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        mention(&db, mem(1), "Priya");

        let out = fetch(&db, &[mem(1), mem(1), mem(1)]);
        assert_eq!(out.len(), 3, "one entry per slot, not per distinct id");
        for g in &out {
            assert_eq!(g.entities.len(), 1);
            assert_eq!(g.entities[0].name, "Priya");
        }
    }

    /// A Mentions edge whose entity row is gone is skipped, not rendered
    /// as a nameless entity. The enrichment is client-facing, and an entry
    /// with an empty name and a real id reads as a corrupt entity rather
    /// than as the absence it is.
    #[test]
    fn a_dangling_mention_is_skipped_not_blanked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        mention(&db, mem(1), "Priya");
        dangling_mention(&db, mem(1));

        let out = fetch(&db, &[mem(1)]);
        assert_eq!(
            out[0].entities.len(),
            1,
            "only the resolvable mention survives: {:?}",
            out[0].entities
        );
        assert_eq!(out[0].entities[0].name, "Priya");
    }

    /// The entity list is capped at 16. A memory that mentions more has
    /// its list truncated rather than growing the response unboundedly —
    /// the cap is on the output, not on how many edges are walked.
    #[test]
    fn the_entity_list_is_capped_at_sixteen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        for i in 0..20 {
            mention(&db, mem(1), &format!("Person{i}"));
        }

        let out = fetch(&db, &[mem(1)]);
        assert_eq!(out[0].entities.len(), 16);
    }

    /// Two memories mentioning different entities don't bleed into each
    /// other: the mention walk is anchored on the memory node, so each
    /// entry sees only its own memory's edges.
    #[test]
    fn one_memorys_mentions_stay_out_of_anothers_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fresh_db(&dir);
        mention(&db, mem(1), "Priya");
        mention(&db, mem(2), "Sam");

        let out = fetch(&db, &[mem(1), mem(2)]);
        assert_eq!(out[0].entities.len(), 1);
        assert_eq!(out[0].entities[0].name, "Priya");
        assert_eq!(out[1].entities.len(), 1);
        assert_eq!(out[1].entities[0].name, "Sam");
    }
}
