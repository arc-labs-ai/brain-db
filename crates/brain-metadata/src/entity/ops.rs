//! Typed CRUD over the entity tables.
//!
//! Free functions over [`redb::ReadTransaction`] /
//! [`redb::WriteTransaction`]. Callers compose them inside their own
//! redb transactions — that matters when callers need multi-table
//! atomicity (HNSW insert + trigram write + this CRUD in a single txn).
//!
//! ## Out of scope
//!
//! - Trigram index writes.
//! - Entity HNSW writes.
//! - Merge / unmerge.
//! - Wire protocol.
//! - Resolver (consumes the read paths here).
//!
//! ## Atomicity
//!
//! Every write function operates inside the caller-supplied
//! `WriteTransaction`. A single redb transaction therefore covers:
//!
//! - The primary row in [`ENTITIES_TABLE`].
//! - The exact-name index ([`ENTITY_BY_CANONICAL_NAME_TABLE`]).
//! - The alias index ([`ENTITY_ALIASES_TABLE`]).
//!
//! Callers must call `wtxn.commit()` themselves.

use std::collections::HashSet;

use brain_core::{Entity, EntityId, EntityTypeId};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::entity::{
    flags, EntityMetadata, ENTITIES_TABLE, ENTITY_ALIASES_TABLE, ENTITY_BY_CANONICAL_NAME_TABLE,
    ENTITY_VECTORS_TABLE, ENTITY_VECTOR_BYTES,
};
use crate::tables::entity_type::ENTITY_TYPES_TABLE;
use crate::tables::scope::RowScope;

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// Errors from the entity CRUD layer.
#[derive(thiserror::Error, Debug)]
pub enum EntityOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("entity {0:?} not found")]
    NotFound(EntityId),

    /// The `merged_into` redirect chain starting at this entity exceeded
    /// [`MERGE_REDIRECT_MAX_HOPS`] — a corrupted cycle. Fail-stop.
    #[error("entity {0:?} merge-redirect chain exceeds the hop cap (cycle?)")]
    MergeRedirectCycle(EntityId),

    #[error("entity type {0:?} is not registered")]
    UnknownEntityType(EntityTypeId),

    #[error(
        "duplicate canonical_name {name:?} for entity_type {type_id:?}; existing id {existing:?}"
    )]
    DuplicateCanonicalName {
        type_id: EntityTypeId,
        name: String,
        existing: EntityId,
    },

    /// Trigram index write/read failure. Forwarded from
    /// [`crate::entity::trigram::TrigramOpError`] when entity_put / update /
    /// tombstone touches the trigram index transactionally.
    #[error("trigram op: {0}")]
    TrigramOp(#[from] super::trigram::TrigramOpError),
}

// ---------------------------------------------------------------------------
// Normalization.
// ---------------------------------------------------------------------------

/// Normalize a name for indexing.
///
/// 1. `trim()` leading/trailing whitespace.
/// 2. `to_lowercase()` — Unicode-aware via the Rust stdlib.
/// 3. Collapse any internal whitespace run (spaces / tabs / newlines)
///    to a single ASCII space.
/// 4. Strip a leading English determiner (`the / a / an / this /
///    that`). LLM extractors routinely emit `"the customer support
///    team"`, `"the Phoenix project"`, etc.; stripping the article
///    folds those into the same canonical key as the bare form so
///    repeated extractions converge on one EntityId.
///
/// Idempotent: `normalize_name(normalize_name(s)) == normalize_name(s)`.
#[must_use]
pub fn normalize_name(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    // Fold to canonical composition (NFC) FIRST so a precomposed "São Paulo"
    // (`ã` = U+00E3) and a decomposed one (`a` + U+0303) produce the same key
    // — otherwise the same real-world name splits into duplicate entities
    // depending on the client's input method. Casefolding follows.
    let collapsed: String = s
        .nfc()
        .collect::<String>()
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    strip_leading_determiner(&collapsed).to_string()
}

/// Strip a leading English determiner from a lowercase normalized
/// name. Returns the input unchanged when no determiner matches or
/// when the residue would be empty (a bare `"the"` stays as
/// `"the"` rather than collapsing to an empty key).
fn strip_leading_determiner(s: &str) -> &str {
    const ARTICLES: &[&str] = &["the ", "a ", "an ", "this ", "that "];
    for art in ARTICLES {
        if let Some(rest) = s.strip_prefix(art) {
            // Don't return an empty key — `normalize_name` must
            // remain a total function whose output is non-empty
            // whenever the input has any non-whitespace content.
            if !rest.is_empty() {
                return rest;
            }
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Read paths.
// ---------------------------------------------------------------------------

/// Fetch an entity by id. Returns `None` if the row doesn't exist.
///
/// This is the RAW lookup: a merged entity is returned as-is (with
/// `merged_into = Some(survivor)`). Callers that want reads to transparently
/// follow the merge redirect use [`entity_get_resolved`].
pub fn entity_get(rtxn: &ReadTransaction, id: EntityId) -> Result<Option<Entity>, EntityOpError> {
    let t = rtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    Ok(row.as_ref().map(Entity::from))
}

/// Hop cap for [`entity_get_resolved`]'s merge-chain walk. A merge chain
/// (`A → B → C → …`) is bounded in practice by the number of merges, but
/// the walk defends against a malformed cycle by stopping here.
pub const MERGE_REDIRECT_MAX_HOPS: usize = 16;

/// Fetch an entity by id, transparently following the `merged_into`
/// redirect to the surviving entity.
///
/// After `merge_entity(survivor, merged)`, a read through `merged`'s id
/// must return the survivor (the merged row is a redirect, not a live
/// entity). Multi-hop chains (`A → B → C`) are collapsed at read time:
/// this walks `merged_into` until it reaches a live row and returns that.
///
/// Returns `Ok(None)` if the starting id has no row. Returns
/// [`EntityOpError::MergeRedirectCycle`] if the chain exceeds
/// [`MERGE_REDIRECT_MAX_HOPS`] (a corrupted cycle) — fail-stop rather than
/// loop forever or silently return a mid-chain node.
pub fn entity_get_resolved(
    rtxn: &ReadTransaction,
    id: EntityId,
) -> Result<Option<Entity>, EntityOpError> {
    // One walk; the chain is discarded here. Callers that need the audit
    // trail of redirect hops use [`entity_get_resolved_with_chain`].
    Ok(entity_get_resolved_with_chain(rtxn, id)?.map(|(entity, _chain)| entity))
}

/// Fetch an entity by id, following the `merged_into` redirect to the
/// surviving entity, and additionally return the **chain of redirect hops**
/// traversed to get there — the audit trail the §Multi-hop merge spec
/// requires (`entity_get(A)` on `A → B → C` returns `C`'s row "with an audit
/// trail showing both merges").
///
/// The returned `Vec<EntityId>` lists the redirect ids walked through, in
/// order, **excluding** the final survivor:
///
/// - live id (no merge): `[]` — the row is returned directly.
/// - `A → B` (get A): `[A]`.
/// - `A → B → C` (get A): `[A, B]`.
///
/// So the vec is exactly the set of ids that were redirected away; the
/// survivor's own id is the returned [`Entity`]'s `id`, not in the chain.
///
/// Returns `Ok(None)` if the starting id has no row. Returns
/// [`EntityOpError::MergeRedirectCycle`] if the chain exceeds
/// [`MERGE_REDIRECT_MAX_HOPS`] (a corrupted cycle) — fail-stop rather than
/// loop forever or silently return a mid-chain node.
pub fn entity_get_resolved_with_chain(
    rtxn: &ReadTransaction,
    id: EntityId,
) -> Result<Option<(Entity, Vec<EntityId>)>, EntityOpError> {
    let t = rtxn.open_table(ENTITIES_TABLE)?;
    let mut current = id;
    let mut chain: Vec<EntityId> = Vec::new();
    for _ in 0..=MERGE_REDIRECT_MAX_HOPS {
        let Some(row) = t.get(&current.to_bytes())?.map(|g| g.value()) else {
            return Ok(None);
        };
        match row.merged_into() {
            None => return Ok(Some((Entity::from(&row), chain))),
            Some(next) => {
                chain.push(current);
                current = next;
            }
        }
    }
    Err(EntityOpError::MergeRedirectCycle(id))
}

/// Tier-1 exact-match resolver lookup. Returns `Some(EntityId)` if a
/// row with the `(type, normalized(candidate))` pair exists, else
/// `None`. Performs normalization internally.
pub fn entity_lookup_by_canonical_name(
    rtxn: &ReadTransaction,
    scope: RowScope,
    type_id: EntityTypeId,
    candidate: &str,
) -> Result<Option<EntityId>, EntityOpError> {
    let normalized = normalize_name(candidate);
    let t = rtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
    let bytes: Option<[u8; 16]> = t
        .get(&(
            scope.namespace_id,
            scope.space_id_bytes,
            type_id.raw(),
            normalized.as_str(),
        ))?
        .map(|g| g.value());
    Ok(bytes.map(EntityId::from))
}

/// Resolve a free-text candidate to entities by exact canonical-name
/// match across **every** registered entity type. The canonical-name
/// index is keyed `(type_id, normalized_name)`, so a bare cue with no
/// type hint needs one point lookup per registered type — the registry
/// is small (a few hundred entries at most). Returns the distinct
/// EntityIds that matched (0, 1, or more); the caller decides how to
/// treat ambiguity. No fuzzy / alias matching — exact canonical only,
/// so this stays a high-precision resolver.
pub fn entity_resolve_canonical_all_types(
    rtxn: &ReadTransaction,
    scope: RowScope,
    candidate: &str,
) -> Result<Vec<EntityId>, EntityOpError> {
    let type_ids: Vec<u32> = {
        let types_t = rtxn.open_table(ENTITY_TYPES_TABLE)?;
        let mut v = Vec::new();
        for entry in types_t.iter()? {
            let (k, _) = entry?;
            v.push(k.value());
        }
        v
    };
    let mut out: Vec<EntityId> = Vec::new();
    for tid in type_ids {
        if let Some(id) =
            entity_lookup_by_canonical_name(rtxn, scope, EntityTypeId::from(tid), candidate)?
        {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

/// Minimum trigram-Jaccard a fuzzy candidate must clear to be offered by
/// [`entity_resolve_scored`]. Read-path resolution feeds a grounded answer,
/// so a wrong subject yields a wrong fact — the floor is deliberately
/// conservative (well above noise, below the write-path auto-merge bar) and
/// the caller still decides whether the top score is decisive.
pub const READ_RESOLVE_TRIGRAM_FLOOR: f32 = 0.5;

/// Read-only, confidence-scored "resolve this surface to an EXISTING entity"
/// helper — the primitive the read path needs to anchor a structured answer.
/// Unlike the write-path resolver it never mints; unlike
/// [`entity_resolve_canonical_all_types`] it returns a graded list so the
/// caller can tell a decisive match from an ambiguous one.
///
/// Tiers, highest confidence first (a given entity keeps only its best score):
///   1. exact canonical-name match  → `1.0`
///   2. alias match                 → `0.95`
///   3. trigram-Jaccard ≥ floor     → the Jaccard score itself
///
/// Embedding-based resolution (entity HNSW) is intentionally NOT here: the
/// in-RAM index lives outside the metadata layer, so the recall path layers
/// it on top of this deterministic redb core when it wants the extra tier.
///
/// Results are sorted by confidence descending; ties keep insertion order.
/// Bounded by `max_candidates` to cap the per-call trigram scan.
pub fn entity_resolve_scored(
    rtxn: &ReadTransaction,
    scope: RowScope,
    surface: &str,
    max_candidates: usize,
) -> Result<Vec<(EntityId, f32)>, EntityOpError> {
    use crate::entity::trigram::{candidates_for_query, jaccard, trigrams_of_entity};
    use brain_core::resolution::trigrams::extract_trigrams;

    let normalized = normalize_name(surface);
    if normalized.is_empty() {
        return Ok(Vec::new());
    }

    let type_ids: Vec<u32> = {
        let types_t = rtxn.open_table(ENTITY_TYPES_TABLE)?;
        let mut v = Vec::new();
        for entry in types_t.iter()? {
            let (k, _) = entry?;
            v.push(k.value());
        }
        v
    };

    // Best score per entity. A later, lower-tier hit never overwrites a
    // higher-tier one (we only raise).
    let mut best: std::collections::HashMap<EntityId, f32> = std::collections::HashMap::new();
    let raise = |id: EntityId, score: f32, best: &mut std::collections::HashMap<EntityId, f32>| {
        best.entry(id)
            .and_modify(|s| {
                if score > *s {
                    *s = score;
                }
            })
            .or_insert(score);
    };

    // Tier 1 + 2: exact canonical (1.0) and alias (0.95) across all types.
    for &tid in &type_ids {
        let type_id = EntityTypeId::from(tid);
        if let Some(id) = entity_lookup_by_canonical_name(rtxn, scope, type_id, surface)? {
            raise(id, 1.0, &mut best);
        }
        for id in entity_lookup_by_alias(rtxn, scope, type_id, surface)? {
            raise(id, 0.95, &mut best);
        }
    }

    // Tier 3: trigram-Jaccard over the fuzzy candidate union. Score each
    // candidate against the query's trigrams; keep those clearing the floor.
    //
    // Tier 3.5 (same scan): partial-name coref. A short cue ("Niraj") is a
    // partial reference to an entity stored under its full name ("Niraj
    // Georgian"). Trigram-Jaccard scores that pair below the floor — the longer
    // name dilutes the shared trigrams — so without this tier the short cue
    // never reaches the full entity's facts. We cannot lean on write-time coref
    // to have folded the short form in as an alias: independent extractor tiers
    // mint the short and full forms as separate nodes (sometimes under different
    // types), so the alias is often absent. A surface whose tokens are a STRICT
    // subset of a candidate's tokens is a partial name of it; we offer the full
    // entity only when EXACTLY ONE candidate is such a superset, since an
    // ambiguous partial ("John" ⊂ both "John Smith" and "John Doe") would
    // conflate distinct referents — decline rather than guess.
    const PARTIAL_NAME_SCORE: f32 = 0.9;
    let surface_tokens: HashSet<&str> = normalized
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .collect();
    let query_trigrams = extract_trigrams(&normalized);
    if !query_trigrams.is_empty() {
        let mut scanned: HashSet<EntityId> = HashSet::new();
        let mut partial_supersets: Vec<EntityId> = Vec::new();
        for &tid in &type_ids {
            let type_id = EntityTypeId::from(tid);
            for cand in candidates_for_query(rtxn, scope, type_id, &normalized)? {
                if !scanned.insert(cand) {
                    continue;
                }
                let Some(entity) = entity_get(rtxn, cand)? else {
                    continue;
                };
                let score = jaccard(&query_trigrams, &trigrams_of_entity(&entity));
                if score >= READ_RESOLVE_TRIGRAM_FLOOR {
                    raise(cand, score, &mut best);
                }
                if !surface_tokens.is_empty() {
                    let cand_norm = normalize_name(&entity.canonical_name);
                    let cand_tokens: HashSet<&str> = cand_norm
                        .split_whitespace()
                        .filter(|t| !t.is_empty())
                        .collect();
                    if surface_tokens.len() < cand_tokens.len()
                        && surface_tokens.iter().all(|t| cand_tokens.contains(t))
                    {
                        partial_supersets.push(cand);
                    }
                }
            }
        }
        if partial_supersets.len() == 1 {
            raise(partial_supersets[0], PARTIAL_NAME_SCORE, &mut best);
        }
    }

    let mut out: Vec<(EntityId, f32)> = best.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(max_candidates);
    Ok(out)
}

/// Wtxn-capable mirror of [`entity_resolve_canonical_all_types`]. Exact
/// canonical-name match across every registered entity type, evaluated inside
/// an open write transaction. Used by the extractor's coined-subject path to
/// reuse an already-typed entity (any type) instead of minting a duplicate
/// generic node — the caller treats a *single* hit as the same referent and
/// leaves 0-or-many to the normal type-scoped mint.
pub fn entity_resolve_canonical_all_types_wtxn(
    wtxn: &WriteTransaction,
    scope: RowScope,
    candidate: &str,
) -> Result<Vec<EntityId>, EntityOpError> {
    let normalized = normalize_name(candidate);
    if normalized.is_empty() {
        return Ok(Vec::new());
    }
    let type_ids: Vec<u32> = {
        let types_t = wtxn.open_table(ENTITY_TYPES_TABLE)?;
        let mut v = Vec::new();
        for entry in types_t.iter()? {
            let (k, _) = entry?;
            v.push(k.value());
        }
        v
    };
    let canon_t = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
    let mut out: Vec<EntityId> = Vec::new();
    for tid in type_ids {
        if let Some(g) = canon_t.get(&(
            scope.namespace_id,
            scope.space_id_bytes,
            tid,
            normalized.as_str(),
        ))? {
            let id = EntityId::from(g.value());
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

/// Alias lookup. Returns every EntityId whose alias set contains
/// `normalize_name(candidate)` under `type_id`. Multi-value index
/// (— "the same alias maps to entities of different
/// types" plus within-type duplicates).
pub fn entity_lookup_by_alias(
    rtxn: &ReadTransaction,
    scope: RowScope,
    type_id: EntityTypeId,
    candidate: &str,
) -> Result<Vec<EntityId>, EntityOpError> {
    let normalized = normalize_name(candidate);
    let t = rtxn.open_table(ENTITY_ALIASES_TABLE)?;
    let lo = (
        scope.namespace_id,
        scope.space_id_bytes,
        type_id.raw(),
        normalized.as_str(),
        [0u8; 16],
    );
    let hi = (
        scope.namespace_id,
        scope.space_id_bytes,
        type_id.raw(),
        normalized.as_str(),
        [0xFFu8; 16],
    );
    let mut out = Vec::new();
    for entry in t.range(lo..=hi)? {
        let (k, _) = entry?;
        let (k_ns, k_space, k_type, k_alias, k_id) = k.value();
        // Defensive guard: range bounds carry the same scope+type+alias,
        // so any entry inside the range must match all. Skip otherwise to
        // be robust against future key-shape changes.
        if k_ns == scope.namespace_id
            && k_space == scope.space_id_bytes
            && k_type == type_id.raw()
            && k_alias == normalized
        {
            out.push(EntityId::from(k_id));
        }
    }
    Ok(out)
}

/// Scan all entities of a given type. O(N) over the primary table;
/// caller bears the cost. Paginated/filtered variants (`name_prefix`,
/// `mention_count_min`) can be layered on later; this is the simplest
/// form.
pub fn entity_list_by_type(
    rtxn: &ReadTransaction,
    scope: RowScope,
    type_id: EntityTypeId,
) -> Result<Vec<Entity>, EntityOpError> {
    let t = rtxn.open_table(ENTITIES_TABLE)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        let m = v.value();
        // Tenant wall (unconditional): a list never crosses the caller's
        // `(namespace, space)`. The primary table is a flat keyspace
        // shared across tenants, so the scope check is what isolates it.
        if m.namespace_id == scope.namespace_id
            && m.space_id_bytes == scope.space_id_bytes
            && m.entity_type_id == type_id.raw()
        {
            out.push((&m).into());
        }
    }
    Ok(out)
}

/// Scan every live (non-tombstoned) entity, returning
/// `(EntityId, canonical_name)`.
///
/// The entity HNSW (resolver tier-3 embedding tie-break) is in-RAM only
/// and not persisted, so on restart it must be rebuilt from the metadata
/// store. This is that rebuild source: the resolver inserts
/// `embed(canonical_name)` at entity-create, so re-embedding each
/// returned name reproduces the stored vectors exactly. O(N) over the
/// primary table, paid once per boot.
pub fn entity_iter_all_live(
    rtxn: &ReadTransaction,
) -> Result<Vec<(EntityId, String)>, EntityOpError> {
    let t = rtxn.open_table(ENTITIES_TABLE)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (k, v) = entry?;
        let m = v.value();
        if m.flags & flags::TOMBSTONED != 0 {
            continue;
        }
        out.push((EntityId::from(k.value()), m.canonical_name));
    }
    Ok(out)
}

/// One live entity as the entity-GC sweeper needs it:
/// `(EntityId, owning scope, created_at_unix_nanos)`. The scope +
/// created-at are the columns the sweeper tests for grace-window +
/// inbound-reference eligibility without re-opening the primary row.
pub type EntityGcCandidate = (EntityId, RowScope, u64);

/// Scan every live (non-tombstoned) entity, yielding the columns the
/// entity-GC sweeper needs to test eligibility. O(N) over the primary
/// table; the sweeper runs daily and off by default, so the full scan
/// is acceptable (mirrors [`entity_iter_all_live`]).
pub fn entity_iter_live_for_gc(
    rtxn: &ReadTransaction,
) -> Result<Vec<EntityGcCandidate>, EntityOpError> {
    let t = rtxn.open_table(ENTITIES_TABLE)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        let m = v.value();
        if m.flags & flags::TOMBSTONED != 0 {
            continue;
        }
        out.push((
            m.entity_id(),
            RowScope::from_bytes(m.namespace_id, m.space_id_bytes),
            m.created_at_unix_nanos,
        ));
    }
    Ok(out)
}

/// Count inbound references to `entity_id` within `scope`, returning as
/// soon as the running sum exceeds zero.
///
/// Inbound references are summed across four sources, in cheapest-first
/// order so the common case (a referenced entity) exits on the first
/// hit:
/// 1. active statements whose **subject** is the entity
///    ([`STATEMENTS_BY_SUBJECT_TABLE`] range for the scope + entity);
/// 2. relations **from** the entity ([`relation_list_from`]);
/// 3. relations **to** the entity ([`relation_list_to`]);
/// 4. entity **mentions** ([`ENTITY_MENTIONS_TABLE`] range).
///
/// Anti-flap: this counts ANY present inbound row — active OR
/// tombstoned-but-not-yet-reclaimed. A reclaimed row is already
/// physically gone, so `== 0` means the entity is truly orphaned and safe
/// to tombstone; a row that is tombstoned-within-grace still counts,
/// because it may yet be reverted and pointing at a GC'd entity would
/// orphan it. The return value is therefore a lower bound on the true
/// reference count — exact only when it is `0`, which is all the caller
/// needs (eligibility is `== 0`).
///
/// The `scope` prefix bounds every range to one `(namespace, space)`, so
/// the count can never observe another tenant's rows.
pub fn entity_inbound_reference_count(
    rtxn: &ReadTransaction,
    scope: RowScope,
    entity_id: EntityId,
) -> Result<u64, crate::relation::ops::RelationOpError> {
    use crate::relation::ops::{relation_list_from, relation_list_to, RelationListFilter};
    use crate::tables::entity::ENTITY_MENTIONS_TABLE;
    use crate::tables::statement::STATEMENTS_BY_SUBJECT_TABLE;

    let ns = scope.namespace_id;
    let sp = scope.space_id_bytes;
    let eid = entity_id.to_bytes();

    // 1. Statements where subject == entity. The subject-anchored index
    // key is `(ns, space, subject, kind, predicate_id, is_current,
    // statement_id)`; range the whole `(kind, predicate, is_current,
    // statement)` suffix for this scope + subject and stop on the first
    // present row.
    {
        let t = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
        let lo = (ns, sp, eid, 0u8, 0u32, 0u8, [0u8; 16]);
        let hi = (ns, sp, eid, u8::MAX, u32::MAX, 1u8, [0xffu8; 16]);
        if t.range(lo..=hi)?.next().is_some() {
            return Ok(1);
        }
    }

    // 2 + 3. Relations from / to the entity, via the unified edge table.
    // The default filter keeps `current_only = false`, so tombstoned-
    // within-grace relations still count (anti-flap).
    let filter = RelationListFilter::default();
    if !relation_list_from(rtxn, scope, entity_id, &filter)?.is_empty() {
        return Ok(1);
    }
    if !relation_list_to(rtxn, scope, entity_id, &filter)?.is_empty() {
        return Ok(1);
    }

    // 4. Entity mentions. Key: `(ns, space, entity, memory)`.
    {
        let t = rtxn.open_table(ENTITY_MENTIONS_TABLE)?;
        let lo = (ns, sp, eid, [0u8; 16]);
        let hi = (ns, sp, eid, [0xffu8; 16]);
        if t.range(lo..=hi)?.next().is_some() {
            return Ok(1);
        }
    }

    Ok(0)
}

/// Little-endian byte image of an entity vector. Safe, no-unsafe
/// conversion (the arena's `bytemuck::Pod` cast lives in `brain-storage`,
/// the one crate that's allowed `unsafe`). Compile-time array sizing
/// keeps the dimensionality honest.
fn vector_to_bytes(vector: &[f32; 384]) -> [u8; ENTITY_VECTOR_BYTES] {
    let mut out = [0u8; ENTITY_VECTOR_BYTES];
    for (i, v) in vector.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

/// Inverse of [`vector_to_bytes`]. Reads 384 little-endian f32s out of
/// a 1536-byte image.
fn bytes_to_vector(bytes: &[u8; ENTITY_VECTOR_BYTES]) -> [f32; 384] {
    let mut out = [0.0f32; 384];
    for (i, slot) in out.iter_mut().enumerate() {
        let chunk: [u8; 4] = bytes[i * 4..(i + 1) * 4]
            .try_into()
            .expect("invariant: fixed slice");
        *slot = f32::from_le_bytes(chunk);
    }
    out
}

/// Persist an entity's embedding vector at write time. Stores the
/// little-endian byte image of the f32 array; the table's fixed-size
/// value enforces the dimensionality. Idempotent: upserts on the
/// EntityId key, so a re-resolved entity overwrites with the same vector.
///
/// Stored vectors let restart skip the synchronous re-embed of
/// canonical names, turning the entity HNSW rebuild from O(N
/// inferences) into O(N redb reads).
pub fn entity_vector_put(
    wtxn: &WriteTransaction,
    id: EntityId,
    vector: &[f32; 384],
) -> Result<(), EntityOpError> {
    let bytes = vector_to_bytes(vector);
    let mut t = wtxn.open_table(ENTITY_VECTORS_TABLE)?;
    t.insert(&id.to_bytes(), &bytes)?;
    Ok(())
}

/// Read a persisted entity vector. Returns `Ok(None)` when the row is
/// absent (entity predates the feature, or its vector hasn't been
/// written yet) — callers fall back to re-embedding.
pub fn entity_vector_get(
    rtxn: &ReadTransaction,
    id: EntityId,
) -> Result<Option<[f32; 384]>, EntityOpError> {
    let t = rtxn.open_table(ENTITY_VECTORS_TABLE)?;
    let row = t.get(&id.to_bytes())?;
    Ok(row.map(|g| bytes_to_vector(&g.value())))
}

/// One row yielded by [`entity_iter_all_live_with_vectors`]:
/// `(EntityId, canonical_name, Option<vector>)`. A `Some` vector goes
/// straight into the HNSW at restart; a `None` triggers re-embed.
pub type EntityRebuildRow = (EntityId, String, Option<[f32; 384]>);

/// Iterate every live entity, returning `(EntityId, canonical_name,
/// Option<vector>)`. The startup rebuild uses this to drive the entity
/// HNSW from durable vectors: rows whose vector is `Some` go straight
/// into the index without an embedder call; rows whose vector is
/// `None` (pre-feature data, or a partial write) fall back to
/// re-embedding the canonical name.
pub fn entity_iter_all_live_with_vectors(
    rtxn: &ReadTransaction,
) -> Result<Vec<EntityRebuildRow>, EntityOpError> {
    let entities = rtxn.open_table(ENTITIES_TABLE)?;
    let vectors = rtxn.open_table(ENTITY_VECTORS_TABLE)?;
    let mut out = Vec::new();
    for entry in entities.iter()? {
        let (k, v) = entry?;
        let m = v.value();
        if m.flags & flags::TOMBSTONED != 0 {
            continue;
        }
        let id_bytes = k.value();
        let vector = vectors.get(&id_bytes)?.map(|g| bytes_to_vector(&g.value()));
        out.push((EntityId::from(id_bytes), m.canonical_name, vector));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Write paths.
// ---------------------------------------------------------------------------

/// Insert a new entity. Writes the primary row + exact-name index +
/// one alias-index row per `entity.aliases` entry.
///
/// Errors:
/// - [`EntityOpError::UnknownEntityType`] if `entity.entity_type` is
///   not present in `entity_types`.
/// - [`EntityOpError::DuplicateCanonicalName`] if `(entity_type,
///   normalize_name(canonical_name))` already maps to an existing
///   EntityId.
///
/// Does NOT write trigrams or HNSW embedding.
pub fn entity_put(
    wtxn: &WriteTransaction,
    scope: RowScope,
    session: brain_core::SessionId,
    entity: &Entity,
) -> Result<(), EntityOpError> {
    require_entity_type_exists(wtxn, entity.entity_type)?;

    let normalized = normalize_name(&entity.canonical_name);
    // Reject duplicate canonical_name within the same (scope, type). The
    // index is keyed single-value PER SCOPE; the same name under a
    // different `(namespace, space)` is a distinct entity, not a
    // collision.
    {
        let t = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
        let existing: Option<[u8; 16]> = t
            .get(&(
                scope.namespace_id,
                scope.space_id_bytes,
                entity.entity_type.raw(),
                normalized.as_str(),
            ))?
            .map(|g| g.value());
        if let Some(bytes) = existing {
            return Err(EntityOpError::DuplicateCanonicalName {
                type_id: entity.entity_type,
                name: normalized,
                existing: EntityId::from(bytes),
            });
        }
    }

    // Primary row — carries the owning scope. `entity_put` only ever
    // CREATES (it rejects a duplicate canonical above), so the session it
    // stamps is this entity's FIRST-MENTION provenance. Later mentions
    // route through `entity_update`, which preserves this value — entity
    // identity is session-agnostic and the session is never overwritten.
    let mut m = EntityMetadata::from_entity(entity, scope);
    m.session_id = session.raw();
    // Make sure the on-disk normalized_name matches what we just
    // computed (the caller may have passed a different form;
    // normalize is canonical).
    m.normalized_name = normalized.clone();
    {
        let mut t = wtxn.open_table(ENTITIES_TABLE)?;
        t.insert(&m.entity_id_bytes, &m)?;
    }

    // Exact-name index.
    {
        let mut t = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
        t.insert(
            &(
                scope.namespace_id,
                scope.space_id_bytes,
                entity.entity_type.raw(),
                normalized.as_str(),
            ),
            &m.entity_id_bytes,
        )?;
    }

    // Alias index — one row per alias, normalized.
    if !entity.aliases.is_empty() {
        let mut t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
        for alias in &entity.aliases {
            let na = normalize_name(alias);
            t.insert(
                &(
                    scope.namespace_id,
                    scope.space_id_bytes,
                    entity.entity_type.raw(),
                    na.as_str(),
                    m.entity_id_bytes,
                ),
                &(),
            )?;
        }
    }

    // Trigram index. Union of canonical_name + every alias contributes
    // to the entity's trigram set.
    let trigrams =
        crate::entity::trigram::trigrams_of_components(&entity.canonical_name, &entity.aliases);
    crate::entity::trigram::index_entity_trigrams(
        wtxn,
        scope,
        entity.entity_type,
        entity.id,
        &trigrams,
    )?;

    Ok(())
}

/// Read-modify-write of an existing entity.
///
/// The caller passes the **desired new state** as `new_state`. This
/// function:
///
/// 1. Loads the current row (errors if absent).
/// 2. If `canonical_name` changed:
///    - Removes the old `entity_by_canonical_name` entry.
///    - Adds a new one (errors if collision).
///    - Moves the old canonical_name into `aliases` (dedup).
///    - Bumps `embedding_version` (the re-embed worker picks this up).
/// 3. Computes the alias delta between `current.aliases` and
///    `new_state.aliases`; removes / adds rows in the alias index.
/// 4. Sets `updated_at_unix_nanos = now_unix_nanos`.
/// 5. Writes the primary row back.
pub fn entity_update(
    wtxn: &WriteTransaction,
    new_state: &Entity,
    now_unix_nanos: u64,
) -> Result<(), EntityOpError> {
    let current = read_entity_inside_wtxn(wtxn, new_state.id)?
        .ok_or(EntityOpError::NotFound(new_state.id))?;

    require_entity_type_exists(wtxn, new_state.entity_type)?;

    // The entity's owning scope is immutable; reuse the one stamped on
    // the existing row so the rewritten secondary-index keys stay in the
    // same tenant keyspace (an update can never re-home a row).
    let scope = current.scope();

    let mut next = new_state.clone();
    next.updated_at_unix_nanos = now_unix_nanos;

    let normalized_old = normalize_name(&current.canonical_name);
    let normalized_new = normalize_name(&next.canonical_name);
    let canonical_changed = normalized_old != normalized_new;

    if canonical_changed {
        // Old canonical_name moves into aliases. The constructor
        // form takes the raw name; we dedupe on the normalized form
        // (the alias index keys on normalized).
        let na_old = normalize_name(&current.canonical_name);
        if !next.aliases.iter().any(|a| normalize_name(a) == na_old) {
            next.aliases.push(current.canonical_name.clone());
        }
        next.embedding_version = current.embedding_version + 1;

        // Update canonical-name index.
        let mut t = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
        t.remove(&(
            scope.namespace_id,
            scope.space_id_bytes,
            current.entity_type_id,
            normalized_old.as_str(),
        ))?;
        let existing: Option<[u8; 16]> = t
            .get(&(
                scope.namespace_id,
                scope.space_id_bytes,
                next.entity_type.raw(),
                normalized_new.as_str(),
            ))?
            .map(|g| g.value());
        if let Some(bytes) = existing {
            return Err(EntityOpError::DuplicateCanonicalName {
                type_id: next.entity_type,
                name: normalized_new,
                existing: EntityId::from(bytes),
            });
        }
        t.insert(
            &(
                scope.namespace_id,
                scope.space_id_bytes,
                next.entity_type.raw(),
                normalized_new.as_str(),
            ),
            &next.id.to_bytes(),
        )?;
    }

    // Alias delta. Compare on normalized forms.
    let old_norms: HashSet<String> = current.aliases.iter().map(|a| normalize_name(a)).collect();
    let new_norms: HashSet<String> = next.aliases.iter().map(|a| normalize_name(a)).collect();

    {
        let mut t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
        for removed in old_norms.difference(&new_norms) {
            t.remove(&(
                scope.namespace_id,
                scope.space_id_bytes,
                current.entity_type_id,
                removed.as_str(),
                current.entity_id_bytes,
            ))?;
        }
        for added in new_norms.difference(&old_norms) {
            t.insert(
                &(
                    scope.namespace_id,
                    scope.space_id_bytes,
                    next.entity_type.raw(),
                    added.as_str(),
                    next.id.to_bytes(),
                ),
                &(),
            )?;
        }
    }

    // Trigram delta. The entity's old trigram set is
    // derived from current.canonical_name + current.aliases; the new
    // set from next.canonical_name + next.aliases. Remove `old - new`,
    // add `new - old`.
    let old_trigrams =
        crate::entity::trigram::trigrams_of_components(&current.canonical_name, &current.aliases);
    let new_trigrams =
        crate::entity::trigram::trigrams_of_components(&next.canonical_name, &next.aliases);
    let to_remove: std::collections::HashSet<[u8; 3]> =
        old_trigrams.difference(&new_trigrams).copied().collect();
    let to_add: std::collections::HashSet<[u8; 3]> =
        new_trigrams.difference(&old_trigrams).copied().collect();
    crate::entity::trigram::remove_entity_trigrams(
        wtxn,
        scope,
        current.entity_type(),
        current.entity_id(),
        &to_remove,
    )?;
    crate::entity::trigram::index_entity_trigrams(wtxn, scope, next.entity_type, next.id, &to_add)?;

    // Write back primary row — re-stamp the immutable owning scope AND
    // preserve the entity's FIRST-MENTION session: an update is a later
    // mention and must never overwrite the session that first created the
    // entity (entity identity is session-agnostic).
    let mut m = EntityMetadata::from_entity(&next, scope);
    m.session_id = current.session_id;
    m.normalized_name = normalized_new;
    {
        let mut t = wtxn.open_table(ENTITIES_TABLE)?;
        t.insert(&m.entity_id_bytes, &m)?;
    }

    Ok(())
}

/// Convenience: rename without recomputing the rest of the entity.
/// Loads the entity, replaces `canonical_name`, dispatches through
/// [`entity_update`].
pub fn entity_rename(
    wtxn: &WriteTransaction,
    id: EntityId,
    new_canonical_name: String,
    now_unix_nanos: u64,
) -> Result<(), EntityOpError> {
    let current = read_entity_inside_wtxn(wtxn, id)?.ok_or(EntityOpError::NotFound(id))?;
    let mut next: Entity = (&current).into();
    next.canonical_name = new_canonical_name;
    entity_update(wtxn, &next, now_unix_nanos)
}

/// Add a single alias (deduplicating on the normalized form). No-op
/// if the alias is already present.
pub fn entity_add_alias(
    wtxn: &WriteTransaction,
    id: EntityId,
    alias: String,
    now_unix_nanos: u64,
) -> Result<(), EntityOpError> {
    let current = read_entity_inside_wtxn(wtxn, id)?.ok_or(EntityOpError::NotFound(id))?;
    let na_new = normalize_name(&alias);
    if current.aliases.iter().any(|a| normalize_name(a) == na_new) {
        return Ok(());
    }
    let mut next: Entity = (&current).into();
    next.aliases.push(alias);
    entity_update(wtxn, &next, now_unix_nanos)
}

/// Remove a single alias by raw string (callers compare on the
/// normalized form). No-op if the alias is absent.
pub fn entity_remove_alias(
    wtxn: &WriteTransaction,
    id: EntityId,
    alias: &str,
    now_unix_nanos: u64,
) -> Result<(), EntityOpError> {
    let current = read_entity_inside_wtxn(wtxn, id)?.ok_or(EntityOpError::NotFound(id))?;
    let na_target = normalize_name(alias);
    let mut next: Entity = (&current).into();
    let before = next.aliases.len();
    next.aliases.retain(|a| normalize_name(a) != na_target);
    if next.aliases.len() == before {
        return Ok(()); // alias not present
    }
    entity_update(wtxn, &next, now_unix_nanos)
}

/// Tombstone an entity. Tears down the secondary indexes so the
/// resolver never sees the row again, sets `flags::TOMBSTONED`, and
/// keeps the primary record for audit / unmerge.
pub fn entity_tombstone(
    wtxn: &WriteTransaction,
    id: EntityId,
    now_unix_nanos: u64,
) -> Result<(), EntityOpError> {
    let current = read_entity_inside_wtxn(wtxn, id)?.ok_or(EntityOpError::NotFound(id))?;
    // Tear down the SAME scoped keys that `entity_put` / `entity_update`
    // wrote — the owning scope is on the row.
    let scope = current.scope();

    // Tear down exact-name index.
    let normalized = normalize_name(&current.canonical_name);
    {
        let mut t = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
        t.remove(&(
            scope.namespace_id,
            scope.space_id_bytes,
            current.entity_type_id,
            normalized.as_str(),
        ))?;
    }
    // Tear down alias index (one row per alias).
    {
        let mut t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
        for alias in &current.aliases {
            let na = normalize_name(alias);
            t.remove(&(
                scope.namespace_id,
                scope.space_id_bytes,
                current.entity_type_id,
                na.as_str(),
                current.entity_id_bytes,
            ))?;
        }
    }
    // Tear down trigram index (one row per trigram in the entity's
    // union set).
    {
        let trigrams = crate::entity::trigram::trigrams_of_components(
            &current.canonical_name,
            &current.aliases,
        );
        crate::entity::trigram::remove_entity_trigrams(
            wtxn,
            scope,
            current.entity_type(),
            current.entity_id(),
            &trigrams,
        )?;
    }
    // Update primary row with tombstone flag + timestamp.
    let mut next = current;
    next.flags |= flags::TOMBSTONED;
    next.updated_at_unix_nanos = now_unix_nanos;
    next.aliases.clear();
    {
        let mut t = wtxn.open_table(ENTITIES_TABLE)?;
        t.insert(&next.entity_id_bytes, &next)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Read the current `EntityMetadata` row for `id` inside a write
/// transaction. Returns `None` if the row doesn't exist.
fn read_entity_inside_wtxn(
    wtxn: &WriteTransaction,
    id: EntityId,
) -> Result<Option<EntityMetadata>, EntityOpError> {
    let t = wtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    Ok(row)
}

/// Read an Entity row inside a write transaction. The wtxn-scoped
/// counterpart to [`entity_get`] — apply functions need this because
/// they receive the wtxn but mustn't open a separate read transaction.
pub fn entity_get_inside_wtxn(
    wtxn: &WriteTransaction,
    id: EntityId,
) -> Result<Option<Entity>, EntityOpError> {
    Ok(read_entity_inside_wtxn(wtxn, id)?
        .as_ref()
        .map(Entity::from))
}

/// Verify `type_id` is present in the `entity_types` registry.
/// Returns `UnknownEntityType` if not.
fn require_entity_type_exists(
    wtxn: &WriteTransaction,
    type_id: EntityTypeId,
) -> Result<(), EntityOpError> {
    let t = wtxn.open_table(ENTITY_TYPES_TABLE)?;
    if t.get(&type_id.raw())?.is_none() {
        return Err(EntityOpError::UnknownEntityType(type_id));
    }
    Ok(())
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

    const NOW: u64 = 1_700_000_000_000_000_000;
    const LATER: u64 = NOW + 60_000_000_000; // +1 minute

    fn test_scope() -> RowScope {
        RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
    }

    fn db_path(dir: &TempDir) -> PathBuf {
        dir.path().join("metadata.redb")
    }

    /// Open a fresh `MetadataDb` (which seeds the Person type at id=1
    /// via the system-schema bootstrap).
    fn fresh_db(dir: &TempDir) -> MetadataDb {
        MetadataDb::open(db_path(dir)).expect("open")
    }

    fn person_entity(canonical: &str) -> Entity {
        Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            canonical.to_owned(),
            normalize_name(canonical),
            NOW,
        )
    }

    // ----- normalize_name ------------------------------------------------

    #[test]
    fn normalize_lowercases_and_collapses() {
        assert_eq!(normalize_name("  Priya   Patel  "), "priya patel");
        assert_eq!(normalize_name("PRIYA"), "priya");
        assert_eq!(normalize_name("Priya\tPatel"), "priya patel");
        assert_eq!(normalize_name("Priya\n\nPatel"), "priya patel");
        assert_eq!(normalize_name(""), "");
        assert_eq!(normalize_name("   "), "");
    }

    #[test]
    fn normalize_handles_unicode() {
        // German ß lowercases to ss; `to_lowercase()` is Unicode-aware.
        assert_eq!(normalize_name("Straße"), "straße");
        // CJK passes through (no case mapping).
        assert_eq!(normalize_name("田中"), "田中");
    }

    #[test]
    fn normalize_folds_nfc_composed_and_decomposed() {
        // "São Paulo" with a precomposed ã (U+00E3) and with a decomposed
        // a + combining tilde (U+0303) must produce the SAME key — otherwise
        // the same city splits into two entities depending on input method.
        let composed = "S\u{00E3}o Paulo";
        let decomposed = "Sa\u{0303}o Paulo";
        assert_ne!(composed, decomposed, "byte-distinct inputs");
        assert_eq!(normalize_name(composed), normalize_name(decomposed));
        // Same for an accented Latin name and a precomposed/decomposed é.
        assert_eq!(
            normalize_name("Jos\u{00E9}"),
            normalize_name("Jose\u{0301}")
        );
    }

    #[test]
    fn cross_type_wtxn_reuses_single_match_only() {
        // A coined surface that already exists as exactly one typed entity
        // resolves to it (no duplicate); 0 or >1 matches return nothing so the
        // caller mints under the generic type instead of risking a wrong merge.
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let aspirin = person_entity("aspirin"); // stand-in for a typed entity
        let id = aspirin.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(
                &wtxn,
                test_scope(),
                brain_core::SessionId::DEFAULT,
                &aspirin,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        let wtxn = db.write_txn().unwrap();
        let hits = entity_resolve_canonical_all_types_wtxn(&wtxn, test_scope(), "Aspirin").unwrap();
        assert_eq!(hits, vec![id], "single cross-type match is reused");
        let none =
            entity_resolve_canonical_all_types_wtxn(&wtxn, test_scope(), "ibuprofen").unwrap();
        assert!(none.is_empty(), "no match → caller mints fresh");
    }

    #[test]
    fn normalize_is_idempotent() {
        for s in ["Priya Patel", "  HELLO ", "Straße", "x", ""] {
            let once = normalize_name(s);
            let twice = normalize_name(&once);
            assert_eq!(once, twice);
        }
    }

    // ----- entity_put + entity_get ---------------------------------------

    #[test]
    fn entity_put_then_get_round_trips() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Priya Patel");
        let id = e.id;

        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().expect("present");
        assert_eq!(got, e);
    }

    // ----- entity_get_resolved_with_chain (merge audit trail) ------------

    #[test]
    fn get_resolved_with_chain_returns_redirect_hops() {
        // A → B → C. get_resolved_with_chain(A) returns C plus the redirect
        // trail [A, B] (the survivor C's id is NOT in the chain).
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let mut a = person_entity("ChainA");
        let mut b = person_entity("ChainB");
        let c = person_entity("ChainC");
        a.merged_into = Some(b.id);
        b.merged_into = Some(c.id);
        let (aid, bid, cid) = (a.id, b.id, c.id);

        let wtxn = db.write_txn().unwrap();
        for e in [&a, &b, &c] {
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, e).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let (survivor, chain) = entity_get_resolved_with_chain(&rtxn, aid)
            .unwrap()
            .expect("present");
        assert_eq!(survivor.id, cid);
        assert_eq!(chain, vec![aid, bid]);

        // get on the mid-chain id B yields [B].
        let (survivor_b, chain_b) = entity_get_resolved_with_chain(&rtxn, bid)
            .unwrap()
            .expect("present");
        assert_eq!(survivor_b.id, cid);
        assert_eq!(chain_b, vec![bid]);
    }

    #[test]
    fn get_resolved_with_chain_empty_for_live_entity() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("LiveOne");
        let id = e.id;
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let (survivor, chain) = entity_get_resolved_with_chain(&rtxn, id)
            .unwrap()
            .expect("present");
        assert_eq!(survivor.id, id);
        assert!(chain.is_empty());
    }

    #[test]
    fn get_resolved_with_chain_detects_cycle() {
        // A malformed cycle A → B → A must fail-stop rather than loop.
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let mut a = person_entity("CycleA");
        let mut b = person_entity("CycleB");
        a.merged_into = Some(b.id);
        b.merged_into = Some(a.id);
        let aid = a.id;

        let wtxn = db.write_txn().unwrap();
        for e in [&a, &b] {
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, e).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        match entity_get_resolved_with_chain(&rtxn, aid) {
            Err(EntityOpError::MergeRedirectCycle(id)) => assert_eq!(id, aid),
            other => panic!("expected MergeRedirectCycle, got {other:?}"),
        }
    }

    #[test]
    fn resolve_canonical_all_types_finds_match_without_type_hint() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Priya Patel");
        let id = e.id;
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        // Resolves by canonical name across all types — no type hint, and
        // normalization folds the casing.
        let ids = entity_resolve_canonical_all_types(&rtxn, test_scope(), "priya patel").unwrap();
        assert_eq!(ids, vec![id]);
        // A name no entity carries resolves to nothing.
        let none = entity_resolve_canonical_all_types(&rtxn, test_scope(), "Nobody Here").unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn entity_put_writes_alias_index() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let mut e = person_entity("Priya Patel");
        e.aliases.push("priya".into());
        e.aliases.push("P. Patel".into()); // mixed case -> normalize
        e.aliases.push("priya p.".into());
        let id = e.id;

        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        for alias in ["priya", "p. patel", "priya p."] {
            let ids =
                entity_lookup_by_alias(&rtxn, test_scope(), EntityType::PERSON_ID, alias).unwrap();
            assert!(ids.contains(&id), "alias {alias:?} missing from index");
        }
    }

    #[test]
    fn entity_put_validates_entity_type_exists() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let mut e = person_entity("X");
        e.entity_type = EntityTypeId(99);

        let wtxn = db.write_txn().unwrap();
        let err = entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e)
            .expect_err("should reject");
        assert!(matches!(
            err,
            EntityOpError::UnknownEntityType(t) if t == EntityTypeId(99)
        ));
        wtxn.commit().unwrap();
    }

    #[test]
    fn entity_put_rejects_duplicate_canonical_name() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let a = person_entity("Priya Patel");
        let b_id = EntityId::new();
        let mut b = person_entity("Priya  Patel"); // normalizes to same
        b.id = b_id;

        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &a).unwrap();
        let err =
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &b).expect_err("dup");
        match err {
            EntityOpError::DuplicateCanonicalName {
                type_id,
                name,
                existing,
            } => {
                assert_eq!(type_id, EntityType::PERSON_ID);
                assert_eq!(name, "priya patel");
                assert_eq!(existing, a.id);
            }
            other => panic!("expected DuplicateCanonicalName, got {other:?}"),
        }
        wtxn.commit().unwrap();
    }

    // ----- lookups -------------------------------------------------------

    #[test]
    fn lookup_by_canonical_name_finds_inserted() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Priya Patel");
        let id = e.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        assert_eq!(
            entity_lookup_by_canonical_name(
                &rtxn,
                test_scope(),
                EntityType::PERSON_ID,
                "PRIYA  PATEL"
            )
            .unwrap(),
            Some(id),
            "lookup must normalize the candidate"
        );
        assert_eq!(
            entity_lookup_by_canonical_name(&rtxn, test_scope(), EntityType::PERSON_ID, "nope")
                .unwrap(),
            None
        );
    }

    #[test]
    fn lookup_by_alias_returns_multiple_ids_for_shared_alias() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        // Two entities with distinct canonical names share an alias.
        let mut a = person_entity("Priya Patel");
        a.aliases.push("Priya".into());
        let mut b = person_entity("Priya Singh");
        b.aliases.push("Priya".into());
        let (a_id, b_id) = (a.id, b.id);

        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &a).unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &b).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let mut ids =
            entity_lookup_by_alias(&rtxn, test_scope(), EntityType::PERSON_ID, "priya").unwrap();
        ids.sort();
        let mut expected = vec![a_id, b_id];
        expected.sort();
        assert_eq!(ids, expected);
    }

    // ----- scored resolution --------------------------------------------

    #[test]
    fn resolve_scored_grades_exact_alias_and_fuzzy() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let mut exact = person_entity("Priya Patel");
        exact.aliases.push("PP".into());
        let exact_id = exact.id;
        // A second entity whose name is a near-miss of the query surface,
        // sharing most trigrams ("priya parel" vs "priya patel").
        let fuzzy = person_entity("Priya Parel");
        let fuzzy_id = fuzzy.id;

        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &exact).unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &fuzzy).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();

        // Exact canonical → 1.0, ranked first.
        let scored = entity_resolve_scored(&rtxn, test_scope(), "Priya Patel", 8).unwrap();
        assert_eq!(scored.first().map(|(id, _)| *id), Some(exact_id));
        assert!((scored[0].1 - 1.0).abs() < f32::EPSILON);
        // The near-miss also surfaces via the trigram tier, below the exact.
        assert!(
            scored.iter().any(|(id, s)| *id == fuzzy_id && *s < 1.0),
            "fuzzy near-miss should surface below the exact match: {scored:?}"
        );

        // Alias resolves to 0.95.
        let by_alias = entity_resolve_scored(&rtxn, test_scope(), "PP", 8).unwrap();
        assert!(
            by_alias
                .iter()
                .any(|(id, s)| *id == exact_id && (*s - 0.95).abs() < f32::EPSILON),
            "alias hit should score 0.95: {by_alias:?}"
        );

        // A surface with no canonical/alias/trigram overlap resolves to nothing.
        assert!(entity_resolve_scored(&rtxn, test_scope(), "Zzxqwv", 8)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn resolve_scored_partial_name_maps_short_cue_to_full_entity() {
        // Fragmentation case: a person is stored under the full name ("Niraj
        // Georgian") while a cue says just "Niraj". Trigram-Jaccard scores that
        // pair below the fuzzy floor (the longer name dilutes the shared
        // trigrams), so only the partial-name tier reaches it — at 0.9.
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let full = person_entity("Niraj Georgian");
        let full_id = full.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &full).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let scored = entity_resolve_scored(&rtxn, test_scope(), "Niraj", 8).unwrap();
        let hit = scored.iter().find(|(id, _)| *id == full_id);
        assert!(
            hit.is_some(),
            "short cue 'Niraj' must resolve to 'Niraj Georgian': {scored:?}"
        );
        assert!(
            (hit.unwrap().1 - 0.9).abs() < f32::EPSILON,
            "partial-name tier scores 0.9: {scored:?}"
        );
    }

    #[test]
    fn resolve_scored_partial_name_declines_when_ambiguous() {
        // "John" is a strict token-subset of BOTH "John Smith" and "John Doe" —
        // two distinct people. The partial-name tier must decline rather than
        // conflate them, so neither full entity is offered at the 0.9 tier.
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let smith = person_entity("John Smith");
        let doe = person_entity("John Doe");
        let (smith_id, doe_id) = (smith.id, doe.id);
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &smith).unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &doe).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let scored = entity_resolve_scored(&rtxn, test_scope(), "John", 8).unwrap();
        assert!(
            !scored.iter().any(
                |(id, s)| (*id == smith_id || *id == doe_id) && (*s - 0.9).abs() < f32::EPSILON
            ),
            "ambiguous partial name must not resolve to either full entity: {scored:?}"
        );
    }

    // ----- update / rename ----------------------------------------------

    #[test]
    fn rename_moves_old_canonical_name_to_aliases() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Priya Patel");
        let id = e.id;
        let original_embedding_version = e.embedding_version;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
            wtxn.commit().unwrap();
        }

        {
            let wtxn = db.write_txn().unwrap();
            entity_rename(&wtxn, id, "Priya Singh".into(), LATER).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(got.canonical_name, "Priya Singh");
        assert!(
            got.aliases.iter().any(|a| a == "Priya Patel"),
            "old canonical_name must move into aliases; got {:?}",
            got.aliases
        );
        assert_eq!(got.embedding_version, original_embedding_version + 1);
        assert_eq!(got.updated_at_unix_nanos, LATER);

        // Old name no longer in canonical-name index; new name is.
        assert_eq!(
            entity_lookup_by_canonical_name(
                &rtxn,
                test_scope(),
                EntityType::PERSON_ID,
                "Priya Patel"
            )
            .unwrap(),
            None
        );
        assert_eq!(
            entity_lookup_by_canonical_name(
                &rtxn,
                test_scope(),
                EntityType::PERSON_ID,
                "Priya Singh"
            )
            .unwrap(),
            Some(id)
        );
    }

    #[test]
    fn update_alias_delta_applied() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let mut e = person_entity("Priya Patel");
        e.aliases = vec!["A".into(), "B".into()];
        let id = e.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
            wtxn.commit().unwrap();
        }

        // Update to {B, C}: remove A, add C, keep B.
        let mut next = e.clone();
        next.aliases = vec!["B".into(), "C".into()];

        {
            let wtxn = db.write_txn().unwrap();
            entity_update(&wtxn, &next, LATER).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        assert!(
            entity_lookup_by_alias(&rtxn, test_scope(), EntityType::PERSON_ID, "a")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            entity_lookup_by_alias(&rtxn, test_scope(), EntityType::PERSON_ID, "b").unwrap(),
            vec![id]
        );
        assert_eq!(
            entity_lookup_by_alias(&rtxn, test_scope(), EntityType::PERSON_ID, "c").unwrap(),
            vec![id]
        );
    }

    #[test]
    fn add_alias_dedupes_on_normalized_form() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let mut e = person_entity("Priya Patel");
        e.aliases.push("Priya".into());
        let id = e.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let wtxn = db.write_txn().unwrap();
            // Different case + extra space → normalizes to same alias.
            entity_add_alias(&wtxn, id, "  PRIYA  ".into(), LATER).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(got.aliases.len(), 1, "dedup on normalized form");
    }

    #[test]
    fn remove_alias_removes_index_row() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let mut e = person_entity("Priya Patel");
        e.aliases = vec!["X".into(), "Y".into()];
        let id = e.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let wtxn = db.write_txn().unwrap();
            entity_remove_alias(&wtxn, id, "X", LATER).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(got.aliases, vec!["Y".to_string()]);
        assert!(
            entity_lookup_by_alias(&rtxn, test_scope(), EntityType::PERSON_ID, "x")
                .unwrap()
                .is_empty()
        );
    }

    // ----- tombstone -----------------------------------------------------

    #[test]
    fn tombstone_removes_from_indexes_but_preserves_primary_row() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let mut e = person_entity("Priya Patel");
        e.aliases = vec!["priya".into()];
        let id = e.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let wtxn = db.write_txn().unwrap();
            entity_tombstone(&wtxn, id, LATER).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        // Indexes empty.
        assert_eq!(
            entity_lookup_by_canonical_name(
                &rtxn,
                test_scope(),
                EntityType::PERSON_ID,
                "Priya Patel"
            )
            .unwrap(),
            None
        );
        assert!(
            entity_lookup_by_alias(&rtxn, test_scope(), EntityType::PERSON_ID, "priya")
                .unwrap()
                .is_empty()
        );
        // Primary row preserved with flag set.
        let got = entity_get(&rtxn, id).unwrap().expect("primary preserved");
        assert!(got.flags & flags::TOMBSTONED != 0);
        assert!(got.aliases.is_empty(), "aliases drained on tombstone");
        assert_eq!(got.updated_at_unix_nanos, LATER);
    }

    #[test]
    fn tombstone_then_recreate_with_same_name_succeeds() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e1 = person_entity("Priya Patel");
        let id1 = e1.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e1).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let wtxn = db.write_txn().unwrap();
            entity_tombstone(&wtxn, id1, LATER).unwrap();
            wtxn.commit().unwrap();
        }
        // Same canonical_name, fresh EntityId — should succeed.
        let e2 = person_entity("Priya Patel");
        let id2 = e2.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e2).unwrap();
            wtxn.commit().unwrap();
        }
        assert_ne!(id1, id2);
        let rtxn = db.read_txn().unwrap();
        assert_eq!(
            entity_lookup_by_canonical_name(
                &rtxn,
                test_scope(),
                EntityType::PERSON_ID,
                "Priya Patel"
            )
            .unwrap(),
            Some(id2)
        );
    }

    // ----- list ----------------------------------------------------------

    #[test]
    fn list_by_type_returns_only_matching() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);

        // Seed a second type so the filter is meaningful.
        {
            use crate::tables::entity_type::{EntityTypeDefinition, ENTITY_TYPES_TABLE};
            let wtxn = db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
                let row =
                    EntityTypeDefinition::new(EntityTypeId(7), "Project".into(), Vec::new(), NOW);
                t.insert(&7u32, &row).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let p1 = person_entity("Alpha");
        let p2 = person_entity("Beta");
        let mut proj = person_entity("ProjectOne");
        proj.entity_type = EntityTypeId(7);

        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &p1).unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &p2).unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &proj).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let persons = entity_list_by_type(&rtxn, test_scope(), EntityType::PERSON_ID).unwrap();
        assert_eq!(persons.len(), 2);
        let projects = entity_list_by_type(&rtxn, test_scope(), EntityTypeId(7)).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].canonical_name, "ProjectOne");
    }

    #[test]
    fn iter_all_live_returns_live_skips_tombstoned() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);

        let alice = person_entity("Alice");
        let bob = person_entity("Bob");
        let bob_id = bob.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &alice).unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &bob).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let wtxn = db.write_txn().unwrap();
            entity_tombstone(&wtxn, bob_id, NOW).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let live = entity_iter_all_live(&rtxn).unwrap();
        // Bob is tombstoned → only Alice survives the rebuild source.
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].1, "Alice");
    }

    // ----- trigram integration -------------------------------------

    #[test]
    fn entity_put_writes_trigrams() {
        use crate::entity::trigram::{extract_trigrams, lookup_candidates_by_trigram};
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Priya Patel");
        let id = e.id;

        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        // Every trigram of the normalized canonical_name resolves back
        // to the inserted EntityId.
        for tg in extract_trigrams("priya patel") {
            let cands =
                lookup_candidates_by_trigram(&rtxn, test_scope(), EntityType::PERSON_ID, tg)
                    .unwrap();
            assert!(
                cands.contains(&id),
                "trigram {tg:?} not in index for inserted entity"
            );
        }
    }

    #[test]
    fn entity_put_aliases_contribute_trigrams() {
        use crate::entity::trigram::lookup_candidates_by_trigram;
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let mut e = person_entity("X"); // canonical "X" — short trigrams only
        e.aliases.push("Priya Patel".into()); // adds rich trigrams
        let id = e.id;

        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        // A trigram that comes only from the alias "Priya Patel", not from the
        // bare canonical "x". Trigrams are opaque buckets, so derive the query
        // from `extract_trigrams` rather than a byte literal.
        use crate::entity::trigram::extract_trigrams;
        let alias_only = extract_trigrams("priya patel")
            .difference(&extract_trigrams("x"))
            .copied()
            .next()
            .expect("alias contributes trigrams the canonical lacks");
        let cands =
            lookup_candidates_by_trigram(&rtxn, test_scope(), EntityType::PERSON_ID, alias_only)
                .unwrap();
        assert!(cands.contains(&id));
    }

    #[test]
    fn entity_rename_updates_trigrams() {
        use crate::entity::trigram::lookup_candidates_by_trigram;
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Alpha");
        let id = e.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let wtxn = db.write_txn().unwrap();
            entity_rename(&wtxn, id, "Bravo".into(), LATER).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        use crate::entity::trigram::extract_trigrams;
        // A "bravo" trigram (new canonical) is indexed. Opaque buckets →
        // derive the query from `extract_trigrams`.
        let bravo_tg = extract_trigrams("bravo")
            .into_iter()
            .next()
            .expect("bravo has trigrams");
        assert!(
            lookup_candidates_by_trigram(&rtxn, test_scope(), EntityType::PERSON_ID, bravo_tg)
                .unwrap()
                .contains(&id)
        );
        // An "alpha" trigram remains: the old canonical_name moves into
        // aliases on rename, so its trigrams stay indexed (resolver
        // continuity).
        let alpha_tg = extract_trigrams("alpha")
            .into_iter()
            .next()
            .expect("alpha has trigrams");
        assert!(
            lookup_candidates_by_trigram(&rtxn, test_scope(), EntityType::PERSON_ID, alpha_tg)
                .unwrap()
                .contains(&id),
            "alpha trigrams should remain (moved to aliases on rename)"
        );
    }

    #[test]
    fn entity_tombstone_removes_trigrams() {
        use crate::entity::trigram::lookup_candidates_by_trigram;
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let mut e = person_entity("Priya Patel");
        e.aliases.push("Priya".into());
        let id = e.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let wtxn = db.write_txn().unwrap();
            entity_tombstone(&wtxn, id, LATER).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        // No trigram of the original entity surfaces it.
        for tg in [*b"pri", *b"riy", *b"pat", *b"tel"] {
            let cands =
                lookup_candidates_by_trigram(&rtxn, test_scope(), EntityType::PERSON_ID, tg)
                    .unwrap();
            assert!(
                !cands.contains(&id),
                "tombstoned entity surfaced via trigram {tg:?}"
            );
        }
    }

    // ----- vector persistence ---------------------------------------------

    fn fixture_vector(seed: f32) -> [f32; 384] {
        let mut v = [0.0f32; 384];
        for (i, slot) in v.iter_mut().enumerate() {
            *slot = seed + (i as f32) * 0.001;
        }
        v
    }

    #[test]
    fn vector_round_trips_bit_exact() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Priya");
        let id = e.id;
        let v = fixture_vector(0.5);

        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
        entity_vector_put(&wtxn, id, &v).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = entity_vector_get(&rtxn, id)
            .unwrap()
            .expect("vector present");
        // Bit-exact: every f32 must survive the little-endian byte round-trip.
        for i in 0..384 {
            assert_eq!(got[i].to_bits(), v[i].to_bits(), "mismatch at {i}");
        }
    }

    #[test]
    fn vector_get_returns_none_for_missing_row() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("NoVector");
        let id = e.id;

        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        // Entity exists, but no vector was persisted → caller falls back
        // to re-embedding.
        assert!(entity_vector_get(&rtxn, id).unwrap().is_none());
    }

    #[test]
    fn iter_all_live_with_vectors_mixes_stored_and_missing() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let with_vec = person_entity("Priya");
        let without_vec = person_entity("Dana");
        let id_with = with_vec.id;
        let id_without = without_vec.id;
        let v = fixture_vector(0.25);

        {
            let wtxn = db.write_txn().unwrap();
            entity_put(
                &wtxn,
                test_scope(),
                brain_core::SessionId::DEFAULT,
                &with_vec,
            )
            .unwrap();
            entity_vector_put(&wtxn, id_with, &v).unwrap();
            entity_put(
                &wtxn,
                test_scope(),
                brain_core::SessionId::DEFAULT,
                &without_vec,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let rows = entity_iter_all_live_with_vectors(&rtxn).unwrap();
        let by_id: std::collections::HashMap<_, _> =
            rows.into_iter().map(|(id, n, v)| (id, (n, v))).collect();
        assert_eq!(by_id.len(), 2);
        let (_, vec_for_with) = &by_id[&id_with];
        let (_, vec_for_without) = &by_id[&id_without];
        assert!(
            vec_for_with.is_some(),
            "stored vector must surface as Some on the rebuild path"
        );
        assert!(
            vec_for_without.is_none(),
            "absent vector must surface as None so caller re-embeds"
        );
    }

    // ----- entity_inbound_reference_count --------------------------------

    fn put_person(db: &MetadataDb, name: &str) -> EntityId {
        let e = person_entity(name);
        let id = e.id;
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    /// Intern a Fact predicate with an Entity object.
    fn intern_fact_pred(db: &MetadataDb, name: &str) -> brain_core::PredicateId {
        use crate::schema::predicate::predicate_intern;
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(brain_core::StatementKind::Fact),
            1, // object: Entity
            1,
            "",
            false,
            NOW,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    #[test]
    fn inbound_count_zero_for_isolated_entity() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let id = put_person(&db, "Isolated Ivy");
        let rtxn = db.read_txn().unwrap();
        let n = entity_inbound_reference_count(&rtxn, test_scope(), id).unwrap();
        assert_eq!(n, 0, "an entity with no inbound rows is orphaned");
    }

    #[test]
    fn inbound_count_positive_for_subject_statement() {
        use crate::statement::crud::statement_create;
        use brain_core::{
            EvidenceRef, ExtractorId, Statement, StatementKind, StatementObject, SubjectRef,
        };
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let subj = put_person(&db, "Subject Sam");
        let obj = put_person(&db, "Object Olly");
        let pred = intern_fact_pred(&db, "likes");
        let s = Statement::new_root(
            brain_core::StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(subj),
            pred,
            StatementObject::Entity(obj),
            0.9,
            EvidenceRef::default(),
            ExtractorId::from(0),
            NOW,
            1,
        );
        {
            let wtxn = db.write_txn().unwrap();
            statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, NOW).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        // subj is referenced (subject of a statement); obj is not (the
        // subject index keys on subject only).
        assert!(entity_inbound_reference_count(&rtxn, test_scope(), subj).unwrap() > 0);
        assert_eq!(
            entity_inbound_reference_count(&rtxn, test_scope(), obj).unwrap(),
            0
        );
    }

    #[test]
    fn inbound_count_positive_for_relation_from_and_to() {
        use crate::relation::ops::relation_create;
        use crate::relation::types::relation_type_intern;
        use brain_core::{Cardinality, ExtractorId, Relation, RelationId};
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let from = put_person(&db, "From Fran");
        let to = put_person(&db, "To Tom");
        let rel_type = {
            let wtxn = db.write_txn().unwrap();
            let id = relation_type_intern(
                &wtxn,
                "test",
                "knows",
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
        };
        let r = Relation::new_root(
            RelationId::new(),
            rel_type,
            from,
            to,
            0.9,
            vec![],
            ExtractorId::from(0),
            NOW,
            false,
        );
        {
            let wtxn = db.write_txn().unwrap();
            relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        // Both endpoints are referenced: `from` via relation_list_from,
        // `to` via relation_list_to.
        assert!(entity_inbound_reference_count(&rtxn, test_scope(), from).unwrap() > 0);
        assert!(entity_inbound_reference_count(&rtxn, test_scope(), to).unwrap() > 0);
    }

    #[test]
    fn inbound_count_positive_for_mention() {
        use crate::tables::entity::{mention_context, MentionMetadata, ENTITY_MENTIONS_TABLE};
        use brain_core::MemoryId;
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let id = put_person(&db, "Mentioned Mia");
        let mem = MemoryId::pack(1, 100, 1);
        let s = test_scope();
        {
            let wtxn = db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(ENTITY_MENTIONS_TABLE).unwrap();
                let key = (
                    s.namespace_id,
                    s.space_id_bytes,
                    id.to_bytes(),
                    mem.to_be_bytes(),
                );
                let m = MentionMetadata::new(NOW, mention_context::IN_TEXT, 0.9);
                t.insert(&key, &m).unwrap();
            }
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        assert!(entity_inbound_reference_count(&rtxn, test_scope(), id).unwrap() > 0);
    }

    #[test]
    fn inbound_count_isolates_by_scope() {
        // A mention in a DIFFERENT scope must not count toward the
        // entity's inbound references in the caller's scope.
        use crate::tables::entity::{mention_context, MentionMetadata, ENTITY_MENTIONS_TABLE};
        use brain_core::MemoryId;
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let id = put_person(&db, "Scoped Sue");
        let other = RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xCD; 16]);
        let mem = MemoryId::pack(2, 200, 1);
        {
            let wtxn = db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(ENTITY_MENTIONS_TABLE).unwrap();
                let key = (
                    other.namespace_id,
                    other.space_id_bytes,
                    id.to_bytes(),
                    mem.to_be_bytes(),
                );
                let m = MentionMetadata::new(NOW, mention_context::IN_TEXT, 0.9);
                t.insert(&key, &m).unwrap();
            }
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        // Counted in `other`, not in `test_scope`.
        assert!(entity_inbound_reference_count(&rtxn, other, id).unwrap() > 0);
        assert_eq!(
            entity_inbound_reference_count(&rtxn, test_scope(), id).unwrap(),
            0
        );
    }
}
