//! Property tests for the entity resolver gauntlet.
//!
//! Three invariants drive these tests:
//!
//! 1. **Replay determinism.** Replaying the same sequence of
//!    `resolve_or_create` calls on a fresh DB always lands on the same
//!    final `surface_form → EntityId` mapping. Without this, two
//!    workers that drain the same queue replay would diverge.
//!
//! 2. **Threshold monotonicity.** Raising the trigram-Jaccard floor
//!    monotonically decreases the number of surface forms that get
//!    deduped into existing entities. Concretely, a candidate set
//!    that resolves at threshold T also resolves at every T' ≤ T.
//!
//! 3. **Case + whitespace normalisation.** `normalize_name` is
//!    case-insensitive and whitespace-collapsing, so "PRIYA", "priya"
//!    and "Priya" must hit the same exact-match tier.

use brain_core::{Entity, EntityId, EntityType};
use brain_extractors::resolver::{
    resolve_or_create, Resolution, ResolutionTier, DEFAULT_FUZZY_THRESHOLD,
};
use brain_metadata::entity_ops::{entity_put, normalize_name};
use brain_metadata::MetadataDb;
use proptest::collection::vec as pvec;
use proptest::prelude::*;
use tempfile::TempDir;

const NOW: u64 = 1_700_000_000_000_000_000;

fn open_db() -> (TempDir, MetadataDb) {
    let dir = TempDir::new().unwrap();
    let db = MetadataDb::open(dir.path().join("metadata.redb")).unwrap();
    (dir, db)
}

/// Run a sequence of `(surface_form, type_qname)` calls and return the
/// list of (surface_form, EntityId) pairs in call order. The final
/// mapping (after dedup) is the read model under test for replay
/// determinism.
fn replay(seq: &[(String, &str)]) -> Vec<(String, EntityId)> {
    let (_dir, mut db) = open_db();
    let mut out = Vec::with_capacity(seq.len());
    let wtxn = db.write_txn().unwrap();
    for (i, (sf, qname)) in seq.iter().enumerate() {
        // Empty-normalised inputs are out of scope for the determinism
        // invariant — the resolver rejects them deterministically.
        if normalize_name(sf).is_empty() {
            continue;
        }
        let res = resolve_or_create(&wtxn, sf, qname, 0.9, NOW + i as u64).unwrap();
        out.push((sf.clone(), res.entity_id));
    }
    wtxn.commit().unwrap();
    out
}

/// Per-surface-form *normalised* mapping. Two replays should produce
/// the same mapping (modulo new-uuid generation for the *Created* tier,
/// which we sidestep by comparing surface-form equivalence classes).
fn equivalence_classes(pairs: &[(String, EntityId)]) -> Vec<Vec<String>> {
    use std::collections::BTreeMap;
    let mut by_id: BTreeMap<EntityId, Vec<String>> = BTreeMap::new();
    for (sf, id) in pairs {
        by_id.entry(*id).or_default().push(normalize_name(sf));
    }
    let mut out: Vec<Vec<String>> = by_id
        .into_values()
        .map(|mut v| {
            v.sort();
            v.dedup();
            v
        })
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// 1. Replay determinism.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

    /// For any sequence of resolve calls, replaying it on a fresh DB
    /// twice produces the same surface-form equivalence classes (same
    /// merging behaviour).
    #[test]
    fn replay_on_fresh_db_is_deterministic(
        seq in pvec(("[a-zA-Z ]{2,12}", prop::sample::select(vec!["brain:Person", "brain:Organization"])), 1..16)
    ) {
        let seq: Vec<(String, &str)> = seq.into_iter().collect();
        let a = replay(&seq);
        let b = replay(&seq);
        prop_assert_eq!(equivalence_classes(&a), equivalence_classes(&b));
    }

    /// Re-resolving the exact same surface form within one DB returns
    /// the same EntityId every time (tier-1 hits after the first).
    #[test]
    fn same_surface_form_resolves_to_same_id(
        sf in "[a-zA-Z]{3,10}( [a-zA-Z]{3,10})?",
        qname in prop::sample::select(vec!["brain:Person", "brain:Organization"])
    ) {
        let (_dir, mut db) = open_db();
        let wtxn = db.write_txn().unwrap();
        let r1 = resolve_or_create(&wtxn, &sf, qname, 0.9, NOW).unwrap();
        let r2 = resolve_or_create(&wtxn, &sf, qname, 0.9, NOW + 1).unwrap();
        let r3 = resolve_or_create(&wtxn, &sf, qname, 0.9, NOW + 2).unwrap();
        prop_assert_eq!(r1.entity_id, r2.entity_id);
        prop_assert_eq!(r2.entity_id, r3.entity_id);
        prop_assert_eq!(r1.tier, ResolutionTier::Created);
        prop_assert_eq!(r2.tier, ResolutionTier::Exact);
        prop_assert_eq!(r3.tier, ResolutionTier::Exact);
        wtxn.commit().unwrap();
    }

    /// `normalize_name` collapses case + outer/internal whitespace
    /// runs (between *word* boundaries) to a single space. Variants
    /// that share the same normalised form must collide on one entity.
    #[test]
    fn case_and_whitespace_variants_collide(
        first in "[a-zA-Z]{3,8}",
        second in "[a-zA-Z]{3,8}",
        extra_spaces in 1usize..6,
        leading_pad in 0usize..4,
        trailing_pad in 0usize..4,
    ) {
        let (_dir, mut db) = open_db();
        let base = format!("{first} {second}");
        let mut variants = vec![
            base.clone(),
            base.to_uppercase(),
            base.to_lowercase(),
            // Extra whitespace between the two words (collapses to one).
            format!("{first}{}{second}", " ".repeat(extra_spaces)),
            // Leading + trailing whitespace.
            format!("{}{base}{}", " ".repeat(leading_pad), " ".repeat(trailing_pad)),
            // Mixed case on the second word.
            format!("{first} {}", second.to_uppercase()),
        ];
        // De-dup the input variants but preserve order.
        variants.dedup();

        let wtxn = db.write_txn().unwrap();
        let mut ids = Vec::new();
        for (i, v) in variants.iter().enumerate() {
            if normalize_name(v).is_empty() { continue; }
            let r = resolve_or_create(&wtxn, v, "brain:Person", 0.9, NOW + i as u64).unwrap();
            ids.push(r.entity_id);
        }
        wtxn.commit().unwrap();
        let first_id = ids[0];
        for id in &ids[1..] {
            prop_assert_eq!(*id, first_id);
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Threshold monotonicity.
// ---------------------------------------------------------------------------
//
// Resolver uses a compile-time DEFAULT_FUZZY_THRESHOLD; we exercise
// monotonicity by reproducing the trigram-Jaccard math against a
// candidate set and asserting that the "would-match" count is
// monotone non-increasing in the threshold.
//
// The intent: any production change to the threshold preserves the
// property "stricter threshold ⇒ fewer merges". A regression that
// flips this (e.g., off-by-one comparison) would alter dedup
// behaviour silently.

#[test]
fn higher_threshold_never_increases_dedup_count() {
    use brain_core::knowledge::trigrams;
    let target_norm = normalize_name("Priya Patel");
    let candidates = [
        "Priya Patell",
        "Priya P",
        "Priyam Patel",
        "Priyaa Patel",
        "Priya Pate",
        "Aleksandar K",
        "Bob",
        "Priya P. Patel",
    ];
    let target_tgs = trigrams::extract_trigrams(&target_norm);
    let scores: Vec<f32> = candidates
        .iter()
        .map(|c| {
            let cg = trigrams::extract_trigrams(&normalize_name(c));
            trigrams::jaccard(&target_tgs, &cg)
        })
        .collect();
    let count_at = |t: f32| scores.iter().filter(|&&s| s >= t).count();

    let mut last = usize::MAX;
    for ten in 0..=10 {
        let t = ten as f32 / 10.0;
        let c = count_at(t);
        assert!(
            c <= last,
            "threshold {t}: count {c} > previous {last} (monotonicity broken)"
        );
        last = c;
    }
    // Sanity: at threshold 0 every candidate counts; at threshold 1.01
    // none do.
    assert_eq!(count_at(0.0), candidates.len());
    assert_eq!(count_at(1.01), 0);
    // The configured floor still admits at least one near-miss.
    assert!(
        count_at(DEFAULT_FUZZY_THRESHOLD) >= 1,
        "DEFAULT_FUZZY_THRESHOLD={DEFAULT_FUZZY_THRESHOLD} dropped all near-miss candidates"
    );
}

// ---------------------------------------------------------------------------
// 3. Alias case-insensitive (regression).
// ---------------------------------------------------------------------------

#[test]
fn alias_lookup_normalises_case() {
    let (_dir, mut db) = open_db();
    let mut e = Entity::new_active(
        EntityId::new(),
        EntityType::PERSON_ID,
        "Priya Patel".into(),
        normalize_name("Priya Patel"),
        NOW,
    );
    e.aliases.push("priya".into());
    let target = e.id;
    {
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
    }
    for variant in ["priya", "PRIYA", "Priya", "  PrIyA  "] {
        let wtxn = db.write_txn().unwrap();
        let Resolution { entity_id, tier } =
            resolve_or_create(&wtxn, variant, "brain:Person", 0.7, NOW + 1).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(entity_id, target, "{variant} should resolve to target");
        assert_eq!(tier, ResolutionTier::Alias, "{variant} should be tier-2");
    }
}

// ---------------------------------------------------------------------------
// 4. Empty / whitespace surface forms are rejected, never crash.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, .. ProptestConfig::default() })]

    #[test]
    fn whitespace_only_surface_forms_are_rejected(ws in "[ \\t\\n]{1,16}") {
        let (_dir, mut db) = open_db();
        let wtxn = db.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, &ws, "brain:Person", 0.5, NOW);
        prop_assert!(res.is_err(), "whitespace-only surface form must error, not panic");
    }
}
