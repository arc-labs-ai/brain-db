//! FORGET cascade operations (sub-task 24.2). Spec §25/00
//! §"Cascading effects of FORGET".
//!
//! When a memory is forgotten, statements / relations whose
//! evidence list referenced it must be updated:
//!
//! 1. Drop `memory_id` from `evidence_inline` (and overflow, when
//!    we get there post-v1).
//! 2. Recompute `confidence` from the remaining evidence per
//!    §25/00 §"Confidence aggregation across evidence".
//! 3. If evidence becomes empty AND confidence < threshold,
//!    tombstone with reason `SourceMemoryForgotten`.
//!
//! ## v1 scope cuts
//!
//! - Overflow evidence lists (post-`INLINE_EVIDENCE_CAP = 8`) are
//!   **not** searched in v1. Statements with overflow evidence
//!   containing the forgotten memory keep the evidence entry; the
//!   v1 confidence is still recomputed on the inline-only set.
//!   Full overflow-aware cascade is a post-v1 enhancement.
//! - Relations are scanned but only have a single-evidence link
//!   per the v1 schema; if that evidence equals `memory_id`, the
//!   relation is tombstoned.
//!
//! ## Audit
//!
//! Audit-event semantics for the cascade live in §25/00 §"The
//! audit log" but the v1 `audit_ops::audit_write` API targets
//! extraction events. Cascade audit rows land as a post-v1
//! enhancement; the cascade still updates the row, so an
//! external observer can see the change via the change feed.

use brain_core::knowledge::TombstoneReason;
use brain_core::{EdgeKindRef, MemoryId, NodeRef, RelationId};
use redb::{ReadableTable, WriteTransaction};

use crate::relation_ops::{relation_tombstone, RelationOpError};
use crate::statement_ops::{statement_tombstone, StatementOpError};
use crate::tables::edge::{self, EdgeKey, EDGES_REVERSE_TABLE, EDGES_TABLE};
use crate::tables::knowledge::relation::{RELATION_BY_EVIDENCE_TABLE, RELATION_METADATA_TABLE};
use crate::tables::knowledge::statement::{EvidenceEntryRow, StatementMetadata, STATEMENTS_TABLE};

/// Default confidence threshold below which a statement that
/// loses its only piece of evidence is tombstoned. Configurable
/// at the caller; spec §25/00 doesn't pin a number.
pub const DEFAULT_CASCADE_CONFIDENCE_THRESHOLD: f32 = 0.2;

/// Outcome of cascading one FORGET against one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum CascadeOutcome {
    /// Evidence list shrank; statement kept.
    EvidenceDropped { new_confidence: f32 },
    /// Evidence became empty AND confidence stayed above
    /// threshold — the row is kept with `stale_evidence`
    /// semantics (statement count unchanged, but the operator
    /// can re-extract).
    KeptStaleEvidence { confidence: f32 },
    /// Tombstoned with reason `SourceMemoryForgotten`.
    Tombstoned,
    /// The statement did not reference this memory.
    Untouched,
}

/// Per-cascade aggregate summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CascadeSummary {
    pub scanned: u64,
    pub evidence_dropped: u64,
    pub kept_stale: u64,
    pub tombstoned: u64,
}

/// Apply the cascade for `memory_id` against every active
/// statement. Returns counts.
///
/// `batch_cap` bounds the scan in a single txn so heavily-
/// referenced memories don't produce an unbounded wtxn. Spec
/// §27/04 §4.5 ("continuation jobs") tracks the post-v1
/// follow-up that resumes from a cursor when the batch cap is
/// hit.
///
/// `confidence_threshold` follows the [`DEFAULT_CASCADE_CONFIDENCE_THRESHOLD`]
/// when the caller doesn't override.
pub fn cascade_forget_to_statements(
    wtxn: &WriteTransaction,
    memory_id: MemoryId,
    confidence_threshold: f32,
    batch_cap: usize,
    now_unix_nanos: u64,
) -> Result<CascadeSummary, StatementOpError> {
    let mut summary = CascadeSummary::default();
    let memory_bytes = memory_id.to_be_bytes();

    // Collect affected statement_ids first; then mutate per-row.
    // Snapshot-then-update avoids interleaving redb reads and
    // writes against the same table.
    let mut affected: Vec<(StatementMetadata, Vec<EvidenceEntryRowLike>)> = Vec::new();
    {
        let table = wtxn.open_table(STATEMENTS_TABLE)?;
        for entry in table.iter()? {
            let (_, v) = entry?;
            let row = v.value();
            summary.scanned += 1;
            if row.is_tombstoned() {
                continue;
            }
            let referenced = row
                .evidence_inline
                .iter()
                .any(|e| e.memory_id_bytes == memory_bytes);
            if !referenced {
                continue;
            }
            let remaining: Vec<EvidenceEntryRowLike> = row
                .evidence_inline
                .iter()
                .filter(|e| e.memory_id_bytes != memory_bytes)
                .map(|e| EvidenceEntryRowLike {
                    memory_id_bytes: e.memory_id_bytes,
                    confidence_milli: e.confidence_milli,
                    timestamp_unix_nanos: e.timestamp_unix_nanos,
                    extractor_id: e.extractor_id,
                })
                .collect();
            affected.push((row, remaining));
            if affected.len() >= batch_cap {
                break;
            }
        }
    }

    // Apply mutations. Each affected statement either becomes
    // evidence-shrunk + confidence-recomputed, or tombstoned.
    //
    // The mutation-side table handle is hoisted out of the loop so
    // we don't pay the open-table cost N times per cascade. We drop
    // it before any `statement_tombstone` call because that helper
    // opens the same table itself and redb prefers one handle in
    // flight.
    let mut to_tombstone: Vec<brain_core::StatementId> = Vec::new();
    {
        let mut table = wtxn.open_table(STATEMENTS_TABLE)?;
        for (mut row, remaining) in affected {
            if remaining.is_empty() {
                // Empty inline evidence — recompute confidence from the empty set
                // and decide tombstone vs keep.
                let new_conf = if row.evidence_overflow_id_bytes.is_some() {
                    // Overflow case: we don't crack open the overflow row in v1,
                    // so we keep the existing confidence and flag the row stale.
                    row.confidence
                } else {
                    0.0
                };
                if new_conf < confidence_threshold && row.evidence_overflow_id_bytes.is_none() {
                    // Defer to a second loop so we can drop the table handle
                    // before statement_tombstone re-opens it.
                    to_tombstone.push(row.statement_id());
                } else {
                    row.evidence_inline.clear();
                    row.confidence = new_conf;
                    table.insert(&row.statement_id_bytes, &row)?;
                    summary.kept_stale += 1;
                }
            } else {
                // Drop the forgotten memory; recompute confidence as a simple
                // mean over the surviving inline entries (spec §25/00 formula
                // takes decay into account; we use the row's stored confidence
                // values which already reflect per-evidence weighting). Full
                // §25/00 formula application is post-v1.
                let new_conf = mean_confidence(&remaining);
                row.evidence_inline = remaining
                    .into_iter()
                    .map(|e| EvidenceEntryRow {
                        memory_id_bytes: e.memory_id_bytes,
                        confidence_milli: e.confidence_milli,
                        timestamp_unix_nanos: e.timestamp_unix_nanos,
                        extractor_id: e.extractor_id,
                    })
                    .collect();
                row.confidence = new_conf;
                table.insert(&row.statement_id_bytes, &row)?;
                summary.evidence_dropped += 1;
            }
        }
    } // table handle dropped here, before statement_tombstone re-opens.

    for id in to_tombstone {
        statement_tombstone(
            wtxn,
            id,
            TombstoneReason::SourceMemoryForgotten,
            now_unix_nanos,
        )?;
        summary.tombstoned += 1;
    }

    Ok(summary)
}

// ---------------------------------------------------------------------------
// Edge + relation cascade (Phase C wiring).
// ---------------------------------------------------------------------------

/// Per-cascade summary for the unified-edge sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EdgeCascadeSummary {
    /// Substrate / mention edges removed from `EDGES_TABLE` +
    /// `EDGES_REVERSE_TABLE` (paired rows).
    pub substrate_unlinked: u64,
    /// Typed relations the forgotten memory was the SOLE evidence
    /// for — flipped to tombstoned via `relation_tombstone`.
    pub relations_tombstoned: u64,
    /// Typed relations with OTHER evidence — the
    /// `(memory_id, relation_id)` row was dropped from
    /// `RELATION_BY_EVIDENCE_TABLE` and the sidecar's
    /// `evidence_inline` shrank.
    pub relations_evidence_dropped: u64,
}

/// Cascade unified-edge cleanup for a forgotten memory.
///
/// Steps (all inside `wtxn`):
/// 1. Walk `EDGES_TABLE[(NodeRef::Memory(memory_id), *, *)]` —
///    every outgoing substrate/mention/typed edge anchored at the
///    forgotten memory. Typed edges are deferred to step 3.
///    Builtin + Mentions edges are unlinked (forward + reverse, with
///    symmetric Builtin auto-mirror).
/// 2. Walk `EDGES_REVERSE_TABLE[(NodeRef::Memory(memory_id), *, *)]`
///    — every incoming substrate/mention edge. Same treatment.
/// 3. Walk `RELATION_BY_EVIDENCE_TABLE[(memory_bytes, *)]` — every
///    typed relation citing this memory. For each:
///    - Drop the `(memory_id, relation_id)` evidence row.
///    - Reload the sidecar; remove `memory_bytes` from
///      `evidence_inline`. If the inline list becomes empty AND no
///      overflow row exists, call `relation_tombstone` (reason left
///      to the caller's audit pipeline).
///    - Otherwise persist the shrunken sidecar.
pub fn cascade_forget_to_edges(
    wtxn: &WriteTransaction,
    memory_id: MemoryId,
    now_unix_nanos: u64,
) -> Result<EdgeCascadeSummary, EdgeCascadeError> {
    let mut summary = EdgeCascadeSummary::default();
    let anchor = NodeRef::Memory(memory_id);

    // Snapshot phase: collect every non-Typed edge row keyed at this
    // memory in either direction. We separate read and mutate so the
    // edge::unlink helper can re-open both tables in its own scope.
    let mut substrate_keys: Vec<EdgeKey> = Vec::new();
    {
        let out_t = wtxn.open_table(EDGES_TABLE)?;
        let prefix = EdgeKey::from_prefix(anchor).to_vec();
        let mut hi = prefix.clone();
        hi.extend_from_slice(&[0xFFu8; EdgeKindRef::MAX_BYTES + NodeRef::BYTES + 16]);
        for entry in out_t.range::<&[u8]>(prefix.as_slice()..=hi.as_slice())? {
            let (k, _) = entry?;
            let key = EdgeKey::decode(k.value())?;
            if key.from != anchor {
                continue;
            }
            if matches!(key.kind, EdgeKindRef::Typed(_)) {
                // Typed edges are owned by their sidecar; step 3
                // handles them. The relation tombstone path keeps
                // the edge row intact.
                continue;
            }
            substrate_keys.push(key);
        }
    }
    {
        let rev_t = wtxn.open_table(EDGES_REVERSE_TABLE)?;
        let prefix = EdgeKey::from_prefix(anchor).to_vec();
        let mut hi = prefix.clone();
        hi.extend_from_slice(&[0xFFu8; EdgeKindRef::MAX_BYTES + NodeRef::BYTES + 16]);
        for entry in rev_t.range::<&[u8]>(prefix.as_slice()..=hi.as_slice())? {
            let (k, _) = entry?;
            let key = EdgeKey::decode(k.value())?;
            if key.from != anchor {
                continue;
            }
            if matches!(key.kind, EdgeKindRef::Typed(_)) {
                continue;
            }
            // The reverse-table key is `(to, kind, from)` from the
            // forward perspective. Flip back so unlink's canonical
            // shape matches. We dedup against `substrate_keys` to
            // avoid removing a row twice (the forward sweep already
            // captured edges where this memory is the source).
            let canonical = EdgeKey {
                from: key.to,
                kind: key.kind,
                to: key.from,
                disambiguator: key.disambiguator,
            };
            if !substrate_keys.contains(&canonical) {
                substrate_keys.push(canonical);
            }
        }
    }

    // Mutate phase: substrate / mention unlinks.
    if !substrate_keys.is_empty() {
        let mut out_t = wtxn.open_table(EDGES_TABLE)?;
        let mut rev_t = wtxn.open_table(EDGES_REVERSE_TABLE)?;
        for key in substrate_keys {
            let removed = edge::unlink(
                &mut out_t,
                &mut rev_t,
                key.from,
                key.kind,
                key.to,
                key.disambiguator,
            )?;
            if removed {
                summary.substrate_unlinked += 1;
            }
        }
    }

    // Snapshot phase: typed relations citing this memory.
    let mut relation_ids: Vec<RelationId> = Vec::new();
    {
        let by_ev = wtxn.open_table(RELATION_BY_EVIDENCE_TABLE)?;
        let mem_bytes = memory_id.to_be_bytes();
        let lo = (mem_bytes, [0u8; 16]);
        let hi = (mem_bytes, [0xFFu8; 16]);
        for entry in by_ev.range(lo..=hi)? {
            let (k, _) = entry?;
            let (k_mem, k_rel) = k.value();
            if k_mem != mem_bytes {
                continue;
            }
            relation_ids.push(RelationId::from(k_rel));
        }
    }

    let mut to_tombstone: Vec<RelationId> = Vec::new();
    {
        let mut by_ev = wtxn.open_table(RELATION_BY_EVIDENCE_TABLE)?;
        let mut sidecar = wtxn.open_table(RELATION_METADATA_TABLE)?;
        let mem_bytes = memory_id.to_be_bytes();
        for rel_id in &relation_ids {
            let rel_bytes = rel_id.to_bytes();
            by_ev.remove(&(mem_bytes, rel_bytes))?;
            let Some(mut meta) = sidecar.get(&rel_bytes)?.map(|g| g.value()) else {
                // Sidecar already gone; skip — the BY_EVIDENCE row
                // was stale.
                continue;
            };
            let before = meta.evidence_inline.len();
            meta.evidence_inline.retain(|e| *e != mem_bytes);
            let shrank = meta.evidence_inline.len() < before;
            if meta.evidence_inline.is_empty() && !meta.is_tombstoned() {
                // Sole evidence — defer tombstone until after the
                // sidecar handle is dropped, since
                // `relation_tombstone` reopens the same table.
                to_tombstone.push(*rel_id);
            } else if shrank {
                sidecar.insert(&rel_bytes, &meta)?;
                summary.relations_evidence_dropped += 1;
            }
        }
    }

    for rel_id in to_tombstone {
        match relation_tombstone(wtxn, rel_id, now_unix_nanos) {
            Ok(()) => summary.relations_tombstoned += 1,
            Err(RelationOpError::NotFound(_)) => {
                // Sidecar disappeared between snapshot and apply —
                // benign (concurrent tombstone). Don't fail the
                // cascade.
            }
            Err(e) => return Err(EdgeCascadeError::Relation(e)),
        }
    }

    Ok(summary)
}

#[derive(thiserror::Error, Debug)]
pub enum EdgeCascadeError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("edge op error: {0}")]
    EdgeOp(#[from] crate::tables::edge::EdgeOpError),

    #[error("edge key decode error: {0}")]
    EdgeKey(#[from] crate::tables::edge::EdgeKeyError),

    #[error("relation op error: {0}")]
    Relation(#[from] RelationOpError),
}

// Local mirror so cascade_ops doesn't depend on the row layout
// from brain-metadata's table module directly. The two are kept
// in sync; cascade-side processing is the only consumer.
struct EvidenceEntryRowLike {
    memory_id_bytes: [u8; 16],
    confidence_milli: u16,
    timestamp_unix_nanos: u64,
    extractor_id: u32,
}

fn mean_confidence(entries: &[EvidenceEntryRowLike]) -> f32 {
    if entries.is_empty() {
        return 0.0;
    }
    let sum: f32 = entries
        .iter()
        .map(|e| f32::from(e.confidence_milli) / 1000.0)
        .sum();
    sum / entries.len() as f32
}

#[cfg(all(test, not(miri)))]
mod edge_cascade_tests {
    use super::*;
    use crate::entity_ops::{entity_put, normalize_name};
    use crate::relation_ops::relation_create;
    use crate::relation_type_ops::relation_type_intern;
    use crate::tables::edge::{
        self, derived_by, origin, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
    };
    use crate::MetadataDb;
    use brain_core::knowledge::{Entity, EntityType, Relation};
    use brain_core::{Cardinality, EntityId, ExtractorId, RelationId, RelationTypeId};

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn open_db() -> (tempfile::TempDir, MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    fn make_entity(db: &mut MetadataDb, name: &str) -> EntityId {
        let id = EntityId::new();
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            normalize_name(name),
            NOW,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_type(db: &mut MetadataDb, name: &str) -> RelationTypeId {
        let wtxn = db.write_txn().unwrap();
        let id = relation_type_intern(
            &wtxn,
            "test",
            name,
            None,
            None,
            Cardinality::ManyToMany,
            false,
            1,
            "",
            NOW,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn seed_substrate_edge(db: &mut MetadataDb, from: MemoryId, to: MemoryId) {
        let wtxn = db.write_txn().unwrap();
        let mut out = wtxn.open_table(EDGES_TABLE).unwrap();
        let mut rev = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        let data = EdgeData::new(0.5, origin::EXPLICIT, derived_by::CLIENT, NOW);
        edge::link(
            &mut out,
            &mut rev,
            NodeRef::Memory(from),
            EdgeKindRef::Builtin(brain_core::EdgeKind::Caused),
            NodeRef::Memory(to),
            edge::zero_disambiguator(),
            &data,
        )
        .unwrap();
        drop(out);
        drop(rev);
        wtxn.commit().unwrap();
    }

    #[test]
    fn cascade_unlinks_substrate_edges_anchored_at_forgotten_memory() {
        let (_dir, mut db) = open_db();
        let m = MemoryId::pack(1, 10, 1);
        let m_other = MemoryId::pack(1, 11, 1);
        // forgotten → other (m is source)
        seed_substrate_edge(&mut db, m, m_other);
        // other → forgotten (m is target)
        seed_substrate_edge(&mut db, m_other, m);

        let wtxn = db.write_txn().unwrap();
        let summary = cascade_forget_to_edges(&wtxn, m, NOW).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(summary.substrate_unlinked, 2);

        // Both edge rows + their reverse rows are gone.
        let rtxn = db.read_txn().unwrap();
        let edges = rtxn.open_table(EDGES_TABLE).unwrap();
        let rev = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        // The two seeded forward rows are removed.
        assert_eq!(edges.iter().unwrap().count(), 0);
        // Both reverse rows are removed too.
        assert_eq!(rev.iter().unwrap().count(), 0);
    }

    #[test]
    fn cascade_tombstones_relation_when_memory_is_sole_evidence() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-sole");
        let b = make_entity(&mut db, "b-sole");
        let t = intern_type(&mut db, "knows_sole");
        let mem = MemoryId::pack(1, 20, 1);

        let mut r = Relation::new_root(
            RelationId::new(),
            t,
            a,
            b,
            0.8,
            vec![mem],
            ExtractorId::from(0),
            NOW,
            false,
        );
        // Defensive: writer normally enforces is_symmetric from the
        // relation type; this is a manual seed.
        r.is_symmetric = false;
        let rid = r.id;

        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r, NOW).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        let summary = cascade_forget_to_edges(&wtxn, mem, NOW + 1).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(summary.relations_tombstoned, 1);
        assert_eq!(summary.relations_evidence_dropped, 0);

        let rtxn = db.read_txn().unwrap();
        let sidecar = rtxn.open_table(RELATION_METADATA_TABLE).unwrap();
        let meta = sidecar.get(&rid.to_bytes()).unwrap().unwrap().value();
        assert_eq!(meta.tombstoned, 1);
        assert_eq!(meta.is_current, 0);
    }

    #[test]
    fn cascade_drops_evidence_entry_when_relation_has_other_evidence() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-multi");
        let b = make_entity(&mut db, "b-multi");
        let t = intern_type(&mut db, "knows_multi");
        let m1 = MemoryId::pack(1, 30, 1);
        let m2 = MemoryId::pack(1, 31, 1);

        let mut r = Relation::new_root(
            RelationId::new(),
            t,
            a,
            b,
            0.8,
            vec![m1, m2],
            ExtractorId::from(0),
            NOW,
            false,
        );
        r.is_symmetric = false;
        let rid = r.id;

        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r, NOW).unwrap();
        wtxn.commit().unwrap();

        // Forget m1 only.
        let wtxn = db.write_txn().unwrap();
        let summary = cascade_forget_to_edges(&wtxn, m1, NOW + 1).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(summary.relations_tombstoned, 0);
        assert_eq!(summary.relations_evidence_dropped, 1);

        let rtxn = db.read_txn().unwrap();
        let sidecar = rtxn.open_table(RELATION_METADATA_TABLE).unwrap();
        let meta = sidecar.get(&rid.to_bytes()).unwrap().unwrap().value();
        // Relation kept; m2 still in evidence; m1 dropped.
        assert_eq!(meta.is_current, 1);
        assert_eq!(meta.tombstoned, 0);
        assert_eq!(meta.evidence_inline.len(), 1);
        assert_eq!(meta.evidence_inline[0], m2.to_be_bytes());

        let by_ev = rtxn.open_table(RELATION_BY_EVIDENCE_TABLE).unwrap();
        // m1 row dropped; m2 row stays.
        assert!(by_ev
            .get(&(m1.to_be_bytes(), rid.to_bytes()))
            .unwrap()
            .is_none());
        assert!(by_ev
            .get(&(m2.to_be_bytes(), rid.to_bytes()))
            .unwrap()
            .is_some());
    }

    #[test]
    fn cascade_leaves_typed_edge_row_intact_on_relation_tombstone() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-keep");
        let b = make_entity(&mut db, "b-keep");
        let t = intern_type(&mut db, "knows_keep");
        let mem = MemoryId::pack(1, 40, 1);

        let mut r = Relation::new_root(
            RelationId::new(),
            t,
            a,
            b,
            0.8,
            vec![mem],
            ExtractorId::from(0),
            NOW,
            false,
        );
        r.is_symmetric = false;

        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r, NOW).unwrap();
        wtxn.commit().unwrap();

        let edges_before = {
            let rtxn = db.read_txn().unwrap();
            rtxn.open_table(EDGES_TABLE)
                .unwrap()
                .iter()
                .unwrap()
                .count()
        };

        let wtxn = db.write_txn().unwrap();
        cascade_forget_to_edges(&wtxn, mem, NOW + 1).unwrap();
        wtxn.commit().unwrap();

        let edges_after = {
            let rtxn = db.read_txn().unwrap();
            rtxn.open_table(EDGES_TABLE)
                .unwrap()
                .iter()
                .unwrap()
                .count()
        };

        // Typed-relation edge rows survive — tombstoning is a sidecar
        // operation, not an edge deletion.
        assert_eq!(edges_before, edges_after);
    }
}
