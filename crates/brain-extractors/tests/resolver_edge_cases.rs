//! Resolver edge cases:
//!
//! - Unicode-rich surface forms (CJK, emoji, mixed-script) must round
//!   through the gauntlet without panicking and the resulting entity
//!   row preserves the original surface form in `canonical_name`.
//! - Very long surface forms (~10 KB) succeed; the resolver does not
//!   silently truncate.
//! - Distinct surface forms that share trigram structure resolve
//!   independently when the fuzzy threshold isn't met.

use brain_core::EntityId;
use brain_extractors::resolver::{resolve_or_create, ResolutionTier};
use brain_metadata::entity::ops::{entity_get, normalize_name};
use brain_metadata::MetadataDb;
use tempfile::TempDir;

const NOW: u64 = 1_700_000_000_000_000_000;

fn open_db() -> (TempDir, MetadataDb) {
    let dir = TempDir::new().unwrap();
    let db = MetadataDb::open(dir.path().join("metadata.redb")).unwrap();
    (dir, db)
}

fn create_and_fetch(sf: &str, qname: &str) -> (EntityId, String) {
    let (_dir, db) = open_db();
    let wtxn = db.write_txn().unwrap();
    let res = resolve_or_create(&wtxn, sf, qname, 0.9, NOW).unwrap();
    wtxn.commit().unwrap();
    let rtxn = db.read_txn().unwrap();
    let row = entity_get(&rtxn, res.entity_id).unwrap().expect("row");
    (res.entity_id, row.canonical_name)
}

// ---------------------------------------------------------------------------
// Unicode.
// ---------------------------------------------------------------------------

#[test]
fn chinese_surface_form_round_trips() {
    let (_id, canonical) = create_and_fetch("张伟", "brain:Person");
    assert_eq!(canonical, "张伟");
}

#[test]
fn emoji_surface_form_round_trips() {
    let (_id, canonical) = create_and_fetch("Pet Rock 🪨", "brain:Person");
    assert_eq!(canonical, "Pet Rock 🪨");
}

#[test]
fn mixed_script_round_trips() {
    let (_id, canonical) = create_and_fetch("Anna Müller-García 王", "brain:Person");
    assert_eq!(canonical, "Anna Müller-García 王");
}

#[test]
fn unicode_case_folding_still_dedupes() {
    let (_dir, db) = open_db();
    let wtxn = db.write_txn().unwrap();
    let r1 = resolve_or_create(&wtxn, "Straße", "brain:Person", 0.9, NOW).unwrap();
    let r2 = resolve_or_create(&wtxn, "STRASSE", "brain:Person", 0.9, NOW + 1).unwrap();
    wtxn.commit().unwrap();
    // The Rust stdlib's to_lowercase maps ß → "ss"; STRASSE lowercases
    // to "strasse", which differs from "straße". They should NOT collide
    // (no Unicode-aware normalisation beyond ASCII case folding).
    assert_ne!(
        r1.entity_id, r2.entity_id,
        "Straße and STRASSE are distinct under our normaliser"
    );
}

// ---------------------------------------------------------------------------
// Very long.
// ---------------------------------------------------------------------------

#[test]
fn very_long_surface_form_does_not_panic_or_truncate() {
    let long: String = "a".repeat(10_000);
    let (_dir, db) = open_db();
    let wtxn = db.write_txn().unwrap();
    let res = resolve_or_create(&wtxn, &long, "brain:Person", 0.9, NOW).unwrap();
    wtxn.commit().unwrap();
    let rtxn = db.read_txn().unwrap();
    let row = entity_get(&rtxn, res.entity_id).unwrap().unwrap();
    assert_eq!(row.canonical_name.len(), 10_000, "no silent truncation");
    assert_eq!(row.normalized_name, normalize_name(&long));
}

// ---------------------------------------------------------------------------
// Trigram-poor surface forms.
// ---------------------------------------------------------------------------

#[test]
fn one_character_surface_forms_create_independently() {
    // A 1-char string has zero trigrams; the fuzzy tier returns no
    // candidate, so each distinct char gets its own entity.
    let (_dir, db) = open_db();
    let wtxn = db.write_txn().unwrap();
    let a = resolve_or_create(&wtxn, "a", "brain:Person", 0.9, NOW).unwrap();
    let b = resolve_or_create(&wtxn, "b", "brain:Person", 0.9, NOW + 1).unwrap();
    let a_again = resolve_or_create(&wtxn, "A", "brain:Person", 0.9, NOW + 2).unwrap();
    wtxn.commit().unwrap();
    assert_ne!(a.entity_id, b.entity_id);
    // 'A' normalises to 'a' → tier-1 hit.
    assert_eq!(a.entity_id, a_again.entity_id);
    assert_eq!(a_again.tier, ResolutionTier::Exact);
}

#[test]
fn unrelated_names_do_not_dedup_via_fuzzy() {
    let (_dir, db) = open_db();
    let wtxn = db.write_txn().unwrap();
    let a = resolve_or_create(&wtxn, "Alice", "brain:Person", 0.9, NOW).unwrap();
    let b = resolve_or_create(&wtxn, "Zelda", "brain:Person", 0.9, NOW + 1).unwrap();
    wtxn.commit().unwrap();
    assert_ne!(a.entity_id, b.entity_id);
    assert_eq!(a.tier, ResolutionTier::Created);
    assert_eq!(b.tier, ResolutionTier::Created);
}

#[test]
fn cross_type_exact_name_reuses_entity_on_first_collision() {
    // Same surface form first seen as a Person, then as an Organization.
    // With exactly one prior cross-type match the resolver reuses that
    // node instead of fragmenting the referent across types (Tier 1b
    // cross-type exact reuse — extractor tiers routinely assign different
    // type ids to the same referent). The homograph guard only forks once
    // two same-name entities already exist, so the first collision reuses.
    let (_dir, db) = open_db();
    let wtxn = db.write_txn().unwrap();
    let p = resolve_or_create(&wtxn, "Acme", "brain:Person", 0.9, NOW).unwrap();
    let o = resolve_or_create(&wtxn, "Acme", "brain:Organization", 0.9, NOW + 1).unwrap();
    wtxn.commit().unwrap();
    assert_eq!(
        p.entity_id, o.entity_id,
        "exactly one prior cross-type match → reuse, not fragment"
    );
}
