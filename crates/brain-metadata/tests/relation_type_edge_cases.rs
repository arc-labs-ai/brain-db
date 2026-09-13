//! Edge-case coverage for the relation-type identifier validator
//! plus the implicit-write cardinality default. Mirrors
//! `predicate_edge_cases.rs` for the relation-type side.
//!
//! Resolutions in force:
//! - Validator stays ASCII-only `[a-z][a-z0-9_]*` (unicode REJECTED).
//! - `NAME_MAX_LEN = 64` (64 accepted, 65 rejected).
//! - Special chars rejected.
//! - Implicit `relation_type_intern_or_get` MUST default cardinality
//!   to `ManyToMany` so schemaless writes never trip the
//!   auto-supersession / CardinalityViolation path.

use brain_core::Cardinality;
use brain_metadata::relation::types::{
    relation_type_intern_or_get, relation_type_lookup_by_qname, RelationTypeOpError,
    NAMESPACE_MAX_LEN, NAME_MAX_LEN,
};
use redb::ReadableDatabase;

fn fresh_db(dir: &tempfile::TempDir) -> redb::Database {
    redb::Database::create(dir.path().join("test.redb")).expect("create redb")
}

fn assert_invalid(err: RelationTypeOpError) {
    matches!(err, RelationTypeOpError::InvalidIdentifier { .. })
        .then_some(())
        .unwrap_or_else(|| panic!("expected InvalidIdentifier, got {err:?}"));
}

// ---------------------------------------------------------------------------
// Length boundaries.
// ---------------------------------------------------------------------------

#[test]
fn name_at_max_len_is_accepted() {
    assert_eq!(NAME_MAX_LEN, 64);
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();

    let name: String = "a".repeat(NAME_MAX_LEN);
    let id = relation_type_intern_or_get(&wtxn, "acme", &name, 0, 0).expect("64 chars accepted");
    wtxn.commit().unwrap();
    assert_eq!(id.raw(), 1);
}

#[test]
fn name_one_over_max_len_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();

    let name: String = "a".repeat(NAME_MAX_LEN + 1);
    let err = relation_type_intern_or_get(&wtxn, "acme", &name, 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn namespace_at_max_len_is_accepted() {
    assert_eq!(NAMESPACE_MAX_LEN, 32);
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();

    let ns: String = "a".repeat(NAMESPACE_MAX_LEN);
    let id =
        relation_type_intern_or_get(&wtxn, &ns, "x", 0, 0).expect("32-char namespace accepted");
    wtxn.commit().unwrap();
    assert_eq!(id.raw(), 1);
}

#[test]
fn namespace_one_over_max_len_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();

    let ns: String = "a".repeat(NAMESPACE_MAX_LEN + 1);
    let err = relation_type_intern_or_get(&wtxn, &ns, "x", 0, 0).unwrap_err();
    assert_invalid(err);
}

// ---------------------------------------------------------------------------
// Empty rejection.
// ---------------------------------------------------------------------------

#[test]
fn empty_namespace_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = relation_type_intern_or_get(&wtxn, "", "x", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn empty_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = relation_type_intern_or_get(&wtxn, "acme", "", 0, 0).unwrap_err();
    assert_invalid(err);
}

// ---------------------------------------------------------------------------
// Unicode rejection.
// ---------------------------------------------------------------------------

#[test]
fn unicode_letter_in_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = relation_type_intern_or_get(&wtxn, "acme", "reports_to\u{0301}", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn unicode_first_char_in_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = relation_type_intern_or_get(&wtxn, "acme", "\u{00DF}rel", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn emoji_in_namespace_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = relation_type_intern_or_get(&wtxn, "ac\u{1F600}me", "x", 0, 0).unwrap_err();
    assert_invalid(err);
}

// ---------------------------------------------------------------------------
// Special characters rejected.
// ---------------------------------------------------------------------------

#[test]
fn colon_in_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = relation_type_intern_or_get(&wtxn, "acme", "rep:to", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn hyphen_in_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = relation_type_intern_or_get(&wtxn, "acme", "reports-to", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn whitespace_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = relation_type_intern_or_get(&wtxn, "acme", "reports to", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn uppercase_first_char_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = relation_type_intern_or_get(&wtxn, "Acme", "x", 0, 0).unwrap_err();
    assert_invalid(err);
    let err = relation_type_intern_or_get(&wtxn, "acme", "Reports", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn leading_digit_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = relation_type_intern_or_get(&wtxn, "9acme", "x", 0, 0).unwrap_err();
    assert_invalid(err);
    let err = relation_type_intern_or_get(&wtxn, "acme", "9reports", 0, 0).unwrap_err();
    assert_invalid(err);
}

// ---------------------------------------------------------------------------
// Namespace collision behavior.
// ---------------------------------------------------------------------------

#[test]
fn same_name_different_namespaces_are_independent_types() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let a = relation_type_intern_or_get(&wtxn, "acme", "reports_to", 0, 0).unwrap();
    let b = relation_type_intern_or_get(&wtxn, "crm", "reports_to", 0, 0).unwrap();
    wtxn.commit().unwrap();
    assert_ne!(a, b);

    let rtxn = db.begin_read().unwrap();
    let a_row = relation_type_lookup_by_qname(&rtxn, "acme", "reports_to")
        .unwrap()
        .unwrap();
    let b_row = relation_type_lookup_by_qname(&rtxn, "crm", "reports_to")
        .unwrap()
        .unwrap();
    assert_eq!(a_row.id, a);
    assert_eq!(b_row.id, b);
    assert_eq!(a_row.namespace, "acme");
    assert_eq!(b_row.namespace, "crm");
}

// ---------------------------------------------------------------------------
// Implicit default: schemaless intern must produce ManyToMany.
// ---------------------------------------------------------------------------

#[test]
fn implicit_intern_defaults_to_many_to_many() {
    // Schemaless writers don't pick a cardinality. Anything other than
    // ManyToMany would silently auto-supersede pre-existing rows on the
    // next create — surprising and lossy.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let _ = relation_type_intern_or_get(&wtxn, "acme", "follows", 0, 0).unwrap();
    wtxn.commit().unwrap();

    let rtxn = db.begin_read().unwrap();
    let rt = relation_type_lookup_by_qname(&rtxn, "acme", "follows")
        .unwrap()
        .expect("row exists");
    assert_eq!(rt.cardinality, Cardinality::ManyToMany);
    assert!(!rt.is_symmetric);
    assert_eq!(rt.schema_version, 0);
    assert!(rt.from_type.is_none());
    assert!(rt.to_type.is_none());
}

#[test]
fn implicit_intern_is_idempotent_and_keeps_default_cardinality() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let id1 = relation_type_intern_or_get(&wtxn, "acme", "follows", 0, 0).unwrap();
    let id2 = relation_type_intern_or_get(&wtxn, "acme", "follows", 7, 100).unwrap();
    wtxn.commit().unwrap();
    assert_eq!(id1, id2);

    let rtxn = db.begin_read().unwrap();
    let rt = relation_type_lookup_by_qname(&rtxn, "acme", "follows")
        .unwrap()
        .unwrap();
    assert_eq!(rt.cardinality, Cardinality::ManyToMany);
}
