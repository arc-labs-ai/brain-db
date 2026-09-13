//! `relation_types` table — user-declared relation types.
//!
//! `Cardinality` is encoded as `u8` per the discriminant in
//! `brain_core::Cardinality`. The row matches `brain_core::RelationType`:
//! namespace, name, cardinality, symmetric, from/to, schema_version,
//! description, created_at, plus a `relation_types_by_qname` lookup
//! index.

use crate::impl_redb_rkyv_value;
use brain_core::RelationType;
use brain_core::{Cardinality, EntityTypeId, RelationTypeId};
use redb::TableDefinition;

/// `relation_types` table. Key is `RelationTypeId.raw()` (u32);
/// value is [`RelationTypeDefinition`].
pub const RELATION_TYPES_TABLE: TableDefinition<'static, u32, RelationTypeDefinition> =
    TableDefinition::new("relation_types");

/// `relation_types_by_qname` — secondary index for
/// `(namespace, name) → RelationTypeId`. Key is the canonical
/// `"namespace:name"` string; value is the type id.
pub const RELATION_TYPES_BY_QNAME_TABLE: TableDefinition<'static, &str, u32> =
    TableDefinition::new("relation_types_by_qname");

/// A registered relation type. The `(namespace, name)` pair is
/// logically unique within a deployment; uniqueness is enforced by
/// [`RELATION_TYPES_BY_QNAME_TABLE`] writes inside
/// `relation_type_intern`.
///
/// `from_entity_type_id` / `to_entity_type_id`: `0` means "any
/// entity type allowed", else the `EntityTypeId.raw()` of the
/// constrained type. Person seeds at `EntityTypeId(1)`, so `0` is safe
/// as a "no constraint" sentinel.
/// Origin of a registered relation type. Mirrors
/// [`crate::tables::predicate::SchemaOrigin`]: tracks
/// whether the row was authored by `SCHEMA_UPLOAD` (strict mode) or
/// interned on demand from an open-vocabulary RELATION_CREATE.
///
/// Implicit-from-write rows carry `cardinality = ManyToMany` because
/// the writer has no contract on duplicate cardinality — only a
/// schema declaration commits the deployment to OneToOne /
/// OneToMany / ManyToOne semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelationTypeOrigin {
    SchemaDeclared { version: u32 },
    ImplicitFromWrite { first_seen_lsn: u64 },
}

impl RelationTypeOrigin {
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            Self::SchemaDeclared { .. } => 0,
            Self::ImplicitFromWrite { .. } => 1,
        }
    }
    #[must_use]
    pub fn payload(self) -> u64 {
        match self {
            Self::SchemaDeclared { version } => u64::from(version),
            Self::ImplicitFromWrite { first_seen_lsn } => first_seen_lsn,
        }
    }
    #[must_use]
    pub fn decode(tag: u8, payload: u64) -> Self {
        match tag {
            1 => Self::ImplicitFromWrite {
                first_seen_lsn: payload,
            },
            _ => Self::SchemaDeclared {
                #[allow(clippy::cast_possible_truncation)]
                version: payload as u32,
            },
        }
    }
    #[must_use]
    pub fn is_schema_declared(self) -> bool {
        matches!(self, Self::SchemaDeclared { .. })
    }
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct RelationTypeDefinition {
    pub relation_type_id: u32,
    pub namespace: String,
    pub name: String,
    pub cardinality: u8,
    pub is_symmetric: u8,
    pub from_entity_type_id: u32,
    pub to_entity_type_id: u32,
    pub schema_version: u32,
    pub description: String,
    pub created_at_unix_nanos: u64,
    pub origin_tag: u8,
    pub origin_payload: u64,
}

impl RelationTypeDefinition {
    #[must_use]
    pub fn id(&self) -> RelationTypeId {
        RelationTypeId::from(self.relation_type_id)
    }

    #[must_use]
    pub fn cardinality(&self) -> Option<Cardinality> {
        Cardinality::from_u8(self.cardinality)
    }

    #[must_use]
    pub fn is_symmetric(&self) -> bool {
        self.is_symmetric != 0
    }

    /// Build a redb row from the brain-core value type. Origin defaults
    /// to `SchemaDeclared` at the relation type's `schema_version` —
    /// `relation_type_intern_or_get` overrides with `ImplicitFromWrite`
    /// for open-vocabulary writes.
    #[must_use]
    pub fn from_relation_type(r: &RelationType, created_at_unix_nanos: u64) -> Self {
        let origin = RelationTypeOrigin::SchemaDeclared {
            version: r.schema_version,
        };
        Self::from_relation_type_with_origin(r, created_at_unix_nanos, origin)
    }

    #[must_use]
    pub fn from_relation_type_with_origin(
        r: &RelationType,
        created_at_unix_nanos: u64,
        origin: RelationTypeOrigin,
    ) -> Self {
        Self {
            relation_type_id: r.id.raw(),
            namespace: r.namespace.clone(),
            name: r.name.clone(),
            cardinality: r.cardinality.as_u8(),
            is_symmetric: u8::from(r.is_symmetric),
            from_entity_type_id: encode_entity_type_id(r.from_type),
            to_entity_type_id: encode_entity_type_id(r.to_type),
            schema_version: r.schema_version,
            description: r.description.clone(),
            created_at_unix_nanos,
            origin_tag: origin.tag(),
            origin_payload: origin.payload(),
        }
    }

    #[must_use]
    pub fn origin(&self) -> RelationTypeOrigin {
        RelationTypeOrigin::decode(self.origin_tag, self.origin_payload)
    }

    /// Project to the brain-core value type. `created_at_unix_nanos`
    /// is intentionally dropped — it lives only in the persisted row.
    #[must_use]
    pub fn to_relation_type(&self) -> RelationType {
        RelationType {
            id: self.id(),
            namespace: self.namespace.clone(),
            name: self.name.clone(),
            cardinality: Cardinality::from_u8(self.cardinality).unwrap_or(Cardinality::ManyToMany),
            is_symmetric: self.is_symmetric != 0,
            from_type: decode_entity_type_id(self.from_entity_type_id),
            to_type: decode_entity_type_id(self.to_entity_type_id),
            schema_version: self.schema_version,
            description: self.description.clone(),
        }
    }
}

/// `0 → None / else → Some(EntityTypeId)`.
#[must_use]
pub fn decode_entity_type_id(raw: u32) -> Option<EntityTypeId> {
    if raw == 0 {
        None
    } else {
        Some(EntityTypeId::from(raw))
    }
}

/// Inverse of [`decode_entity_type_id`]. `EntityTypeId(0)` is a
/// reserved sentinel — Person is `EntityTypeId(1)`.
#[must_use]
pub fn encode_entity_type_id(t: Option<EntityTypeId>) -> u32 {
    t.map(|e| e.raw()).unwrap_or(0)
}

impl_redb_rkyv_value!(
    RelationTypeDefinition,
    "brain_metadata::RelationTypeDefinition"
);

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let rt = RelationType {
            id: RelationTypeId::from(3),
            namespace: "acme".into(),
            name: "reports_to".into(),
            cardinality: Cardinality::ManyToOne,
            is_symmetric: false,
            from_type: Some(EntityTypeId(1)),
            to_type: Some(EntityTypeId(1)),
            schema_version: 1,
            description: "Reports-to chain".into(),
        };
        let row = RelationTypeDefinition::from_relation_type(&rt, 1_700_000_000_000_000_000);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(RELATION_TYPES_TABLE).unwrap();
            t.insert(&row.relation_type_id, &row).unwrap();
            let mut q = wtxn.open_table(RELATION_TYPES_BY_QNAME_TABLE).unwrap();
            q.insert("acme:reports_to", &row.relation_type_id).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(RELATION_TYPES_TABLE).unwrap();
        let got = t.get(&row.relation_type_id).unwrap().unwrap().value();
        assert_eq!(got, row);
        assert_eq!(got.to_relation_type(), rt);
        assert_eq!(got.cardinality(), Some(Cardinality::ManyToOne));
        assert!(!got.is_symmetric());

        let q = rtxn.open_table(RELATION_TYPES_BY_QNAME_TABLE).unwrap();
        let by_qname = q.get("acme:reports_to").unwrap().unwrap().value();
        assert_eq!(by_qname, row.relation_type_id);
    }

    #[test]
    fn entity_type_id_sentinel_round_trip() {
        assert_eq!(decode_entity_type_id(0), None);
        assert_eq!(decode_entity_type_id(1), Some(EntityTypeId(1)));
        assert_eq!(decode_entity_type_id(42), Some(EntityTypeId(42)));

        assert_eq!(encode_entity_type_id(None), 0);
        assert_eq!(encode_entity_type_id(Some(EntityTypeId(1))), 1);
        assert_eq!(encode_entity_type_id(Some(EntityTypeId(42))), 42);
    }
}
