//! Trigram extraction, Jaccard similarity, and the `entity_trigrams`
//! index ops.
//!
//! Implements the tier-2 fuzzy-resolution primitives. Free functions
//! over redb transactions; matches the `entity_ops` precedent so
//! callers can compose multi-table writes within one transaction.
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

use super::ops::normalize_name;
use crate::tables::entity::ENTITY_TRIGRAMS_TABLE;

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
// Extraction + similarity — re-exported from brain-core.
//
// The pure trigram functions live in brain-core so the resolver (in
// brain-core) can use them without taking a dep on brain-metadata. The
// redb integration stays here; the pure functions are re-exported for
// compatibility with existing callers.
// ---------------------------------------------------------------------------

pub use brain_core::resolution::trigrams::{extract_trigrams, jaccard};

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
/// The resolver feeds the result through Jaccard scoring + the
/// configured threshold. This function returns *candidates*, not
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

    // ----- Extraction + Jaccard ----------------------------------------
    //
    // The pure functions live in brain-core::typed-graph::trigrams and are
    // tested there. One re-export sanity test here ensures the public
    // path through brain-metadata still works.

    #[test]
    fn re_export_extract_trigrams_works() {
        let t = extract_trigrams("priya");
        assert!(t.contains(b"  p"));
        assert!(t.contains(b"ya "));
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
            assert!(
                with_alias.contains(tg),
                "alias union must include canonical"
            );
        }
        // "pat" is only in the alias.
        assert!(with_alias.contains(b"pat"));
        assert!(!canonical_only.contains(b"pat"));
    }

    #[test]
    fn re_export_jaccard_works() {
        let a = extract_trigrams("priya patel");
        let b = a.clone();
        assert!((jaccard(&a, &b) - 1.0).abs() < f32::EPSILON);
    }

    // ----- redb integration --------------------------------------------

    #[test]
    fn index_then_lookup_round_trips() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let id = EntityId::new();
        let trigrams: HashSet<[u8; 3]> = [*b"pri", *b"riy", *b"iya"].into_iter().collect();

        let wtxn = db.write_txn().unwrap();
        index_entity_trigrams(&wtxn, EntityType::PERSON_ID, id, &trigrams).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        for tg in &trigrams {
            let ids = lookup_candidates_by_trigram(&rtxn, EntityType::PERSON_ID, *tg).unwrap();
            assert_eq!(ids, vec![id]);
        }
    }

    #[test]
    fn remove_clears_index_rows() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
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
            assert!(
                lookup_candidates_by_trigram(&rtxn, EntityType::PERSON_ID, *tg)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn candidates_for_query_unions_across_trigrams() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
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
        let db = fresh_db(&dir);

        // Seed a second entity type so the filter is meaningful.
        {
            use crate::tables::entity_type::{EntityTypeDefinition, ENTITY_TYPES_TABLE};
            let wtxn = db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
                let row =
                    EntityTypeDefinition::new(EntityTypeId(7), "Project".into(), Vec::new(), 0);
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
        let person_cands = lookup_candidates_by_trigram(&rtxn, EntityType::PERSON_ID, tg).unwrap();
        assert_eq!(person_cands, vec![person]);
        let project_cands = lookup_candidates_by_trigram(&rtxn, EntityTypeId(7), tg).unwrap();
        assert_eq!(project_cands, vec![project]);
    }
}
