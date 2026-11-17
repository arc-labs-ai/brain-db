//! Resolver for the client-facing relation-type qname.
//!
//! Sibling of [`crate::entity::types::EntityTypeQnames`] and
//! [`crate::schema::qnames::PredicateQnames`]; the three cover the three
//! registries a read path has to name.

use std::collections::HashMap;

use brain_core::RelationTypeId;
use redb::ReadTransaction;

use crate::tables::relation_type::{RelationTypeDefinition, RELATION_TYPES_TABLE};

/// Resolves relation-type ids to the canonical qname a client receives:
/// `"<namespace>:<name>"`, the wire form
/// [`brain_core::RelationType::canonical`] produces.
///
/// # Caching
///
/// One instance per read, reused across rows. An edge page or a traversal
/// walks many rows over few distinct types, so the caller pays one registry
/// lookup per DISTINCT type instead of one per row.
///
/// Not a [`ReadTransaction`] field: the txn outlives individual calls, and
/// borrowing it for this type's lifetime would force every caller to order
/// its locals around it.
#[derive(Debug, Default)]
pub struct RelationTypeQnames {
    by_id: HashMap<u32, String>,
}

impl RelationTypeQnames {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The canonical qname for `id`.
    ///
    /// Infallible: one unnameable relation type must not be able to fail the
    /// whole read. A relation whose type row has gone projects as a degraded
    /// row the caller can still see, rather than denying every healthy edge
    /// on the same page.
    ///
    /// Returns the empty string when the type cannot be named — an
    /// unreadable registry or an id with no row. A relation created through
    /// the wire API always carries a non-empty `"<namespace>:<name>"`, so an
    /// empty qname on this surface is unambiguous: the vocabulary behind the
    /// edge is gone, not absent by design.
    ///
    /// Never the bare name: an unprefixed name cannot be attributed to a
    /// schema, and returning one would be a worse answer than nothing.
    pub fn resolve(&mut self, rtxn: &ReadTransaction, id: RelationTypeId) -> String {
        if let Some(hit) = self.by_id.get(&id.raw()) {
            return hit.clone();
        }
        let row: Option<RelationTypeDefinition> = rtxn
            .open_table(RELATION_TYPES_TABLE)
            .ok()
            .and_then(|t| t.get(&id.raw()).ok().flatten())
            .map(|g| g.value());
        let qname = match row {
            Some(row) => row.to_relation_type().canonical(),
            None => {
                // A live edge pointing at a relation type the registry has no
                // row for is an inconsistency inside one read txn, not a
                // client error. Say so and degrade to unnamed.
                tracing::warn!(
                    relation_type_id = id.raw(),
                    "relation names a relation type with no registry row; projecting unnamed"
                );
                String::new()
            }
        };
        self.by_id.insert(id.raw(), qname.clone());
        qname
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use crate::tables::scope::DeclScope;
    use brain_core::Cardinality;

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn open_db() -> (tempfile::TempDir, redb::Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        (dir, db)
    }

    fn intern(db: &redb::Database, namespace: &str, name: &str) -> RelationTypeId {
        let wtxn = db.begin_write().unwrap();
        let id = crate::relation::types::relation_type_intern(
            &wtxn,
            DeclScope::base(namespace),
            name,
            None,
            None,
            Cardinality::ManyToMany,
            false,
            1,
            "",
            NOW,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn qname(db: &redb::Database, id: RelationTypeId) -> String {
        use redb::ReadableDatabase;
        let rtxn = db.begin_read().unwrap();
        RelationTypeQnames::new().resolve(&rtxn, id)
    }

    #[test]
    fn a_known_relation_type_resolves_to_its_qname() {
        let (_d, db) = open_db();
        let rt = intern(&db, "acme", "works_at");
        assert_eq!(qname(&db, rt), "acme:works_at");
    }

    /// The whole point of the resolver: an id the registry lost is a label
    /// problem, not a read failure.
    #[test]
    fn an_unknown_relation_type_id_is_unnamed_rather_than_an_error() {
        let (_d, db) = open_db();
        assert_eq!(qname(&db, RelationTypeId::from(4242)), "");
    }

    /// The cache is an optimisation, so a hit answers identically to a miss.
    #[test]
    fn the_cache_returns_the_same_answer_on_a_hit() {
        use redb::ReadableDatabase;
        let (_d, db) = open_db();
        let rt = intern(&db, "acme", "works_at");
        let rtxn = db.begin_read().unwrap();
        let mut r = RelationTypeQnames::new();
        let first = r.resolve(&rtxn, rt);
        let second = r.resolve(&rtxn, rt);
        assert_eq!(first, "acme:works_at");
        assert_eq!(first, second);
    }
}
