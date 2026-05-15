//! `merge_log` table — entity merge history.
//!
//! See `spec/18_entities/00_purpose.md` (merge + unmerge with grace
//! period). Key is `(timestamp_unix_nanos, MergeId.to_bytes())` for
//! time-ordered traversal. Grace-period unmerge consults this table
//! to reconstruct the pre-merge state.

use crate::impl_redb_rkyv_value;
use brain_core::{EntityId, MergeId};
use redb::TableDefinition;

pub const MERGE_LOG_TABLE: TableDefinition<'static, (u64, [u8; 16]), MergeRecord> =
    TableDefinition::new("merge_log");

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct MergeRecord {
    pub merge_id_bytes: [u8; 16],
    pub survivor_bytes: [u8; 16],
    pub merged_bytes: [u8; 16],
    pub merged_at_unix_nanos: u64,
    pub grace_period_until_unix_nanos: u64,
    pub confidence: f32,
    /// `0` = reversible (within grace), `1` = finalized.
    pub finalized: u8,
}

impl MergeRecord {
    #[must_use]
    pub fn new(
        merge_id: MergeId,
        survivor: EntityId,
        merged: EntityId,
        merged_at_unix_nanos: u64,
        grace_period_until_unix_nanos: u64,
        confidence: f32,
    ) -> Self {
        Self {
            merge_id_bytes: merge_id.to_bytes(),
            survivor_bytes: survivor.to_bytes(),
            merged_bytes: merged.to_bytes(),
            merged_at_unix_nanos,
            grace_period_until_unix_nanos,
            confidence,
            finalized: 0,
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

    #[must_use]
    pub fn is_finalized(&self) -> bool {
        self.finalized != 0
    }
}

impl_redb_rkyv_value!(MergeRecord, "brain_metadata::MergeRecord::v1");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let survivor = EntityId::new();
        let merged = EntityId::new();
        let merge_id = MergeId::new();
        let rec = MergeRecord::new(
            merge_id,
            survivor,
            merged,
            1_700_000_000_000_000_000,
            1_700_604_800_000_000_000, // 7 days later
            0.92,
        );
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
        assert_eq!(got.survivor(), survivor);
        assert_eq!(got.merged(), merged);
        assert_eq!(got.merge_id(), merge_id);
        assert!(!got.is_finalized());
    }
}
