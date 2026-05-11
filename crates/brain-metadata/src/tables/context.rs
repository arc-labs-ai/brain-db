//! Three interlocked context tables, co-located because every
//! context-create touches all three:
//!
//! - [`CONTEXTS_TABLE`] — `ContextId` → [`ContextMetadata`]: the full
//!   record, looked up by ID.
//! - [`CONTEXT_NAMES_TABLE`] — `(AgentId, &str)` → `ContextId`: the
//!   name index, scoped to agent (spec §07/05 §2.2).
//! - [`AGENT_CONTEXTS_TABLE`] — `(AgentId, ContextId)` → `()`: the
//!   membership index, supporting "list contexts for agent A" via a
//!   prefix range scan (spec §07/05 §2.3).
//!
//! See `spec/07_metadata_graph/05_context_table.md` (full).

use brain_core::{AgentId, ContextId};
use redb::TableDefinition;

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------

/// `ContextId` → full [`ContextMetadata`] record.
pub const CONTEXTS_TABLE: TableDefinition<'static, u64, ContextMetadata> =
    TableDefinition::new("contexts");

/// `(AgentId, name)` → `ContextId`. Index for name-based lookup.
pub const CONTEXT_NAMES_TABLE: TableDefinition<'static, (&'static [u8; 16], &'static str), u64> =
    TableDefinition::new("context_names");

/// `(AgentId, ContextId)` → `()`. Index for "list contexts of agent" via
/// prefix range scan over the leading 16 bytes.
pub const AGENT_CONTEXTS_TABLE: TableDefinition<'static, ([u8; 16], u64), ()> =
    TableDefinition::new("agent_contexts");

// ---------------------------------------------------------------------------
// Naming conventions.
// ---------------------------------------------------------------------------

/// Names starting with `_` are reserved (spec §07/05 §6). The writer
/// task (Phase 9) enforces this against client input; the storage layer
/// itself doesn't validate.
pub const RESERVED_NAME_PREFIX: &str = "_";

/// The implicit "default" context name created on first ENCODE if no
/// context is specified (spec §07/05 §6).
pub const DEFAULT_CONTEXT_NAME: &str = "_default";

// ---------------------------------------------------------------------------
// ContextMetadata.
// ---------------------------------------------------------------------------

/// Per-context metadata row. Spec §07/05 §2.1.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ContextMetadata {
    /// Mirrors the table key for convenience.
    pub context_id: u64,
    pub agent_id_bytes: [u8; 16],
    pub name: String,
    pub created_at_unix_nanos: u64,
    pub last_active_at_unix_nanos: u64,
    /// Denormalized; periodically reconciled by the maintenance worker.
    pub memory_count: u32,
    pub description: Option<String>,
    pub tags: Vec<String>,
}

impl ContextMetadata {
    #[must_use]
    pub fn new(
        context_id: ContextId,
        agent_id: AgentId,
        name: String,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            context_id: context_id.raw(),
            agent_id_bytes: agent_id.into(),
            name,
            created_at_unix_nanos,
            last_active_at_unix_nanos: created_at_unix_nanos,
            memory_count: 0,
            description: None,
            tags: Vec::new(),
        }
    }

    #[must_use]
    pub fn context_id(&self) -> ContextId {
        ContextId(self.context_id)
    }

    #[must_use]
    pub fn agent_id(&self) -> AgentId {
        AgentId::from(self.agent_id_bytes)
    }
}

impl redb::Value for ContextMetadata {
    type SelfType<'a> = ContextMetadata;
    type AsBytes<'a> = Vec<u8>;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        rkyv::from_bytes::<ContextMetadata>(data)
            .expect("ContextMetadata bytes failed rkyv validation; redb file is corrupt")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        rkyv::to_bytes::<_, 256>(value)
            .expect("ContextMetadata is rkyv-serializable")
            .into_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("brain_metadata::ContextMetadata::v1")
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_core::{AgentId, ContextId};
    use redb::{Database, ReadableDatabase};

    fn aid(byte: u8) -> AgentId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    fn sample(context_id: u64, agent_byte: u8, name: &str) -> ContextMetadata {
        ContextMetadata::new(
            ContextId(context_id),
            aid(agent_byte),
            name.to_string(),
            1_700_000_000_000_000_000,
        )
    }

    // ----- contexts table ------------------------------------------------

    #[test]
    fn contexts_insert_get_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let m = sample(100, 0x42, "alpha");

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(CONTEXTS_TABLE).unwrap();
            t.insert(&100u64, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(CONTEXTS_TABLE).unwrap();
        let got = t.get(&100u64).unwrap().unwrap().value();
        assert_eq!(got, m);
        assert_eq!(got.context_id(), ContextId(100));
        assert_eq!(got.agent_id(), aid(0x42));
    }

    // ----- context_names index ------------------------------------------

    #[test]
    fn context_names_lookup_by_agent_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let agent_a = aid(0xAA);
        let agent_a_bytes: [u8; 16] = agent_a.into();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(CONTEXT_NAMES_TABLE).unwrap();
            t.insert(&(&agent_a_bytes, "personal"), &101u64).unwrap();
            t.insert(&(&agent_a_bytes, "work"), &102u64).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(CONTEXT_NAMES_TABLE).unwrap();
        let v = t.get(&(&agent_a_bytes, "personal")).unwrap().unwrap();
        assert_eq!(v.value(), 101);
        let v = t.get(&(&agent_a_bytes, "work")).unwrap().unwrap();
        assert_eq!(v.value(), 102);
        // Missing name returns None.
        assert!(t.get(&(&agent_a_bytes, "nonexistent")).unwrap().is_none());
    }

    // ----- agent_contexts index -----------------------------------------

    #[test]
    fn agent_contexts_range_scan_for_agent() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let agent_a: [u8; 16] = aid(0xAA).into();
        let agent_b: [u8; 16] = aid(0xBB).into();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(AGENT_CONTEXTS_TABLE).unwrap();
            t.insert(&(agent_a, 100u64), &()).unwrap();
            t.insert(&(agent_a, 200u64), &()).unwrap();
            t.insert(&(agent_a, 300u64), &()).unwrap();
            t.insert(&(agent_b, 400u64), &()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(AGENT_CONTEXTS_TABLE).unwrap();
        // Range scan: all entries for agent_a.
        let start = (agent_a, 0u64);
        let end = (agent_a, u64::MAX);
        let mut ctx_ids: Vec<u64> = t
            .range(start..=end)
            .unwrap()
            .map(|entry| {
                let (k, _v) = entry.unwrap();
                k.value().1
            })
            .collect();
        ctx_ids.sort();
        assert_eq!(ctx_ids, vec![100, 200, 300]);
    }

    // ----- Cross-agent isolation ----------------------------------------

    #[test]
    fn cross_agent_name_isolation() {
        // Spec §07/05 §13: two agents can each have a context named
        // "personal"; they're distinct (different ContextIds).
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let agent_a: [u8; 16] = aid(0xAA).into();
        let agent_b: [u8; 16] = aid(0xBB).into();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(CONTEXT_NAMES_TABLE).unwrap();
            t.insert(&(&agent_a, "personal"), &1001u64).unwrap();
            t.insert(&(&agent_b, "personal"), &2002u64).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(CONTEXT_NAMES_TABLE).unwrap();
        assert_eq!(
            t.get(&(&agent_a, "personal")).unwrap().unwrap().value(),
            1001
        );
        assert_eq!(
            t.get(&(&agent_b, "personal")).unwrap().unwrap().value(),
            2002
        );
    }

    // ----- Variable-length rkyv round-trip ------------------------------

    #[test]
    fn description_and_tags_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let mut m = sample(500, 0x33, "project_alpha");
        m.description = Some("Notes and ideas for project alpha".to_string());
        m.tags = vec![
            "active".to_string(),
            "engineering".to_string(),
            "q1-2026".to_string(),
        ];

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(CONTEXTS_TABLE).unwrap();
            t.insert(&500u64, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(CONTEXTS_TABLE).unwrap();
        let got = t.get(&500u64).unwrap().unwrap().value();
        assert_eq!(
            got.description.as_deref(),
            Some("Notes and ideas for project alpha")
        );
        assert_eq!(got.tags.len(), 3);
        assert_eq!(got.tags[1], "engineering");
    }

    // ----- Naming constants sanity --------------------------------------

    #[test]
    fn default_context_name_uses_reserved_prefix() {
        assert!(DEFAULT_CONTEXT_NAME.starts_with(RESERVED_NAME_PREFIX));
    }
}
