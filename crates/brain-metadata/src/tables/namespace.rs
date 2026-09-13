//! `namespaces` table — the interned tenant (company) registry.
//!
//! A namespace is the company-level data boundary. Every memory,
//! entity, statement, and relation is owned by exactly one namespace;
//! the interned [`brain_core::NamespaceId`] is what gets stamped onto
//! rows and folded into secondary-index keys so one tenant's data is a
//! contiguous, never-cross-scanned keyspace.
//!
//! Two tables, mirroring the predicate registry's qname pattern:
//! - [`NAMESPACES_TABLE`]: `id → NamespaceDefinition` (the record).
//! - [`NAMESPACE_BY_NAME_TABLE`]: `name → id` (the reverse lookup hit
//!   once per connection at AUTH).
//!
//! The reserved system namespace `brain` is seeded at id
//! [`brain_core::NamespaceId::SYSTEM`] (`0`); user namespaces are
//! interned starting at `1`.

use crate::impl_redb_rkyv_value;
use brain_core::NamespaceId;
use redb::TableDefinition;

pub const NAMESPACES_TABLE: TableDefinition<'static, u32, NamespaceDefinition> =
    TableDefinition::new("namespaces");

pub const NAMESPACE_BY_NAME_TABLE: TableDefinition<'static, &'static str, u32> =
    TableDefinition::new("namespace_by_name");

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct NamespaceDefinition {
    pub namespace_id: u32,
    pub name: String,
    pub created_at_unix_nanos: u64,
}

impl NamespaceDefinition {
    #[must_use]
    pub fn new(id: NamespaceId, name: String, created_at_unix_nanos: u64) -> Self {
        Self {
            namespace_id: id.raw(),
            name,
            created_at_unix_nanos,
        }
    }

    #[must_use]
    pub fn id(&self) -> NamespaceId {
        NamespaceId::from(self.namespace_id)
    }
}

impl_redb_rkyv_value!(NamespaceDefinition, "brain_metadata::NamespaceDefinition");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let ns = NamespaceDefinition::new(NamespaceId::from(1), "acme".into(), 1_700_000_000);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(NAMESPACES_TABLE).unwrap();
            t.insert(&ns.namespace_id, &ns).unwrap();
            let mut by_name = wtxn.open_table(NAMESPACE_BY_NAME_TABLE).unwrap();
            by_name.insert(ns.name.as_str(), &ns.namespace_id).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(NAMESPACES_TABLE).unwrap();
        let got = t.get(&ns.namespace_id).unwrap().unwrap().value();
        assert_eq!(got, ns);
        assert_eq!(got.id(), NamespaceId::from(1));
    }
}
