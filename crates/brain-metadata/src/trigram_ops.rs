//! Trigram extraction, Jaccard similarity, and the `entity_trigrams`
//! index ops. Sub-task 16.4.
//!
//! Implements the tier-2 fuzzy-resolution primitives per
//! `spec/18_entities/01_resolution.md` § Tier 2. Free functions over
//! redb transactions; matches the `entity_ops` precedent so callers
//! can compose multi-table writes within one transaction.
//!
//! ## Trigram extraction style
//!
//! pg_trgm convention: split the normalized name into whitespace-
//! separated words, pad each word as `"  " + word + " "` (two
//! leading spaces, one trailing), then extract every 3-byte window.
//! Operates on **bytes**, not Unicode code points — the same
//! convention pg_trgm uses; Unicode multi-byte sequences may be
//! sliced by a 3-byte window. Acceptable as long as both write and
//! read paths extract the same way.

use std::collections::HashSet;

use brain_core::{Entity, EntityId, EntityTypeId};
use redb::{ReadTransaction, WriteTransaction};

use crate::entity_ops::normalize_name;
use crate::tables::knowledge::entity::ENTITY_TRIGRAMS_TABLE;

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum TrigramOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
}

// ---------------------------------------------------------------------------
// Extraction.
// ---------------------------------------------------------------------------

/// Extract the trigram set of a normalized string.
///
/// Caller is responsible for pre-normalizing (`entity_ops::normalize_name`).
/// Empty input returns an empty set.
#[must_use]
pub fn extract_trigrams(normalized: &str) -> HashSet<[u8; 3]> {
    let mut out = HashSet::new();
    for word in normalized.split_whitespace() {
        // Pad each word: 2 leading spaces + word + 1 trailing space.
        let mut padded = Vec::with_capacity(word.len() + 3);
        padded.extend_from_slice(b"  ");
        padded.extend_from_slice(word.as_bytes());
        padded.push(b' ');
        for window in padded.windows(3) {
            // windows(3) yields slices of exactly length 3; the conversion
            // is infallible.
            if let Ok(arr) = <[u8; 3]>::try_from(window) {
                out.insert(arr);
            }
        }
    }
    out
}

/// Union of trigrams across an entity's `canonical_name` and every
/// alias. Normalizes each component internally.
#[must_use]
pub fn trigrams_of_entity(entity: &Entity) -> HashSet<[u8; 3]> {
    let mut out = HashSet::new();
    out.extend(extract_trigrams(&normalize_name(&entity.canonical_name)));
    for alias in &entity.aliases {
        out.extend(extract_trigrams(&normalize_name(alias)));
    }
    out
}

/// Convenience: extract trigrams for an entity assembled from raw
/// strings (canonical_name + aliases). Used by `entity_ops`'s
/// integration helpers where we don't have a full `Entity` value.
#[must_use]
pub fn trigrams_of_components(canonical_name: &str, aliases: &[String]) -> HashSet<[u8; 3]> {
    let mut out = HashSet::new();
    out.extend(extract_trigrams(&normalize_name(canonical_name)));
    for alias in aliases {
        out.extend(extract_trigrams(&normalize_name(alias)));
    }
    out
}

// ---------------------------------------------------------------------------
// Similarity.
// ---------------------------------------------------------------------------

/// Jaccard similarity between two trigram sets: `|A ∩ B| / |A ∪ B|`.
/// Returns `0.0` when both sets are empty (avoids 0/0).
#[must_use]
pub fn jaccard(a: &HashSet<[u8; 3]>, b: &HashSet<[u8; 3]>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        (intersection as f32) / (union as f32)
    }
}

// ---------------------------------------------------------------------------
// redb writes.
// ---------------------------------------------------------------------------

/// Insert one `entity_trigrams` row per trigram in `trigrams`.
pub fn index_entity_trigrams(
    wtxn: &WriteTransaction,
    type_id: EntityTypeId,
    entity_id: EntityId,
    trigrams: &HashSet<[u8; 3]>,
) -> Result<(), TrigramOpError> {
    if trigrams.is_empty() {
        return Ok(());
    }
    let mut t = wtxn.open_table(ENTITY_TRIGRAMS_TABLE)?;
    let id_bytes = entity_id.to_bytes();
    for tg in trigrams {
        t.insert(&(type_id.raw(), *tg, id_bytes), &())?;
    }
    Ok(())
}

/// Remove one `entity_trigrams` row per trigram in `trigrams`. No-op
/// if a row is already absent (redb's `remove` returns `Ok(None)` in
/// that case).
pub fn remove_entity_trigrams(
    wtxn: &WriteTransaction,
    type_id: EntityTypeId,
    entity_id: EntityId,
    trigrams: &HashSet<[u8; 3]>,
) -> Result<(), TrigramOpError> {
    if trigrams.is_empty() {
        return Ok(());
    }
    let mut t = wtxn.open_table(ENTITY_TRIGRAMS_TABLE)?;
    let id_bytes = entity_id.to_bytes();
    for tg in trigrams {
        t.remove(&(type_id.raw(), *tg, id_bytes))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// redb reads.
// ---------------------------------------------------------------------------

/// All EntityIds whose trigram set contains `trigram` under `type_id`.
/// Range-scans the multi-value index at prefix `(type_id, trigram, *)`.
pub fn lookup_candidates_by_trigram(
    rtxn: &ReadTransaction,
    type_id: EntityTypeId,
    trigram: [u8; 3],
) -> Result<Vec<EntityId>, TrigramOpError> {
    let t = rtxn.open_table(ENTITY_TRIGRAMS_TABLE)?;
    let lo = (type_id.raw(), trigram, [0u8; 16]);
    let hi = (type_id.raw(), trigram, [0xFFu8; 16]);
    let mut out = Vec::new();
    for entry in t.range(lo..=hi)? {
        let (k, _) = entry?;
        let (k_type, k_tg, k_id) = k.value();
        if k_type == type_id.raw() && k_tg == trigram {
            out.push(EntityId::from(k_id));
        }
    }
    Ok(out)
}

/// Tier-2 candidate union: for every trigram of `query_normalized`,
/// collect EntityIds from the index and return the deduplicated set.
///
/// The resolver (16.5) feeds the result through Jaccard scoring +
/// the configured threshold. This function returns *candidates*, not
/// resolved matches.
pub fn candidates_for_query(
    rtxn: &ReadTransaction,
    type_id: EntityTypeId,
    query_normalized: &str,
) -> Result<HashSet<EntityId>, TrigramOpError> {
    let qg = extract_trigrams(query_normalized);
    let mut out = HashSet::new();
    if qg.is_empty() {
        return Ok(out);
    }
    let t = rtxn.open_table(ENTITY_TRIGRAMS_TABLE)?;
    for tg in qg {
        let lo = (type_id.raw(), tg, [0u8; 16]);
        let hi = (type_id.raw(), tg, [0xFFu8; 16]);
        for entry in t.range(lo..=hi)? {
            let (k, _) = entry?;
            let (k_type, k_tg, k_id) = k.value();
            if k_type == type_id.raw() && k_tg == tg {
                out.insert(EntityId::from(k_id));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::MetadataDb;
    use brain_core::EntityType;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn db_path(dir: &TempDir) -> PathBuf {
        dir.path().join("metadata.redb")
    }

    fn fresh_db(dir: &TempDir) -> MetadataDb {
        MetadataDb::open(db_path(dir)).expect("open")
    }

    // ----- Extraction ---------------------------------------------------

    #[test]
    fn extract_trigrams_pg_trgm_style_single_word() {
        // "priya" → padded "  priya " → windows: "  p", " pr", "pri",
        // "riy", "iya", "ya "
        let t = extract_trigrams("priya");
        let expected: HashSet<[u8; 3]> = [
            *b"  p", *b" pr", *b"pri", *b"riy", *b"iya", *b"ya ",
        ]
        .into_iter()
        .collect();
        assert_eq!(t, expected);
    }

    #[test]
    fn extract_trigrams_two_words_unions_and_dedupes() {
        // "priya patel": both padded words contribute; "  p" is shared.
        let t = extract_trigrams("priya patel");
        // Union of both words; dedupe.
        let expected: HashSet<[u8; 3]> = [
            *b"  p", *b" pr", *b"pri", *b"riy", *b"iya", *b"ya ", // priya
            *b" pa", *b"pat", *b"ate", *b"tel", *b"el ", // patel
        ]
        .into_iter()
        .collect();
        assert_eq!(t, expected);
    }

    #[test]
    fn extract_trigrams_empty_string_is_empty_set() {
        assert!(extract_trigrams("").is_empty());
        assert!(extract_trigrams("   ").is_empty()); // pure whitespace
    }

    #[test]
    fn extract_trigrams_single_char_yields_two() {
        // "x" → padded "  x " → windows: "  x", " x "
        let t = extract_trigrams("x");
        assert_eq!(t.len(), 2);
        assert!(t.contains(b"  x"));
        assert!(t.contains(b" x "));
    }

    #[test]
    fn extract_trigrams_unicode_does_not_panic_and_is_byte_level() {
        let t = extract_trigrams("straße");
        // Specific shape is pg_trgm convention applied byte-by-byte;
        // not asserting the exact set — just that we got SOME trigrams
        // and the call returned without panic.
        assert!(!t.is_empty());
    }

    #[test]
    fn trigrams_of_entity_unions_canonical_and_aliases() {
        let mut e = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Priya".into(),
            "priya".into(),
            0,
        );
        let canonical_only = trigrams_of_entity(&e);
        e.aliases.push("Patel".into());
        let with_alias = trigrams_of_entity(&e);
        assert!(with_alias.len() > canonical_only.len());
        for tg in &canonical_only {
            assert!(with_alias.contains(tg), "alias union must include canonical");
        }
        // "pat" is only in the alias.
        assert!(with_alias.contains(b"pat"));
        assert!(!canonical_only.contains(b"pat"));
    }

    // ----- Jaccard ------------------------------------------------------

    #[test]
    fn jaccard_identical_sets_is_one() {
        let a = extract_trigrams("priya patel");
        let b = a.clone();
        assert!((jaccard(&a, &b) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn jaccard_disjoint_sets_is_zero() {
        let mut a = HashSet::new();
        a.insert(*b"abc");
        a.insert(*b"def");
        let mut b = HashSet::new();
        b.insert(*b"xyz");
        b.insert(*b"123");
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn jaccard_empty_empty_is_zero() {
        let a: HashSet<[u8; 3]> = HashSet::new();
        let b: HashSet<[u8; 3]> = HashSet::new();
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        // {abc, def, ghi} vs {def, ghi, jkl} → intersection 2, union 4 → 0.5
        let a: HashSet<[u8; 3]> = [*b"abc", *b"def", *b"ghi"].into_iter().collect();
        let b: HashSet<[u8; 3]> = [*b"def", *b"ghi", *b"jkl"].into_iter().collect();
        assert!((jaccard(&a, &b) - 0.5).abs() < f32::EPSILON);
    }

    // ----- redb integration --------------------------------------------

    #[test]
    fn index_then_lookup_round_trips() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let id = EntityId::new();
        let trigrams: HashSet<[u8; 3]> = [*b"pri", *b"riy", *b"iya"].into_iter().collect();

        let wtxn = db.write_txn().unwrap();
        index_entity_trigrams(&wtxn, EntityType::PERSON_ID, id, &trigrams).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        for tg in &trigrams {
            let ids =
                lookup_candidates_by_trigram(&rtxn, EntityType::PERSON_ID, *tg).unwrap();
            assert_eq!(ids, vec![id]);
        }
    }

    #[test]
    fn remove_clears_index_rows() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let id = EntityId::new();
        let trigrams: HashSet<[u8; 3]> = [*b"pri", *b"riy"].into_iter().collect();

        let wtxn = db.write_txn().unwrap();
        index_entity_trigrams(&wtxn, EntityType::PERSON_ID, id, &trigrams).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        remove_entity_trigrams(&wtxn, EntityType::PERSON_ID, id, &trigrams).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        for tg in &trigrams {
            assert!(lookup_candidates_by_trigram(&rtxn, EntityType::PERSON_ID, *tg)
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn candidates_for_query_unions_across_trigrams() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let alpha = EntityId::new();
        let beta = EntityId::new();

        let wtxn = db.write_txn().unwrap();
        index_entity_trigrams(
            &wtxn,
            EntityType::PERSON_ID,
            alpha,
            &extract_trigrams("priya"),
        )
        .unwrap();
        index_entity_trigrams(
            &wtxn,
            EntityType::PERSON_ID,
            beta,
            &extract_trigrams("paris"),
        )
        .unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        // Query "priya" should match alpha strongly + beta weakly (they
        // share "  p", " p?", etc.). candidates_for_query returns the
        // UNION — both are in the candidate set.
        let cands = candidates_for_query(&rtxn, EntityType::PERSON_ID, "priya").unwrap();
        assert!(cands.contains(&alpha));
        assert!(cands.contains(&beta));
    }

    #[test]
    fn lookup_filters_by_type_id() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);

        // Seed a second entity type so the filter is meaningful.
        {
            use crate::tables::knowledge::entity_type::{
                EntityTypeDefinition, ENTITY_TYPES_TABLE,
            };
            let wtxn = db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
                let row = EntityTypeDefinition::new(
                    EntityTypeId(7),
                    "Project".into(),
                    Vec::new(),
                    0,
                );
                t.insert(&7u32, &row).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let person = EntityId::new();
        let project = EntityId::new();
        let tg = *b"pri";
        let trigrams: HashSet<[u8; 3]> = [tg].into_iter().collect();

        let wtxn = db.write_txn().unwrap();
        index_entity_trigrams(&wtxn, EntityType::PERSON_ID, person, &trigrams).unwrap();
        index_entity_trigrams(&wtxn, EntityTypeId(7), project, &trigrams).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let person_cands =
            lookup_candidates_by_trigram(&rtxn, EntityType::PERSON_ID, tg).unwrap();
        assert_eq!(person_cands, vec![person]);
        let project_cands = lookup_candidates_by_trigram(&rtxn, EntityTypeId(7), tg).unwrap();
        assert_eq!(project_cands, vec![project]);
    }
}
