//! Relation CRUD + cardinality-driven supersession + symmetric
//! canonicalisation. Sub-task 18.4.
//!
//! Free functions over `redb::{ReadTransaction, WriteTransaction}`,
//! mirroring [`crate::statement_ops`] (17.4) and the entity-ops
//! precedent.
//!
//! Spec refs:
//! - `spec/20_relations/00_purpose.md` — schema + ops.
//! - `spec/20_relations/01_cardinality.md` — supersession rules.
//! - `spec/20_relations/02_symmetric.md` — canonical ordering + dual
//!   indexing.
//! - `spec/20_relations/03_storage.md` — per-op write paths.
//! - `spec/20_relations/05_evidence.md` — flat evidence vec.

use brain_core::knowledge::{canonical_pair, Relation};
use brain_core::{
    Cardinality, EntityId, MemoryId, RelationId, RelationTypeId,
};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::entity_ops::EntityOpError;
use crate::relation_type_ops::RelationTypeOpError;
use crate::tables::knowledge::relation::{
    metadata_from_relation, relation_from_metadata, RelationMetadata,
    RELATIONS_BY_EVIDENCE_TABLE, RELATIONS_BY_FROM_TABLE, RELATIONS_BY_TO_TABLE, RELATIONS_TABLE,
};

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum RelationOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("relation {0:?} not found")]
    NotFound(RelationId),

    #[error("relation {0:?} already exists")]
    AlreadyExists(RelationId),

    #[error("relation type {0:?} not registered")]
    UnknownRelationType(RelationTypeId),

    #[error("entity {0:?} not registered")]
    UnknownEntity(EntityId),

    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),

    #[error("relation {0:?} already superseded by {1:?}")]
    AlreadySuperseded(RelationId, RelationId),

    #[error("relation {0:?} is tombstoned")]
    AlreadyTombstoned(RelationId),

    #[error(
        "relation type mismatch on supersede: old={old:?} new={new:?}"
    )]
    TypeMismatch {
        old: RelationTypeId,
        new: RelationTypeId,
    },

    #[error("endpoint mismatch on supersede")]
    EndpointMismatch,

    #[error(
        "cardinality violation ({variant:?}): {conflicting} existing current relation(s) conflict"
    )]
    CardinalityViolation {
        variant: Cardinality,
        conflicting: usize,
    },

    #[error("metadata row decode failed — file may be corrupt")]
    DecodeFailed,

    #[error("relation type op: {0}")]
    RelationTypeOp(#[from] RelationTypeOpError),

    #[error("entity op: {0}")]
    EntityOp(#[from] EntityOpError),
}

// ---------------------------------------------------------------------------
// Filter.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct RelationListFilter {
    pub relation_type: Option<RelationTypeId>,
    pub current_only: bool,
    /// Hard cap on returned rows. `0` defaults to [`DEFAULT_LIST_LIMIT`].
    pub limit: usize,
}

pub const DEFAULT_LIST_LIMIT: usize = 1_000;

// ---------------------------------------------------------------------------
// Read paths.
// ---------------------------------------------------------------------------

/// Fetch a relation by id. Returns `None` if absent.
pub fn relation_get(
    rtxn: &ReadTransaction,
    id: RelationId,
) -> Result<Option<Relation>, RelationOpError> {
    let t = rtxn.open_table(RELATIONS_TABLE)?;
    let row: Option<RelationMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    Ok(row.as_ref().map(relation_from_metadata))
}

/// Walk a supersession chain. Anchor may be the chain root or any
/// chain member. Returns chain in version-ascending order.
pub fn relation_history(
    rtxn: &ReadTransaction,
    anchor: RelationId,
) -> Result<Vec<Relation>, RelationOpError> {
    let t = rtxn.open_table(RELATIONS_TABLE)?;
    let anchor_row: Option<RelationMetadata> = t.get(&anchor.to_bytes())?.map(|g| g.value());
    let Some(anchor_row) = anchor_row else {
        return Err(RelationOpError::NotFound(anchor));
    };
    let chain_root_bytes = anchor_row.chain_root_bytes;

    // Walk every row with this chain root. Linear scan because there's
    // no chain-table-style secondary index for relations in v1; relation
    // chains are typically short (1–3 entries) so cost is fine.
    //
    // TODO(phase 22+): add RELATION_CHAIN_TABLE if chain reads become
    // hot.
    let mut chain = Vec::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        let m = v.value();
        if m.chain_root_bytes == chain_root_bytes {
            chain.push(relation_from_metadata(&m));
        }
    }
    chain.sort_by_key(|r| r.version);
    Ok(chain)
}

/// List relations where `entity` appears as `from` (or, for
/// symmetric relations, where the relation is dual-indexed under
/// `entity` in `RELATIONS_BY_FROM`).
pub fn relation_list_from(
    rtxn: &ReadTransaction,
    entity: EntityId,
    filter: &RelationListFilter,
) -> Result<Vec<Relation>, RelationOpError> {
    list_via_directional_index(rtxn, entity, filter, /* from_side */ true)
}

/// List relations where `entity` appears as `to`.
pub fn relation_list_to(
    rtxn: &ReadTransaction,
    entity: EntityId,
    filter: &RelationListFilter,
) -> Result<Vec<Relation>, RelationOpError> {
    list_via_directional_index(rtxn, entity, filter, /* from_side */ false)
}

fn list_via_directional_index(
    rtxn: &ReadTransaction,
    entity: EntityId,
    filter: &RelationListFilter,
    from_side: bool,
) -> Result<Vec<Relation>, RelationOpError> {
    let cap = if filter.limit == 0 {
        DEFAULT_LIST_LIMIT
    } else {
        filter.limit.min(DEFAULT_LIST_LIMIT)
    };

    let idx = if from_side {
        rtxn.open_table(RELATIONS_BY_FROM_TABLE)?
    } else {
        rtxn.open_table(RELATIONS_BY_TO_TABLE)?
    };

    let mut ids: Vec<[u8; 16]> = Vec::new();
    let lo = (entity.to_bytes(), 0u32, 0u8);
    let hi = (entity.to_bytes(), u32::MAX, 1u8);
    for entry in idx.range(lo..=hi)? {
        let (k, v) = entry?;
        let (_, k_type, k_current) = k.value();
        if filter.current_only && k_current == 0 {
            continue;
        }
        if let Some(want) = filter.relation_type {
            if k_type != want.raw() {
                continue;
            }
        }
        ids.push(v.value());
        if ids.len() >= cap {
            break;
        }
    }

    let t = rtxn.open_table(RELATIONS_TABLE)?;
    let mut out = Vec::with_capacity(ids.len());
    let mut seen: std::collections::HashSet<[u8; 16]> = std::collections::HashSet::new();
    for rid in ids {
        if !seen.insert(rid) {
            continue;
        }
        let row: Option<RelationMetadata> = t.get(&rid)?.map(|g| g.value());
        if let Some(m) = row {
            out.push(relation_from_metadata(&m));
        }
    }
    Ok(out)
}

/// Returns ids of all relations that cite `memory_id` as evidence.
/// Used by the FORGET cascade in spec §20/05 §5.
pub fn relations_with_evidence(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
) -> Result<Vec<RelationId>, RelationOpError> {
    let t = rtxn.open_table(RELATIONS_BY_EVIDENCE_TABLE)?;
    let mem_bytes = memory_id.to_be_bytes();
    let lo = (mem_bytes, [0u8; 16]);
    let hi = (mem_bytes, [0xFFu8; 16]);
    let mut out = Vec::new();
    for entry in t.range(lo..=hi)? {
        let (k, _) = entry?;
        let (k_mem, k_rel) = k.value();
        if k_mem != mem_bytes {
            continue;
        }
        out.push(RelationId::from(k_rel));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Write paths.
// ---------------------------------------------------------------------------

/// Create a new relation.
///
/// 1. Validates `r.from_entity / r.to_entity` exist.
/// 2. Validates `r.relation_type` is registered; pulls `Cardinality`
///    + `is_symmetric` from the registry (callers may set
///    `r.is_symmetric` from the registry first; this function
///    re-reads it to enforce the source of truth).
/// 3. Canonicalises `(from, to)` for symmetric relations.
/// 4. Runs the cardinality auto-supersede probe per §20/01 §2.
/// 5. Inserts primary + 1–2 BY_FROM + 1–2 BY_TO + evidence rows.
pub fn relation_create(
    wtxn: &WriteTransaction,
    r: &Relation,
    now_unix_nanos: u64,
) -> Result<RelationId, RelationOpError> {
    if r.confidence.is_nan() || !(0.0..=1.0).contains(&r.confidence) {
        return Err(RelationOpError::InvalidArgument(
            "confidence must be in [0, 1] and not NaN",
        ));
    }

    // Endpoints exist.
    require_entity_exists(wtxn, r.from_entity)?;
    require_entity_exists(wtxn, r.to_entity)?;

    // Relation type exists; pull authoritative cardinality + symmetry.
    let (cardinality, is_symmetric) = lookup_type(wtxn, r.relation_type)?;

    // ID uniqueness.
    {
        let t = wtxn.open_table(RELATIONS_TABLE)?;
        if t.get(&r.id.to_bytes())?.is_some() {
            return Err(RelationOpError::AlreadyExists(r.id));
        }
    }

    // Canonicalise for symmetric.
    let mut to_insert = r.clone();
    to_insert.is_symmetric = is_symmetric;
    if is_symmetric {
        let (a, b) = canonical_pair(to_insert.from_entity, to_insert.to_entity);
        to_insert.from_entity = a;
        to_insert.to_entity = b;
    }

    // Cardinality auto-supersession.
    let conflicting =
        find_cardinality_conflicts(wtxn, &to_insert, cardinality)?;
    match conflicting.len() {
        0 => {
            insert_new_relation(wtxn, &to_insert)?;
            Ok(to_insert.id)
        }
        1 => {
            // Auto-supersede the single prior current.
            let old_id = conflicting[0];
            relation_supersede(wtxn, old_id, &to_insert, now_unix_nanos)
        }
        _ => Err(RelationOpError::CardinalityViolation {
            variant: cardinality,
            conflicting: conflicting.len(),
        }),
    }
}

/// Supersede `old_id` with `new_relation`. Atomic two-step in `wtxn`.
pub fn relation_supersede(
    wtxn: &WriteTransaction,
    old_id: RelationId,
    new_relation: &Relation,
    _now_unix_nanos: u64,
) -> Result<RelationId, RelationOpError> {
    if new_relation.confidence.is_nan() || !(0.0..=1.0).contains(&new_relation.confidence) {
        return Err(RelationOpError::InvalidArgument(
            "confidence must be in [0, 1] and not NaN",
        ));
    }

    // Load old.
    let mut old = {
        let t = wtxn.open_table(RELATIONS_TABLE)?;
        let row: Option<RelationMetadata> = t.get(&old_id.to_bytes())?.map(|g| g.value());
        row.ok_or(RelationOpError::NotFound(old_id))?
    };

    if old.is_tombstoned() {
        return Err(RelationOpError::AlreadyTombstoned(old_id));
    }
    if let Some(succ) = old.superseded_by_bytes {
        return Err(RelationOpError::AlreadySuperseded(
            old_id,
            RelationId::from(succ),
        ));
    }
    if old.relation_type_id != new_relation.relation_type.raw() {
        return Err(RelationOpError::TypeMismatch {
            old: RelationTypeId::from(old.relation_type_id),
            new: new_relation.relation_type,
        });
    }
    // For symmetric, both should already be canonicalised by the
    // caller (relation_create canonicalises before delegating);
    // verify the canonical endpoints match.
    let new_from = new_relation.from_entity.to_bytes();
    let new_to = new_relation.to_entity.to_bytes();
    if old.from_entity_bytes != new_from || old.to_entity_bytes != new_to {
        return Err(RelationOpError::EndpointMismatch);
    }

    // ID uniqueness on new.
    {
        let t = wtxn.open_table(RELATIONS_TABLE)?;
        if t.get(&new_relation.id.to_bytes())?.is_some() {
            return Err(RelationOpError::AlreadyExists(new_relation.id));
        }
    }

    let chain_root_bytes = if old.supersedes_bytes.is_none() {
        old.relation_id_bytes
    } else {
        old.chain_root_bytes
    };
    let new_version = old.version.saturating_add(1);

    let mut new_to_insert = new_relation.clone();
    new_to_insert.version = new_version;
    new_to_insert.supersedes = Some(old_id);
    new_to_insert.superseded_by = None;
    new_to_insert.chain_root = RelationId::from(chain_root_bytes);
    new_to_insert.is_symmetric = old.is_symmetric();

    // Capture old direction-index keys before mutation.
    let old_from = old.from_entity_bytes;
    let old_to = old.to_entity_bytes;
    let old_type = old.relation_type_id;
    let old_was_current = old.is_current != 0;
    let old_is_symmetric = old.is_symmetric();

    // Update old in place.
    old.superseded_by_bytes = Some(new_to_insert.id.to_bytes());
    if old.valid_to_unix_nanos.is_none() {
        old.valid_to_unix_nanos = Some(new_to_insert.extracted_at_unix_nanos);
    }
    old.is_current = 0;

    {
        let mut t = wtxn.open_table(RELATIONS_TABLE)?;
        t.insert(&old.relation_id_bytes, &old)?;
    }

    // Flip BY_FROM + BY_TO is_current bits for old. For symmetric,
    // both endpoints appear in BOTH indexes — flip all 4.
    if old_was_current {
        flip_is_current(wtxn, old_from, old_to, old_type, old_is_symmetric, old.relation_id_bytes)?;
    }

    // Insert new relation + all indexes.
    insert_new_relation(wtxn, &new_to_insert)?;

    Ok(new_to_insert.id)
}

/// Soft delete. Flips `is_current` in storage + directional indexes.
pub fn relation_tombstone(
    wtxn: &WriteTransaction,
    id: RelationId,
    now_unix_nanos: u64,
) -> Result<(), RelationOpError> {
    let mut row = {
        let t = wtxn.open_table(RELATIONS_TABLE)?;
        let r: Option<RelationMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
        r.ok_or(RelationOpError::NotFound(id))?
    };
    if row.is_tombstoned() {
        return Ok(());
    }
    let was_current = row.is_current != 0;
    let from = row.from_entity_bytes;
    let to = row.to_entity_bytes;
    let type_id = row.relation_type_id;
    let was_symmetric = row.is_symmetric();

    row.tombstoned = 1;
    row.tombstoned_at_unix_nanos = Some(now_unix_nanos);
    row.is_current = 0;

    {
        let mut t = wtxn.open_table(RELATIONS_TABLE)?;
        t.insert(&row.relation_id_bytes, &row)?;
    }
    if was_current {
        flip_is_current(wtxn, from, to, type_id, was_symmetric, row.relation_id_bytes)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers.
// ---------------------------------------------------------------------------

fn require_entity_exists(
    wtxn: &WriteTransaction,
    id: EntityId,
) -> Result<(), RelationOpError> {
    use crate::tables::knowledge::entity::{EntityMetadata, ENTITIES_TABLE};
    let t = wtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    if row.is_none() {
        return Err(RelationOpError::UnknownEntity(id));
    }
    Ok(())
}

fn lookup_type(
    wtxn: &WriteTransaction,
    id: RelationTypeId,
) -> Result<(Cardinality, bool), RelationOpError> {
    use crate::tables::knowledge::relation_type::{
        RelationTypeDefinition, RELATION_TYPES_TABLE,
    };
    let t = wtxn.open_table(RELATION_TYPES_TABLE)?;
    let row: Option<RelationTypeDefinition> = t.get(&id.raw())?.map(|g| g.value());
    let row = row.ok_or(RelationOpError::UnknownRelationType(id))?;
    let cardinality = Cardinality::from_u8(row.cardinality).ok_or(
        RelationOpError::InvalidArgument("relation type has unknown cardinality"),
    )?;
    Ok((cardinality, row.is_symmetric != 0))
}

/// Pre-create cardinality probe per §20/01 §2. Returns ids of any
/// existing current relations that conflict with the new one under
/// the given cardinality. Caller decides how to react.
fn find_cardinality_conflicts(
    wtxn: &WriteTransaction,
    r: &Relation,
    cardinality: Cardinality,
) -> Result<Vec<RelationId>, RelationOpError> {
    let mut found = Vec::new();

    let want_from_lookup = matches!(cardinality, Cardinality::ManyToOne | Cardinality::OneToOne);
    let want_to_lookup = matches!(cardinality, Cardinality::OneToMany | Cardinality::OneToOne);

    if want_from_lookup {
        let by_from = wtxn.open_table(RELATIONS_BY_FROM_TABLE)?;
        let key = (r.from_entity.to_bytes(), r.relation_type.raw(), 1u8);
        let bytes: Option<[u8; 16]> = by_from.get(&key)?.map(|g| g.value());
        if let Some(b) = bytes {
            let candidate = RelationId::from(b);
            // For symmetric OneToOne, this might be the same relation
            // we're trying to insert (won't be — id uniqueness check
            // already passed). Add unconditionally.
            if !found.contains(&candidate) {
                found.push(candidate);
            }
        }
    }
    if want_to_lookup {
        let by_to = wtxn.open_table(RELATIONS_BY_TO_TABLE)?;
        let key = (r.to_entity.to_bytes(), r.relation_type.raw(), 1u8);
        let bytes: Option<[u8; 16]> = by_to.get(&key)?.map(|g| g.value());
        if let Some(b) = bytes {
            let candidate = RelationId::from(b);
            if !found.contains(&candidate) {
                found.push(candidate);
            }
        }
    }
    Ok(found)
}

/// Insert a fresh relation row + every secondary index.
fn insert_new_relation(
    wtxn: &WriteTransaction,
    r: &Relation,
) -> Result<(), RelationOpError> {
    let m = metadata_from_relation(r);

    // Primary.
    {
        let mut t = wtxn.open_table(RELATIONS_TABLE)?;
        t.insert(&m.relation_id_bytes, &m)?;
    }

    let type_id = m.relation_type_id;
    let from = m.from_entity_bytes;
    let to = m.to_entity_bytes;
    let current = m.is_current;
    let is_symmetric = m.is_symmetric();

    // BY_FROM: always under `from`. For symmetric, also under `to`.
    {
        let mut t = wtxn.open_table(RELATIONS_BY_FROM_TABLE)?;
        t.insert(&(from, type_id, current), &m.relation_id_bytes)?;
        if is_symmetric {
            t.insert(&(to, type_id, current), &m.relation_id_bytes)?;
        }
    }
    // BY_TO: always under `to`. For symmetric, also under `from`.
    {
        let mut t = wtxn.open_table(RELATIONS_BY_TO_TABLE)?;
        t.insert(&(to, type_id, current), &m.relation_id_bytes)?;
        if is_symmetric {
            t.insert(&(from, type_id, current), &m.relation_id_bytes)?;
        }
    }
    // BY_EVIDENCE.
    {
        let mut t = wtxn.open_table(RELATIONS_BY_EVIDENCE_TABLE)?;
        for mem in &r.evidence {
            t.insert(&(mem.to_be_bytes(), m.relation_id_bytes), &())?;
        }
    }

    Ok(())
}

/// Flip `is_current` from 1 → 0 in BY_FROM and BY_TO. For symmetric,
/// both endpoints appear in BOTH indexes — flip all 4 keys.
fn flip_is_current(
    wtxn: &WriteTransaction,
    from_bytes: [u8; 16],
    to_bytes: [u8; 16],
    type_id: u32,
    is_symmetric: bool,
    relation_id_bytes: [u8; 16],
) -> Result<(), RelationOpError> {
    {
        let mut t = wtxn.open_table(RELATIONS_BY_FROM_TABLE)?;
        t.remove(&(from_bytes, type_id, 1u8))?;
        t.insert(&(from_bytes, type_id, 0u8), &relation_id_bytes)?;
        if is_symmetric {
            t.remove(&(to_bytes, type_id, 1u8))?;
            t.insert(&(to_bytes, type_id, 0u8), &relation_id_bytes)?;
        }
    }
    {
        let mut t = wtxn.open_table(RELATIONS_BY_TO_TABLE)?;
        t.remove(&(to_bytes, type_id, 1u8))?;
        t.insert(&(to_bytes, type_id, 0u8), &relation_id_bytes)?;
        if is_symmetric {
            t.remove(&(from_bytes, type_id, 1u8))?;
            t.insert(&(from_bytes, type_id, 0u8), &relation_id_bytes)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::entity_ops::{entity_put, normalize_name};
    use crate::relation_type_ops::relation_type_intern;
    use brain_core::knowledge::{Entity, EntityType};
    use brain_core::ExtractorId;

    fn open_db() -> (tempfile::TempDir, crate::MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    fn make_entity(db: &mut crate::MetadataDb, name: &str) -> EntityId {
        let id = EntityId::new();
        let n = normalize_name(name);
        let e = Entity::new_active(id, EntityType::PERSON_ID, name.into(), n, 1_700_000_000_000_000_000);
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_type(
        db: &mut crate::MetadataDb,
        name: &str,
        cardinality: Cardinality,
        symmetric: bool,
    ) -> RelationTypeId {
        let wtxn = db.write_txn().unwrap();
        let id = relation_type_intern(
            &wtxn,
            "test",
            name,
            None,
            None,
            cardinality,
            symmetric,
            1,
            "",
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn fresh_rel(
        type_id: RelationTypeId,
        from: EntityId,
        to: EntityId,
        symmetric: bool,
    ) -> Relation {
        Relation::new_root(
            RelationId::new(),
            type_id,
            from,
            to,
            0.9,
            vec![],
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            symmetric,
        )
    }

    // ----- Create + projection -----

    #[test]
    fn create_asymmetric_round_trips() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a");
        let b = make_entity(&mut db, "b");
        let t = intern_type(&mut db, "knows", Cardinality::ManyToMany, false);
        let r = fresh_rel(t, a, b, false);

        let wtxn = db.write_txn().unwrap();
        let id = relation_create(&wtxn, &r, 0).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(id, r.id);

        let rtxn = db.read_txn().unwrap();
        let got = relation_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(got, r);
    }

    #[test]
    fn create_symmetric_canonicalises_ordering() {
        let (_dir, mut db) = open_db();
        let a = EntityId::from([2u8; 16]);
        let b = EntityId::from([1u8; 16]);
        let _ = make_entity_with(&mut db, a, "a");
        let _ = make_entity_with(&mut db, b, "b");
        let t = intern_type(&mut db, "knows_sym", Cardinality::ManyToMany, true);
        // Caller passes a, b but canonical is (b, a) since b < a.
        let r = fresh_rel(t, a, b, true);

        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = relation_get(&rtxn, r.id).unwrap().unwrap();
        assert_eq!(got.from_entity, b);
        assert_eq!(got.to_entity, a);
        assert!(got.is_symmetric);
    }

    fn make_entity_with(db: &mut crate::MetadataDb, id: EntityId, name: &str) -> EntityId {
        let n = normalize_name(name);
        let e = Entity::new_active(id, EntityType::PERSON_ID, name.into(), n, 1_700_000_000_000_000_000);
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    #[test]
    fn create_symmetric_indexes_both_sides() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-sym");
        let b = make_entity(&mut db, "b-sym");
        let t = intern_type(&mut db, "discussed_with", Cardinality::ManyToMany, true);
        let r = fresh_rel(t, a, b, true);

        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let filter = RelationListFilter {
            relation_type: Some(t),
            current_only: true,
            ..Default::default()
        };
        // Query from a — finds the relation (dual-indexed).
        let from_a = relation_list_from(&rtxn, a, &filter).unwrap();
        assert_eq!(from_a.len(), 1);
        // Query from b — also finds it (canonical_from is whichever
        // came first byte-wise; the other endpoint is in BY_FROM too).
        let from_b = relation_list_from(&rtxn, b, &filter).unwrap();
        assert_eq!(from_b.len(), 1);
    }

    #[test]
    fn create_self_loop_allowed() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "self");
        let t = intern_type(&mut db, "knows_self", Cardinality::ManyToMany, false);
        let r = fresh_rel(t, a, a, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r, 0).unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn create_unknown_relation_type_rejected() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-ut");
        let b = make_entity(&mut db, "b-ut");
        let r = fresh_rel(RelationTypeId::from(9999), a, b, false);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, &r, 0).unwrap_err();
        matches!(err, RelationOpError::UnknownRelationType(_))
            .then_some(())
            .expect("expected UnknownRelationType");
    }

    #[test]
    fn create_unknown_endpoint_rejected() {
        let (_dir, mut db) = open_db();
        let t = intern_type(&mut db, "knows_ue", Cardinality::ManyToMany, false);
        let r = fresh_rel(t, EntityId::new(), EntityId::new(), false);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, &r, 0).unwrap_err();
        matches!(err, RelationOpError::UnknownEntity(_))
            .then_some(())
            .expect("expected UnknownEntity");
    }

    #[test]
    fn many_to_one_auto_supersedes_on_from_side() {
        let (_dir, mut db) = open_db();
        let priya = make_entity(&mut db, "priya");
        let alice = make_entity(&mut db, "alice");
        let bob = make_entity(&mut db, "bob");
        let t = intern_type(&mut db, "reports_to", Cardinality::ManyToOne, false);

        let r1 = fresh_rel(t, priya, alice, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r1, 0).unwrap();
        wtxn.commit().unwrap();

        // priya now reports to bob — auto-supersede r1.
        let r2 = fresh_rel(t, priya, bob, false);
        let wtxn = db.write_txn().unwrap();
        let result_id = relation_create(&wtxn, &r2, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = relation_get(&rtxn, r1.id).unwrap().unwrap();
        let g2 = relation_get(&rtxn, result_id).unwrap().unwrap();
        assert_eq!(g1.superseded_by, Some(g2.id));
        assert_eq!(g2.supersedes, Some(r1.id));
        assert_eq!(g2.chain_root, r1.id);
        assert_eq!(g2.version, 2);
    }

    #[test]
    fn one_to_many_auto_supersedes_on_to_side() {
        let (_dir, mut db) = open_db();
        let priya = make_entity(&mut db, "priya2");
        let acme = make_entity(&mut db, "acme");
        let other_employer = make_entity(&mut db, "other-co");
        let t = intern_type(&mut db, "employed_by", Cardinality::OneToMany, false);

        let r1 = fresh_rel(t, priya, acme, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r1, 0).unwrap();
        wtxn.commit().unwrap();

        // Someone else now employed by acme — for OneToMany the
        // constraint is on the `to` side: only one current relation
        // can have acme as `to`. So the second create supersedes the
        // first.
        let new_employee = make_entity(&mut db, "new-employee");
        let r2 = fresh_rel(t, new_employee, acme, false);
        let wtxn = db.write_txn().unwrap();
        let result_id = relation_create(&wtxn, &r2, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = relation_get(&rtxn, r1.id).unwrap().unwrap();
        assert_eq!(g1.superseded_by, Some(result_id));
        let _ = other_employer; // suppress unused
    }

    #[test]
    fn many_to_many_no_supersession() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "mm-a");
        let b = make_entity(&mut db, "mm-b");
        let t = intern_type(&mut db, "knows_mm", Cardinality::ManyToMany, false);

        let r1 = fresh_rel(t, a, b, false);
        let r2 = fresh_rel(t, a, b, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r1, 0).unwrap();
        relation_create(&wtxn, &r2, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = relation_get(&rtxn, r1.id).unwrap().unwrap();
        let g2 = relation_get(&rtxn, r2.id).unwrap().unwrap();
        assert!(g1.superseded_by.is_none(), "MM: no auto-supersede");
        assert!(g2.superseded_by.is_none());
    }

    // ----- Tombstone -----

    #[test]
    fn tombstone_flips_is_current_in_both_indexes() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "ta");
        let b = make_entity(&mut db, "tb");
        let t = intern_type(&mut db, "knows_t", Cardinality::ManyToMany, false);
        let r = fresh_rel(t, a, b, false);

        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r, 0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        relation_tombstone(&wtxn, r.id, 999).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let filter = RelationListFilter {
            current_only: true,
            ..Default::default()
        };
        let from_a = relation_list_from(&rtxn, a, &filter).unwrap();
        assert!(from_a.is_empty(), "current_only excludes tombstoned");

        let filter_all = RelationListFilter::default();
        let from_a_all = relation_list_from(&rtxn, a, &filter_all).unwrap();
        assert_eq!(from_a_all.len(), 1, "without current_only, sees tombstoned");
        assert!(from_a_all[0].tombstoned);
    }

    #[test]
    fn cardinality_freed_after_tombstone() {
        let (_dir, mut db) = open_db();
        let priya = make_entity(&mut db, "p-card");
        let alice = make_entity(&mut db, "a-card");
        let bob = make_entity(&mut db, "b-card");
        let t = intern_type(&mut db, "reports_to_card", Cardinality::ManyToOne, false);

        let r1 = fresh_rel(t, priya, alice, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r1, 0).unwrap();
        wtxn.commit().unwrap();

        // Tombstone r1 — slot freed.
        let wtxn = db.write_txn().unwrap();
        relation_tombstone(&wtxn, r1.id, 1).unwrap();
        wtxn.commit().unwrap();

        // r2 can now be created without auto-supersede.
        let r2 = fresh_rel(t, priya, bob, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r2, 2).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = relation_get(&rtxn, r1.id).unwrap().unwrap();
        let g2 = relation_get(&rtxn, r2.id).unwrap().unwrap();
        assert!(g1.tombstoned);
        assert!(g2.supersedes.is_none(), "tombstoned slot frees cardinality");
    }

    // ----- History -----

    #[test]
    fn history_walks_chain() {
        let (_dir, mut db) = open_db();
        let priya = make_entity(&mut db, "p-hist");
        let a = make_entity(&mut db, "a-hist");
        let b = make_entity(&mut db, "b-hist");
        let c = make_entity(&mut db, "c-hist");
        let t = intern_type(&mut db, "reports_to_hist", Cardinality::ManyToOne, false);

        let r1 = fresh_rel(t, priya, a, false);
        let r2 = fresh_rel(t, priya, b, false);
        let r3 = fresh_rel(t, priya, c, false);

        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r1, 0).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r2, 1).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r3, 2).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let chain = relation_history(&rtxn, r1.id).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id, r1.id);
        assert_eq!(chain[1].id, r2.id);
        assert_eq!(chain[2].id, r3.id);
    }

    // ----- List filters -----

    #[test]
    fn list_with_type_filter() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "f-a");
        let b = make_entity(&mut db, "f-b");
        let t1 = intern_type(&mut db, "type_one", Cardinality::ManyToMany, false);
        let t2 = intern_type(&mut db, "type_two", Cardinality::ManyToMany, false);

        let r1 = fresh_rel(t1, a, b, false);
        let r2 = fresh_rel(t2, a, b, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r1, 0).unwrap();
        relation_create(&wtxn, &r2, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let filter = RelationListFilter {
            relation_type: Some(t1),
            current_only: true,
            ..Default::default()
        };
        let out = relation_list_from(&rtxn, a, &filter).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation_type, t1);
    }

    // ----- Reverse evidence index -----

    #[test]
    fn relations_with_evidence_returns_dependents() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "ev-a");
        let b = make_entity(&mut db, "ev-b");
        let t = intern_type(&mut db, "ev_type", Cardinality::ManyToMany, false);
        let mem = MemoryId::pack(1, brain_core::ContextId::DEFAULT.into(), 0);
        let mut r = fresh_rel(t, a, b, false);
        r.evidence = vec![mem];

        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, &r, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let deps = relations_with_evidence(&rtxn, mem).unwrap();
        assert_eq!(deps, vec![r.id]);
    }
}
