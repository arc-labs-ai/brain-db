//! `predicates` table — interned predicate registry.
//!
//! See `spec/19_statements/00_purpose.md` (predicate vocabulary) and
//! `spec/26_knowledge_storage/00_purpose.md` (table catalog).
//!
//! Phase 15.1 — types only. Schema DSL (phase 19) populates this
//! table at `SCHEMA_UPLOAD` time; statements then reference
//! predicates by their assigned `PredicateId`.

use crate::impl_redb_rkyv_value;
use brain_core::PredicateId;
use redb::TableDefinition;

/// `predicates` table. Key is the `PredicateId.raw()` u32; value is
/// [`PredicateDefinition`].
pub const PREDICATES_TABLE: TableDefinition<'static, u32, PredicateDefinition> =
    TableDefinition::new("predicates");

/// A registered predicate. The `(namespace, name)` pair is logically
/// unique within a deployment; uniqueness is enforced at insert time
/// (a separate `predicates_by_qname` lookup table can be added if
/// needed by later phases — not part of 15.1).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct PredicateDefinition {
    pub predicate_id: u32,
    pub namespace: String,
    pub name: String,
    pub created_at_unix_nanos: u64,
}

impl PredicateDefinition {
    #[must_use]
    pub fn new(id: PredicateId, namespace: String, name: String, created_at_unix_nanos: u64) -> Self {
        Self {
            predicate_id: id.raw(),
            namespace,
            name,
            created_at_unix_nanos,
        }
    }

    #[must_use]
    pub fn id(&self) -> PredicateId {
        PredicateId::from(self.predicate_id)
    }
}

impl_redb_rkyv_value!(PredicateDefinition, "brain_metadata::PredicateDefinition::v1");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let pred = PredicateDefinition::new(
            PredicateId::from(1),
            "acme".into(),
            "reports_to".into(),
            1_700_000_000_000_000_000,
        );

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(PREDICATES_TABLE).unwrap();
            t.insert(&pred.predicate_id, &pred).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(PREDICATES_TABLE).unwrap();
        let got = t.get(&pred.predicate_id).unwrap().unwrap().value();
        assert_eq!(got, pred);
        assert_eq!(got.id(), PredicateId::from(1));
    }
}
