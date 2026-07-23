//! `spaces` table: per-space metadata.
//!
//! ## Minimal shape
//!
//! The row stores the load-bearing fields — SpaceId, display name,
//! created_at, and stats (memory/context counts) — and defers
//! "configuration overrides". Typical workloads don't use overrides,
//! and an `Option<config>` can be added later without a migration.

use brain_core::SpaceId;
use redb::TableDefinition;

/// The `spaces` table. Key is the `SpaceId`'s 16-byte UUID raw form;
/// value is [`SpaceMetadata`].
pub const SPACES_TABLE: TableDefinition<'static, [u8; 16], SpaceMetadata> =
    TableDefinition::new("spaces");

/// Per-space metadata row.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct SpaceMetadata {
    pub space_id_bytes: [u8; 16],
    pub display_name: Option<String>,
    pub created_at_unix_nanos: u64,
    pub last_active_at_unix_nanos: u64,
    /// Denormalized; updated by the maintenance worker.
    pub memory_count: u64,
    /// Denormalized; same.
    pub context_count: u32,
}

impl SpaceMetadata {
    #[must_use]
    pub fn new(
        space_id: SpaceId,
        display_name: Option<String>,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            space_id_bytes: space_id.into(),
            display_name,
            created_at_unix_nanos,
            last_active_at_unix_nanos: created_at_unix_nanos,
            memory_count: 0,
            context_count: 0,
        }
    }

    #[must_use]
    pub fn space_id(&self) -> SpaceId {
        SpaceId::from(self.space_id_bytes)
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
        // rkyv 0.7's validation includes alignment; redb returns bytes
        // at arbitrary alignment, so copy into an AlignedVec first.
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
    use brain_core::SpaceId;
    use redb::{Database, ReadableDatabase};

    fn aid(byte: u8) -> SpaceId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    fn sample(byte: u8) -> SpaceMetadata {
        SpaceMetadata::new(
            aid(byte),
            Some(format!("space-{byte:02x}")),
            1_700_000_000_000_000_000,
        )
    }

    #[test]
    fn insert_and_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let m = sample(7);
        let key = m.space_id_bytes;

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
    fn brain_core_type_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let space = aid(0x42);
        let m = SpaceMetadata::new(space, None, 1_700_000_000_000_000_000);
        let key = m.space_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SPACES_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SPACES_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got.space_id(), space);
        assert_eq!(got.display_name, None);
    }
}
