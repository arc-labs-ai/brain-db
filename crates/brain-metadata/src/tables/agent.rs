//! `agents` table: per-agent metadata.
//!
//! ## Minimal shape
//!
//! The row stores the load-bearing fields — AgentId, display name,
//! created_at, and stats (memory/context counts) — and defers
//! "configuration overrides". Typical workloads don't use overrides,
//! and an `Option<config>` can be added later without a migration.

use brain_core::AgentId;
use redb::TableDefinition;

/// The `agents` table. Key is the `AgentId`'s 16-byte UUID raw form;
/// value is [`AgentMetadata`].
pub const AGENTS_TABLE: TableDefinition<'static, [u8; 16], AgentMetadata> =
    TableDefinition::new("agents");

/// Per-agent metadata row.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct AgentMetadata {
    pub agent_id_bytes: [u8; 16],
    pub display_name: Option<String>,
    pub created_at_unix_nanos: u64,
    pub last_active_at_unix_nanos: u64,
    /// Denormalized; updated by the maintenance worker.
    pub memory_count: u64,
    /// Denormalized; same.
    pub context_count: u32,
}

impl AgentMetadata {
    #[must_use]
    pub fn new(
        agent_id: AgentId,
        display_name: Option<String>,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            agent_id_bytes: agent_id.into(),
            display_name,
            created_at_unix_nanos,
            last_active_at_unix_nanos: created_at_unix_nanos,
            memory_count: 0,
            context_count: 0,
        }
    }

    #[must_use]
    pub fn agent_id(&self) -> AgentId {
        AgentId::from(self.agent_id_bytes)
    }
}

impl redb::Value for AgentMetadata {
    type SelfType<'a> = AgentMetadata;
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
        rkyv::from_bytes::<AgentMetadata>(&buf)
            .expect("AgentMetadata bytes failed rkyv validation; redb file is corrupt")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        rkyv::to_bytes::<_, 256>(value)
            .expect("AgentMetadata is rkyv-serializable")
            .into_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("brain_metadata::AgentMetadata")
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_core::AgentId;
    use redb::{Database, ReadableDatabase};

    fn aid(byte: u8) -> AgentId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    fn sample(byte: u8) -> AgentMetadata {
        AgentMetadata::new(
            aid(byte),
            Some(format!("agent-{byte:02x}")),
            1_700_000_000_000_000_000,
        )
    }

    #[test]
    fn insert_and_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let m = sample(7);
        let key = m.agent_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(AGENTS_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(AGENTS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, m);
    }

    #[test]
    fn brain_core_type_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let agent = aid(0x42);
        let m = AgentMetadata::new(agent, None, 1_700_000_000_000_000_000);
        let key = m.agent_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(AGENTS_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(AGENTS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got.agent_id(), agent);
        assert_eq!(got.display_name, None);
    }
}
