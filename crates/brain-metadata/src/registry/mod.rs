//! Space & session registry operations.
//!
//! Both registries are **derived, recomputable** view state: every row is
//! reconstructable from the memory/graph data it summarizes, so a maintenance
//! worker reconciles the denormalized counts and recovery replays the write
//! records cleanly. These helpers are the single write/read surface both the
//! apply layer (live + recovery) and the wire handlers call.
//!
//! - A space/session row is created two ways: **implicitly** on first write
//!   ([`touch_on_write`], zero-ceremony) and **explicitly** via the CRUD
//!   helpers ([`space_create`] / [`session_create`], idempotent provision).
//! - Isolation is the table key's leading `(namespace_id, space_id)` prefix —
//!   a range scan for one scope can never traverse another's rows.

use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::session::{
    session_key, session_range_bounds, session_scope_key, session_scope_range_bounds,
    SessionMetadata, SESSIONS_TABLE, SESSION_BY_SCOPE_TABLE,
};
use crate::tables::space::{space_key, space_range_bounds, SpaceMetadata, SPACES_TABLE};

/// Failure operating the space/session registry.
#[derive(thiserror::Error, Debug)]
pub enum RegistryError {
    #[error("registry storage error: {0}")]
    Storage(String),
}

fn store<E: std::fmt::Display>(e: E) -> RegistryError {
    RegistryError::Storage(e.to_string())
}

/// One space in a [`space_list`] page.
#[derive(Debug, Clone, PartialEq)]
pub struct SpaceListEntry {
    pub space_id: [u8; 16],
    pub meta: SpaceMetadata,
}

/// One session in a [`session_list`] page.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionListEntry {
    pub session_id: u64,
    pub meta: SessionMetadata,
}

// ---------------------------------------------------------------------------
// Implicit create/update on write.
// ---------------------------------------------------------------------------

/// Upsert the space + session registry rows for a memory write, IN THE SAME
/// wtxn as the memory. Creates rows on first sight (stamping `created_at`),
/// otherwise bumps `last_active` and the denormalized memory count. Called by
/// both the live apply path and WAL recovery so the registry survives a crash.
pub fn touch_on_write(
    wtxn: &WriteTransaction,
    namespace_id: u32,
    space_id: [u8; 16],
    space_string: &str,
    session_id: u64,
    at_unix_nanos: u64,
) -> Result<(), RegistryError> {
    let session_created =
        touch_session_row(wtxn, namespace_id, space_id, session_id, at_unix_nanos)?;
    touch_space_row(
        wtxn,
        namespace_id,
        space_id,
        space_string,
        at_unix_nanos,
        session_created,
    )?;
    Ok(())
}

fn touch_session_row(
    wtxn: &WriteTransaction,
    namespace_id: u32,
    space_id: [u8; 16],
    session_id: u64,
    at: u64,
) -> Result<bool, RegistryError> {
    let key = session_key(namespace_id, space_id, session_id);
    let mut sessions = wtxn.open_table(SESSIONS_TABLE).map_err(store)?;
    let mut scope = wtxn.open_table(SESSION_BY_SCOPE_TABLE).map_err(store)?;
    let existing = {
        let g = sessions.get(&key).map_err(store)?;
        g.map(|v| v.value())
    };
    match existing {
        Some(mut m) => {
            if at > m.last_active_unix_nanos {
                let old =
                    session_scope_key(namespace_id, space_id, m.last_active_unix_nanos, session_id);
                scope.remove(&old).map_err(store)?;
                m.last_active_unix_nanos = at;
                scope
                    .insert(
                        &session_scope_key(namespace_id, space_id, at, session_id),
                        &(),
                    )
                    .map_err(store)?;
            }
            m.memory_count = m.memory_count.saturating_add(1);
            sessions.insert(&key, &m).map_err(store)?;
            Ok(false)
        }
        None => {
            let mut m = SessionMetadata::new(at, None);
            m.memory_count = 1;
            sessions.insert(&key, &m).map_err(store)?;
            scope
                .insert(
                    &session_scope_key(namespace_id, space_id, at, session_id),
                    &(),
                )
                .map_err(store)?;
            Ok(true)
        }
    }
}

fn touch_space_row(
    wtxn: &WriteTransaction,
    namespace_id: u32,
    space_id: [u8; 16],
    space_string: &str,
    at: u64,
    session_created: bool,
) -> Result<(), RegistryError> {
    let key = space_key(namespace_id, space_id);
    let mut spaces = wtxn.open_table(SPACES_TABLE).map_err(store)?;
    let existing = {
        let g = spaces.get(&key).map_err(store)?;
        g.map(|v| v.value())
    };
    match existing {
        Some(mut m) => {
            m.last_active_unix_nanos = m.last_active_unix_nanos.max(at);
            m.memory_count = m.memory_count.saturating_add(1);
            if session_created {
                m.session_count = m.session_count.saturating_add(1);
            }
            // Back-fill the human string on a row first created without one
            // (e.g. an implicit touch that preceded any string-bearing write).
            if m.space_string.is_empty() && !space_string.is_empty() {
                m.space_string = space_string.to_string();
            }
            spaces.insert(&key, &m).map_err(store)?;
        }
        None => {
            let mut m = SpaceMetadata::new(at, space_string.to_string(), None);
            m.memory_count = 1;
            m.session_count = u32::from(session_created);
            spaces.insert(&key, &m).map_err(store)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Space CRUD.
// ---------------------------------------------------------------------------

/// Explicit space provision. Idempotent: a create for an existing
/// `(namespace, space)` returns the existing row (not a conflict). Returns
/// `(row, created)`.
pub fn space_create(
    wtxn: &WriteTransaction,
    namespace_id: u32,
    space_id: [u8; 16],
    space_string: String,
    at_unix_nanos: u64,
    metadata: Option<Vec<u8>>,
) -> Result<(SpaceMetadata, bool), RegistryError> {
    let key = space_key(namespace_id, space_id);
    let mut spaces = wtxn.open_table(SPACES_TABLE).map_err(store)?;
    let existing = {
        let g = spaces.get(&key).map_err(store)?;
        g.map(|v| v.value())
    };
    if let Some(m) = existing {
        return Ok((m, false));
    }
    let m = SpaceMetadata::new(at_unix_nanos, space_string, metadata);
    spaces.insert(&key, &m).map_err(store)?;
    Ok((m, true))
}

/// Read one space registry row.
pub fn space_get(
    rtxn: &ReadTransaction,
    namespace_id: u32,
    space_id: [u8; 16],
) -> Result<Option<SpaceMetadata>, RegistryError> {
    let t = rtxn.open_table(SPACES_TABLE).map_err(store)?;
    Ok(t.get(&space_key(namespace_id, space_id))
        .map_err(store)?
        .map(|g| g.value()))
}

/// List a namespace's spaces via a single range scan over the namespace
/// prefix. `limit == 0` means "no cap".
pub fn space_list(
    rtxn: &ReadTransaction,
    namespace_id: u32,
    limit: usize,
) -> Result<Vec<SpaceListEntry>, RegistryError> {
    let t = rtxn.open_table(SPACES_TABLE).map_err(store)?;
    let (start, end) = space_range_bounds(namespace_id);
    let mut out = Vec::new();
    for entry in t.range(start..=end).map_err(store)? {
        let (k, v) = entry.map_err(store)?;
        let key = k.value();
        let mut space_id = [0u8; 16];
        space_id.copy_from_slice(&key[4..20]);
        out.push(SpaceListEntry {
            space_id,
            meta: v.value(),
        });
        if limit != 0 && out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Remove the space registry row plus every session row and scope-index row
/// under `(namespace, space)`. Returns whether the space row existed. The
/// underlying memory/graph data cascade is the handler's job (it reuses the
/// FORGET machinery); this clears only the registry view.
pub fn space_delete_registry(
    wtxn: &WriteTransaction,
    namespace_id: u32,
    space_id: [u8; 16],
) -> Result<bool, RegistryError> {
    // Collect the scope's session ids + their last_active so we can drop both
    // the record and the scope-index rows.
    let mut victims: Vec<(u64, u64)> = Vec::new();
    {
        let sessions = wtxn.open_table(SESSIONS_TABLE).map_err(store)?;
        let (start, end) = session_range_bounds(namespace_id, space_id);
        for entry in sessions.range(start..=end).map_err(store)? {
            let (k, v) = entry.map_err(store)?;
            let session_id = u64::from_be_bytes(
                k.value()[20..28]
                    .try_into()
                    .expect("invariant: 8-byte session_id slice from a fixed-width key"),
            );
            victims.push((session_id, v.value().last_active_unix_nanos));
        }
    }
    {
        let mut sessions = wtxn.open_table(SESSIONS_TABLE).map_err(store)?;
        let mut scope = wtxn.open_table(SESSION_BY_SCOPE_TABLE).map_err(store)?;
        for (session_id, last_active) in &victims {
            sessions
                .remove(&session_key(namespace_id, space_id, *session_id))
                .map_err(store)?;
            scope
                .remove(&session_scope_key(
                    namespace_id,
                    space_id,
                    *last_active,
                    *session_id,
                ))
                .map_err(store)?;
        }
    }
    let mut spaces = wtxn.open_table(SPACES_TABLE).map_err(store)?;
    let existed = spaces
        .remove(&space_key(namespace_id, space_id))
        .map_err(store)?
        .is_some();
    Ok(existed)
}

// ---------------------------------------------------------------------------
// Session CRUD.
// ---------------------------------------------------------------------------

/// Explicit session provision. Idempotent: a create for an existing
/// `(namespace, space, session_id)` returns the existing row. Ensures the
/// owning space registry row exists (creating it on first sight). Returns
/// `(row, created)`.
pub fn session_create(
    wtxn: &WriteTransaction,
    namespace_id: u32,
    space_id: [u8; 16],
    session_id: u64,
    at_unix_nanos: u64,
    title: Option<String>,
) -> Result<(SessionMetadata, bool), RegistryError> {
    let key = session_key(namespace_id, space_id, session_id);
    {
        let sessions = wtxn.open_table(SESSIONS_TABLE).map_err(store)?;
        let existing = {
            let g = sessions.get(&key).map_err(store)?;
            g.map(|v| v.value())
        };
        if let Some(m) = existing {
            return Ok((m, false));
        }
    }
    let m = SessionMetadata::new(at_unix_nanos, title);
    {
        let mut sessions = wtxn.open_table(SESSIONS_TABLE).map_err(store)?;
        sessions.insert(&key, &m).map_err(store)?;
        let mut scope = wtxn.open_table(SESSION_BY_SCOPE_TABLE).map_err(store)?;
        scope
            .insert(
                &session_scope_key(namespace_id, space_id, at_unix_nanos, session_id),
                &(),
            )
            .map_err(store)?;
    }
    // Keep the owning space's session_count coherent; create the space row on
    // first sight so a session never dangles without its parent. A session
    // create carries no space string of its own; the owning space's string is
    // set by its own create / first string-bearing write.
    touch_space_row(wtxn, namespace_id, space_id, "", at_unix_nanos, true)?;
    Ok((m, true))
}

/// Read one session registry row.
pub fn session_get(
    rtxn: &ReadTransaction,
    namespace_id: u32,
    space_id: [u8; 16],
    session_id: u64,
) -> Result<Option<SessionMetadata>, RegistryError> {
    let t = rtxn.open_table(SESSIONS_TABLE).map_err(store)?;
    Ok(t.get(&session_key(namespace_id, space_id, session_id))
        .map_err(store)?
        .map(|g| g.value()))
}

/// List one `(namespace, space)`'s sessions newest-first via a reverse range
/// scan over the scope index. `limit == 0` means "no cap".
pub fn session_list(
    rtxn: &ReadTransaction,
    namespace_id: u32,
    space_id: [u8; 16],
    limit: usize,
) -> Result<Vec<SessionListEntry>, RegistryError> {
    let scope = rtxn.open_table(SESSION_BY_SCOPE_TABLE).map_err(store)?;
    let sessions = rtxn.open_table(SESSIONS_TABLE).map_err(store)?;
    let (start, end) = session_scope_range_bounds(namespace_id, space_id);
    let mut out = Vec::new();
    for entry in scope.range(start..=end).map_err(store)?.rev() {
        let (k, _) = entry.map_err(store)?;
        let session_id = u64::from_be_bytes(
            k.value()[28..36]
                .try_into()
                .expect("invariant: 8-byte session_id slice from a fixed-width key"),
        );
        if let Some(g) = sessions
            .get(&session_key(namespace_id, space_id, session_id))
            .map_err(store)?
        {
            out.push(SessionListEntry {
                session_id,
                meta: g.value(),
            });
        }
        if limit != 0 && out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Remove one session's registry row + scope-index row and decrement the
/// owning space's session_count. Returns whether the session existed. The
/// memory/graph cascade is the handler's job.
pub fn session_delete_registry(
    wtxn: &WriteTransaction,
    namespace_id: u32,
    space_id: [u8; 16],
    session_id: u64,
) -> Result<bool, RegistryError> {
    let key = session_key(namespace_id, space_id, session_id);
    let existing: Option<SessionMetadata> = {
        let sessions = wtxn.open_table(SESSIONS_TABLE).map_err(store)?;
        let g = sessions.get(&key).map_err(store)?;
        g.map(|x| x.value())
    };
    let Some(m) = existing else {
        return Ok(false);
    };
    {
        let mut sessions = wtxn.open_table(SESSIONS_TABLE).map_err(store)?;
        sessions.remove(&key).map_err(store)?;
        let mut scope = wtxn.open_table(SESSION_BY_SCOPE_TABLE).map_err(store)?;
        scope
            .remove(&session_scope_key(
                namespace_id,
                space_id,
                m.last_active_unix_nanos,
                session_id,
            ))
            .map_err(store)?;
    }
    let mut spaces = wtxn.open_table(SPACES_TABLE).map_err(store)?;
    let skey = space_key(namespace_id, space_id);
    let cur = {
        let g = spaces.get(&skey).map_err(store)?;
        g.map(|v| v.value())
    };
    if let Some(mut sm) = cur {
        sm.session_count = sm.session_count.saturating_sub(1);
        spaces.insert(&skey, &sm).map_err(store)?;
    }
    Ok(true)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::MetadataDb;

    fn db() -> (tempfile::TempDir, MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("m.redb")).unwrap();
        (dir, db)
    }

    #[test]
    fn implicit_touch_creates_then_bumps() {
        let (_d, db) = db();
        let space = [0x11; 16];
        let w = db.write_txn().unwrap();
        touch_on_write(&w, 3, space, "u:1", 0, 100).unwrap();
        touch_on_write(&w, 3, space, "u:1", 0, 200).unwrap();
        touch_on_write(&w, 3, space, "u:1", 5, 300).unwrap();
        w.commit().unwrap();

        let r = db.read_txn().unwrap();
        let s = space_get(&r, 3, space).unwrap().unwrap();
        assert_eq!(
            s.space_string, "u:1",
            "implicit touch records the human string"
        );
        assert_eq!(s.created_at_unix_nanos, 100);
        assert_eq!(s.last_active_unix_nanos, 300);
        assert_eq!(s.memory_count, 3);
        assert_eq!(s.session_count, 2, "default + session 5");
        let sessions = session_list(&r, 3, space, 0).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, 5, "newest-first");
    }

    #[test]
    fn create_is_idempotent_then_delete() {
        let (_d, db) = db();
        let space = [0x22; 16];
        let w = db.write_txn().unwrap();
        let (_m, created) = space_create(&w, 1, space, "u:22".into(), 10, None).unwrap();
        assert!(created);
        let (_m, created2) = space_create(&w, 1, space, "u:22".into(), 20, None).unwrap();
        assert!(!created2, "second create returns existing");
        session_create(&w, 1, space, 7, 30, Some("t".into())).unwrap();
        w.commit().unwrap();

        let w = db.write_txn().unwrap();
        assert!(space_delete_registry(&w, 1, space).unwrap());
        w.commit().unwrap();
        let r = db.read_txn().unwrap();
        assert!(space_get(&r, 1, space).unwrap().is_none());
        assert!(session_list(&r, 1, space, 0).unwrap().is_empty());
    }
}
