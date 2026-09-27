//! `sessions` registry: per-session metadata scoped to `(namespace, space)`.
//!
//! Two tables, co-located because a session-create touches both:
//!
//! - [`SESSIONS_TABLE`] — `(namespace_id, space_id, session_id)` →
//!   [`SessionMetadata`]: the record, looked up by id.
//! - [`SESSION_BY_SCOPE_TABLE`] — `(namespace_id, space_id,
//!   last_active, session_id)` → `()`: the ordering index, so
//!   `SESSION_LIST` returns newest-first via a reverse prefix range scan
//!   over one `(namespace, space)` scope.

use redb::TableDefinition;

/// `(namespace_id, space_id, session_id)` → full [`SessionMetadata`].
/// Key is the 28-byte `[ns (4, BE) | space (16) | session_id (8, BE)]`
/// composite.
pub const SESSIONS_TABLE: TableDefinition<'static, [u8; 28], SessionMetadata> =
    TableDefinition::new("sessions");

/// `(namespace_id, space_id, last_active, session_id)` → `()`. Orders a
/// scope's sessions by activity so `SESSION_LIST` returns newest-first
/// via a reverse range scan over the 20-byte `(ns, space)` prefix. Key is
/// the 36-byte `[ns (4) | space (16) | last_active (8, BE) | session_id
/// (8, BE)]` composite.
pub const SESSION_BY_SCOPE_TABLE: TableDefinition<'static, [u8; 36], ()> =
    TableDefinition::new("session_by_scope");

/// The default session (`session_id = 0`). Present implicitly for every
/// space; memories encoded without an explicit session land here. It is
/// never deletable.
pub const DEFAULT_SESSION_ID: u64 = 0;

/// Build the 28-byte `(namespace, space, session)` record key.
#[must_use]
pub fn session_key(namespace_id: u32, space_id: [u8; 16], session_id: u64) -> [u8; 28] {
    let mut k = [0u8; 28];
    k[0..4].copy_from_slice(&namespace_id.to_be_bytes());
    k[4..20].copy_from_slice(&space_id);
    k[20..28].copy_from_slice(&session_id.to_be_bytes());
    k
}

/// Build the 36-byte `(namespace, space, last_active, session)` scope-index
/// key.
#[must_use]
pub fn session_scope_key(
    namespace_id: u32,
    space_id: [u8; 16],
    last_active_unix_nanos: u64,
    session_id: u64,
) -> [u8; 36] {
    let mut k = [0u8; 36];
    k[0..4].copy_from_slice(&namespace_id.to_be_bytes());
    k[4..20].copy_from_slice(&space_id);
    k[20..28].copy_from_slice(&last_active_unix_nanos.to_be_bytes());
    k[28..36].copy_from_slice(&session_id.to_be_bytes());
    k
}

/// Inclusive `(start, end)` bounds for a range scan over every session in
/// one `(namespace, space)` scope (records table).
#[must_use]
pub fn session_range_bounds(namespace_id: u32, space_id: [u8; 16]) -> ([u8; 28], [u8; 28]) {
    (
        session_key(namespace_id, space_id, u64::MIN),
        session_key(namespace_id, space_id, u64::MAX),
    )
}

/// Inclusive `(start, end)` bounds for a range scan over the scope index
/// of one `(namespace, space)`. Iterate the result reversed for
/// newest-first ordering.
#[must_use]
pub fn session_scope_range_bounds(namespace_id: u32, space_id: [u8; 16]) -> ([u8; 36], [u8; 36]) {
    (
        session_scope_key(namespace_id, space_id, u64::MIN, u64::MIN),
        session_scope_key(namespace_id, space_id, u64::MAX, u64::MAX),
    )
}

/// Per-session registry row. The `(namespace, space, session_id)` scope
/// lives in the table key, not the value.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
pub struct SessionMetadata {
    pub created_at_unix_nanos: u64,
    pub last_active_unix_nanos: u64,
    /// Optional caller-supplied display title.
    pub title: Option<String>,
    /// Denormalized live memory count; display-only, reconciled by the
    /// counter-reconcile maintenance worker.
    pub memory_count: u32,
}

impl SessionMetadata {
    #[must_use]
    pub fn new(created_at_unix_nanos: u64, title: Option<String>) -> Self {
        Self {
            created_at_unix_nanos,
            last_active_unix_nanos: created_at_unix_nanos,
            title,
            memory_count: 0,
        }
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
        let mut buf = rkyv::util::AlignedVec::<16>::with_capacity(data.len());
        buf.extend_from_slice(data);
        rkyv::from_bytes::<SessionMetadata, rkyv::rancor::Error>(&buf)
            .expect("SessionMetadata bytes failed rkyv validation; redb file is corrupt")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        rkyv::to_bytes::<rkyv::rancor::Error>(value)
            .expect("SessionMetadata is rkyv-serializable")
            .into_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("brain_metadata::SessionMetadata")
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase};

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    #[test]
    fn sessions_insert_get_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let key = session_key(1, [0x42; 16], 100);
        let m = SessionMetadata::new(1_700_000_000_000_000_000, Some("alpha".into()));

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SESSIONS_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SESSIONS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, m);
    }

    #[test]
    fn scope_index_orders_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let space = [0xAA; 16];
        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SESSION_BY_SCOPE_TABLE).unwrap();
            t.insert(&session_scope_key(1, space, 100, 10), &())
                .unwrap();
            t.insert(&session_scope_key(1, space, 300, 30), &())
                .unwrap();
            t.insert(&session_scope_key(1, space, 200, 20), &())
                .unwrap();
            // A different space must not appear in the scan.
            t.insert(&session_scope_key(1, [0xBB; 16], 999, 99), &())
                .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SESSION_BY_SCOPE_TABLE).unwrap();
        let (start, end) = session_scope_range_bounds(1, space);
        let ids: Vec<u64> = t
            .range(start..=end)
            .unwrap()
            .rev()
            .map(|e| {
                let (k, _) = e.unwrap();
                u64::from_be_bytes(k.value()[28..36].try_into().unwrap())
            })
            .collect();
        assert_eq!(ids, vec![30, 20, 10], "newest-first by last_active");
    }
}
