//! `merge_log` table — entity merge history.
//!
//! See `spec/18_entities/02_storage.md` (`MergeRecord` shape) +
//! `spec/18_entities/03_merge.md` (merge mechanics) +
//! `spec/18_entities/04_unmerge.md` (unmerge replays this record in reverse).
//!
//! Key is `(timestamp_unix_nanos, MergeId.to_bytes())` for time-ordered
//! traversal. Grace-period unmerge consults this table to reconstruct
//! the pre-merge state.
//!
//! ## v1 → v2 (phase 16.7)
//!
//! Phase 16.5 stubbed a thin `v1` shape with only
//! `(survivor, merged, timestamps, confidence, finalized)`. Phase 16.7
//! widens to the full spec'd shape so unmerge can replay the diff:
//! aliases contributed, attribute conflicts, mention_count delta, audit
//! lifecycle. No production data exists on `v1`, so the migration is a
//! straight replacement — dev databases must be wiped.

use crate::impl_redb_rkyv_value;
use brain_core::{EntityId, MergeId};
use redb::TableDefinition;

pub const MERGE_LOG_TABLE: TableDefinition<'static, (u64, [u8; 16]), MergeRecord> =
    TableDefinition::new("merge_log");

/// Overflow rows for merges that re-routed many statements / relations
/// (phase 17/18+ — the lists live here; phase 16.7 never writes the
/// table since statement/relation tables don't yet exist).
///
/// Spec: `spec/18_entities/02_storage.md` §"entity_merge_log".
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
/// background worker; `Agent` is an operator agent_id over the wire.
pub mod actor_kind {
    pub const SYSTEM: u8 = 0;
    pub const AGENT: u8 = 1;
}

/// Conflict-resolution policy byte values for [`AttributeConflictRecord::policy`].
/// Mirrors `spec/18_entities/03_merge.md` §6.
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
/// attribute values). Phase 16.7 treats these as opaque bytes; phase
/// 19's schema validator gets typed access.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
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
// MergeRecord (v2).
// ---------------------------------------------------------------------------

/// Full merge audit row. Carries the complete diff between pre-merge
/// and post-merge state — unmerge replays this in reverse.
///
/// Phase scope: `statements_rerouted` / `relations_rerouted` are always
/// `0` in phase 16.7 (statement / relation tables don't exist yet).
/// Phases 17 / 18 sweep this table and populate the counts + overflow
/// rows. See `spec/18_entities/03_merge.md` §0.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct MergeRecord {
    pub merge_id_bytes: [u8; 16],
    pub survivor_bytes: [u8; 16],
    pub merged_bytes: [u8; 16],

    // Pre-merge / post-merge identity.
    pub merged_at_unix_nanos: u64,
    pub grace_period_until_unix_nanos: u64,
    pub confidence: f32,
    /// Operator-supplied reason. ≤ 4 KiB per `spec/28/04_validation.md` §1.
    pub reason: String,
    /// See [`actor_kind`].
    pub actor_kind: u8,
    /// `[0; 16]` when `actor_kind == SYSTEM`.
    pub actor_agent_bytes: [u8; 16],

    // Diffs against the survivor (replayed in reverse by unmerge).
    /// Aliases that were `merged`'s but weren't already on `survivor`
    /// — including `merged.canonical_name` itself.
    pub aliases_added: Vec<String>,
    /// Trigrams derived from `aliases_added` plus `merged.canonical_name`.
    /// Stored explicitly so unmerge doesn't need to recompute.
    pub trigrams_added: Vec<[u8; 3]>,
    pub attribute_conflicts: Vec<AttributeConflictRecord>,

    // Re-routing counts (lists live in the overflow table). Always
    // `0` in phase 16.7.
    pub statements_rerouted: u32,
    pub relations_rerouted: u32,
    /// `survivor.mention_count += this` on merge; reversed on unmerge.
    pub mention_count_added: u32,

    // Status.
    /// `0` = reversible (within grace); `1` = finalized (post-grace
    /// or unmerged).
    pub finalized: u8,
    /// `0` = still merged; otherwise the unmerge time.
    pub unmerged_at_unix_nanos: u64,
    /// See [`actor_kind`]; `0` if not unmerged.
    pub unmerged_by_actor_kind: u8,
    /// `[0; 16]` if not unmerged or unmerge actor is `SYSTEM`.
    pub unmerged_by_agent_bytes: [u8; 16],
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
        actor_agent_bytes: [u8; 16],
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
            actor_agent_bytes,
            aliases_added: Vec::new(),
            trigrams_added: Vec::new(),
            attribute_conflicts: Vec::new(),
            statements_rerouted: 0,
            relations_rerouted: 0,
            mention_count_added: 0,
            finalized: 0,
            unmerged_at_unix_nanos: 0,
            unmerged_by_actor_kind: 0,
            unmerged_by_agent_bytes: [0; 16],
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
/// the chunking. Phase 16.7 declares the table but never writes it
/// (`statements_rerouted` / `relations_rerouted` always `0` in 16.7).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct MergeAuditOverflow {
    pub rerouted_statement_ids: Vec<[u8; 16]>,
    pub rerouted_relation_ids: Vec<[u8; 16]>,
}

impl_redb_rkyv_value!(MergeRecord, "brain_metadata::MergeRecord::v2");
impl_redb_rkyv_value!(
    AttributeConflictRecord,
    "brain_metadata::AttributeConflictRecord::v1"
);
impl_redb_rkyv_value!(MergeAuditOverflow, "brain_metadata::MergeAuditOverflow::v1");

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
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
            actor_kind::AGENT,
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
        rec.unmerged_by_actor_kind = actor_kind::AGENT;
        rec.unmerged_by_agent_bytes = [9u8; 16];
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
        assert_eq!(got.unmerged_by_actor_kind, actor_kind::AGENT);
        assert_eq!(got.unmerged_by_agent_bytes, [9u8; 16]);
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
