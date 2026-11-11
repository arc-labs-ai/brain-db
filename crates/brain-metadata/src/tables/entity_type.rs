//! `entity_types` table — user-declared entity types.
//!
//! The attribute schema is stored as an opaque `Vec<u8>` blob; the
//! typed AST is defined by the schema DSL.

use crate::impl_redb_rkyv_value;
use brain_core::EntityTypeId;
use redb::TableDefinition;

pub const ENTITY_TYPES_TABLE: TableDefinition<'static, u32, EntityTypeDefinition> =
    TableDefinition::new("entity_types");

/// `entity_types_by_scope` — secondary index for
/// `(namespace_id, space, name) → EntityTypeId`.
///
/// The registry used to be keyed on bare `name` alone, globally. Two
/// tenants declaring the same type name therefore collided: identical
/// definitions silently shared one row, and differing ones made the
/// SECOND tenant's `SCHEMA_UPLOAD` fail with `AlreadyExists` over a name
/// they could not see and did not choose. `namespace_id` is the same
/// tenant key every other row carries (`RowScope.namespace_id`), so an
/// entity type is scoped at least as tightly as the entities that use it.
///
/// `space` EXTENDS that one level further, for the same reason a second
/// time. A namespace is the tenant; a space is one application inside it,
/// and two applications in one company do not necessarily share a domain
/// model — a sales copilot's `Stage` has nothing to do with a support
/// bot's, and unioning them into one tenant registry pollutes both
/// extractors with the other's vocabulary.
///
/// [`SPACE_BASE`] (all-zero) is the NAMESPACE-WIDE row: the tenant's own
/// base schema, which every space in it sees. A non-zero space is an
/// OVERLAY that only that space sees, and which shadows the base for the
/// same name. So the resolution chain is
/// `(ns, space) → (ns, base) → (SYSTEM, base)`, which is the existing
/// two-step with one level added above it — see
/// [`crate::entity::types::entity_type_lookup`].
///
/// A tenant that never uploads a space overlay is keyed exactly as before
/// (every row at `SPACE_BASE`), so the common shape pays nothing for this.
pub const ENTITY_TYPES_BY_SCOPE_TABLE: TableDefinition<
    'static,
    (u32, [u8; 16], &'static str),
    u32,
> = TableDefinition::new("entity_types_by_scope");

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
pub struct EntityTypeDefinition {
    pub entity_type_id: u32,
    pub name: String,
    /// Owning tenant. `NamespaceId::SYSTEM` for the built-ins (Person,
    /// Organization, …), which stay shared across every namespace.
    /// Appended last so the field order stays append-only.
    pub namespace_id: u32,
    /// rkyv-encoded attribute schema. The schema DSL defines the typed
    /// shape; for now it's an opaque payload.
    pub schema_blob: Vec<u8>,
    pub created_at_unix_nanos: u64,
}

impl EntityTypeDefinition {
    #[must_use]
    pub fn new(
        id: EntityTypeId,
        namespace_id: u32,
        name: String,
        schema_blob: Vec<u8>,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            entity_type_id: id.raw(),
            name,
            namespace_id,
            schema_blob,
            created_at_unix_nanos,
        }
    }

    #[must_use]
    pub fn id(&self) -> EntityTypeId {
        EntityTypeId::from(self.entity_type_id)
    }
}

impl_redb_rkyv_value!(EntityTypeDefinition, "brain_metadata::EntityTypeDefinition");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let et = EntityTypeDefinition::new(
            EntityTypeId::from(7),
            brain_core::NamespaceId::SYSTEM.raw(),
            "Person".into(),
            vec![1, 2, 3, 4],
            1_700_000_000_000_000_000,
        );

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
            t.insert(&et.entity_type_id, &et).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
        let got = t.get(&et.entity_type_id).unwrap().unwrap().value();
        assert_eq!(got, et);
        assert_eq!(got.id(), EntityTypeId::from(7));
    }
}
