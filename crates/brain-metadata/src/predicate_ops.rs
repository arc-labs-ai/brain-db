//! Typed CRUD + interning over the predicate registry. Sub-task 17.3.
//!
//! Free functions over `redb::{ReadTransaction, WriteTransaction}`
//! mirroring the [`crate::entity_ops`] precedent: callers compose them
//! inside their own redb txns so a phase-17.4 `statement_create` can
//! validate-and-write atomically.
//!
//! Spec refs:
//! - `spec/19_statements/00_purpose.md` §"Predicate vocabulary" —
//!   field shape + built-in catalog.
//! - `spec/26_knowledge_storage/00_purpose.md` — predicate row lives
//!   in the knowledge storage catalog.

use brain_core::knowledge::{Predicate, StatementKind};
use brain_core::PredicateId;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::knowledge::predicate::{
    PredicateDefinition, PREDICATES_BY_QNAME_TABLE, PREDICATES_TABLE,
};

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum PredicateOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("invalid predicate identifier: {reason}")]
    InvalidIdentifier { reason: &'static str },

    #[error(
        "predicate {qname:?} already exists with id {existing_id:?} but constraints differ"
    )]
    AlreadyExists {
        qname: String,
        existing_id: PredicateId,
    },
}

// ---------------------------------------------------------------------------
// Identifier validation.
// ---------------------------------------------------------------------------

/// Max length for `namespace`.
pub const NAMESPACE_MAX_LEN: usize = 32;
/// Max length for `name`.
pub const NAME_MAX_LEN: usize = 64;

/// Validate a namespace segment of a predicate qname. Conservative
/// grammar: `[a-z][a-z0-9_]*`, ASCII only, no leading digit, no `:`
/// (qname separator). Empty is rejected.
fn validate_namespace(s: &str) -> Result<(), PredicateOpError> {
    validate_identifier(s, NAMESPACE_MAX_LEN, "namespace")
}

/// Validate a name segment of a predicate qname. Same grammar as
/// namespace; different length bound.
fn validate_name(s: &str) -> Result<(), PredicateOpError> {
    validate_identifier(s, NAME_MAX_LEN, "name")
}

fn validate_identifier(s: &str, max: usize, label: &'static str) -> Result<(), PredicateOpError> {
    if s.is_empty() {
        return Err(PredicateOpError::InvalidIdentifier {
            reason: match label {
                "namespace" => "namespace must not be empty",
                _ => "name must not be empty",
            },
        });
    }
    if s.len() > max {
        return Err(PredicateOpError::InvalidIdentifier {
            reason: match label {
                "namespace" => "namespace exceeds 32 chars",
                _ => "name exceeds 64 chars",
            },
        });
    }
    let mut chars = s.chars();
    let first = chars.next().expect("non-empty checked above");
    if !first.is_ascii_lowercase() {
        return Err(PredicateOpError::InvalidIdentifier {
            reason: match label {
                "namespace" => "namespace must start with [a-z]",
                _ => "name must start with [a-z]",
            },
        });
    }
    for c in std::iter::once(first).chain(chars) {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_';
        if !ok {
            return Err(PredicateOpError::InvalidIdentifier {
                reason: match label {
                    "namespace" => "namespace must match [a-z][a-z0-9_]*",
                    _ => "name must match [a-z][a-z0-9_]*",
                },
            });
        }
    }
    Ok(())
}

/// Canonical qname: `"namespace:name"`.
#[must_use]
fn qname(namespace: &str, name: &str) -> String {
    format!("{namespace}:{name}")
}

// ---------------------------------------------------------------------------
// Read paths.
// ---------------------------------------------------------------------------

/// Fetch a predicate by id. Returns `None` if the row doesn't exist.
pub fn predicate_get(
    rtxn: &ReadTransaction,
    id: PredicateId,
) -> Result<Option<Predicate>, PredicateOpError> {
    let t = rtxn.open_table(PREDICATES_TABLE)?;
    let row: Option<PredicateDefinition> = t.get(&id.raw())?.map(|g| g.value());
    Ok(row.as_ref().map(PredicateDefinition::to_predicate))
}

/// Look up a predicate by its namespaced qname. Identifier validation
/// is enforced — invalid namespace/name produces
/// [`PredicateOpError::InvalidIdentifier`] instead of `Ok(None)`.
pub fn predicate_lookup_by_qname(
    rtxn: &ReadTransaction,
    namespace: &str,
    name: &str,
) -> Result<Option<Predicate>, PredicateOpError> {
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);
    let idx = rtxn.open_table(PREDICATES_BY_QNAME_TABLE)?;
    let id_raw: Option<u32> = idx.get(q.as_str())?.map(|g| g.value());
    let Some(id_raw) = id_raw else {
        return Ok(None);
    };

    let t = rtxn.open_table(PREDICATES_TABLE)?;
    let row: Option<PredicateDefinition> = t.get(&id_raw)?.map(|g| g.value());
    Ok(row.as_ref().map(PredicateDefinition::to_predicate))
}

/// List predicates. With `namespace_filter = None`, returns every
/// registered predicate; with `Some(ns)`, only that namespace's
/// predicates. O(N) over the primary table; predicate count is small.
pub fn predicate_list(
    rtxn: &ReadTransaction,
    namespace_filter: Option<&str>,
) -> Result<Vec<Predicate>, PredicateOpError> {
    if let Some(ns) = namespace_filter {
        validate_namespace(ns)?;
    }
    let t = rtxn.open_table(PREDICATES_TABLE)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        let row = v.value();
        if let Some(ns) = namespace_filter {
            if row.namespace != ns {
                continue;
            }
        }
        out.push(row.to_predicate());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Write path.
// ---------------------------------------------------------------------------

/// Intern (or look up) a predicate by its qname.
///
/// Semantics:
/// - If no row exists for `(namespace, name)`: allocate a fresh
///   `PredicateId` (scan-max-and-increment, starting at 1), write the
///   primary row + qname index, return the new id.
/// - If a row exists and **all** constraint fields match the requested
///   values: return the existing id (idempotent re-registration).
/// - If a row exists with **different** constraints: error
///   [`PredicateOpError::AlreadyExists`] — caller must not silently
///   override schema decisions.
///
/// Validation is enforced before any storage access.
#[allow(clippy::too_many_arguments)]
pub fn predicate_intern(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
    kind_constraint: Option<StatementKind>,
    object_type_constraint_byte: u8,
    schema_version: u32,
    description: &str,
    now_unix_nanos: u64,
) -> Result<PredicateId, PredicateOpError> {
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);

    // Idempotency probe.
    {
        let idx = wtxn.open_table(PREDICATES_BY_QNAME_TABLE)?;
        let existing_id: Option<u32> = idx.get(q.as_str())?.map(|g| g.value());
        if let Some(id_raw) = existing_id {
            let t = wtxn.open_table(PREDICATES_TABLE)?;
            let row: Option<PredicateDefinition> = t.get(&id_raw)?.map(|g| g.value());
            let row = row.expect("qname index points to a missing row — file is corrupt");

            let same = row.kind_constraint
                == crate::tables::knowledge::predicate::encode_kind_constraint(kind_constraint)
                && row.object_type_constraint_byte == object_type_constraint_byte
                && row.schema_version == schema_version
                && row.description == description;

            if same {
                return Ok(PredicateId::from(id_raw));
            }
            return Err(PredicateOpError::AlreadyExists {
                qname: q,
                existing_id: PredicateId::from(id_raw),
            });
        }
    }

    // Fresh registration. Allocate id = max(existing) + 1.
    let next_id_raw: u32 = {
        let t = wtxn.open_table(PREDICATES_TABLE)?;
        let mut max: u32 = 0;
        for entry in t.iter()? {
            let (k, _) = entry?;
            let id = k.value();
            if id > max {
                max = id;
            }
        }
        max.checked_add(1).expect("PredicateId space exhausted")
    };

    let pred = Predicate {
        id: PredicateId::from(next_id_raw),
        namespace: namespace.to_string(),
        name: name.to_string(),
        kind_constraint,
        object_type_constraint_byte,
        schema_version,
        description: description.to_string(),
    };
    let row = PredicateDefinition::from_predicate(&pred, now_unix_nanos);

    {
        let mut t = wtxn.open_table(PREDICATES_TABLE)?;
        t.insert(&row.predicate_id, &row)?;
    }
    {
        let mut idx = wtxn.open_table(PREDICATES_BY_QNAME_TABLE)?;
        idx.insert(q.as_str(), &row.predicate_id)?;
    }

    Ok(PredicateId::from(next_id_raw))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use redb::ReadableDatabase;

    fn open_db() -> (tempfile::TempDir, redb::Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        (dir, db)
    }

    #[test]
    fn intern_fresh_allocates_id_one() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id = predicate_intern(
            &wtxn,
            "brain",
            "is_a",
            Some(StatementKind::Fact),
            1,
            1,
            "entity type assertion",
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(id, PredicateId::from(1));

        // Round-trip via get + lookup_by_qname.
        let rtxn = db.begin_read().unwrap();
        let by_id = predicate_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(by_id.canonical(), "brain:is_a");
        assert_eq!(by_id.kind_constraint, Some(StatementKind::Fact));

        let by_qname = predicate_lookup_by_qname(&rtxn, "brain", "is_a")
            .unwrap()
            .unwrap();
        assert_eq!(by_qname.id, id);
    }

    #[test]
    fn intern_idempotent_returns_same_id() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id1 =
            predicate_intern(&wtxn, "brain", "is_a", Some(StatementKind::Fact), 1, 1, "x", 0)
                .unwrap();
        let id2 =
            predicate_intern(&wtxn, "brain", "is_a", Some(StatementKind::Fact), 1, 1, "x", 999)
                .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(id1, id2);

        let rtxn = db.begin_read().unwrap();
        let all = predicate_list(&rtxn, None).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn intern_conflict_on_constraint_mismatch() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let _ = predicate_intern(
            &wtxn,
            "brain",
            "is_a",
            Some(StatementKind::Fact),
            1,
            1,
            "x",
            0,
        )
        .unwrap();
        // Same qname, different kind_constraint → conflict.
        let err = predicate_intern(
            &wtxn,
            "brain",
            "is_a",
            Some(StatementKind::Preference),
            1,
            1,
            "x",
            0,
        )
        .unwrap_err();
        match err {
            PredicateOpError::AlreadyExists { qname, .. } => {
                assert_eq!(qname, "brain:is_a");
            }
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn intern_two_distinct_qnames_get_distinct_ids() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let a = predicate_intern(&wtxn, "brain", "is_a", Some(StatementKind::Fact), 0, 1, "", 0)
            .unwrap();
        let b = predicate_intern(&wtxn, "brain", "mentions", None, 0, 1, "", 0).unwrap();
        wtxn.commit().unwrap();
        assert_ne!(a, b);
        assert_eq!(a, PredicateId::from(1));
        assert_eq!(b, PredicateId::from(2));
    }

    #[test]
    fn lookup_missing_returns_none() {
        let (_dir, db) = open_db();
        let rtxn = db.begin_read().unwrap();
        // Table must exist; touch it in a write txn first.
        drop(rtxn);
        let wtxn = db.begin_write().unwrap();
        let _ = wtxn.open_table(PREDICATES_TABLE).unwrap();
        let _ = wtxn.open_table(PREDICATES_BY_QNAME_TABLE).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = predicate_lookup_by_qname(&rtxn, "brain", "nothing").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn list_by_namespace_filters() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let _ = predicate_intern(&wtxn, "brain", "is_a", None, 0, 1, "", 0).unwrap();
        let _ = predicate_intern(&wtxn, "brain", "mentions", None, 0, 1, "", 0).unwrap();
        let _ = predicate_intern(&wtxn, "acme", "manages", None, 1, 1, "", 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let brain_only = predicate_list(&rtxn, Some("brain")).unwrap();
        assert_eq!(brain_only.len(), 2);
        let acme_only = predicate_list(&rtxn, Some("acme")).unwrap();
        assert_eq!(acme_only.len(), 1);
        let all = predicate_list(&rtxn, None).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn invalid_namespace_empty() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let err = predicate_intern(&wtxn, "", "name", None, 0, 1, "", 0).unwrap_err();
        matches!(err, PredicateOpError::InvalidIdentifier { .. })
            .then_some(())
            .expect("expected InvalidIdentifier");
    }

    #[test]
    fn invalid_namespace_uppercase() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let err = predicate_intern(&wtxn, "Brain", "name", None, 0, 1, "", 0).unwrap_err();
        matches!(err, PredicateOpError::InvalidIdentifier { .. })
            .then_some(())
            .expect("expected InvalidIdentifier");
    }

    #[test]
    fn invalid_namespace_leading_digit() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let err = predicate_intern(&wtxn, "1brain", "name", None, 0, 1, "", 0).unwrap_err();
        matches!(err, PredicateOpError::InvalidIdentifier { .. })
            .then_some(())
            .expect("expected InvalidIdentifier");
    }

    #[test]
    fn invalid_namespace_with_colon() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let err = predicate_intern(&wtxn, "br:ain", "name", None, 0, 1, "", 0).unwrap_err();
        matches!(err, PredicateOpError::InvalidIdentifier { .. })
            .then_some(())
            .expect("expected InvalidIdentifier");
    }

    #[test]
    fn invalid_name_empty() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let err = predicate_intern(&wtxn, "brain", "", None, 0, 1, "", 0).unwrap_err();
        matches!(err, PredicateOpError::InvalidIdentifier { .. })
            .then_some(())
            .expect("expected InvalidIdentifier");
    }

    #[test]
    fn invalid_name_too_long() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let long = "a".repeat(NAME_MAX_LEN + 1);
        let err = predicate_intern(&wtxn, "brain", &long, None, 0, 1, "", 0).unwrap_err();
        matches!(err, PredicateOpError::InvalidIdentifier { .. })
            .then_some(())
            .expect("expected InvalidIdentifier");
    }

    #[test]
    fn invalid_name_with_hyphen() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let err = predicate_intern(&wtxn, "brain", "is-a", None, 0, 1, "", 0).unwrap_err();
        matches!(err, PredicateOpError::InvalidIdentifier { .. })
            .then_some(())
            .expect("expected InvalidIdentifier");
    }
}
