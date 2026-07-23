//! Three interlocked session tables, co-located because every
//! session-create touches all three:
//!
//! - [`SESSIONS_TABLE`] — `SessionId` → [`SessionMetadata`]: the full
//!   record, looked up by ID.
//! - [`SESSION_NAMES_TABLE`] — `(SpaceId, &str)` → `SessionId`: the
//!   name index, scoped to space.
//! - [`SPACE_SESSIONS_TABLE`] — `(SpaceId, SessionId)` → `()`: the
//!   membership index, supporting "list sessions for space A" via a
//!   prefix range scan.

use brain_core::{SpaceId, SessionId};
use redb::TableDefinition;

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------

/// `SessionId` → full [`SessionMetadata`] record.
pub const SESSIONS_TABLE: TableDefinition<'static, u64, SessionMetadata> =
    TableDefinition::new("sessions");

/// `(SpaceId, name)` → `SessionId`. Index for name-based lookup.
pub const SESSION_NAMES_TABLE: TableDefinition<'static, (&'static [u8; 16], &'static str), u64> =
    TableDefinition::new("session_names");

/// `(SpaceId, SessionId)` → `()`. Index for "list sessions of space" via
/// prefix range scan over the leading 16 bytes.
pub const SPACE_SESSIONS_TABLE: TableDefinition<'static, ([u8; 16], u64), ()> =
    TableDefinition::new("space_sessions");

// ---------------------------------------------------------------------------
// Naming conventions.
// ---------------------------------------------------------------------------

/// Names starting with `_` are reserved. The writer task enforces this
/// against client input; the storage layer itself doesn't validate.
pub const RESERVED_NAME_PREFIX: &str = "_";

/// The implicit "default" session name created on first ENCODE if no
/// session is specified.
pub const DEFAULT_SESSION_NAME: &str = "_default";

// ---------------------------------------------------------------------------
// SessionMetadata.
// ---------------------------------------------------------------------------

/// Per-session metadata row.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct SessionMetadata {
    /// Mirrors the table key for convenience.
    pub session_id: u64,
    pub space_id_bytes: [u8; 16],
    pub name: String,
    pub created_at_unix_nanos: u64,
    pub last_active_at_unix_nanos: u64,
    /// Denormalized; periodically reconciled by the maintenance worker.
    pub memory_count: u32,
    pub description: Option<String>,
    pub tags: Vec<String>,
}

impl SessionMetadata {
    #[must_use]
    pub fn new(
        session_id: SessionId,
        space_id: SpaceId,
        name: String,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            session_id: session_id.raw(),
            space_id_bytes: space_id.into(),
            name,
            created_at_unix_nanos,
            last_active_at_unix_nanos: created_at_unix_nanos,
            memory_count: 0,
            description: None,
            tags: Vec::new(),
        }
    }

    #[must_use]
    pub fn session_id(&self) -> SessionId {
        SessionId(self.session_id)
    }

    #[must_use]
    pub fn space_id(&self) -> SpaceId {
        SpaceId::from(self.space_id_bytes)
    }
}

impl redb::Value for SessionMetadata {
    type SelfType<'a> = SessionMetadata;
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
        rkyv::from_bytes::<SessionMetadata>(&buf)
            .expect("SessionMetadata bytes failed rkyv validation; redb file is corrupt")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        rkyv::to_bytes::<_, 256>(value)
            .expect("SessionMetadata is rkyv-serializable")
            .into_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("brain_metadata::SessionMetadata")
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_core::{SpaceId, SessionId};
    use redb::{Database, ReadableDatabase};

    fn aid(byte: u8) -> SpaceId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    fn sample(session_id: u64, space_byte: u8, name: &str) -> SessionMetadata {
        SessionMetadata::new(
            SessionId(session_id),
            aid(space_byte),
            name.to_string(),
            1_700_000_000_000_000_000,
        )
    }

    // ----- sessions table ------------------------------------------------

    #[test]
    fn sessions_insert_get_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let m = sample(100, 0x42, "alpha");

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SESSIONS_TABLE).unwrap();
            t.insert(&100u64, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SESSIONS_TABLE).unwrap();
        let got = t.get(&100u64).unwrap().unwrap().value();
        assert_eq!(got, m);
        assert_eq!(got.session_id(), SessionId(100));
        assert_eq!(got.space_id(), aid(0x42));
    }

    // ----- session_names index ------------------------------------------

    #[test]
    fn session_names_lookup_by_space_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let space_a = aid(0xAA);
        let space_a_bytes: [u8; 16] = space_a.into();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SESSION_NAMES_TABLE).unwrap();
            t.insert(&(&space_a_bytes, "personal"), &101u64).unwrap();
            t.insert(&(&space_a_bytes, "work"), &102u64).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SESSION_NAMES_TABLE).unwrap();
        let v = t.get(&(&space_a_bytes, "personal")).unwrap().unwrap();
        assert_eq!(v.value(), 101);
        let v = t.get(&(&space_a_bytes, "work")).unwrap().unwrap();
        assert_eq!(v.value(), 102);
        // Missing name returns None.
        assert!(t.get(&(&space_a_bytes, "nonexistent")).unwrap().is_none());
    }

    // ----- space_sessions index -----------------------------------------

    #[test]
    fn space_sessions_range_scan_for_space() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let space_a: [u8; 16] = aid(0xAA).into();
        let space_b: [u8; 16] = aid(0xBB).into();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SPACE_SESSIONS_TABLE).unwrap();
            t.insert(&(space_a, 100u64), &()).unwrap();
            t.insert(&(space_a, 200u64), &()).unwrap();
            t.insert(&(space_a, 300u64), &()).unwrap();
            t.insert(&(space_b, 400u64), &()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SPACE_SESSIONS_TABLE).unwrap();
        // Range scan: all entries for space_a.
        let start = (space_a, 0u64);
        let end = (space_a, u64::MAX);
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

    // ----- Cross-space isolation ----------------------------------------

    #[test]
    fn cross_space_name_isolation() {
        // two spaces can each have a session named
        // "personal"; they're distinct (different SessionIds).
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let space_a: [u8; 16] = aid(0xAA).into();
        let space_b: [u8; 16] = aid(0xBB).into();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SESSION_NAMES_TABLE).unwrap();
            t.insert(&(&space_a, "personal"), &1001u64).unwrap();
            t.insert(&(&space_b, "personal"), &2002u64).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SESSION_NAMES_TABLE).unwrap();
        assert_eq!(
            t.get(&(&space_a, "personal")).unwrap().unwrap().value(),
            1001
        );
        assert_eq!(
            t.get(&(&space_b, "personal")).unwrap().unwrap().value(),
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
            let mut t = wtxn.open_table(SESSIONS_TABLE).unwrap();
            t.insert(&500u64, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SESSIONS_TABLE).unwrap();
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
    fn default_session_name_uses_reserved_prefix() {
        assert!(DEFAULT_SESSION_NAME.starts_with(RESERVED_NAME_PREFIX));
    }
}
