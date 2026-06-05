//! FORGET cascade operations — cascading effects of FORGET.
//!
//! When a memory is forgotten, statements / relations whose
//! evidence list referenced it must be updated:
//!
//! 1. Drop `memory_id` from the statement's evidence — inline buffer
//!    on the row when small, the overflow row when the list spilled.
//! 2. Recompute `confidence` from the remaining evidence via
//!    confidence aggregation across evidence.
//! 3. If evidence becomes empty AND confidence < threshold,
//!    tombstone with reason `SourceMemoryForgotten` and reclaim
//!    the orphaned overflow row.
//!
//! ## Scope notes
//!
//! - Overflow evidence lists are walked alongside inline. A statement
//!   whose forgotten memory lives only in the overflow row gets the
//!   entry dropped + the row rewritten (or reclaimed if the rewrite
//!   leaves it empty). When the entry rewrite would bring the list
//!   back inside [`INLINE_EVIDENCE_CAP`] the row also collapses back
//!   onto the inline buffer.
//! - Relations are scanned through `RELATION_BY_EVIDENCE_TABLE`; if
//!   the forgotten memory was the sole evidence, the relation is
//!   tombstoned.
//!
//! ## Audit
//!
//! The `audit_ops::audit_write` API currently targets extraction
//! events. Cascade audit rows are a future enhancement; the cascade
//! still updates the row, so an external observer can see the change
//! via the change feed.

use brain_core::{
    aggregate_confidence, ConfidenceConfig, EvidenceEntry, EvidenceOverflowId, StatementKind,
    TombstoneReason, INLINE_EVIDENCE_CAP,
};
use brain_core::{EdgeKindRef, MemoryId, NodeRef, RelationId};
use redb::{ReadableTable, WriteTransaction};

use crate::relation::ops::{relation_tombstone, RelationOpError};
use crate::statement::tombstone::statement_tombstone;
use crate::statement::{rekey_predicate_index, StatementOpError};
use crate::tables::edge::{self, EdgeKey, EDGES_REVERSE_TABLE, EDGES_TABLE};
use crate::tables::relation::{RELATION_BY_EVIDENCE_TABLE, RELATION_METADATA_TABLE};
use crate::tables::statement::{
    EvidenceEntryRow, EvidenceOverflow, StatementMetadata, EVIDENCE_OVERFLOW_TABLE,
    STATEMENTS_TABLE,
};

/// Default confidence threshold below which a statement that
/// loses its only piece of evidence is tombstoned. Configurable
/// at the caller doesn't pin a number.
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
/// referenced memories don't produce an unbounded wtxn. A future
/// continuation-job mechanism will resume from a cursor when the batch
/// cap is hit.
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

    // Snapshot phase. We collect every row that references the
    // forgotten memory along with the surviving evidence list (with
    // the forgotten entry removed) so the mutate phase has a single
    // source of truth per statement. Reading inline + overflow in
    // the same pass avoids re-scanning the statements table.
    let mut affected: Vec<AffectedStatement> = Vec::new();
    {
        let table = wtxn.open_table(STATEMENTS_TABLE)?;
        for entry in table.iter()? {
            let (_, v) = entry?;
            let row = v.value();
            summary.scanned += 1;
            if row.is_tombstoned() {
                continue;
            }
            let inline_hit = row
                .evidence_inline
                .iter()
                .any(|e| e.memory_id_bytes == memory_bytes);

            let overflow_id = row.evidence_overflow_id_bytes.map(EvidenceOverflowId::from);

            // The overflow row needs probing too — the inline list is
            // empty when the row owns evidence-overflow form, so a
            // pure inline check would silently miss those statements.
            let (overflow_hit, overflow_entries) = if let Some(oid) = overflow_id {
                load_overflow_entries(wtxn, oid)?
                    .map(|entries| {
                        let hit = entries.iter().any(|e| e.memory_id_bytes == memory_bytes);
                        (hit, Some(entries))
                    })
                    .unwrap_or((false, None))
            } else {
                (false, None)
            };

            if !inline_hit && !overflow_hit {
                continue;
            }

            // Build the surviving entries from whichever side holds
            // the row's evidence. By construction these are mutually
            // exclusive (see `metadata_from_statement`), so it's safe
            // to source from inline OR overflow exclusively.
            let source_entries: Vec<EvidenceEntryRow> = if let Some(over) = overflow_entries {
                over
            } else {
                row.evidence_inline.clone()
            };
            let remaining: Vec<EvidenceEntryRow> = source_entries
                .into_iter()
                .filter(|e| e.memory_id_bytes != memory_bytes)
                .collect();

            affected.push(AffectedStatement {
                row,
                remaining,
                prior_overflow_id: overflow_id,
            });
            if affected.len() >= batch_cap {
                break;
            }
        }
    }

    // Mutate phase. Each affected statement either becomes evidence-
    // shrunk + confidence-recomputed, or tombstoned.
    //
    // Re-derivation uses the noisy-OR formula across the surviving
    // evidence with per-kind decay — same math the resolver and the
    // confidence-sweep worker use, so a forgotten-memory cascade and
    // a routine sweep produce identical confidences for identical
    // inputs. Falling back to a flat mean would silently diverge from
    // the rest of the system on every cascade.
    //
    // The mutation-side table handles are hoisted out of the loop so
    // we don't pay the open-table cost N times per cascade. We drop
    // them before any `statement_tombstone` call because that helper
    // opens the same table itself and redb prefers one handle in
    // flight.
    let confidence_cfg = ConfidenceConfig::default_v1();
    // Tombstone deferral + overflow reclamation deferral — both side-
    // effects open `STATEMENTS_TABLE` / `EVIDENCE_OVERFLOW_TABLE`, so
    // we collect them and apply after the mutation handles drop.
    let mut to_tombstone: Vec<brain_core::StatementId> = Vec::new();
    let mut overflow_reclaim: Vec<EvidenceOverflowId> = Vec::new();
    // Predicate-bucket re-keys for rows whose confidence changed. Applied
    // after the STATEMENTS handle drops (and before the tombstone pass,
    // so a subsequently-tombstoned row's index entry sits under its new
    // bucket where `statement_tombstone` will find and remove it).
    // Tuple: (predicate_id, kind, old_confidence, new_confidence, id).
    let mut rekey_moves: Vec<(u32, u8, f32, f32, [u8; 16])> = Vec::new();
    {
        let mut table = wtxn.open_table(STATEMENTS_TABLE)?;
        let mut overflow_table = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
        for AffectedStatement {
            mut row,
            remaining,
            prior_overflow_id,
        } in affected
        {
            let kind = StatementKind::from_u8(row.kind).unwrap_or(StatementKind::Fact);
            // A previously-overflowed statement that drops back to ≤
            // INLINE_EVIDENCE_CAP entries collapses onto the inline
            // buffer; the now-orphaned overflow row gets reclaimed.
            // Statements that stay overflowed get the row rewritten
            // in place — same id, smaller payload. Statements that
            // grow past the cap (not possible here, but the code is
            // symmetric) would allocate a new overflow.
            if remaining.is_empty() {
                let old_conf = row.confidence;
                row.evidence_inline.clear();
                row.evidence_overflow_id_bytes = None;
                row.confidence = 0.0;
                rekey_moves.push((
                    row.predicate_id,
                    row.kind,
                    old_conf,
                    0.0,
                    row.statement_id_bytes,
                ));
                table.insert(&row.statement_id_bytes, &row)?;
                if let Some(oid) = prior_overflow_id {
                    overflow_reclaim.push(oid);
                }
                if 0.0 < confidence_threshold {
                    to_tombstone.push(row.statement_id());
                } else {
                    summary.kept_stale += 1;
                }
                continue;
            }

            let entries: Vec<EvidenceEntry> =
                remaining.iter().map(EvidenceEntryRow::to_entry).collect();
            let new_conf = aggregate_confidence(&entries, now_unix_nanos, kind, &confidence_cfg);
            let old_conf = row.confidence;
            row.confidence = new_conf;
            rekey_moves.push((
                row.predicate_id,
                row.kind,
                old_conf,
                new_conf,
                row.statement_id_bytes,
            ));

            if remaining.len() <= INLINE_EVIDENCE_CAP {
                row.evidence_inline = remaining;
                row.evidence_overflow_id_bytes = None;
                if let Some(oid) = prior_overflow_id {
                    overflow_reclaim.push(oid);
                }
            } else {
                // Rewrite the overflow row in place when one already
                // exists, or allocate a fresh id when the row used to
                // be inline (can't happen during a single-entry FORGET
                // since the list only shrinks, but the branch keeps
                // the helper symmetric for future callers).
                let oid = prior_overflow_id.unwrap_or_else(EvidenceOverflowId::new);
                let entries_clone: Vec<EvidenceEntry> = entries.clone();
                let overflow_row =
                    EvidenceOverflow::from_entries(oid, &entries_clone, now_unix_nanos);
                overflow_table.insert(&oid.to_bytes(), &overflow_row)?;
                row.evidence_inline.clear();
                row.evidence_overflow_id_bytes = Some(oid.to_bytes());
            }

            table.insert(&row.statement_id_bytes, &row)?;
            summary.evidence_dropped += 1;
        }
    } // table handles dropped before tombstone / overflow-reclaim re-open.

    // Move predicate-bucket entries for rows whose confidence changed.
    // Runs before the tombstone pass so a row about to be tombstoned has
    // its index entry under the new (zeroed) bucket, where
    // `statement_tombstone` will then remove it.
    for (pred, kind, old_conf, new_conf, id) in rekey_moves {
        rekey_predicate_index(wtxn, pred, kind, old_conf, new_conf, &id)?;
    }

    // Reclaim orphaned overflow rows. Safe to do regardless of
    // tombstone outcome — once the statement no longer references the
    // id, the row is dead weight.
    {
        let mut overflow_table = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
        for oid in overflow_reclaim {
            overflow_table.remove(&oid.to_bytes())?;
        }
    }

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

/// Helper used by the snapshot phase. Pulls the overflow row's
/// entries into a `Vec<EvidenceEntryRow>` so callers can apply the
/// same `retain`-style filter they use for inline entries.
fn load_overflow_entries(
    wtxn: &WriteTransaction,
    overflow_id: EvidenceOverflowId,
) -> Result<Option<Vec<EvidenceEntryRow>>, StatementOpError> {
    let t = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE)?;
    let row: Option<EvidenceOverflow> = t.get(&overflow_id.to_bytes())?.map(|g| g.value());
    let Some(over) = row else {
        return Ok(None);
    };
    let n = over
        .memory_ids
        .len()
        .min(over.extractor_ids.len())
        .min(over.confidences_milli.len())
        .min(over.timestamps_unix_nanos.len());
    let entries = (0..n)
        .map(|i| EvidenceEntryRow {
            memory_id_bytes: over.memory_ids[i],
            confidence_milli: over.confidences_milli[i],
            timestamp_unix_nanos: over.timestamps_unix_nanos[i],
            extractor_id: over.extractor_ids[i],
        })
        .collect();
    Ok(Some(entries))
}

/// One snapshot-phase entry: the existing row, the surviving evidence
/// list (forgotten memory already filtered out), and the prior
/// overflow id (if any) so the mutate phase can rewrite or reclaim
/// it. The overflow id stays separate from the row because the row's
/// `evidence_overflow_id_bytes` may be cleared mid-flow when the
/// remaining list collapses back inline.
struct AffectedStatement {
    row: StatementMetadata,
    remaining: Vec<EvidenceEntryRow>,
    prior_overflow_id: Option<EvidenceOverflowId>,
}

// ---------------------------------------------------------------------------
// Edge + relation cascade.
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

#[cfg(all(test, not(miri)))]
mod edge_cascade_tests {
    use super::*;
    use crate::entity::ops::{entity_put, normalize_name};
    use crate::relation::ops::relation_create;
    use crate::relation::types::relation_type_intern;
    use crate::tables::edge::{
        self, derived_by, origin, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
    };
    use crate::MetadataDb;
    use brain_core::{Cardinality, EntityId, ExtractorId, RelationId, RelationTypeId};
    use brain_core::{Entity, EntityType, Relation};

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

#[cfg(all(test, not(miri)))]
mod statement_cascade_overflow_tests {
    use super::*;
    use crate::entity::ops::{entity_put, normalize_name};
    use crate::schema::predicate::predicate_intern;
    use crate::statement::evidence::{pack_evidence_ids, read_evidence_ids};
    use crate::statement::statement_create;
    use crate::tables::statement::{StatementMetadata, EVIDENCE_OVERFLOW_TABLE, STATEMENTS_TABLE};
    use crate::MetadataDb;
    use brain_core::{
        ContextId, Entity, EntityType, EvidenceRef, ExtractorId, PredicateId, Statement,
        StatementId, StatementKind, StatementObject, StatementValue, SubjectRef,
    };

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn open_db() -> (tempfile::TempDir, MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    fn make_subject(db: &mut MetadataDb, name: &str) -> brain_core::EntityId {
        let id = brain_core::EntityId::new();
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

    fn intern_pred(db: &mut MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Fact),
            2,
            1,
            "",
            false,
            NOW,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn pack_ids_for_test(db: &mut MetadataDb, ids: Vec<MemoryId>) -> EvidenceRef {
        let wtxn = db.write_txn().unwrap();
        let r = pack_evidence_ids(&wtxn, ids, 0.9, NOW, ExtractorId::from(0)).unwrap();
        wtxn.commit().unwrap();
        r
    }

    fn make_statement(
        db: &mut MetadataDb,
        subject: brain_core::EntityId,
        predicate: PredicateId,
        evidence: EvidenceRef,
    ) -> StatementId {
        let id = StatementId::new();
        let s = Statement::new_root(
            id,
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Value(StatementValue::Text("placeholder".into())),
            0.9,
            evidence,
            ExtractorId::from(0),
            NOW,
            1,
        );
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, NOW).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn ids(n: usize) -> Vec<MemoryId> {
        (0..n)
            .map(|i| MemoryId::pack(i as u16 + 1, ContextId::DEFAULT.into(), 0))
            .collect()
    }

    fn statement_row(db: &MetadataDb, id: StatementId) -> StatementMetadata {
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
        t.get(&id.to_bytes()).unwrap().unwrap().value()
    }

    #[test]
    fn cascade_drops_forgotten_memory_from_overflow_and_collapses_back_inline() {
        let (_dir, mut db) = open_db();
        let subj = make_subject(&mut db, "ada");
        let pred = intern_pred(&mut db, "knows_collapse");

        // 9 evidence ids — guaranteed overflow.
        let memory_ids = ids(9);
        let ev = pack_ids_for_test(&mut db, memory_ids.clone());
        assert!(matches!(ev, EvidenceRef::Overflow(_)));
        let stmt = make_statement(&mut db, subj, pred, ev);

        // Forget the first memory — leaves 8 surviving, fits inline.
        let wtxn = db.write_txn().unwrap();
        let summary =
            cascade_forget_to_statements(&wtxn, memory_ids[0], 0.2, 100, NOW + 1).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(summary.evidence_dropped, 1);
        assert_eq!(summary.tombstoned, 0);

        let row = statement_row(&db, stmt);
        assert_eq!(row.evidence_inline.len(), 8);
        assert!(row.evidence_overflow_id_bytes.is_none());
    }

    #[test]
    fn cascade_drops_forgotten_memory_keeping_overflow_form() {
        let (_dir, mut db) = open_db();
        let subj = make_subject(&mut db, "ada-keep");
        let pred = intern_pred(&mut db, "knows_keep");

        // 12 ids — stays overflow after forgetting one (11 > 8).
        let memory_ids = ids(12);
        let ev = pack_ids_for_test(&mut db, memory_ids.clone());
        let stmt = make_statement(&mut db, subj, pred, ev);

        let wtxn = db.write_txn().unwrap();
        cascade_forget_to_statements(&wtxn, memory_ids[3], 0.2, 100, NOW + 1).unwrap();
        wtxn.commit().unwrap();

        let row = statement_row(&db, stmt);
        assert!(row.evidence_overflow_id_bytes.is_some());
        assert!(row.evidence_inline.is_empty());

        // The overflow row now holds 11 ids.
        let oid = EvidenceOverflowId::from(row.evidence_overflow_id_bytes.unwrap());
        let reference = EvidenceRef::Overflow(oid);
        let rtxn = db.read_txn().unwrap();
        let back = read_evidence_ids(&rtxn, &reference).unwrap();
        assert_eq!(back.len(), 11);
        assert!(!back.contains(&memory_ids[3]));
    }

    #[test]
    fn cascade_tombstones_statement_and_reclaims_overflow_when_evidence_empty() {
        let (_dir, mut db) = open_db();
        let subj = make_subject(&mut db, "ada-tomb");
        let pred = intern_pred(&mut db, "knows_tomb");

        // 9 ids — overflow. After forgetting all but one, then forgetting
        // the last, the row should tombstone + reclaim.
        let memory_ids = ids(9);
        let ev = pack_ids_for_test(&mut db, memory_ids.clone());
        let stmt = make_statement(&mut db, subj, pred, ev);

        // Forget all 9 in sequence; the last call should tombstone.
        for mid in &memory_ids {
            let wtxn = db.write_txn().unwrap();
            cascade_forget_to_statements(&wtxn, *mid, 0.2, 100, NOW + 1).unwrap();
            wtxn.commit().unwrap();
        }

        let row = statement_row(&db, stmt);
        assert!(row.is_tombstoned());
        assert!(row.evidence_inline.is_empty());
        assert!(row.evidence_overflow_id_bytes.is_none());

        // The overflow row is reclaimed.
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(EVIDENCE_OVERFLOW_TABLE).unwrap();
        assert_eq!(t.iter().unwrap().count(), 0);
    }

    #[test]
    fn cascade_recomputes_confidence_over_overflow_evidence() {
        let (_dir, mut db) = open_db();
        let subj = make_subject(&mut db, "ada-conf");
        let pred = intern_pred(&mut db, "knows_conf");

        // 50 ids — overflow. Recompute is observable: the per-entry
        // confidence_milli (0 from pack_evidence_ids — actually 900 since
        // we pass 0.9 → milli) yields a noisy-OR aggregate the cascade
        // can re-derive after dropping one.
        let memory_ids = ids(50);
        let ev = pack_ids_for_test(&mut db, memory_ids.clone());
        let stmt = make_statement(&mut db, subj, pred, ev);
        let before = statement_row(&db, stmt).confidence;

        let wtxn = db.write_txn().unwrap();
        let summary =
            cascade_forget_to_statements(&wtxn, memory_ids[7], 0.05, 100, NOW + 1).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(summary.evidence_dropped, 1);

        let after_row = statement_row(&db, stmt);
        let after = after_row.confidence;
        // 49 surviving entries still aggregate well above the floor.
        assert!(after > 0.0);
        // Confidence either stays equal or shrinks — never grows when an
        // entry is removed.
        assert!(after <= before + 1e-6);
        assert!(after_row.evidence_overflow_id_bytes.is_some());

        // Cross-check the overflow row holds 49 ids.
        let oid = EvidenceOverflowId::from(after_row.evidence_overflow_id_bytes.unwrap());
        let rtxn = db.read_txn().unwrap();
        let back = read_evidence_ids(&rtxn, &EvidenceRef::Overflow(oid)).unwrap();
        assert_eq!(back.len(), 49);
    }
}
