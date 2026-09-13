//! Typed CRUD + interning over the namespace (tenant) registry.
//!
//! A namespace is the company-level data boundary. Interning maps the
//! human namespace name (`acme`) to a compact [`NamespaceId`] that gets
//! stamped onto every owned row and folded into secondary-index keys.
//! The reserved `brain` system namespace is seeded at
//! [`NamespaceId::SYSTEM`] (`0`); user namespaces start at `1`.
//!
//! Mirrors the predicate registry: a forward record table keyed by id
//! and a `name → id` reverse index for the once-per-connection AUTH-time
//! lookup.

use brain_core::NamespaceId;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::system_schema::SYSTEM_SCHEMA_NAMESPACE;
use crate::tables::namespace::{NamespaceDefinition, NAMESPACES_TABLE, NAMESPACE_BY_NAME_TABLE};

#[derive(thiserror::Error, Debug)]
pub enum NamespaceOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
}

/// Resolve a namespace name to its id within a read transaction.
/// `Ok(None)` if the namespace has never been interned.
pub fn namespace_lookup_by_name(
    rtxn: &ReadTransaction,
    name: &str,
) -> Result<Option<NamespaceId>, NamespaceOpError> {
    let t = rtxn.open_table(NAMESPACE_BY_NAME_TABLE)?;
    let hit = t.get(name)?;
    Ok(hit.map(|v| NamespaceId::from(v.value())))
}

/// Write-transaction counterpart to [`namespace_lookup_by_name`].
pub fn namespace_lookup_by_name_wtxn(
    wtxn: &WriteTransaction,
    name: &str,
) -> Result<Option<NamespaceId>, NamespaceOpError> {
    let t = wtxn.open_table(NAMESPACE_BY_NAME_TABLE)?;
    let hit = t.get(name)?;
    Ok(hit.map(|v| NamespaceId::from(v.value())))
}

/// Resolve a namespace id back to its name (e.g. for capability
/// listing or diagnostics). `Ok(None)` if the id is unknown.
pub fn namespace_name(
    rtxn: &ReadTransaction,
    id: NamespaceId,
) -> Result<Option<String>, NamespaceOpError> {
    let t = rtxn.open_table(NAMESPACES_TABLE)?;
    let hit = t.get(&id.raw())?;
    Ok(hit.map(|v| v.value().name))
}

/// Intern a namespace by name, returning its stable id. Idempotent: a
/// name already present returns its existing id. The reserved `brain`
/// system namespace always resolves to [`NamespaceId::SYSTEM`]. User
/// namespaces are allocated `max(existing) + 1`, so the first user
/// namespace gets id `1` (system `brain` holds `0`).
pub fn namespace_intern_or_get(
    wtxn: &WriteTransaction,
    name: &str,
    now_unix_nanos: u64,
) -> Result<NamespaceId, NamespaceOpError> {
    if name == SYSTEM_SCHEMA_NAMESPACE {
        return Ok(NamespaceId::SYSTEM);
    }
    if let Some(existing) = namespace_lookup_by_name_wtxn(wtxn, name)? {
        return Ok(existing);
    }

    let next_id_raw: u32 = {
        let t = wtxn.open_table(NAMESPACES_TABLE)?;
        let mut max: u32 = 0;
        for entry in t.iter()? {
            let (k, _v) = entry?;
            let id = k.value();
            if id > max {
                max = id;
            }
        }
        max.checked_add(1).expect("NamespaceId space exhausted")
    };

    write_namespace_row(wtxn, NamespaceId::from(next_id_raw), name, now_unix_nanos)?;
    Ok(NamespaceId::from(next_id_raw))
}

/// Seed the reserved `brain` system namespace at id `0`. Idempotent —
/// a no-op once the row exists. Called during system-schema bootstrap so
/// every shard has the system namespace present from byte zero.
pub fn seed_system_namespace(
    wtxn: &WriteTransaction,
    now_unix_nanos: u64,
) -> Result<(), NamespaceOpError> {
    if namespace_lookup_by_name_wtxn(wtxn, SYSTEM_SCHEMA_NAMESPACE)?.is_some() {
        return Ok(());
    }
    write_namespace_row(
        wtxn,
        NamespaceId::SYSTEM,
        SYSTEM_SCHEMA_NAMESPACE,
        now_unix_nanos,
    )
}

fn write_namespace_row(
    wtxn: &WriteTransaction,
    id: NamespaceId,
    name: &str,
    now_unix_nanos: u64,
) -> Result<(), NamespaceOpError> {
    let row = NamespaceDefinition::new(id, name.to_string(), now_unix_nanos);
    {
        let mut t = wtxn.open_table(NAMESPACES_TABLE)?;
        t.insert(&row.namespace_id, &row)?;
    }
    {
        let mut by_name = wtxn.open_table(NAMESPACE_BY_NAME_TABLE)?;
        by_name.insert(name, &row.namespace_id)?;
    }
    Ok(())
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn system_namespace_seeds_at_zero_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        seed_system_namespace(&wtxn, 1).unwrap();
        seed_system_namespace(&wtxn, 2).unwrap(); // second call no-ops
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            namespace_lookup_by_name(&rtxn, "brain").unwrap(),
            Some(NamespaceId::SYSTEM)
        );
        assert_eq!(
            namespace_name(&rtxn, NamespaceId::SYSTEM)
                .unwrap()
                .as_deref(),
            Some("brain")
        );
    }

    #[test]
    fn intern_allocates_from_one_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        seed_system_namespace(&wtxn, 1).unwrap();
        let acme = namespace_intern_or_get(&wtxn, "acme", 1).unwrap();
        let globex = namespace_intern_or_get(&wtxn, "globex", 1).unwrap();
        let acme_again = namespace_intern_or_get(&wtxn, "acme", 1).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(acme, NamespaceId::from(1));
        assert_eq!(globex, NamespaceId::from(2));
        assert_eq!(acme_again, acme, "intern is idempotent on name");
        assert!(!acme.is_system());
    }

    #[test]
    fn brain_name_always_resolves_to_system_even_via_intern() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        let id = namespace_intern_or_get(&wtxn, "brain", 1).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(id, NamespaceId::SYSTEM);
    }
}
