//! Edge-case coverage for the predicate identifier validator.
//!
//! The schema-strictness work added more user-reachable code paths
//! that take a `(namespace, name)` qname and validate it before any
//! storage access. The validator's contract is intentionally narrow
//! — ASCII-only `[a-z][a-z0-9_]*`, namespace ≤ 32 chars, name ≤ 64.
//! These cases lock that contract down by example so a well-meaning
//! refactor cannot silently relax the grammar.
//!
//! Resolutions in force:
//! - Validator stays ASCII-only `[a-z][a-z0-9_]*` (unicode REJECTED).
//! - `NAME_MAX_LEN = 64` (64 accepted, 65 rejected).
//! - Special chars rejected.

use brain_metadata::schema::predicate::{
    predicate_intern_or_get, predicate_lookup_by_qname, PredicateOpError, NAMESPACE_MAX_LEN,
    NAME_MAX_LEN,
};
use redb::ReadableDatabase;

fn fresh_db(dir: &tempfile::TempDir) -> redb::Database {
    redb::Database::create(dir.path().join("test.redb")).expect("create redb")
}

fn assert_invalid(err: PredicateOpError) {
    matches!(err, PredicateOpError::InvalidIdentifier { .. })
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

    // First char must be [a-z]; pad with 'a' to reach exactly 64.
    let name: String = "a".repeat(NAME_MAX_LEN);
    assert_eq!(name.len(), 64);
    let id = predicate_intern_or_get(&wtxn, "acme", &name, 0, 0).expect("64 chars accepted");
    wtxn.commit().unwrap();
    assert_eq!(id.raw(), 1);

    let rtxn = db.begin_read().unwrap();
    let got = predicate_lookup_by_qname(&rtxn, "acme", &name)
        .unwrap()
        .expect("row found");
    assert_eq!(got.name, name);
}

#[test]
fn name_one_over_max_len_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();

    let name: String = "a".repeat(NAME_MAX_LEN + 1);
    assert_eq!(name.len(), 65);
    let err = predicate_intern_or_get(&wtxn, "acme", &name, 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn namespace_at_max_len_is_accepted() {
    assert_eq!(NAMESPACE_MAX_LEN, 32);
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();

    let ns: String = "a".repeat(NAMESPACE_MAX_LEN);
    let id = predicate_intern_or_get(&wtxn, &ns, "x", 0, 0).expect("32-char namespace accepted");
    wtxn.commit().unwrap();
    assert_eq!(id.raw(), 1);
}

#[test]
fn namespace_one_over_max_len_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();

    let ns: String = "a".repeat(NAMESPACE_MAX_LEN + 1);
    let err = predicate_intern_or_get(&wtxn, &ns, "x", 0, 0).unwrap_err();
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
    let err = predicate_intern_or_get(&wtxn, "", "x", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn empty_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = predicate_intern_or_get(&wtxn, "acme", "", 0, 0).unwrap_err();
    assert_invalid(err);
}

// ---------------------------------------------------------------------------
// Predicate NAMES are an open vocabulary — any-script letters/digits are
// accepted (a relation coined from non-English text must not be dropped).
// The NAMESPACE stays ASCII-strict (it's a deliberate identifier).
// ---------------------------------------------------------------------------

#[test]
fn unicode_letter_in_name_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    // Cyrillic 'а' (U+0430) is a real letter — a coined predicate may use it.
    predicate_intern_or_get(&wtxn, "acme", "loves\u{0430}", 0, 0)
        .expect("non-ASCII letter is a valid open-vocab predicate name");
}

#[test]
fn unicode_first_char_in_name_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    // German sharp s leading — a letter, so a valid name start.
    predicate_intern_or_get(&wtxn, "acme", "\u{00DF}name", 0, 0)
        .expect("a leading non-ASCII letter is valid");
}

#[test]
fn unicode_in_namespace_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = predicate_intern_or_get(&wtxn, "ac\u{0301}me", "x", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn emoji_in_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = predicate_intern_or_get(&wtxn, "acme", "love\u{1F600}", 0, 0).unwrap_err();
    assert_invalid(err);
}

// ---------------------------------------------------------------------------
// Special characters rejected.
// ---------------------------------------------------------------------------

#[test]
fn colon_in_name_is_rejected() {
    // Colon is the qname separator — accepting it inside a segment
    // would break the canonical `"namespace:name"` parse.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = predicate_intern_or_get(&wtxn, "acme", "lo:ves", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn hyphen_in_name_is_accepted() {
    // A hyphen is a legitimate connector in domain relations (`5-ht2a`,
    // `co-occurs`); the open-vocab name grammar accepts it.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    predicate_intern_or_get(&wtxn, "acme", "is-a", 0, 0).expect("hyphen is a valid name connector");
}

#[test]
fn dot_in_namespace_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = predicate_intern_or_get(&wtxn, "ac.me", "x", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn whitespace_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = predicate_intern_or_get(&wtxn, "acme", "love it", 0, 0).unwrap_err();
    assert_invalid(err);
    let err = predicate_intern_or_get(&wtxn, "ac me", "x", 0, 0).unwrap_err();
    assert_invalid(err);
}

#[test]
fn uppercase_first_char_namespace_rejected_name_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    // Namespace stays ASCII-lowercase-strict.
    let err = predicate_intern_or_get(&wtxn, "Acme", "x", 0, 0).unwrap_err();
    assert_invalid(err);
    // A name is open vocabulary — case isn't a structural hazard.
    predicate_intern_or_get(&wtxn, "acme", "Loves", 0, 0).expect("uppercase name is accepted");
}

#[test]
fn leading_digit_namespace_rejected_name_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    // Namespace must not start with a digit.
    let err = predicate_intern_or_get(&wtxn, "1acme", "x", 0, 0).unwrap_err();
    assert_invalid(err);
    // A digit-leading name (`5ht2a_agonist`) is a real relation surface.
    predicate_intern_or_get(&wtxn, "acme", "1loves", 0, 0)
        .expect("digit-leading name is accepted");
}

#[test]
fn leading_underscore_is_rejected() {
    // `[a-z]` is the first-char rule — underscore-prefixed names
    // (reserved for internal-only tables in `schema.rs`) must be
    // unreachable from the user-facing intern path.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = predicate_intern_or_get(&wtxn, "_acme", "x", 0, 0).unwrap_err();
    assert_invalid(err);
}

// ---------------------------------------------------------------------------
// Namespace collisions — the same `name` under different namespaces
// are independent predicates with independent ids.
// ---------------------------------------------------------------------------

#[test]
fn same_name_different_namespaces_are_independent_predicates() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let a = predicate_intern_or_get(&wtxn, "acme", "loves", 0, 0).unwrap();
    let b = predicate_intern_or_get(&wtxn, "crm", "loves", 0, 0).unwrap();
    wtxn.commit().unwrap();
    assert_ne!(
        a, b,
        "qname is `namespace:name`; different namespace ⇒ different predicate"
    );

    let rtxn = db.begin_read().unwrap();
    let a_row = predicate_lookup_by_qname(&rtxn, "acme", "loves")
        .unwrap()
        .unwrap();
    let b_row = predicate_lookup_by_qname(&rtxn, "crm", "loves")
        .unwrap()
        .unwrap();
    assert_eq!(a_row.id, a);
    assert_eq!(b_row.id, b);
    assert_eq!(a_row.namespace, "acme");
    assert_eq!(b_row.namespace, "crm");
}

#[test]
fn name_containing_namespace_text_does_not_collide() {
    // `"acme:loves"` vs `("acme", "loves")` — a malicious / careless
    // caller can't sneak a colon through to forge the qname because
    // the validator rejects colons inside segments.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let wtxn = db.begin_write().unwrap();
    let err = predicate_intern_or_get(&wtxn, "acme", "acme:loves", 0, 0).unwrap_err();
    assert_invalid(err);
}
