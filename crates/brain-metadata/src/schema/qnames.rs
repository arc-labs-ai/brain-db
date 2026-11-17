//! Resolver for the client-facing predicate qname.
//!
//! Sibling of [`crate::entity::types::EntityTypeQnames`] and
//! [`crate::relation::qnames::RelationTypeQnames`]; the three cover the
//! three registries a read path has to name.

use std::collections::HashMap;

use brain_core::PredicateId;
use redb::ReadTransaction;

use crate::tables::predicate::{PredicateDefinition, PREDICATES_TABLE};

/// Resolves predicate ids to the canonical qname a client receives:
/// `"<namespace>:<name>"`, the wire form [`brain_core::Predicate::canonical`]
/// produces.
///
/// # Caching
///
/// One instance per read, reused across rows. A projection loop over a page
/// of statements shares one predicate vocabulary, so the caller pays one
/// registry lookup per DISTINCT predicate instead of one per row.
///
/// Not a [`ReadTransaction`] field: the txn outlives individual calls, and
/// borrowing it for this type's lifetime would force every caller to order
/// its locals around it.
#[derive(Debug, Default)]
pub struct PredicateQnames {
    by_id: HashMap<u32, String>,
}

impl PredicateQnames {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The canonical qname for `id`.
    ///
    /// Infallible: one unnameable predicate must not be able to fail the
    /// whole read. A statement whose predicate row has gone projects as a
    /// degraded row the caller can still see, rather than denying every
    /// healthy row on the same page.
    ///
    /// Returns the empty string when the predicate cannot be named — an
    /// unreadable registry or an id with no row. Empty is the established
    /// "no type known" sentinel on these projections.
    ///
    /// Never the bare name: an unprefixed name cannot be attributed to a
    /// schema, and returning one would be a worse answer than nothing.
    pub fn resolve(&mut self, rtxn: &ReadTransaction, id: PredicateId) -> String {
        if let Some(hit) = self.by_id.get(&id.raw()) {
            return hit.clone();
        }
        let row: Option<PredicateDefinition> = rtxn
            .open_table(PREDICATES_TABLE)
            .ok()
            .and_then(|t| t.get(&id.raw()).ok().flatten())
            .map(|g| g.value());
        let qname = match row {
            Some(row) => row.to_predicate().canonical(),
            None => {
                // A live statement pointing at a predicate the registry has
                // no row for is an inconsistency inside one read txn, not a
                // client error. Say so and degrade to unnamed.
                tracing::warn!(
                    predicate_id = id.raw(),
                    "statement names a predicate with no registry row; projecting unnamed"
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

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn open_db() -> (tempfile::TempDir, redb::Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        (dir, db)
    }

    fn intern(db: &redb::Database, namespace: &str, name: &str) -> PredicateId {
        let wtxn = db.begin_write().unwrap();
        let id = crate::schema::predicate::predicate_intern(
            &wtxn,
            DeclScope::base(namespace),
            name,
            None,
            crate::schema::predicate::ObjectConstraint::ANY,
            1,
            "",
            false,
            NOW,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn qname(db: &redb::Database, id: PredicateId) -> String {
        use redb::ReadableDatabase;
        let rtxn = db.begin_read().unwrap();
        PredicateQnames::new().resolve(&rtxn, id)
    }

    #[test]
    fn a_known_predicate_resolves_to_its_qname() {
        let (_d, db) = open_db();
        let p = intern(&db, "acme", "employs");
        assert_eq!(qname(&db, p), "acme:employs");
    }

    /// The whole point of the resolver: an id the registry lost is a label
    /// problem, not a read failure.
    #[test]
    fn an_unknown_predicate_id_is_unnamed_rather_than_an_error() {
        let (_d, db) = open_db();
        assert_eq!(qname(&db, PredicateId::from(4242)), "");
    }

    /// The cache is an optimisation, so a hit answers identically to a miss.
    #[test]
    fn the_cache_returns_the_same_answer_on_a_hit() {
        use redb::ReadableDatabase;
        let (_d, db) = open_db();
        let p = intern(&db, "acme", "employs");
        let rtxn = db.begin_read().unwrap();
        let mut r = PredicateQnames::new();
        let first = r.resolve(&rtxn, p);
        let second = r.resolve(&rtxn, p);
        assert_eq!(first, "acme:employs");
        assert_eq!(first, second);
    }
}
