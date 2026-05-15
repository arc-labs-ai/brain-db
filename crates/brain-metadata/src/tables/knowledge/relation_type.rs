//! `relation_types` table — user-declared relation types.
//!
//! See `spec/20_relations/00_purpose.md` (cardinality, symmetry) and
//! `spec/21_schema_dsl/00_purpose.md` (declaration). `Cardinality` is
//! encoded as `u8` per the discriminant in `brain_core::Cardinality`.

use crate::impl_redb_rkyv_value;
use brain_core::{Cardinality, RelationTypeId};
use redb::TableDefinition;

pub const RELATION_TYPES_TABLE: TableDefinition<'static, u32, RelationTypeDefinition> =
    TableDefinition::new("relation_types");

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct RelationTypeDefinition {
    pub relation_type_id: u32,
    pub name: String,
    pub cardinality: u8,
    pub is_symmetric: u8,
    pub from_entity_type_id: u32,
    pub to_entity_type_id: u32,
    pub created_at_unix_nanos: u64,
}

impl RelationTypeDefinition {
    #[must_use]
    pub fn new(
        id: RelationTypeId,
        name: String,
        cardinality: Cardinality,
        is_symmetric: bool,
        from_entity_type_id: u32,
        to_entity_type_id: u32,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            relation_type_id: id.raw(),
            name,
            cardinality: cardinality.as_u8(),
            is_symmetric: u8::from(is_symmetric),
            from_entity_type_id,
            to_entity_type_id,
            created_at_unix_nanos,
        }
    }

    #[must_use]
    pub fn id(&self) -> RelationTypeId {
        RelationTypeId::from(self.relation_type_id)
    }

    pub fn cardinality(&self) -> Option<Cardinality> {
        Cardinality::from_u8(self.cardinality)
    }

    #[must_use]
    pub fn is_symmetric(&self) -> bool {
        self.is_symmetric != 0
    }
}

impl_redb_rkyv_value!(RelationTypeDefinition, "brain_metadata::RelationTypeDefinition::v1");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let rt = RelationTypeDefinition::new(
            RelationTypeId::from(3),
            "reports_to".into(),
            Cardinality::ManyToOne,
            false,
            1,
            1,
            1_700_000_000_000_000_000,
        );

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(RELATION_TYPES_TABLE).unwrap();
            t.insert(&rt.relation_type_id, &rt).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(RELATION_TYPES_TABLE).unwrap();
        let got = t.get(&rt.relation_type_id).unwrap().unwrap().value();
        assert_eq!(got, rt);
        assert_eq!(got.cardinality(), Some(Cardinality::ManyToOne));
        assert!(!got.is_symmetric());
    }
}
