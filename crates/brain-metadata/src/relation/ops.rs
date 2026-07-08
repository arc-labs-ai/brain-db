//! Relation CRUD over the unified edge table + sidecar metadata.
//!
//! Substrate edges and typed relations are collapsed into one redb
//! table pair. This module is the typed-relation writer
//! plus reader projections on that pair:
//!
//! - `relation_create` writes one row to
//!   [`crate::tables::edge::EDGES_TABLE`] + its reverse, and one row
//!   to [`RELATION_METADATA_TABLE`] (the sidecar) + 0..N rows to
//!   [`RELATION_BY_EVIDENCE_TABLE`].
//! - `relation_supersede` updates the old sidecar (`is_current = 0`,
//!   `superseded_by = new_id`, `valid_to = now`) and inserts the new
//!   relation. The edge row of the superseded relation stays — the
//!   sidecar carries history.
//! - `relation_tombstone` flips the sidecar `tombstoned` bit; the
//!   edge row stays.
//! - `relation_list_from` / `_to` walk the unified edge table
//!   (`walk_outgoing` / `walk_incoming` filtered to `Typed`) and
//!   project sidecar metadata back to [`Relation`].
//! - `relation_get` is a sidecar point lookup keyed by `RelationId`.
//! - Cardinality probes prefix-scan the unified table by
//!   `(from, Typed(rel_type_id), *)` and filter on sidecar
//!   `is_current = 1`.

use brain_core::{canonical_pair, Relation};
use brain_core::{
    Cardinality, EdgeKindRef, EntityId, MemoryId, NodeRef, RelationId, RelationTypeId,
};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::entity::ops::EntityOpError;
use crate::relation::types::RelationTypeOpError;
use crate::tables::edge::{
    self, derived_by, origin, EdgeData, EdgeKeyError, EdgeOpError, EDGES_REVERSE_TABLE, EDGES_TABLE,
};
use crate::tables::relation::{
    metadata_from_relation, relation_from_metadata, RelationMetadata, RELATION_BY_EVIDENCE_TABLE,
    RELATION_METADATA_TABLE,
};
use crate::tables::scope::RowScope;

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum RelationOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("edge op error: {0}")]
    EdgeOp(#[from] EdgeOpError),

    #[error("edge key decode error: {0}")]
    EdgeKey(#[from] EdgeKeyError),

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

    #[error("relation type mismatch on supersede: old={old:?} new={new:?}")]
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
    let t = rtxn.open_table(RELATION_METADATA_TABLE)?;
    let row: Option<RelationMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    Ok(row.as_ref().map(|m| relation_from_metadata(id, m)))
}

/// Walk a supersession chain. Anchor may be the chain root or any
/// chain member. Returns chain in version-ascending order.
pub fn relation_history(
    rtxn: &ReadTransaction,
    anchor: RelationId,
) -> Result<Vec<Relation>, RelationOpError> {
    let t = rtxn.open_table(RELATION_METADATA_TABLE)?;
    let anchor_row: Option<RelationMetadata> = t.get(&anchor.to_bytes())?.map(|g| g.value());
    let Some(anchor_row) = anchor_row else {
        return Err(RelationOpError::NotFound(anchor));
    };
    let chain_root_bytes = anchor_row.chain_root_bytes;
    // A supersession chain never crosses a tenant boundary, so the
    // anchor's own scope bounds the whole chain — filter the scan to it.
    let anchor_ns = anchor_row.namespace_id;
    let anchor_agent = anchor_row.agent_id_bytes;

    // Linear scan: chains are short (1–3 entries typical). A
    // chain-root secondary index can be added if this becomes hot.
    let mut chain = Vec::new();
    for entry in t.iter()? {
        let (k, v) = entry?;
        let m = v.value();
        if m.chain_root_bytes == chain_root_bytes
            && m.namespace_id == anchor_ns
            && m.agent_id_bytes == anchor_agent
        {
            let id = RelationId::from(k.value());
            chain.push(relation_from_metadata(id, &m));
        }
    }
    chain.sort_by_key(|r| r.version);
    Ok(chain)
}

/// List relations where `entity` is the `from` endpoint, optionally
/// filtered by relation type and `current_only`.
pub fn relation_list_from(
    rtxn: &ReadTransaction,
    scope: RowScope,
    entity: EntityId,
    filter: &RelationListFilter,
) -> Result<Vec<Relation>, RelationOpError> {
    list_directional(rtxn, scope, entity, filter, /* outgoing */ true)
}

/// List relations where `entity` is the `to` endpoint.
pub fn relation_list_to(
    rtxn: &ReadTransaction,
    scope: RowScope,
    entity: EntityId,
    filter: &RelationListFilter,
) -> Result<Vec<Relation>, RelationOpError> {
    list_directional(rtxn, scope, entity, filter, /* outgoing */ false)
}

fn list_directional(
    rtxn: &ReadTransaction,
    scope: RowScope,
    entity: EntityId,
    filter: &RelationListFilter,
    outgoing: bool,
) -> Result<Vec<Relation>, RelationOpError> {
    let cap = if filter.limit == 0 {
        DEFAULT_LIST_LIMIT
    } else {
        filter.limit.min(DEFAULT_LIST_LIMIT)
    };

    let kind_filter = filter.relation_type.map(EdgeKindRef::Typed);
    let rows = if outgoing {
        edge::walk_outgoing(rtxn, NodeRef::Entity(entity), kind_filter)?
    } else {
        edge::walk_incoming(rtxn, NodeRef::Entity(entity), kind_filter)?
    };

    let sidecar = rtxn.open_table(RELATION_METADATA_TABLE)?;
    let mut out = Vec::new();
    for (kind, _other, disambiguator, _data) in rows {
        // Only Typed edges represent typed relations; ignore
        // substrate Builtin / Mentions edges that might also be
        // anchored at this entity (none today, future-proof).
        if !matches!(kind, EdgeKindRef::Typed(_)) {
            continue;
        }
        let id = RelationId::from(disambiguator);
        let Some(meta) = sidecar.get(&disambiguator)?.map(|g| g.value()) else {
            continue;
        };
        // Unconditional scope wall. The shared EDGES_TABLE is not
        // re-keyed by scope, so the directional walk can surface an edge
        // owned by another tenant; the sidecar carries the owning scope
        // and is the authority that filters it out.
        if meta.namespace_id != scope.namespace_id || meta.agent_id_bytes != scope.agent_id_bytes {
            continue;
        }
        if filter.current_only && !meta.is_current() {
            continue;
        }
        if let Some(want) = filter.relation_type {
            if meta.relation_type_id != want.raw() {
                continue;
            }
        }
        out.push(relation_from_metadata(id, &meta));
        if out.len() >= cap {
            break;
        }
    }
    Ok(out)
}

/// Returns ids of all relations that cite `memory_id` as evidence.
pub fn relations_with_evidence(
    rtxn: &ReadTransaction,
    scope: RowScope,
    memory_id: MemoryId,
) -> Result<Vec<RelationId>, RelationOpError> {
    let t = rtxn.open_table(RELATION_BY_EVIDENCE_TABLE)?;
    let mem_bytes = memory_id.to_be_bytes();
    let lo = (
        scope.namespace_id,
        scope.agent_id_bytes,
        mem_bytes,
        [0u8; 16],
    );
    let hi = (
        scope.namespace_id,
        scope.agent_id_bytes,
        mem_bytes,
        [0xFFu8; 16],
    );
    let mut out = Vec::new();
    for entry in t.range(lo..=hi)? {
        let (k, _) = entry?;
        let (k_ns, k_agent, k_mem, k_rel) = k.value();
        if k_ns != scope.namespace_id || k_agent != scope.agent_id_bytes || k_mem != mem_bytes {
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
pub fn relation_create(
    wtxn: &WriteTransaction,
    scope: RowScope,
    r: &Relation,
    now_unix_nanos: u64,
) -> Result<RelationId, RelationOpError> {
    if r.confidence.is_nan() || !(0.0..=1.0).contains(&r.confidence) {
        return Err(RelationOpError::InvalidArgument(
            "confidence must be in [0, 1] and not NaN",
        ));
    }

    require_entity_exists(wtxn, r.from_entity)?;
    require_entity_exists(wtxn, r.to_entity)?;
    let (cardinality, is_symmetric) = lookup_type(wtxn, r.relation_type)?;

    {
        let t = wtxn.open_table(RELATION_METADATA_TABLE)?;
        if t.get(&r.id.to_bytes())?.is_some() {
            return Err(RelationOpError::AlreadyExists(r.id));
        }
    }

    let mut to_insert = r.clone();
    to_insert.is_symmetric = is_symmetric;
    if is_symmetric {
        let (a, b) = canonical_pair(to_insert.from_entity, to_insert.to_entity);
        to_insert.from_entity = a;
        to_insert.to_entity = b;
    }

    let conflicting = find_cardinality_conflicts(wtxn, scope, &to_insert, cardinality)?;
    match conflicting.len() {
        0 => {
            insert_new_relation(wtxn, scope, &to_insert, now_unix_nanos)?;
            Ok(to_insert.id)
        }
        1 => relation_supersede(wtxn, scope, conflicting[0], &to_insert, now_unix_nanos),
        _ => Err(RelationOpError::CardinalityViolation {
            variant: cardinality,
            conflicting: conflicting.len(),
        }),
    }
}

/// Supersede `old_id` with `new_relation`.
pub fn relation_supersede(
    wtxn: &WriteTransaction,
    _scope: RowScope,
    old_id: RelationId,
    new_relation: &Relation,
    now_unix_nanos: u64,
) -> Result<RelationId, RelationOpError> {
    if new_relation.confidence.is_nan() || !(0.0..=1.0).contains(&new_relation.confidence) {
        return Err(RelationOpError::InvalidArgument(
            "confidence must be in [0, 1] and not NaN",
        ));
    }

    let mut old = {
        let t = wtxn.open_table(RELATION_METADATA_TABLE)?;
        let row = t.get(&old_id.to_bytes())?.map(|g| g.value());
        row.ok_or(RelationOpError::NotFound(old_id))?
    };
    // The old row's owning scope is authoritative; the new row + evidence
    // rows inherit it (a supersede can never re-home a relation).
    let scope = old.scope();
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
    {
        let t = wtxn.open_table(RELATION_METADATA_TABLE)?;
        if t.get(&new_relation.id.to_bytes())?.is_some() {
            return Err(RelationOpError::AlreadyExists(new_relation.id));
        }
    }

    let chain_root_bytes = if old.supersedes_bytes.is_none() {
        old_id.to_bytes()
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

    old.superseded_by_bytes = Some(new_to_insert.id.to_bytes());
    if old.valid_to_unix_nanos.is_none() {
        old.valid_to_unix_nanos = Some(new_to_insert.extracted_at_unix_nanos);
    }
    old.is_current = 0;

    {
        let mut t = wtxn.open_table(RELATION_METADATA_TABLE)?;
        t.insert(&old_id.to_bytes(), &old)?;
    }

    insert_new_relation(wtxn, scope, &new_to_insert, now_unix_nanos)?;
    Ok(new_to_insert.id)
}

/// Soft delete. Flips the sidecar `tombstoned` + `is_current` bits.
/// The unified edge row stays — tombstone is a sidecar property.
pub fn relation_tombstone(
    wtxn: &WriteTransaction,
    id: RelationId,
    now_unix_nanos: u64,
) -> Result<(), RelationOpError> {
    let mut row = {
        let t = wtxn.open_table(RELATION_METADATA_TABLE)?;
        let row = t.get(&id.to_bytes())?.map(|g| g.value());
        row.ok_or(RelationOpError::NotFound(id))?
    };
    if row.is_tombstoned() {
        return Ok(());
    }
    row.tombstoned = 1;
    row.tombstoned_at_unix_nanos = Some(now_unix_nanos);
    row.is_current = 0;
    {
        let mut t = wtxn.open_table(RELATION_METADATA_TABLE)?;
        t.insert(&id.to_bytes(), &row)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers.
// ---------------------------------------------------------------------------

fn require_entity_exists(wtxn: &WriteTransaction, id: EntityId) -> Result<(), RelationOpError> {
    use crate::tables::entity::{EntityMetadata, ENTITIES_TABLE};
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
    use crate::tables::relation_type::{RelationTypeDefinition, RELATION_TYPES_TABLE};
    let t = wtxn.open_table(RELATION_TYPES_TABLE)?;
    let row: Option<RelationTypeDefinition> = t.get(&id.raw())?.map(|g| g.value());
    let row = row.ok_or(RelationOpError::UnknownRelationType(id))?;
    let cardinality = Cardinality::from_u8(row.cardinality).ok_or(
        RelationOpError::InvalidArgument("relation type has unknown cardinality"),
    )?;
    Ok((cardinality, row.is_symmetric != 0))
}

/// Cardinality probe.
///
/// For each side covered by `cardinality`, walk every current Typed
/// edge of `r.relation_type` anchored at that endpoint and collect
/// the conflicting [`RelationId`]s. Caller resolves single-conflict
/// → auto-supersede and multi-conflict → `CardinalityViolation`.
fn find_cardinality_conflicts(
    wtxn: &WriteTransaction,
    scope: RowScope,
    r: &Relation,
    cardinality: Cardinality,
) -> Result<Vec<RelationId>, RelationOpError> {
    let mut found: Vec<RelationId> = Vec::new();

    let want_from = matches!(cardinality, Cardinality::ManyToOne | Cardinality::OneToOne);
    let want_to = matches!(cardinality, Cardinality::OneToMany | Cardinality::OneToOne);

    if want_from {
        collect_typed_conflicts(
            wtxn,
            scope,
            NodeRef::Entity(r.from_entity),
            r.relation_type,
            /* outgoing */ true,
            r.id,
            &mut found,
        )?;
    }
    if want_to {
        collect_typed_conflicts(
            wtxn,
            scope,
            NodeRef::Entity(r.to_entity),
            r.relation_type,
            /* outgoing */ false,
            r.id,
            &mut found,
        )?;
    }
    Ok(found)
}

#[allow(clippy::too_many_arguments)]
fn collect_typed_conflicts(
    wtxn: &WriteTransaction,
    scope: RowScope,
    anchor: NodeRef,
    rel_type: RelationTypeId,
    outgoing: bool,
    new_id: RelationId,
    out: &mut Vec<RelationId>,
) -> Result<(), RelationOpError> {
    let kind_filter = Some(EdgeKindRef::Typed(rel_type));
    // walk_outgoing / walk_incoming take a read transaction; reopen
    // the relevant table on the write txn and range-scan directly.
    let key_prefix = anchor.to_bytes().to_vec();
    let mut prefix_with_kind = key_prefix.clone();
    EdgeKindRef::Typed(rel_type).encode_into(&mut prefix_with_kind);
    let mut hi = prefix_with_kind.clone();
    hi.extend_from_slice(&[0xFF; 17 + 16]);

    let table = if outgoing {
        wtxn.open_table(EDGES_TABLE)?
    } else {
        wtxn.open_table(EDGES_REVERSE_TABLE)?
    };
    let sidecar = wtxn.open_table(RELATION_METADATA_TABLE)?;
    for entry in table.range::<&[u8]>(prefix_with_kind.as_slice()..=hi.as_slice())? {
        let (k, _) = entry?;
        let key = edge::EdgeKey::decode(k.value())?;
        if key.from != anchor {
            continue;
        }
        if !matches!(key.kind, EdgeKindRef::Typed(rt) if rt == rel_type) {
            continue;
        }
        let candidate = RelationId::from(key.disambiguator);
        if candidate == new_id {
            continue;
        }
        let Some(meta) = sidecar.get(&key.disambiguator)?.map(|g| g.value()) else {
            continue;
        };
        // Cardinality conflicts are scoped: an `acme` relation must not
        // be superseded by (or block) a `globex` one. The shared edge
        // table can surface a foreign-tenant edge; the sidecar scope is
        // the wall.
        if meta.namespace_id != scope.namespace_id || meta.agent_id_bytes != scope.agent_id_bytes {
            continue;
        }
        if meta.is_current() && !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    // Suppress kind_filter unused warning — it's documentation that
    // we expect typed edges here.
    let _ = kind_filter;
    Ok(())
}

/// Insert a fresh relation row.
///
/// Writes:
/// 1. one row to `EDGES_TABLE` + reverse mirror keyed by
///    `(from, Typed(rt_id), to, RelationId.bytes)`
/// 2. one row to `RELATION_METADATA_TABLE`
/// 3. one row to `RELATION_BY_EVIDENCE_TABLE` per evidence MemoryId
fn insert_new_relation(
    wtxn: &WriteTransaction,
    scope: RowScope,
    r: &Relation,
    now_unix_nanos: u64,
) -> Result<(), RelationOpError> {
    // Edge row(s). Symmetric typed relations write the mirror
    // explicitly here — `edge::link`'s auto-mirror is reserved for
    // substrate `Builtin` kinds. Mirroring at the relation layer
    // keeps the `is_symmetric` bit colocated with the rest of the
    // sidecar metadata.
    {
        let mut edges = wtxn.open_table(EDGES_TABLE)?;
        let mut reverse = wtxn.open_table(EDGES_REVERSE_TABLE)?;
        let data = EdgeData::new(
            1.0,
            origin::AUTO_DERIVED,
            derived_by::CLIENT,
            now_unix_nanos,
        );
        edge::link(
            &mut edges,
            &mut reverse,
            NodeRef::Entity(r.from_entity),
            EdgeKindRef::Typed(r.relation_type),
            NodeRef::Entity(r.to_entity),
            r.id.to_bytes(),
            &data,
        )?;
        if r.is_symmetric && r.from_entity != r.to_entity {
            edge::link(
                &mut edges,
                &mut reverse,
                NodeRef::Entity(r.to_entity),
                EdgeKindRef::Typed(r.relation_type),
                NodeRef::Entity(r.from_entity),
                r.id.to_bytes(),
                &data,
            )?;
        }
    }

    // Sidecar — carries the owning scope.
    let m = metadata_from_relation(r, scope);
    {
        let mut t = wtxn.open_table(RELATION_METADATA_TABLE)?;
        t.insert(&r.id.to_bytes(), &m)?;
    }

    // Evidence reverse index.
    {
        let mut t = wtxn.open_table(RELATION_BY_EVIDENCE_TABLE)?;
        for mem in &r.evidence {
            t.insert(
                &(
                    scope.namespace_id,
                    scope.agent_id_bytes,
                    mem.to_be_bytes(),
                    r.id.to_bytes(),
                ),
                &(),
            )?;
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
    use crate::entity::ops::{entity_put, normalize_name};
    use crate::relation::types::relation_type_intern;
    use brain_core::ExtractorId;
    use brain_core::{Entity, EntityType};
    fn test_scope() -> RowScope {
        RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
    }

    fn open_db() -> (tempfile::TempDir, crate::MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    fn make_entity(db: &mut crate::MetadataDb, name: &str) -> EntityId {
        let id = EntityId::new();
        let n = normalize_name(name);
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            n,
            1_700_000_000_000_000_000,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn make_entity_with(db: &mut crate::MetadataDb, id: EntityId, name: &str) -> EntityId {
        let n = normalize_name(name);
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            n,
            1_700_000_000_000_000_000,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), &e).unwrap();
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

    #[test]
    fn create_asymmetric_round_trips() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a");
        let b = make_entity(&mut db, "b");
        let t = intern_type(&mut db, "knows", Cardinality::ManyToMany, false);
        let r = fresh_rel(t, a, b, false);

        let wtxn = db.write_txn().unwrap();
        let id = relation_create(&wtxn, test_scope(), &r, 0).unwrap();
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
        let r = fresh_rel(t, a, b, true);

        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), &r, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = relation_get(&rtxn, r.id).unwrap().unwrap();
        assert_eq!(got.from_entity, b);
        assert_eq!(got.to_entity, a);
        assert!(got.is_symmetric);
    }

    #[test]
    fn create_self_loop_allowed() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "self");
        let t = intern_type(&mut db, "knows_self", Cardinality::ManyToMany, false);
        let r = fresh_rel(t, a, a, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), &r, 0).unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn create_unknown_relation_type_rejected() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-ut");
        let b = make_entity(&mut db, "b-ut");
        let r = fresh_rel(RelationTypeId::from(9999), a, b, false);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, test_scope(), &r, 0).unwrap_err();
        assert!(matches!(err, RelationOpError::UnknownRelationType(_)));
    }

    #[test]
    fn create_unknown_endpoint_rejected() {
        let (_dir, mut db) = open_db();
        let t = intern_type(&mut db, "knows_ue", Cardinality::ManyToMany, false);
        let r = fresh_rel(t, EntityId::new(), EntityId::new(), false);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, test_scope(), &r, 0).unwrap_err();
        assert!(matches!(err, RelationOpError::UnknownEntity(_)));
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
        relation_create(&wtxn, test_scope(), &r1, 0).unwrap();
        wtxn.commit().unwrap();

        let r2 = fresh_rel(t, priya, bob, false);
        let wtxn = db.write_txn().unwrap();
        let result_id = relation_create(&wtxn, test_scope(), &r2, 1).unwrap();
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
        let t = intern_type(&mut db, "employed_by", Cardinality::OneToMany, false);

        let r1 = fresh_rel(t, priya, acme, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), &r1, 0).unwrap();
        wtxn.commit().unwrap();

        let new_employee = make_entity(&mut db, "new-employee");
        let r2 = fresh_rel(t, new_employee, acme, false);
        let wtxn = db.write_txn().unwrap();
        let result_id = relation_create(&wtxn, test_scope(), &r2, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = relation_get(&rtxn, r1.id).unwrap().unwrap();
        assert_eq!(g1.superseded_by, Some(result_id));
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
        relation_create(&wtxn, test_scope(), &r1, 0).unwrap();
        relation_create(&wtxn, test_scope(), &r2, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = relation_get(&rtxn, r1.id).unwrap().unwrap();
        let g2 = relation_get(&rtxn, r2.id).unwrap().unwrap();
        assert!(g1.superseded_by.is_none());
        assert!(g2.superseded_by.is_none());
    }

    #[test]
    fn open_vocab_relation_accumulates_as_set() {
        // The extraction apply path mints open-vocab relation types via
        // `relation_type_intern_or_get`, which defaults to ManyToMany. A
        // subject linked to several distinct places (e.g. several `camped_at`
        // sites) must therefore yield several CURRENT rows — none superseded —
        // so both enumerate and count read off the same live set.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "camper");
        let p1 = make_entity(&mut db, "lake");
        let p2 = make_entity(&mut db, "ridge");
        let p3 = make_entity(&mut db, "valley");

        let t = {
            let wtxn = db.write_txn().unwrap();
            let t = crate::relation::types::relation_type_intern_or_get(
                &wtxn, "brain", "camped_at", 0, 1,
            )
            .unwrap();
            wtxn.commit().unwrap();
            t
        };

        for (lsn, place) in [p1, p2, p3].into_iter().enumerate() {
            let r = fresh_rel(t, subj, place, false);
            let wtxn = db.write_txn().unwrap();
            relation_create(&wtxn, test_scope(), &r, lsn as u64).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let filter = RelationListFilter {
            current_only: true,
            ..Default::default()
        };
        let current = relation_list_from(&rtxn, test_scope(), subj, &filter).unwrap();
        // All three places remain current — enumerate + count both work.
        assert_eq!(current.len(), 3);
        let tos: Vec<_> = current.iter().map(|r| r.to_entity).collect();
        assert!(tos.contains(&p1));
        assert!(tos.contains(&p2));
        assert!(tos.contains(&p3));
    }

    #[test]
    fn tombstone_drops_from_current_listing() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "ta");
        let b = make_entity(&mut db, "tb");
        let t = intern_type(&mut db, "knows_t", Cardinality::ManyToMany, false);
        let r = fresh_rel(t, a, b, false);

        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), &r, 0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        relation_tombstone(&wtxn, r.id, 999).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let filter = RelationListFilter {
            current_only: true,
            ..Default::default()
        };
        let from_a = relation_list_from(&rtxn, test_scope(), a, &filter).unwrap();
        assert!(from_a.is_empty(), "current_only excludes tombstoned");

        let filter_all = RelationListFilter::default();
        let from_a_all = relation_list_from(&rtxn, test_scope(), a, &filter_all).unwrap();
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
        relation_create(&wtxn, test_scope(), &r1, 0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        relation_tombstone(&wtxn, r1.id, 1).unwrap();
        wtxn.commit().unwrap();

        let r2 = fresh_rel(t, priya, bob, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), &r2, 2).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = relation_get(&rtxn, r1.id).unwrap().unwrap();
        let g2 = relation_get(&rtxn, r2.id).unwrap().unwrap();
        assert!(g1.tombstoned);
        assert!(g2.supersedes.is_none(), "tombstoned slot frees cardinality");
    }

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
        relation_create(&wtxn, test_scope(), &r1, 0).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), &r2, 1).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), &r3, 2).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let chain = relation_history(&rtxn, r1.id).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id, r1.id);
        assert_eq!(chain[1].id, r2.id);
        assert_eq!(chain[2].id, r3.id);
    }

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
        relation_create(&wtxn, test_scope(), &r1, 0).unwrap();
        relation_create(&wtxn, test_scope(), &r2, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let filter = RelationListFilter {
            relation_type: Some(t1),
            current_only: true,
            ..Default::default()
        };
        let out = relation_list_from(&rtxn, test_scope(), a, &filter).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation_type, t1);
    }

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
        relation_create(&wtxn, test_scope(), &r, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let deps = relations_with_evidence(&rtxn, test_scope(), mem).unwrap();
        assert_eq!(deps, vec![r.id]);
    }

    #[test]
    fn relation_list_to_finds_incoming() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "lt-a");
        let b = make_entity(&mut db, "lt-b");
        let t = intern_type(&mut db, "lt_type", Cardinality::ManyToMany, false);

        let r = fresh_rel(t, a, b, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), &r, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let filter = RelationListFilter::default();
        let to_b = relation_list_to(&rtxn, test_scope(), b, &filter).unwrap();
        assert_eq!(to_b.len(), 1);
        assert_eq!(to_b[0].id, r.id);
    }
}
