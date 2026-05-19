//! `entity_types` table — user-declared entity types.
//!
//! See `spec/18_entities/00_purpose.md` (type system) and
//! `spec/21_schema_dsl/00_purpose.md` (declaration syntax). The
//! attribute schema is stored as an opaque `Vec<u8>` blob in 15.1; the
//! typed AST lands in phase 19 (schema DSL).

use crate::impl_redb_rkyv_value;
use brain_core::EntityTypeId;
use redb::TableDefinition;

pub const ENTITY_TYPES_TABLE: TableDefinition<'static, u32, EntityTypeDefinition> =
    TableDefinition::new("entity_types");

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EntityTypeDefinition {
    pub entity_type_id: u32,
    pub name: String,
    /// rkyv-encoded attribute schema. Phase 19 (schema DSL) defines
    /// the typed shape; for now it's an opaque payload.
    pub schema_blob: Vec<u8>,
    pub created_at_unix_nanos: u64,
}

impl EntityTypeDefinition {
    #[must_use]
    pub fn new(
        id: EntityTypeId,
        name: String,
        schema_blob: Vec<u8>,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            entity_type_id: id.raw(),
            name,
            schema_blob,
            created_at_unix_nanos,
        }
    }

    #[must_use]
    pub fn id(&self) -> EntityTypeId {
        EntityTypeId::from(self.entity_type_id)
    }
}

impl_redb_rkyv_value!(
    EntityTypeDefinition,
    "brain_metadata::EntityTypeDefinition::v1"
);

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let et = EntityTypeDefinition::new(
            EntityTypeId::from(7),
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
