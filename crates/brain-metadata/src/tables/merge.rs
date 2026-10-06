//! `merge_log` table — entity merge history.
//!
//! Unmerge replays this record in reverse. Key is
//! `(timestamp_unix_nanos, MergeId.to_bytes())` for time-ordered
//! traversal. Grace-period unmerge consults this table to reconstruct
//! the pre-merge state.
//!
//! The row carries everything unmerge needs to replay the diff:
//! aliases contributed, attribute conflicts, mention_count delta, and
//! audit lifecycle.

use crate::impl_redb_rkyv_value;
use brain_core::{EntityId, MergeId};
use redb::TableDefinition;

pub const MERGE_LOG_TABLE: TableDefinition<'static, (u64, [u8; 16]), MergeRecord> =
    TableDefinition::new("merge_log");

/// Overflow rows for merges that re-routed many statements / relations
/// — the re-routed-id lists live here when they don't fit inline.
pub const ENTITY_MERGE_AUDIT_OVERFLOW: TableDefinition<
    'static,
    ([u8; 16], u32),
    MergeAuditOverflow,
> = TableDefinition::new("entity_merge_audit_overflow");

// ---------------------------------------------------------------------------
// Constants.
// ---------------------------------------------------------------------------

/// Actor-kind byte values for [`MergeRecord::actor_kind`] and
/// [`MergeRecord::unmerged_by_actor_kind`]. `System` is the resolver /
/// background worker; `Space` is an operator space_id over the wire.
pub mod actor_kind {
    pub const SYSTEM: u8 = 0;
    pub const SPACE: u8 = 1;
}

/// Conflict-resolution policy byte values for [`AttributeConflictRecord::policy`].
pub mod conflict_policy {
    pub const SURVIVOR_WINS: u8 = 1;
    pub const MERGED_WINS: u8 = 2;
    pub const NEWEST_WINS: u8 = 3;
    pub const CONCAT_TEXT: u8 = 4;
    pub const REJECT_MERGE: u8 = 5;
}

/// Outcome byte values for [`AttributeConflictRecord::outcome`].
pub mod conflict_outcome {
    pub const KEPT_SURVIVOR: u8 = 1;
    pub const REPLACED_WITH_MERGED: u8 = 2;
    pub const CONCATENATED: u8 = 3;
}

// ---------------------------------------------------------------------------
// AttributeConflictRecord.
// ---------------------------------------------------------------------------

/// One conflicting attribute resolved during merge. Stored so unmerge
/// can restore the original split.
///
/// `survivor_value_blob` and `merged_value_blob` carry rkyv-encoded
/// `StatementValueWire` bytes (the wire-level union of typed
/// attribute values). The merge path treats these as opaque bytes; the
/// schema validator gets typed access.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
pub struct AttributeConflictRecord {
    pub attribute_key: String,
    pub survivor_value_blob: Vec<u8>,
    pub merged_value_blob: Vec<u8>,
    /// See [`conflict_policy`].
    pub policy: u8,
    /// See [`conflict_outcome`].
    pub outcome: u8,
}

// ---------------------------------------------------------------------------
// Re-route records — the exact per-row diff unmerge replays in reverse.
// ---------------------------------------------------------------------------

/// One statement re-routed off the merged entity during a merge.
///
/// A statement is re-routed on its subject side (subject was the merged
/// entity), its object side (object was `Entity(merged)`), or both
/// (a self-referential statement). The subject-side reroute bumps the
/// row `version`; `old_version` / `new_version` capture that so unmerge
/// restores the exact prior value and rewrites the chain-table key back.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
pub struct StatementReroute {
    pub statement_id_bytes: [u8; 16],
    /// `1` iff the subject was re-pointed merged → survivor.
    pub subject_changed: u8,
    /// `1` iff the object `Entity(merged)` was re-pointed to survivor.
    pub object_changed: u8,
    /// Version before the subject-side bump. Equals `new_version` when
    /// only the object changed (no bump).
    pub old_version: u32,
    /// Version after the subject-side bump.
    pub new_version: u32,
    /// Chain root (unchanged by reroute) — needed to rewrite the
    /// version-keyed chain-table row on unmerge.
    pub chain_root_bytes: [u8; 16],
}

/// One relation re-routed off the merged entity during a merge. Stores
/// both the pre- and post-merge endpoints so unmerge can unlink the
/// survivor-side edge rows and relink the original merged-side ones
/// exactly, without recomputing symmetric canonicalisation.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
pub struct RelationReroute {
    pub relation_id_bytes: [u8; 16],
    pub old_from_bytes: [u8; 16],
    pub old_to_bytes: [u8; 16],
    pub new_from_bytes: [u8; 16],
    pub new_to_bytes: [u8; 16],
    /// `1` iff the `from` endpoint was re-pointed merged → survivor.
    pub from_changed: u8,
    /// `1` iff the `to` endpoint was re-pointed merged → survivor.
    pub to_changed: u8,
}

// ---------------------------------------------------------------------------
// MergeRecord (v2).
// ---------------------------------------------------------------------------

/// Full merge audit row. Carries the complete diff between pre-merge
/// and post-merge state — unmerge replays this in reverse.
///
/// `statements_rerouted` / `relations_rerouted` count re-routed graph
/// rows; the id lists themselves live in the overflow table.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
pub struct MergeRecord {
    pub merge_id_bytes: [u8; 16],
    pub survivor_bytes: [u8; 16],
    pub merged_bytes: [u8; 16],

    // Pre-merge / post-merge identity.
    pub merged_at_unix_nanos: u64,
    pub grace_period_until_unix_nanos: u64,
    pub confidence: f32,
    /// Operator-supplied reason. Capped at 4 KiB.
    pub reason: String,
    /// See [`actor_kind`].
    pub actor_kind: u8,
    /// `[0; 16]` when `actor_kind == SYSTEM`.
    pub actor_space_bytes: [u8; 16],

    // Diffs against the survivor (replayed in reverse by unmerge).
    /// Aliases that were `merged`'s but weren't already on `survivor`
    /// — including `merged.canonical_name` itself.
    pub aliases_added: Vec<String>,
    /// Trigrams derived from `aliases_added` plus `merged.canonical_name`.
    /// Stored explicitly so unmerge doesn't need to recompute.
    pub trigrams_added: Vec<[u8; 3]>,
    pub attribute_conflicts: Vec<AttributeConflictRecord>,

    // Re-routing counts.
    pub statements_rerouted: u32,
    pub relations_rerouted: u32,
    /// `survivor.mention_count += this` on merge; reversed on unmerge.
    pub mention_count_added: u32,

    /// Per-row statement re-route diff. Inlined (typical mention_count is
    /// well under 1000, so the row stays far below redb's per-value cap);
    /// the `entity_merge_audit_overflow` table stays reserved for the
    /// very-high-degree case.
    pub rerouted_statements: Vec<StatementReroute>,
    /// Per-row relation re-route diff.
    pub rerouted_relations: Vec<RelationReroute>,
    /// Survivor's `attributes_blob` before the merge folded merged's in.
    /// Unmerge restores this verbatim (the attribute fold is
    /// survivor-wins, so this is the survivor's own bytes unless the
    /// survivor had no attributes and adopted merged's whole blob).
    pub survivor_attributes_before: Vec<u8>,

    // Status.
    /// `0` = reversible (within grace); `1` = finalized (post-grace
    /// or unmerged).
    pub finalized: u8,
    /// `0` = still merged; otherwise the unmerge time.
    pub unmerged_at_unix_nanos: u64,
    /// See [`actor_kind`]; `0` if not unmerged.
    pub unmerged_by_actor_kind: u8,
    /// `[0; 16]` if not unmerged or unmerge actor is `SYSTEM`.
    pub unmerged_by_space_bytes: [u8; 16],
}

impl MergeRecord {
    /// Build a fresh merge record with empty diff lists. Callers fill
    /// in the diffs (aliases_added, attribute_conflicts, etc.) before
    /// inserting.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        merge_id: MergeId,
        survivor: EntityId,
        merged: EntityId,
        merged_at_unix_nanos: u64,
        grace_period_until_unix_nanos: u64,
        confidence: f32,
        reason: String,
        actor_kind: u8,
        actor_space_bytes: [u8; 16],
    ) -> Self {
        Self {
            merge_id_bytes: merge_id.to_bytes(),
            survivor_bytes: survivor.to_bytes(),
            merged_bytes: merged.to_bytes(),
            merged_at_unix_nanos,
            grace_period_until_unix_nanos,
            confidence,
            reason,
            actor_kind,
            actor_space_bytes,
            aliases_added: Vec::new(),
            trigrams_added: Vec::new(),
            attribute_conflicts: Vec::new(),
            statements_rerouted: 0,
            relations_rerouted: 0,
            mention_count_added: 0,
            rerouted_statements: Vec::new(),
            rerouted_relations: Vec::new(),
            survivor_attributes_before: Vec::new(),
            finalized: 0,
            unmerged_at_unix_nanos: 0,
            unmerged_by_actor_kind: 0,
            unmerged_by_space_bytes: [0; 16],
        }
    }

    #[must_use]
    pub fn merge_id(&self) -> MergeId {
        MergeId::from(self.merge_id_bytes)
    }

    #[must_use]
    pub fn survivor(&self) -> EntityId {
        EntityId::from(self.survivor_bytes)
    }

    #[must_use]
    pub fn merged(&self) -> EntityId {
        EntityId::from(self.merged_bytes)
    }

    /// True iff the merge is past its grace window or has been
    /// explicitly unmerged (either way, no further reversal is
    /// allowed).
    #[must_use]
    pub fn is_finalized(&self) -> bool {
        self.finalized != 0
    }

    /// True iff `unmerged_at_unix_nanos != 0`.
    #[must_use]
    pub fn is_unmerged(&self) -> bool {
        self.unmerged_at_unix_nanos != 0
    }
}

// ---------------------------------------------------------------------------
// MergeAuditOverflow.
// ---------------------------------------------------------------------------

/// Overflow chunk for very-large re-route lists. Each chunk holds up
/// to a few thousand re-routed ids; redb's per-value 1 MiB cap drives
/// the chunking.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
pub struct MergeAuditOverflow {
    pub rerouted_statement_ids: Vec<[u8; 16]>,
    pub rerouted_relation_ids: Vec<[u8; 16]>,
}

impl_redb_rkyv_value!(MergeRecord, "brain_metadata::MergeRecord");
impl_redb_rkyv_value!(
    AttributeConflictRecord,
    "brain_metadata::AttributeConflictRecord"
);
impl_redb_rkyv_value!(MergeAuditOverflow, "brain_metadata::MergeAuditOverflow");

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::{ReadableDatabase, ReadableTable};

    fn sample_record() -> MergeRecord {
        let survivor = EntityId::new();
        let merged = EntityId::new();
        let merge_id = MergeId::new();
        let mut rec = MergeRecord::new(
            merge_id,
            survivor,
            merged,
            1_700_000_000_000_000_000,
            1_700_604_800_000_000_000,
            0.92,
            "duplicate detected".to_owned(),
            actor_kind::SPACE,
            [7u8; 16],
        );
        rec.aliases_added = vec!["P. Patel".into(), "Priya P".into()];
        rec.trigrams_added = vec![*b"  p", *b" p ", *b"pat"];
        rec.attribute_conflicts.push(AttributeConflictRecord {
            attribute_key: "email".into(),
            survivor_value_blob: b"survivor".to_vec(),
            merged_value_blob: b"merged".to_vec(),
            policy: conflict_policy::SURVIVOR_WINS,
            outcome: conflict_outcome::KEPT_SURVIVOR,
        });
        rec.mention_count_added = 17;
        rec
    }

    #[test]
    fn merge_record_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let rec = sample_record();
        let key = (rec.merged_at_unix_nanos, rec.merge_id_bytes);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MERGE_LOG_TABLE).unwrap();
            t.insert(&key, &rec).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MERGE_LOG_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, rec);
        assert_eq!(got.survivor(), rec.survivor());
        assert_eq!(got.merged(), rec.merged());
        assert_eq!(got.merge_id(), rec.merge_id());
        assert!(!got.is_finalized());
        assert!(!got.is_unmerged());
        assert_eq!(got.aliases_added.len(), 2);
        assert_eq!(got.attribute_conflicts.len(), 1);
    }

    #[test]
    fn merge_record_unmerge_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let mut rec = sample_record();
        let key = (rec.merged_at_unix_nanos, rec.merge_id_bytes);

        // Insert the merge audit.
        {
            let wtxn = db.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(MERGE_LOG_TABLE).unwrap();
                t.insert(&key, &rec).unwrap();
            }
            wtxn.commit().unwrap();
        }

        // Simulate an unmerge.
        rec.unmerged_at_unix_nanos = rec.merged_at_unix_nanos + 60_000_000_000;
        rec.unmerged_by_actor_kind = actor_kind::SPACE;
        rec.unmerged_by_space_bytes = [9u8; 16];
        rec.finalized = 1;
        {
            let wtxn = db.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(MERGE_LOG_TABLE).unwrap();
                t.insert(&key, &rec).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MERGE_LOG_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert!(got.is_unmerged());
        assert!(got.is_finalized());
        assert_eq!(got.unmerged_by_actor_kind, actor_kind::SPACE);
        assert_eq!(got.unmerged_by_space_bytes, [9u8; 16]);
    }

    #[test]
    fn overflow_table_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let merge_id_bytes = MergeId::new().to_bytes();
        let payload = MergeAuditOverflow {
            rerouted_statement_ids: vec![[1u8; 16], [2u8; 16]],
            rerouted_relation_ids: vec![[3u8; 16]],
        };
        let key = (merge_id_bytes, 0u32);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(ENTITY_MERGE_AUDIT_OVERFLOW).unwrap();
            t.insert(&key, &payload).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(ENTITY_MERGE_AUDIT_OVERFLOW).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, payload);
    }

    #[test]
    fn time_ordered_range_scan() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        // Insert three audits with strictly increasing timestamps.
        let ts_base = 1_700_000_000_000_000_000u64;
        let merge_ids: Vec<_> = (0..3).map(|_| MergeId::new()).collect();
        let recs: Vec<_> = merge_ids
            .iter()
            .enumerate()
            .map(|(i, mid)| {
                MergeRecord::new(
                    *mid,
                    EntityId::new(),
                    EntityId::new(),
                    ts_base + i as u64 * 1_000_000_000,
                    ts_base + i as u64 * 1_000_000_000 + 604_800_000_000_000,
                    0.9,
                    format!("audit #{i}"),
                    actor_kind::SYSTEM,
                    [0; 16],
                )
            })
            .collect();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MERGE_LOG_TABLE).unwrap();
            for rec in &recs {
                t.insert(&(rec.merged_at_unix_nanos, rec.merge_id_bytes), rec)
                    .unwrap();
            }
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MERGE_LOG_TABLE).unwrap();
        let collected: Vec<_> = t
            .iter()
            .unwrap()
            .map(|entry| entry.unwrap().1.value().merged_at_unix_nanos)
            .collect();
        assert_eq!(
            collected,
            vec![ts_base, ts_base + 1_000_000_000, ts_base + 2_000_000_000,]
        );
    }
}
