//! `spaces` table: per-space registry metadata.
//!
//! Keyed by `(namespace_id, space_id)` so each namespace's spaces form a
//! contiguous keyspace — one range scan over the 4-byte namespace prefix
//! lists every space a namespace owns. The row carries the load-bearing
//! provenance fields (created/last-active timestamps) plus denormalized
//! display counts reconciled by a maintenance worker.

use redb::TableDefinition;

/// The `spaces` registry table. Key is the 20-byte
/// `[namespace_id (4, BE) | space_id (16)]` composite; value is
/// [`SpaceMetadata`]. The leading namespace bytes make each namespace's
/// spaces a contiguous, range-scannable keyspace.
pub const SPACES_TABLE: TableDefinition<'static, [u8; 20], SpaceMetadata> =
    TableDefinition::new("spaces");

/// Build the 20-byte `(namespace_id, space_id)` registry key.
#[must_use]
pub fn space_key(namespace_id: u32, space_id: [u8; 16]) -> [u8; 20] {
    let mut k = [0u8; 20];
    k[0..4].copy_from_slice(&namespace_id.to_be_bytes());
    k[4..20].copy_from_slice(&space_id);
    k
}

/// Inclusive `(start, end)` key bounds for a range scan over every space
/// owned by `namespace_id`.
#[must_use]
pub fn space_range_bounds(namespace_id: u32) -> ([u8; 20], [u8; 20]) {
    (
        space_key(namespace_id, [0x00; 16]),
        space_key(namespace_id, [0xFF; 16]),
    )
}

/// Per-space registry row. The `(namespace_id, space_id)` scope lives in
/// the table key, not the value.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct SpaceMetadata {
    pub created_at_unix_nanos: u64,
    pub last_active_unix_nanos: u64,
    /// Denormalized live memory count; display-only, reconciled by the
    /// counter-reconcile maintenance worker.
    pub memory_count: u64,
    /// Denormalized live session count; display-only, same reconciliation.
    pub session_count: u32,
    /// Opaque caller-supplied metadata blob (quota hints, labels).
    /// `None` for zero-ceremony implicit creates.
    pub metadata: Option<Vec<u8>>,
}

impl SpaceMetadata {
    #[must_use]
    pub fn new(created_at_unix_nanos: u64, metadata: Option<Vec<u8>>) -> Self {
        Self {
            created_at_unix_nanos,
            last_active_unix_nanos: created_at_unix_nanos,
            memory_count: 0,
            session_count: 0,
            metadata,
        }
    }
}

impl redb::Value for SpaceMetadata {
    type SelfType<'a> = SpaceMetadata;
    type AsBytes<'a> = Vec<u8>;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let mut buf = rkyv::AlignedVec::with_capacity(data.len());
        buf.extend_from_slice(data);
        rkyv::from_bytes::<SpaceMetadata>(&buf)
            .expect("SpaceMetadata bytes failed rkyv validation; redb file is corrupt")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        rkyv::to_bytes::<_, 256>(value)
            .expect("SpaceMetadata is rkyv-serializable")
            .into_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("brain_metadata::SpaceMetadata")
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase, ReadableTable};

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    #[test]
    fn insert_and_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let key = space_key(7, [0x42; 16]);
        let m = SpaceMetadata::new(1_700_000_000_000_000_000, Some(vec![1, 2, 3]));

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SPACES_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SPACES_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, m);
    }

    #[test]
    fn range_scan_isolates_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SPACES_TABLE).unwrap();
            t.insert(&space_key(1, [0x01; 16]), &SpaceMetadata::new(1, None))
                .unwrap();
            t.insert(&space_key(1, [0x02; 16]), &SpaceMetadata::new(2, None))
                .unwrap();
            t.insert(&space_key(2, [0x03; 16]), &SpaceMetadata::new(3, None))
                .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SPACES_TABLE).unwrap();
        let (start, end) = space_range_bounds(1);
        let count = t.range(start..=end).unwrap().count();
        assert_eq!(count, 2, "namespace 1 owns exactly two spaces");
    }
}
