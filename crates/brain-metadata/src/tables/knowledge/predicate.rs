//! `predicates` table — interned predicate registry.
//!
//! See `spec/19_statements/00_purpose.md` (predicate vocabulary) and
//! `spec/26_knowledge_storage/00_purpose.md` (table catalog).
//!
//! Phase 15.1 declared the table with a minimal row. Phase 17.3 widens
//! the row to match `spec/19_statements/00_purpose.md` §"Predicate
//! vocabulary" — adds `kind_constraint`, `object_type_constraint_byte`,
//! `schema_version`, and `description`, and adds a `predicates_by_qname`
//! lookup index. Schema DSL (phase 19) populates user predicates at
//! `SCHEMA_UPLOAD` time; phase 17.3 owns the built-ins.

use crate::impl_redb_rkyv_value;
use brain_core::knowledge::{Predicate, StatementKind};
use brain_core::PredicateId;
use redb::TableDefinition;

/// `predicates` table. Key is `PredicateId.raw()` (u32); value is
/// [`PredicateDefinition`].
pub const PREDICATES_TABLE: TableDefinition<'static, u32, PredicateDefinition> =
    TableDefinition::new("predicates");

/// `predicates_by_qname` — secondary index for `(namespace, name) →
/// PredicateId`. Phase 17.3. Key is the canonical `"namespace:name"`
/// string; value is the predicate id.
pub const PREDICATES_BY_QNAME_TABLE: TableDefinition<'static, &str, u32> =
    TableDefinition::new("predicates_by_qname");

/// A registered predicate. The `(namespace, name)` pair is logically
/// unique within a deployment; uniqueness is enforced by
/// [`PREDICATES_BY_QNAME_TABLE`] writes inside `predicate_intern`.
///
/// `kind_constraint`: `0` means "any kind allowed", else `1=Fact /
/// 2=Preference / 3=Event` (matching [`StatementKind::as_u8`] offset by
/// 1). `object_type_constraint_byte`: `0` means "any object type", else
/// `1=Entity / 2=Value / 3=Memory / 4=Statement` (matches
/// `StatementObject::discriminant()` offset by 1).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct PredicateDefinition {
    pub predicate_id: u32,
    pub namespace: String,
    pub name: String,
    pub kind_constraint: u8,
    pub object_type_constraint_byte: u8,
    pub schema_version: u32,
    pub description: String,
    pub created_at_unix_nanos: u64,
}

impl PredicateDefinition {
    #[must_use]
    pub fn id(&self) -> PredicateId {
        PredicateId::from(self.predicate_id)
    }

    /// Build a redb row from the brain-core value type.
    #[must_use]
    pub fn from_predicate(p: &Predicate, created_at_unix_nanos: u64) -> Self {
        Self {
            predicate_id: p.id.raw(),
            namespace: p.namespace.clone(),
            name: p.name.clone(),
            kind_constraint: encode_kind_constraint(p.kind_constraint),
            object_type_constraint_byte: p.object_type_constraint_byte,
            schema_version: p.schema_version,
            description: p.description.clone(),
            created_at_unix_nanos,
        }
    }

    /// Project to the brain-core value type. `created_at_unix_nanos`
    /// is intentionally dropped — it lives only in the persisted row.
    #[must_use]
    pub fn to_predicate(&self) -> Predicate {
        Predicate {
            id: self.id(),
            namespace: self.namespace.clone(),
            name: self.name.clone(),
            kind_constraint: decode_kind_constraint(self.kind_constraint),
            object_type_constraint_byte: self.object_type_constraint_byte,
            schema_version: self.schema_version,
            description: self.description.clone(),
        }
    }
}

/// `0 → None` / `1 → Fact / 2 → Preference / 3 → Event`. Unknown
/// bytes collapse to `None` (forwards-compatible).
#[must_use]
pub fn decode_kind_constraint(b: u8) -> Option<StatementKind> {
    match b {
        1 => Some(StatementKind::Fact),
        2 => Some(StatementKind::Preference),
        3 => Some(StatementKind::Event),
        _ => None,
    }
}

/// Inverse of [`decode_kind_constraint`].
#[must_use]
pub fn encode_kind_constraint(k: Option<StatementKind>) -> u8 {
    match k {
        Some(StatementKind::Fact) => 1,
        Some(StatementKind::Preference) => 2,
        Some(StatementKind::Event) => 3,
        None => 0,
    }
}

impl_redb_rkyv_value!(PredicateDefinition, "brain_metadata::PredicateDefinition::v2");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let pred = Predicate {
            id: PredicateId::from(7),
            namespace: "acme".into(),
            name: "reports_to".into(),
            kind_constraint: Some(StatementKind::Fact),
            object_type_constraint_byte: 1,
            schema_version: 3,
            description: "Reports-to relation".into(),
        };
        let row = PredicateDefinition::from_predicate(&pred, 1_700_000_000_000_000_000);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(PREDICATES_TABLE).unwrap();
            t.insert(&row.predicate_id, &row).unwrap();
            let mut q = wtxn.open_table(PREDICATES_BY_QNAME_TABLE).unwrap();
            q.insert("acme:reports_to", &row.predicate_id).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(PREDICATES_TABLE).unwrap();
        let got = t.get(&row.predicate_id).unwrap().unwrap().value();
        assert_eq!(got, row);
        assert_eq!(got.to_predicate(), pred);

        let q = rtxn.open_table(PREDICATES_BY_QNAME_TABLE).unwrap();
        let by_qname = q.get("acme:reports_to").unwrap().unwrap().value();
        assert_eq!(by_qname, row.predicate_id);
    }

    #[test]
    fn kind_constraint_round_trip() {
        assert_eq!(decode_kind_constraint(0), None);
        assert_eq!(decode_kind_constraint(1), Some(StatementKind::Fact));
        assert_eq!(decode_kind_constraint(2), Some(StatementKind::Preference));
        assert_eq!(decode_kind_constraint(3), Some(StatementKind::Event));
        assert_eq!(decode_kind_constraint(99), None);

        assert_eq!(encode_kind_constraint(None), 0);
        assert_eq!(encode_kind_constraint(Some(StatementKind::Fact)), 1);
        assert_eq!(encode_kind_constraint(Some(StatementKind::Preference)), 2);
        assert_eq!(encode_kind_constraint(Some(StatementKind::Event)), 3);
    }
}
