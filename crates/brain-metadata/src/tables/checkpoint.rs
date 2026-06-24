//! `checkpoints` table: durable record of completed WAL checkpoints.
//!
//! ## Why this matters
//!
//! Without checkpointing, recovery replays the entire WAL from the
//! first record — recovery time grows unbounded. A checkpoint marks an
//! LSN below which records are reflected in arena and metadata, so
//! recovery can skip everything before it.
//!
//! Multiple checkpoint rows can exist; the substrate keeps the most
//! recent one as the recovery target. [`latest`] returns the
//! highest-id row in O(log N).
//!
//! ## What lives here
//!
//! - [`CHECKPOINTS_TABLE`] — `checkpoint_id: u64` → [`CheckpointMeta`].
//! - [`CheckpointMeta`] — rkyv-derived row with the six u64 fields.
//! - [`latest`] — read-only "most recent checkpoint" query; the
//!   recovery target.
//!
//! ## What does NOT live here
//!
//! - **Composition with `brain_storage::wal::checkpoint::write_checkpoint`**
//!   — `MetadataSink` owns the conversion from `CheckpointReport` to
//!   [`CheckpointMeta`], filling `metadata_version_at_checkpoint` from
//!   [`crate::storage_version::CURRENT_SCHEMA_VERSION`].
//! - **Retention sweep** (delete old checkpoints); maintenance worker.
//! - **Recovery handshake** (read [`latest`], replay WAL after its
//!   `durable_lsn`).

use redb::{ReadOnlyTable, ReadableTable, TableDefinition};

/// The `checkpoints` table. Key is the monotonic `checkpoint_id`
/// ("Monotonic counter"); value is the durable
/// checkpoint record.
pub const CHECKPOINTS_TABLE: TableDefinition<'static, u64, CheckpointMeta> =
    TableDefinition::new("checkpoints");

/// Persisted checkpoint record.
///
/// Time fields `started_at` / `completed_at` are suffixed
/// `_unix_nanos` to match this crate's time-field convention.
///
/// Named `CheckpointMeta` rather than `Checkpoint` (collides with the
/// WAL `Checkpoint` opcode) or `CheckpointInfo`
/// (inconsistent with this crate's `*Metadata`/`*Info` mix). All other
/// metadata rows in this crate end in `Metadata` (e.g.
/// [`crate::tables::memory::MemoryMetadata`]); the `Meta` suffix
/// preserves that pattern without colliding.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct CheckpointMeta {
    /// Mirrors the table key. Monotonic across checkpoint attempts;
    /// the writer task assigns these (see
    /// `brain_storage::wal::checkpoint::CheckpointPlan`).
    pub checkpoint_id: u64,

    /// All WAL records with LSN ≤ this value are reflected in arena
    /// and metadata. Recovery resumes replay at `durable_lsn + 1`.
    pub durable_lsn: u64,

    /// Arena capacity at the moment of the checkpoint. Recovery uses
    /// this to detect arena truncation between checkpoint and crash.
    pub arena_capacity_at_checkpoint: u64,

    /// The metadata store's schema version at the moment of the
    /// checkpoint. Persisted so a substrate upgrade (newer schema)
    /// can recognise stale checkpoints from older versions.
    pub metadata_version_at_checkpoint: u64,

    /// Unix nanoseconds at the moment the checkpoint worker began
    /// step 1 of (the `CHECKPOINT_BEGIN` write).
    pub started_at_unix_nanos: u64,

    /// Unix nanoseconds at the moment the checkpoint worker finished
    /// step 6 (the `CHECKPOINT_END` write).
    pub completed_at_unix_nanos: u64,
}

impl CheckpointMeta {
    /// Convenience constructor in canonical field order.
    #[must_use]
    pub fn new(
        checkpoint_id: u64,
        durable_lsn: u64,
        arena_capacity_at_checkpoint: u64,
        metadata_version_at_checkpoint: u64,
        started_at_unix_nanos: u64,
        completed_at_unix_nanos: u64,
    ) -> Self {
        Self {
            checkpoint_id,
            durable_lsn,
            arena_capacity_at_checkpoint,
            metadata_version_at_checkpoint,
            started_at_unix_nanos,
            completed_at_unix_nanos,
        }
    }
}

impl redb::Value for CheckpointMeta {
    type SelfType<'a> = CheckpointMeta;
    type AsBytes<'a> = Vec<u8>;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        // rkyv 0.7's validation includes alignment; redb returns bytes
        // at arbitrary alignment, so copy into an AlignedVec first.
        let mut buf = rkyv::AlignedVec::with_capacity(data.len());
        buf.extend_from_slice(data);
        rkyv::from_bytes::<CheckpointMeta>(&buf)
            .expect("CheckpointMeta bytes failed rkyv validation; redb file is corrupt")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        rkyv::to_bytes::<_, 256>(value)
            .expect("CheckpointMeta is rkyv-serializable")
            .into_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("brain_metadata::CheckpointMeta")
    }
}

/// Return the checkpoint with the highest `checkpoint_id`, or `None`
/// if the table is empty: "the substrate keeps the
/// most recent one as the recovery target."
///
/// Implementation: `iter().next_back()` walks one B-tree path to the
/// rightmost leaf — O(log N) regardless of how many checkpoints exist.
pub fn latest(
    table: &ReadOnlyTable<u64, CheckpointMeta>,
) -> Result<Option<CheckpointMeta>, redb::StorageError> {
    match table.iter()?.next_back() {
        Some(entry) => {
            let (_id, value) = entry?;
            Ok(Some(value.value()))
        }
        None => Ok(None),
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase};

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    fn sample(id: u64) -> CheckpointMeta {
        CheckpointMeta::new(
            id,
            id * 1000,   // durable_lsn
            1024 * 1024, // arena_capacity
            1,           // metadata_version
            1_700_000_000_000_000_000 + id * 1_000_000,
            1_700_000_000_000_000_000 + id * 1_000_000 + 12_000,
        )
    }

    #[test]
    fn all_fields_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let m = CheckpointMeta::new(
            7,
            12_345_678,
            64 * 1024 * 1024,
            42,
            1_700_000_000_000_000_000,
            1_700_000_000_001_500_000,
        );

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(CHECKPOINTS_TABLE).unwrap();
            t.insert(&7u64, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(CHECKPOINTS_TABLE).unwrap();
        let got = t.get(&7u64).unwrap().unwrap().value();
        // Spot-check each field individually so a silent reorder
        // produces an obvious failure rather than a generic
        // structural-equality mismatch.
        assert_eq!(got.checkpoint_id, 7);
        assert_eq!(got.durable_lsn, 12_345_678);
        assert_eq!(got.arena_capacity_at_checkpoint, 64 * 1024 * 1024);
        assert_eq!(got.metadata_version_at_checkpoint, 42);
        assert_eq!(got.started_at_unix_nanos, 1_700_000_000_000_000_000);
        assert_eq!(got.completed_at_unix_nanos, 1_700_000_000_001_500_000);
    }

    #[test]
    fn latest_returns_max_id() {
        // The recovery-target pin: insert ids out of order; `latest()`
        // must return the highest id regardless of insert order. This
        // also implicitly validates redb's u64-key ordering matches
        // numerical order (same property pinned by 3.7).
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(CHECKPOINTS_TABLE).unwrap();
            t.insert(&2u64, &sample(2)).unwrap();
            t.insert(&10u64, &sample(10)).unwrap();
            t.insert(&5u64, &sample(5)).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(CHECKPOINTS_TABLE).unwrap();
        let got = latest(&t).unwrap().expect("latest should return some row");
        assert_eq!(got.checkpoint_id, 10);
        assert_eq!(got, sample(10));
    }

    #[test]
    fn latest_returns_none_on_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        {
            let _t = wtxn.open_table(CHECKPOINTS_TABLE).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(CHECKPOINTS_TABLE).unwrap();
        assert!(latest(&t).unwrap().is_none());
    }

    #[test]
    fn latest_after_update() {
        // If the same checkpoint_id row is rewritten (e.g. a worker
        // re-running before durably persisting the previous attempt),
        // `latest()` returns the updated row.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = 5u64;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(CHECKPOINTS_TABLE).unwrap();
            t.insert(&id, &sample(id)).unwrap();
        }
        wtxn.commit().unwrap();

        let mut updated = sample(id);
        updated.durable_lsn = 7_777_777;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(CHECKPOINTS_TABLE).unwrap();
            t.insert(&id, &updated).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(CHECKPOINTS_TABLE).unwrap();
        let got = latest(&t).unwrap().unwrap();
        assert_eq!(got.checkpoint_id, id);
        assert_eq!(got.durable_lsn, 7_777_777);
    }
}
