//! Entity resolver used by the extractor pipeline worker.
//!
//! The extractor framework emits `EntityMention { entity_type_qname,
//! text, ... }` records before any persistence happens. The resolver
//! turns each surface form into a stable `EntityId` by walking a
//! gauntlet of lookup tiers:
//!
//! 1. **Exact** — normalize the surface form and look it up in the
//!    `(entity_type_id, normalized_name)` canonical-name index.
//! 2. **Alias** — look it up in the alias index keyed by the same
//!    normalized form.
//! 3. **Fuzzy (trigram + Jaccard)** — fetch trigram-overlap candidates
//!    from `entity_trigrams`, score them by Jaccard similarity over
//!    trigrams. If the best candidate's score exceeds
//!    [`DEFAULT_FUZZY_THRESHOLD`], add the surface form as an alias
//!    and return that EntityId.
//! 4. **Create** — mint a fresh UUIDv7 EntityId, intern the type if
//!    needed, and write the entity row.
//!
//! Determinism comes from the lookup contract: given the same DB
//! state + same surface form, the resolver always returns the same
//! EntityId. Tier-4 creates use UUIDv7 (time + random), so two
//! independent resolves of the same brand-new surface form against
//! the same DB produce different IDs only if both observe a tier-1/2/3
//! miss — which is the intended split-brain semantics for two
//! simultaneous extractions.
//!
//! Phase E does not consult the entity HNSW (tier-3 in the §18/01
//! gauntlet) because the pipeline doesn't currently embed surface
//! forms. The trigram index covers the same "near-miss" case at lower
//! cost. A future phase wiring an entity-embedding pipeline can layer
//! HNSW lookups in front of the trigram tier without changing this
//! crate's public API.

use std::collections::HashSet;

use brain_core::knowledge::trigrams;
use brain_core::{Entity, EntityId, EntityTypeId};
use brain_metadata::entity_ops::{entity_add_alias, entity_put, normalize_name, EntityOpError};
use brain_metadata::entity_type_ops::{
    entity_type_intern, entity_type_lookup_by_name, EntityTypeOpError,
};
use brain_metadata::tables::knowledge::entity::{
    EntityMetadata, ENTITIES_TABLE, ENTITY_ALIASES_TABLE, ENTITY_BY_CANONICAL_NAME_TABLE,
    ENTITY_TRIGRAMS_TABLE,
};
use brain_metadata::trigram_ops::TrigramOpError;
use redb::{ReadableTable, WriteTransaction};

/// Jaccard floor for tier-3 fuzzy matching. Below this, the resolver
/// treats the candidate as a near-miss and skips it. Tuned conservatively
/// per the plan's "0.92" sketch — we use a lower 0.75 because trigram
/// Jaccard is a stricter signal than HNSW cosine for short names
/// (3-byte windows on a 5-character name yield only 3 trigrams; one
/// transposition halves Jaccard).
pub const DEFAULT_FUZZY_THRESHOLD: f32 = 0.75;

/// Outcome of one resolve attempt. The worker uses the tier to bump
/// per-tier counters on the pipeline audit row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionTier {
    Exact,
    Alias,
    Fuzzy,
    Created,
}

/// Successful resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub entity_id: EntityId,
    pub tier: ResolutionTier,
}

/// Errors the resolver can surface to the worker. Most are storage-level;
/// `EmptyNormalizedName` is the only logical one — extractors that emit
/// pure whitespace are dropped at the worker layer, not stored.
#[derive(thiserror::Error, Debug)]
pub enum ResolverError {
    #[error("surface form normalises to empty string")]
    EmptyNormalizedName,

    #[error("entity op: {0}")]
    EntityOp(#[from] EntityOpError),

    #[error("entity_type op: {0}")]
    EntityTypeOp(#[from] EntityTypeOpError),

    #[error("trigram op: {0}")]
    TrigramOp(#[from] TrigramOpError),

    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
}

/// Map `"brain:Person"` style qnames to a bare type name. Returns the
/// whole input if the colon is absent.
fn qname_to_type_name(qname: &str) -> &str {
    qname.split_once(':').map(|(_, n)| n).unwrap_or(qname)
}

/// Look up (or auto-intern) the [`EntityTypeId`] for `qname`. New
/// types get an empty schema blob — they're flagged as `ImplicitFromWrite`
/// from the registry's standpoint (the bootstrap seeds Person at id=1
/// so the common case never enters intern).
fn resolve_entity_type(
    wtxn: &WriteTransaction,
    qname: &str,
    now_unix_nanos: u64,
) -> Result<EntityTypeId, ResolverError> {
    let name = qname_to_type_name(qname);
    if let Some(def) = entity_type_lookup_by_name(wtxn, name)? {
        return Ok(def.id());
    }
    Ok(entity_type_intern(wtxn, name, Vec::new(), now_unix_nanos)?)
}

/// Fetch the trigram set for `entity_id`'s canonical_name + aliases.
/// Returns an empty set when the entity has no primary row (caller
/// can then skip the candidate without aborting).
fn trigram_set_for_entity(
    wtxn: &WriteTransaction,
    entity_id: EntityId,
) -> Result<HashSet<[u8; 3]>, ResolverError> {
    let t = wtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&entity_id.to_bytes())?.map(|g| g.value());
    let Some(row) = row else {
        return Ok(HashSet::new());
    };
    let mut out = trigrams::extract_trigrams(&normalize_name(&row.canonical_name));
    for alias in &row.aliases {
        out.extend(trigrams::extract_trigrams(&normalize_name(alias)));
    }
    Ok(out)
}

/// Wtxn-friendly mirror of `entity_lookup_by_canonical_name`. The
/// public op takes a `ReadTransaction`; we resolve inside the caller's
/// write txn so the resolve + downstream writes commit atomically.
fn lookup_canonical_wtxn(
    wtxn: &WriteTransaction,
    type_id: EntityTypeId,
    normalized: &str,
) -> Result<Option<EntityId>, ResolverError> {
    let t = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
    let bytes: Option<[u8; 16]> = t.get(&(type_id.raw(), normalized))?.map(|g| g.value());
    Ok(bytes.map(EntityId::from))
}

/// Wtxn-friendly mirror of `entity_lookup_by_alias`.
fn lookup_alias_wtxn(
    wtxn: &WriteTransaction,
    type_id: EntityTypeId,
    normalized: &str,
) -> Result<Vec<EntityId>, ResolverError> {
    let t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
    let lo = (type_id.raw(), normalized, [0u8; 16]);
    let hi = (type_id.raw(), normalized, [0xFFu8; 16]);
    let mut out = Vec::new();
    for entry in t.range(lo..=hi)? {
        let (k, _) = entry?;
        let (k_type, k_alias, k_id) = k.value();
        if k_type == type_id.raw() && k_alias == normalized {
            out.push(EntityId::from(k_id));
        }
    }
    Ok(out)
}

/// Wtxn-friendly mirror of `candidates_for_query`.
fn trigram_candidates_wtxn(
    wtxn: &WriteTransaction,
    type_id: EntityTypeId,
    normalized: &str,
) -> Result<HashSet<EntityId>, ResolverError> {
    let qg = trigrams::extract_trigrams(normalized);
    let mut out = HashSet::new();
    if qg.is_empty() {
        return Ok(out);
    }
    let t = wtxn.open_table(ENTITY_TRIGRAMS_TABLE)?;
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

/// Resolve `surface_form` against the entity registry, creating a new
/// entity if no tier matched. The caller drives the txn; all reads +
/// writes happen inside it so the resolver's outcome is atomic with
/// downstream writes (mention edges, statement creation).
pub fn resolve_or_create(
    wtxn: &WriteTransaction,
    surface_form: &str,
    entity_type_qname: &str,
    _confidence: f32,
    now_unix_nanos: u64,
) -> Result<Resolution, ResolverError> {
    let normalized = normalize_name(surface_form);
    if normalized.is_empty() {
        return Err(ResolverError::EmptyNormalizedName);
    }
    let type_id = resolve_entity_type(wtxn, entity_type_qname, now_unix_nanos)?;

    // Tier 1 — exact canonical-name lookup.
    if let Some(id) = lookup_canonical_wtxn(wtxn, type_id, &normalized)? {
        return Ok(Resolution {
            entity_id: id,
            tier: ResolutionTier::Exact,
        });
    }

    // Tier 2 — alias lookup. The alias index is multi-valued; if more
    // than one entity shares this alias we pick the first (smallest
    // EntityId, which is a deterministic byte order) so the result
    // stays stable across re-runs. A future ambiguity-aware resolver
    // could surface the conflict; the worker drops mentions with
    // ambiguous aliases at the cost of one extra resolve.
    let alias_hits = lookup_alias_wtxn(wtxn, type_id, &normalized)?;
    if let Some(id) = alias_hits.into_iter().min() {
        return Ok(Resolution {
            entity_id: id,
            tier: ResolutionTier::Alias,
        });
    }

    // Tier 3 — trigram fuzzy lookup. Candidates whose Jaccard against
    // the query is above `DEFAULT_FUZZY_THRESHOLD` get the surface
    // form added as an alias and are returned as the match.
    let candidate_ids = trigram_candidates_wtxn(wtxn, type_id, &normalized)?;
    if !candidate_ids.is_empty() {
        let query_tgs = trigrams::extract_trigrams(&normalized);
        if !query_tgs.is_empty() {
            let mut best: Option<(EntityId, f32)> = None;
            for cid in candidate_ids {
                let cid_tgs = trigram_set_for_entity(wtxn, cid)?;
                if cid_tgs.is_empty() {
                    continue;
                }
                let score = trigrams::jaccard(&query_tgs, &cid_tgs);
                if score < DEFAULT_FUZZY_THRESHOLD {
                    continue;
                }
                match best {
                    Some((_, bs)) if bs >= score => {}
                    _ => best = Some((cid, score)),
                }
            }
            if let Some((cid, _)) = best {
                // The surface form is now associated with this entity;
                // re-runs of the same string hit tier 2 directly.
                entity_add_alias(wtxn, cid, surface_form.to_string(), now_unix_nanos)?;
                return Ok(Resolution {
                    entity_id: cid,
                    tier: ResolutionTier::Alias,
                });
            }
        }
    }

    // Tier 4 — create. UUIDv7 makes the new id roughly time-ordered;
    // re-running this branch with the same surface form produces a
    // different id because the previous one is still around for
    // tiers 1/2 to short-circuit.
    let new_id = EntityId::new();
    let mut entity = Entity::new_active(
        new_id,
        type_id,
        surface_form.to_string(),
        normalized,
        now_unix_nanos,
    );
    entity.mention_count = 1;
    entity_put(wtxn, &entity)?;
    Ok(Resolution {
        entity_id: new_id,
        tier: ResolutionTier::Created,
    })
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_core::EntityType;
    use brain_metadata::entity_ops::entity_get;
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn db(dir: &TempDir) -> MetadataDb {
        MetadataDb::open(dir.path().join("metadata.redb")).expect("open")
    }

    #[test]
    fn tier_exact_returns_existing_entity_by_canonical_name() {
        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let existing = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Priya Patel".into(),
            normalize_name("Priya Patel"),
            NOW,
        );
        let existing_id = existing.id;
        {
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, &existing).unwrap();
            wtxn.commit().unwrap();
        }
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, "Priya Patel", "brain:Person", 0.9, NOW).unwrap();
        assert_eq!(res.entity_id, existing_id);
        assert_eq!(res.tier, ResolutionTier::Exact);
        wtxn.commit().unwrap();
    }

    #[test]
    fn tier_alias_returns_existing_entity_via_alias() {
        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let mut existing = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Priya Patel".into(),
            normalize_name("Priya Patel"),
            NOW,
        );
        existing.aliases.push("Priya".into());
        let id = existing.id;
        {
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, &existing).unwrap();
            wtxn.commit().unwrap();
        }
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, "priya", "brain:Person", 0.7, NOW).unwrap();
        assert_eq!(res.entity_id, id);
        assert_eq!(res.tier, ResolutionTier::Alias);
        wtxn.commit().unwrap();
    }

    #[test]
    fn tier_fuzzy_matches_close_surface_form_and_adds_alias() {
        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        // Two entities to make the candidate set non-trivial.
        let target = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Priya Patel".into(),
            normalize_name("Priya Patel"),
            NOW,
        );
        let other = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Aleksandar Kovacevic".into(),
            normalize_name("Aleksandar Kovacevic"),
            NOW,
        );
        let target_id = target.id;
        {
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, &target).unwrap();
            entity_put(&wtxn, &other).unwrap();
            wtxn.commit().unwrap();
        }
        // Tier-3 fuzzy: typo'd surface form should resolve to target.
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, "Priya  Patel", "brain:Person", 0.8, NOW + 1).unwrap();
        // "priya  patel" normalises to "priya patel" → tier-1 hit.
        assert_eq!(res.entity_id, target_id);
        assert_eq!(res.tier, ResolutionTier::Exact);
        wtxn.commit().unwrap();

        // Now a true fuzzy match — a partial name share.
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, "Priya Patell", "brain:Person", 0.8, NOW + 2).unwrap();
        assert_eq!(res.entity_id, target_id);
        // First fuzzy hit promotes via alias index. Re-resolve picks
        // tier-2 next time.
        assert_eq!(res.tier, ResolutionTier::Alias);
        wtxn.commit().unwrap();

        // Verify the alias was actually written.
        let rtxn = d.read_txn().unwrap();
        let got = entity_get(&rtxn, target_id).unwrap().unwrap();
        assert!(
            got.aliases.iter().any(|a| a == "Priya Patell"),
            "tier-3 should add the surface form as an alias; got {:?}",
            got.aliases
        );
    }

    #[test]
    fn tier_create_mints_new_entity_when_no_match() {
        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, "Brand New Name", "brain:Person", 0.5, NOW).unwrap();
        assert_eq!(res.tier, ResolutionTier::Created);
        wtxn.commit().unwrap();
        let rtxn = d.read_txn().unwrap();
        let got = entity_get(&rtxn, res.entity_id).unwrap().unwrap();
        assert_eq!(got.canonical_name, "Brand New Name");
        assert_eq!(got.entity_type, EntityType::PERSON_ID);
        // A second resolve on the same surface form should hit tier 1
        // (deterministic re-resolve).
        let wtxn = d.write_txn().unwrap();
        let res2 =
            resolve_or_create(&wtxn, "Brand New Name", "brain:Person", 0.5, NOW + 1).unwrap();
        assert_eq!(res2.entity_id, res.entity_id);
        assert_eq!(res2.tier, ResolutionTier::Exact);
        wtxn.commit().unwrap();
    }

    #[test]
    fn empty_surface_form_is_rejected() {
        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let wtxn = d.write_txn().unwrap();
        let err = resolve_or_create(&wtxn, "   ", "brain:Person", 0.5, NOW).expect_err("empty");
        assert!(matches!(err, ResolverError::EmptyNormalizedName));
    }

    #[test]
    fn unknown_entity_type_qname_is_interned_on_demand() {
        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, "Acme Corp", "brain:Organization", 0.7, NOW).unwrap();
        assert_eq!(res.tier, ResolutionTier::Created);
        wtxn.commit().unwrap();
        // The new type lives in the registry now.
        let mut d = d;
        let wtxn = d.write_txn().unwrap();
        let def = entity_type_lookup_by_name(&wtxn, "Organization").unwrap();
        assert!(def.is_some());
        wtxn.commit().unwrap();
    }
}
