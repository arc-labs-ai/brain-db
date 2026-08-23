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
    Cardinality, EdgeKindRef, EntityId, EntityTypeId, MemoryId, NodeRef, RelationId, RelationTypeId,
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

    /// The relation type declares a concrete `from` / `to` entity type
    /// and the supplied endpoint has a different one. `side` is
    /// `"from"` or `"to"`.
    #[error(
        "entity type violation on {side}: relation type requires {expected:?} but entity {entity:?} is {actual:?}"
    )]
    EndpointTypeViolation {
        side: &'static str,
        entity: EntityId,
        expected: EntityTypeId,
        actual: EntityTypeId,
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
    let anchor_space = anchor_row.space_id_bytes;

    // Linear scan: chains are short (1–3 entries typical). A
    // chain-root secondary index can be added if this becomes hot.
    let mut chain = Vec::new();
    for entry in t.iter()? {
        let (k, v) = entry?;
        let m = v.value();
        if m.chain_root_bytes == chain_root_bytes
            && m.namespace_id == anchor_ns
            && m.space_id_bytes == anchor_space
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
        if meta.namespace_id != scope.namespace_id || meta.space_id_bytes != scope.space_id_bytes {
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
        scope.space_id_bytes,
        mem_bytes,
        [0u8; 16],
    );
    let hi = (
        scope.namespace_id,
        scope.space_id_bytes,
        mem_bytes,
        [0xFFu8; 16],
    );
    let mut out = Vec::new();
    for entry in t.range(lo..=hi)? {
        let (k, _) = entry?;
        let (k_ns, k_space, k_mem, k_rel) = k.value();
        if k_ns != scope.namespace_id || k_space != scope.space_id_bytes || k_mem != mem_bytes {
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
/// Endpoints are validated against the entity types the relation type
/// declares. An inverted relation — one whose endpoints are the exact
/// mirror image of a concretely declared `from` / `to` pair — is
/// corrected by exchanging them (and logged); any other violation is
/// rejected with [`RelationOpError::EndpointTypeViolation`]. See
/// `resolve_endpoint_direction`.
pub fn relation_create(
    wtxn: &WriteTransaction,
    scope: RowScope,
    session: brain_core::SessionId,
    r: &Relation,
    now_unix_nanos: u64,
) -> Result<RelationId, RelationOpError> {
    if r.confidence.is_nan() || !(0.0..=1.0).contains(&r.confidence) {
        return Err(RelationOpError::InvalidArgument(
            "confidence must be in [0, 1] and not NaN",
        ));
    }

    let from_entity_type = require_entity_exists(wtxn, r.from_entity)?;
    let to_entity_type = require_entity_exists(wtxn, r.to_entity)?;
    let declared = lookup_type(wtxn, r.relation_type)?;
    let (cardinality, is_symmetric) = (declared.cardinality, declared.is_symmetric);

    // Declared endpoint types are enforced on write, exactly like the
    // declared cardinality below. Checked before the symmetric
    // canonicalisation so `from` / `to` still mean what the caller
    // said. A relation type with no declared type (`Any`, or an
    // implicit-from-write type) constrains nothing.
    let direction = resolve_endpoint_direction(r, from_entity_type, to_entity_type, &declared)?;

    {
        let t = wtxn.open_table(RELATION_METADATA_TABLE)?;
        if t.get(&r.id.to_bytes())?.is_some() {
            return Err(RelationOpError::AlreadyExists(r.id));
        }
    }

    let mut to_insert = r.clone();
    if direction == EndpointDirection::Swapped {
        // Extraction inverts relation direction often enough that the
        // declared types are the only signal that can catch it. A swap
        // rewrites what the caller asked for, so it is never silent.
        tracing::warn!(
            target: "brain_metadata::relation",
            relation = ?r.id,
            relation_type = ?r.relation_type,
            from_entity = ?r.from_entity,
            to_entity = ?r.to_entity,
            from_actual_type = ?from_entity_type,
            to_actual_type = ?to_entity_type,
            declared_from_type = ?declared.from_type,
            declared_to_type = ?declared.to_type,
            "relation_create: endpoints violate declared from/to types but conform when \
             exchanged; swapping from/to"
        );
        std::mem::swap(&mut to_insert.from_entity, &mut to_insert.to_entity);
    }
    to_insert.is_symmetric = is_symmetric;
    if is_symmetric {
        let (a, b) = canonical_pair(to_insert.from_entity, to_insert.to_entity);
        to_insert.from_entity = a;
        to_insert.to_entity = b;
    }

    // Content dedup: a transient LLM failure makes the extractor worker
    // re-run the whole pipeline and re-apply the already-committed
    // pattern-tier rows. Cardinality supersession only fires on the sides
    // a `single`/`One*` cardinality constrains, so a `ManyToMany` (or the
    // unconstrained side of a `*ToMany`) tuple has nothing to catch a
    // byte-identical repeat and would leak a duplicate `(from, type, to)`
    // edge. Skip the insert and return the existing id — a no-op. Distinct
    // Many tuples (different `from`/`to`) never match here, so legitimate
    // multi-values are preserved.
    if let Some(existing) = find_identical_active_relation(wtxn, scope, &to_insert)? {
        return Ok(existing);
    }

    let conflicting = find_cardinality_conflicts(wtxn, scope, &to_insert, cardinality)?;
    match conflicting.len() {
        0 => {
            insert_new_relation(wtxn, scope, session, &to_insert, now_unix_nanos)?;
            Ok(to_insert.id)
        }
        1 => relation_supersede(
            wtxn,
            scope,
            session,
            conflicting[0],
            &to_insert,
            now_unix_nanos,
        ),
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
    session: brain_core::SessionId,
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

    insert_new_relation(wtxn, scope, session, &new_to_insert, now_unix_nanos)?;
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

/// Assert the endpoint entity exists and return its entity type.
fn require_entity_exists(
    wtxn: &WriteTransaction,
    id: EntityId,
) -> Result<EntityTypeId, RelationOpError> {
    use crate::tables::entity::{EntityMetadata, ENTITIES_TABLE};
    let t = wtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    let row = row.ok_or(RelationOpError::UnknownEntity(id))?;
    Ok(EntityTypeId::from(row.entity_type_id))
}

/// The write-time constraints a relation type declares.
struct DeclaredConstraints {
    cardinality: Cardinality,
    is_symmetric: bool,
    /// `None` = declared `Any` (or an implicit-from-write type): the
    /// endpoint may be any entity type.
    from_type: Option<EntityTypeId>,
    to_type: Option<EntityTypeId>,
}

fn lookup_type(
    wtxn: &WriteTransaction,
    id: RelationTypeId,
) -> Result<DeclaredConstraints, RelationOpError> {
    use crate::tables::relation_type::{
        decode_entity_type_id, RelationTypeDefinition, RELATION_TYPES_TABLE,
    };
    let t = wtxn.open_table(RELATION_TYPES_TABLE)?;
    let row: Option<RelationTypeDefinition> = t.get(&id.raw())?.map(|g| g.value());
    let row = row.ok_or(RelationOpError::UnknownRelationType(id))?;
    let cardinality = Cardinality::from_u8(row.cardinality).ok_or(
        RelationOpError::InvalidArgument("relation type has unknown cardinality"),
    )?;
    Ok(DeclaredConstraints {
        cardinality,
        is_symmetric: row.is_symmetric != 0,
        from_type: decode_entity_type_id(row.from_entity_type_id),
        to_type: decode_entity_type_id(row.to_entity_type_id),
    })
}

/// Which way round the endpoints go into storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndpointDirection {
    /// The supplied endpoints satisfy the declared types.
    AsGiven,
    /// The supplied endpoints violate the declared types, and
    /// exchanging them satisfies both.
    Swapped,
}

/// Validate the endpoints against the entity types the relation type
/// declares, and decide whether an inverted relation should be
/// corrected rather than rejected.
///
/// `declared == None` (the `Any` sentinel) is fully permissive — the
/// seeded `brain:` relation types rely on it.
///
/// The swap rule is deliberately narrow:
///
/// - Both declared sides must be concrete. An `Any` side carries no
///   directional information, so it can never be the evidence that a
///   relation is inverted: with `from: Person / to: Any`, every
///   `(NonPerson, Person)` pair would become "fixable", and a correctly
///   directed relation whose subject merely has the wrong type would be
///   silently reversed. When one side declares nothing, there is no
///   well-defined "right way round" to swap into, so the violation is
///   reported instead.
/// - The exchange must satisfy *both* declared sides. A partial
///   improvement (one side would now match, the other still would not)
///   is not a direction error and is rejected as before.
///
/// Together these make the swap unambiguous: it fires only when the
/// declared pair is concrete, asymmetric in type, and the supplied
/// endpoints are exactly its mirror image.
///
/// The check runs *before* the symmetric `canonical_pair`
/// canonicalisation, so `from` / `to` still mean what the caller said.
/// The two compose without double-swapping: `canonical_pair` sorts by
/// [`EntityId`] and is therefore order-insensitive, so for a symmetric
/// relation type the stored row is byte-identical whether or not the
/// swap fired — the swap only turns a would-be rejection into an
/// accepted write, which is the correct outcome for a relation type
/// whose endpoints are interchangeable by definition.
fn resolve_endpoint_direction(
    r: &Relation,
    from_actual: EntityTypeId,
    to_actual: EntityTypeId,
    declared: &DeclaredConstraints,
) -> Result<EndpointDirection, RelationOpError> {
    let from_ok = declared.from_type.is_none_or(|want| want == from_actual);
    let to_ok = declared.to_type.is_none_or(|want| want == to_actual);
    if from_ok && to_ok {
        return Ok(EndpointDirection::AsGiven);
    }

    if let (Some(want_from), Some(want_to)) = (declared.from_type, declared.to_type) {
        if want_from == to_actual && want_to == from_actual {
            return Ok(EndpointDirection::Swapped);
        }
    }

    // Not fixable by exchange — report the violating side, `from` first
    // so the error is deterministic when both sides are wrong.
    let (side, entity, expected, actual) = if from_ok {
        ("to", r.to_entity, declared.to_type, to_actual)
    } else {
        ("from", r.from_entity, declared.from_type, from_actual)
    };
    Err(RelationOpError::EndpointTypeViolation {
        side,
        entity,
        expected: expected.expect("invariant: the violating side has a concrete declared type"),
        actual,
    })
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

/// Find a byte-identical active relation for the exact `(from_entity,
/// relation_type, to_entity)` tuple, regardless of cardinality.
///
/// Unlike [`find_cardinality_conflicts`], this matches on the full tuple
/// (both endpoints), so it only ever fires on an exact repeat — never on a
/// legitimately-distinct Many-cardinality edge that shares one endpoint.
/// Used to make relation creation idempotent under extractor retries.
fn find_identical_active_relation(
    wtxn: &WriteTransaction,
    scope: RowScope,
    r: &Relation,
) -> Result<Option<RelationId>, RelationOpError> {
    let anchor = NodeRef::Entity(r.from_entity);
    let target = NodeRef::Entity(r.to_entity);

    let mut prefix = anchor.to_bytes().to_vec();
    EdgeKindRef::Typed(r.relation_type).encode_into(&mut prefix);
    let mut hi = prefix.clone();
    hi.extend_from_slice(&[0xFF; 17 + 16]);

    let table = wtxn.open_table(EDGES_TABLE)?;
    let sidecar = wtxn.open_table(RELATION_METADATA_TABLE)?;
    for entry in table.range::<&[u8]>(prefix.as_slice()..=hi.as_slice())? {
        let (k, _) = entry?;
        let key = edge::EdgeKey::decode(k.value())?;
        if key.from != anchor {
            continue;
        }
        if !matches!(key.kind, EdgeKindRef::Typed(rt) if rt == r.relation_type) {
            continue;
        }
        if key.to != target {
            continue;
        }
        let candidate = RelationId::from(key.disambiguator);
        if candidate == r.id {
            continue;
        }
        let Some(meta) = sidecar.get(&key.disambiguator)?.map(|g| g.value()) else {
            continue;
        };
        // Same scope wall as the cardinality probe — a foreign-tenant edge
        // surfaced by the shared table is not a duplicate of this one.
        if meta.namespace_id != scope.namespace_id || meta.space_id_bytes != scope.space_id_bytes {
            continue;
        }
        if meta.is_current() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
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
        if meta.namespace_id != scope.namespace_id || meta.space_id_bytes != scope.space_id_bytes {
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
    session: brain_core::SessionId,
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

    // Sidecar — carries the owning scope + the per-utterance session
    // (stamped after building; the brain-core `Relation` has no session
    // slot). Session is a grouping column, never a key.
    let mut m = metadata_from_relation(r, scope);
    m.session_id = session.raw();
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
                    scope.space_id_bytes,
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
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    /// Insert an entity of an arbitrary entity type (the `make_entity`
    /// helper always makes a Person).
    fn make_entity_typed(
        db: &mut crate::MetadataDb,
        name: &str,
        entity_type: brain_core::EntityTypeId,
    ) -> EntityId {
        let id = EntityId::new();
        let n = normalize_name(name);
        let e = Entity::new_active(id, entity_type, name.into(), n, 1_700_000_000_000_000_000);
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    /// Insert an entity with both a chosen id and a chosen entity type.
    fn make_entity_typed_with(
        db: &mut crate::MetadataDb,
        id: EntityId,
        name: &str,
        entity_type: brain_core::EntityTypeId,
    ) -> EntityId {
        let n = normalize_name(name);
        let e = Entity::new_active(id, entity_type, name.into(), n, 1_700_000_000_000_000_000);
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
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
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &e).unwrap();
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

    /// Intern a relation type that declares concrete `from` / `to`
    /// entity types.
    fn intern_typed_endpoints(
        db: &mut crate::MetadataDb,
        name: &str,
        from_type: brain_core::EntityTypeId,
        to_type: brain_core::EntityTypeId,
    ) -> RelationTypeId {
        let wtxn = db.write_txn().unwrap();
        let id = relation_type_intern(
            &wtxn,
            "test",
            name,
            Some(from_type),
            Some(to_type),
            Cardinality::ManyToMany,
            false,
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
        let id =
            relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
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
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
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
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn create_unknown_relation_type_rejected() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-ut");
        let b = make_entity(&mut db, "b-ut");
        let r = fresh_rel(RelationTypeId::from(9999), a, b, false);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0)
            .unwrap_err();
        assert!(matches!(err, RelationOpError::UnknownRelationType(_)));
    }

    #[test]
    fn create_unknown_endpoint_rejected() {
        let (_dir, mut db) = open_db();
        let t = intern_type(&mut db, "knows_ue", Cardinality::ManyToMany, false);
        let r = fresh_rel(t, EntityId::new(), EntityId::new(), false);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0)
            .unwrap_err();
        assert!(matches!(err, RelationOpError::UnknownEntity(_)));
    }

    // ----- Declared from / to entity type enforcement. -----

    const ORGANIZATION_ID: brain_core::EntityTypeId = brain_core::EntityTypeId(2);

    /// Intern a relation type with per-side control over the declared
    /// entity types and the symmetric flag.
    #[allow(clippy::too_many_arguments)]
    fn intern_endpoints_full(
        db: &mut crate::MetadataDb,
        name: &str,
        from_type: Option<brain_core::EntityTypeId>,
        to_type: Option<brain_core::EntityTypeId>,
        symmetric: bool,
    ) -> RelationTypeId {
        let wtxn = db.write_txn().unwrap();
        let id = relation_type_intern(
            &wtxn,
            "test",
            name,
            from_type,
            to_type,
            Cardinality::ManyToMany,
            symmetric,
            1,
            "",
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    #[test]
    fn declared_endpoint_types_accept_conforming_relation() {
        let (_dir, mut db) = open_db();
        let person = make_entity(&mut db, "conforming-person");
        let org = make_entity_typed(&mut db, "conforming-org", ORGANIZATION_ID);
        let t = intern_typed_endpoints(
            &mut db,
            "works_at_ok",
            EntityType::PERSON_ID,
            ORGANIZATION_ID,
        );

        let r = fresh_rel(t, person, org, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
        wtxn.commit().unwrap();

        // Conforming endpoints are stored exactly as supplied — the
        // direction defence must never fire on a correct relation.
        let rtxn = db.read_txn().unwrap();
        let got = relation_get(&rtxn, r.id).unwrap().unwrap();
        assert_eq!(got.from_entity, person);
        assert_eq!(got.to_entity, org);
    }

    // ----- Auto-swap of inverted relations. -----

    #[test]
    fn inverted_endpoints_are_swapped_not_rejected() {
        // The published mitigation for extractor direction errors: when
        // the violation is fixable by exchanging subject and object,
        // exchange them.
        let (_dir, mut db) = open_db();
        let person = make_entity(&mut db, "inverted-person");
        let org = make_entity_typed(&mut db, "inverted-org", ORGANIZATION_ID);
        let t = intern_typed_endpoints(
            &mut db,
            "works_at_swap",
            EntityType::PERSON_ID,
            ORGANIZATION_ID,
        );

        // Supplied backwards: (Organization, Person).
        let r = fresh_rel(t, org, person, false);
        let wtxn = db.write_txn().unwrap();
        let id =
            relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(id, r.id, "the swap keeps the caller's relation id");

        let rtxn = db.read_txn().unwrap();
        let got = relation_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(got.from_entity, person, "from is now the Person");
        assert_eq!(got.to_entity, org, "to is now the Organization");

        // …and the corrected relation is reachable from the corrected
        // side of the unified edge table, not the supplied one.
        let out = relation_list_from(
            &rtxn,
            test_scope(),
            person,
            &RelationListFilter {
                relation_type: Some(t),
                current_only: true,
                limit: 0,
            },
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, id);
    }

    #[test]
    fn partial_match_does_not_swap() {
        // from conforms, to does not, and exchanging would break the
        // side that currently conforms. Not a direction error — reject.
        let (_dir, mut db) = open_db();
        let person = make_entity(&mut db, "partial-person");
        let other_person = make_entity(&mut db, "partial-person-2");
        let t = intern_typed_endpoints(
            &mut db,
            "works_at_partial",
            EntityType::PERSON_ID,
            ORGANIZATION_ID,
        );

        let r = fresh_rel(t, person, other_person, false);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0)
            .unwrap_err();
        assert!(matches!(
            err,
            RelationOpError::EndpointTypeViolation { side: "to", .. }
        ));
        drop(wtxn);

        let rtxn = db.read_txn().unwrap();
        assert!(relation_get(&rtxn, r.id).unwrap().is_none());
    }

    #[test]
    fn unfixable_violation_still_rejected() {
        // Neither side matches and exchanging fixes neither.
        let (_dir, mut db) = open_db();
        let place_a = make_entity_typed(&mut db, "unfixable-a", brain_core::EntityTypeId(5));
        let place_b = make_entity_typed(&mut db, "unfixable-b", brain_core::EntityTypeId(5));
        let t = intern_typed_endpoints(
            &mut db,
            "works_at_unfixable",
            EntityType::PERSON_ID,
            ORGANIZATION_ID,
        );

        let r = fresh_rel(t, place_a, place_b, false);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0)
            .unwrap_err();
        match err {
            RelationOpError::EndpointTypeViolation {
                side,
                entity,
                expected,
                actual,
            } => {
                assert_eq!(side, "from", "both sides wrong reports `from` first");
                assert_eq!(entity, place_a);
                assert_eq!(expected, EntityType::PERSON_ID);
                assert_eq!(actual, brain_core::EntityTypeId(5));
            }
            other => panic!("expected EndpointTypeViolation, got {other:?}"),
        }
    }

    #[test]
    fn any_side_never_grounds_a_swap() {
        // `from: Person / to: Any`. An `Any` side carries no directional
        // information, so a from-side violation is not evidence of an
        // inversion — reject rather than silently reverse the caller.
        let (_dir, mut db) = open_db();
        let person = make_entity(&mut db, "any-side-person");
        let org = make_entity_typed(&mut db, "any-side-org", ORGANIZATION_ID);
        let t = intern_endpoints_full(
            &mut db,
            "half_declared",
            Some(EntityType::PERSON_ID),
            None,
            false,
        );

        let r = fresh_rel(t, org, person, false);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0)
            .unwrap_err();
        assert!(matches!(
            err,
            RelationOpError::EndpointTypeViolation { side: "from", .. }
        ));
        drop(wtxn);

        let rtxn = db.read_txn().unwrap();
        assert!(relation_get(&rtxn, r.id).unwrap().is_none());
    }

    #[test]
    fn swap_composes_with_symmetric_canonicalisation() {
        // The type check runs before `canonical_pair`. Because
        // `canonical_pair` sorts by id it is order-insensitive, so the
        // swap can never double-apply: the stored pair is the canonical
        // one either way, and the only effect of the swap is that the
        // write is accepted at all.
        let (_dir, mut db) = open_db();
        // Ids chosen so canonical (ascending) order puts the Org first —
        // i.e. the opposite of the declared from/to order.
        let org = make_entity_typed_with(
            &mut db,
            EntityId::from([1u8; 16]),
            "sym-org",
            ORGANIZATION_ID,
        );
        let person = make_entity_typed_with(
            &mut db,
            EntityId::from([2u8; 16]),
            "sym-person",
            EntityType::PERSON_ID,
        );
        let t = intern_endpoints_full(
            &mut db,
            "sym_works_at",
            Some(EntityType::PERSON_ID),
            Some(ORGANIZATION_ID),
            true,
        );

        // Supplied backwards relative to the declaration.
        let r = fresh_rel(t, org, person, true);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = relation_get(&rtxn, r.id).unwrap().unwrap();
        let (a, b) = canonical_pair(person, org);
        assert_eq!((got.from_entity, got.to_entity), (a, b));
        assert!(got.is_symmetric);
    }

    #[test]
    fn declared_from_type_rejects_mismatch() {
        let (_dir, mut db) = open_db();
        let org_as_from = make_entity_typed(&mut db, "bad-from-org", ORGANIZATION_ID);
        let org = make_entity_typed(&mut db, "target-org", ORGANIZATION_ID);
        let t = intern_typed_endpoints(
            &mut db,
            "works_at_from",
            EntityType::PERSON_ID,
            ORGANIZATION_ID,
        );

        let r = fresh_rel(t, org_as_from, org, false);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0)
            .unwrap_err();
        match err {
            RelationOpError::EndpointTypeViolation {
                side,
                entity,
                expected,
                actual,
            } => {
                assert_eq!(side, "from");
                assert_eq!(entity, org_as_from);
                assert_eq!(expected, EntityType::PERSON_ID);
                assert_eq!(actual, ORGANIZATION_ID);
            }
            other => panic!("expected EndpointTypeViolation, got {other:?}"),
        }
    }

    #[test]
    fn declared_to_type_rejects_mismatch() {
        let (_dir, mut db) = open_db();
        let person = make_entity(&mut db, "bad-to-person");
        let other_person = make_entity(&mut db, "person-as-org");
        let t = intern_typed_endpoints(
            &mut db,
            "works_at_to",
            EntityType::PERSON_ID,
            ORGANIZATION_ID,
        );

        let r = fresh_rel(t, person, other_person, false);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0)
            .unwrap_err();
        assert!(matches!(
            err,
            RelationOpError::EndpointTypeViolation { side: "to", .. }
        ));
    }

    #[test]
    fn any_endpoint_type_permits_every_entity_type() {
        // `from: Any / to: Any` — the shape every seeded `brain:`
        // relation type but `family_of` uses. Must stay unconstrained.
        let (_dir, mut db) = open_db();
        let concept = make_entity_typed(&mut db, "a-concept", brain_core::EntityTypeId(6));
        let place = make_entity_typed(&mut db, "a-place", brain_core::EntityTypeId(5));
        let t = intern_type(&mut db, "any_to_any", Cardinality::ManyToMany, false);

        let r = fresh_rel(t, concept, place, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn open_vocab_type_has_no_endpoint_constraint() {
        // Implicit-from-write types carry no from / to declaration, so
        // extraction-minted relations are never type-rejected.
        let (_dir, mut db) = open_db();
        let concept = make_entity_typed(&mut db, "implicit-concept", brain_core::EntityTypeId(6));
        let person = make_entity(&mut db, "implicit-person");
        let t = {
            let wtxn = db.write_txn().unwrap();
            let t =
                crate::relation::types::relation_type_intern_or_get(&wtxn, "test", "coined", 0, 1)
                    .unwrap();
            wtxn.commit().unwrap();
            t
        };

        let r = fresh_rel(t, concept, person, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn seeded_brain_schema_relations_still_write() {
        // Highest-risk regression of endpoint-type enforcement: a fresh
        // boot must keep accepting normal writes. `brain:related_to` is
        // `Any / Any` (anything goes); `brain:family_of` is
        // `Person / Person` (only people).
        let (_dir, mut db) = open_db();
        let person_a = make_entity(&mut db, "seeded-person-a");
        let person_b = make_entity(&mut db, "seeded-person-b");
        let place = make_entity_typed(&mut db, "seeded-place", brain_core::EntityTypeId(5));

        let (related_to, family_of) = {
            let rtxn = db.read_txn().unwrap();
            let related =
                crate::relation::types::relation_type_lookup_by_qname(&rtxn, "brain", "related_to")
                    .unwrap()
                    .expect("brain:related_to is seeded at open");
            let family =
                crate::relation::types::relation_type_lookup_by_qname(&rtxn, "brain", "family_of")
                    .unwrap()
                    .expect("brain:family_of is seeded at open");
            (related, family)
        };
        assert_eq!(related_to.from_type, None, "seeded Any stays unconstrained");
        assert_eq!(related_to.to_type, None);

        // Any / Any accepts a Person → Place edge.
        let r = fresh_rel(related_to.id, person_a, place, related_to.is_symmetric);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
        wtxn.commit().unwrap();

        // Person / Person accepts two people.
        let r = fresh_rel(family_of.id, person_a, person_b, family_of.is_symmetric);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 1).unwrap();
        wtxn.commit().unwrap();

        // …and rejects a non-Person endpoint. `Person / Person` can
        // never be fixed by exchanging endpoints, so the auto-swap is
        // unreachable for the one seeded type that declares anything.
        let r = fresh_rel(family_of.id, person_a, place, family_of.is_symmetric);
        let wtxn = db.write_txn().unwrap();
        let err = relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 2)
            .unwrap_err();
        assert!(matches!(
            err,
            RelationOpError::EndpointTypeViolation { side: "to", .. }
        ));
        drop(wtxn);

        // Every other seeded relation type is `Any / Any`: a fresh boot
        // must neither reject nor swap. `reports_to` is asymmetric, so
        // the stored row proves the caller's direction survived intact.
        let reports_to = {
            let rtxn = db.read_txn().unwrap();
            crate::relation::types::relation_type_lookup_by_qname(&rtxn, "brain", "reports_to")
                .unwrap()
                .expect("brain:reports_to is seeded at open")
        };
        assert_eq!(reports_to.from_type, None);
        assert_eq!(reports_to.to_type, None);
        assert!(!reports_to.is_symmetric);

        // A Place → Person edge is nonsense semantically but the seeded
        // declaration constrains nothing, so it must be stored verbatim.
        let r = fresh_rel(reports_to.id, place, person_b, reports_to.is_symmetric);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 3).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = relation_get(&rtxn, r.id).unwrap().unwrap();
        assert_eq!(got.from_entity, place, "Any/Any must not swap");
        assert_eq!(got.to_entity, person_b, "Any/Any must not swap");
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
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r1, 0).unwrap();
        wtxn.commit().unwrap();

        let r2 = fresh_rel(t, priya, bob, false);
        let wtxn = db.write_txn().unwrap();
        let result_id =
            relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r2, 1).unwrap();
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
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r1, 0).unwrap();
        wtxn.commit().unwrap();

        let new_employee = make_entity(&mut db, "new-employee");
        let r2 = fresh_rel(t, new_employee, acme, false);
        let wtxn = db.write_txn().unwrap();
        let result_id =
            relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r2, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = relation_get(&rtxn, r1.id).unwrap().unwrap();
        assert_eq!(g1.superseded_by, Some(result_id));
    }

    #[test]
    fn many_to_many_no_supersession() {
        // Two DISTINCT ManyToMany tuples that share the `from` endpoint but
        // point at different `to` entities are genuine multi-values: both
        // stay current, neither supersedes the other.
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "mm-a");
        let b = make_entity(&mut db, "mm-b");
        let c = make_entity(&mut db, "mm-c");
        let t = intern_type(&mut db, "knows_mm", Cardinality::ManyToMany, false);

        let r1 = fresh_rel(t, a, b, false);
        let r2 = fresh_rel(t, a, c, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r1, 0).unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r2, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = relation_get(&rtxn, r1.id).unwrap().unwrap();
        let g2 = relation_get(&rtxn, r2.id).unwrap().unwrap();
        assert!(g1.superseded_by.is_none());
        assert!(g2.superseded_by.is_none());
    }

    #[test]
    fn many_to_many_identical_reapply_is_noop() {
        // A transient LLM failure makes the extractor worker re-run the whole
        // pipeline, re-applying an already-committed pattern-tier ManyToMany
        // relation with a freshly-minted RelationId. The exact same
        // `(from, type, to)` tuple must NOT leak a second edge: the second
        // create is a no-op returning the first row's id, and only one active
        // row exists.
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "dup-a");
        let b = make_entity(&mut db, "dup-b");
        let t = intern_type(&mut db, "knows_dup", Cardinality::ManyToMany, false);

        let r1 = fresh_rel(t, a, b, false);
        let wtxn = db.write_txn().unwrap();
        let id1 =
            relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r1, 0).unwrap();
        wtxn.commit().unwrap();

        // The retry: same tuple, different minted id.
        let r2 = fresh_rel(t, a, b, false);
        assert_ne!(r1.id, r2.id);
        let wtxn = db.write_txn().unwrap();
        let id2 =
            relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r2, 1).unwrap();
        wtxn.commit().unwrap();

        // No-op: the create returns the pre-existing id and never files r2.
        assert_eq!(id2, id1);
        let rtxn = db.read_txn().unwrap();
        assert!(relation_get(&rtxn, r2.id).unwrap().is_none());

        let filter = RelationListFilter {
            current_only: true,
            ..Default::default()
        };
        let current = relation_list_from(&rtxn, test_scope(), a, &filter).unwrap();
        assert_eq!(
            current.len(),
            1,
            "identical retry must not add a second edge"
        );
        assert_eq!(current[0].id, r1.id);
    }

    #[test]
    fn distinct_many_tuples_coexist_after_dedup() {
        // Dedup must never collapse legitimately-distinct Many values: three
        // different `to` entities on a ManyToMany type all stay current.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "multi-subj");
        let x = make_entity(&mut db, "multi-x");
        let y = make_entity(&mut db, "multi-y");
        let z = make_entity(&mut db, "multi-z");
        let t = intern_type(&mut db, "linked_to", Cardinality::ManyToMany, false);

        for (lsn, to) in [x, y, z].into_iter().enumerate() {
            let r = fresh_rel(t, subj, to, false);
            let wtxn = db.write_txn().unwrap();
            relation_create(
                &wtxn,
                test_scope(),
                brain_core::SessionId::DEFAULT,
                &r,
                lsn as u64,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let filter = RelationListFilter {
            current_only: true,
            ..Default::default()
        };
        let current = relation_list_from(&rtxn, test_scope(), subj, &filter).unwrap();
        assert_eq!(current.len(), 3);
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
                &wtxn,
                "brain",
                "camped_at",
                0,
                1,
            )
            .unwrap();
            wtxn.commit().unwrap();
            t
        };

        for (lsn, place) in [p1, p2, p3].into_iter().enumerate() {
            let r = fresh_rel(t, subj, place, false);
            let wtxn = db.write_txn().unwrap();
            relation_create(
                &wtxn,
                test_scope(),
                brain_core::SessionId::DEFAULT,
                &r,
                lsn as u64,
            )
            .unwrap();
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
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
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
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r1, 0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.write_txn().unwrap();
        relation_tombstone(&wtxn, r1.id, 1).unwrap();
        wtxn.commit().unwrap();

        let r2 = fresh_rel(t, priya, bob, false);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r2, 2).unwrap();
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
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r1, 0).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r2, 1).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r3, 2).unwrap();
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
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r1, 0).unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r2, 1).unwrap();
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
        let mem = MemoryId::pack(1, brain_core::SessionId::DEFAULT.into(), 0);
        let mut r = fresh_rel(t, a, b, false);
        r.evidence = vec![mem];

        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
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
        relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let filter = RelationListFilter::default();
        let to_b = relation_list_to(&rtxn, test_scope(), b, &filter).unwrap();
        assert_eq!(to_b.len(), 1);
        assert_eq!(to_b[0].id, r.id);
    }
}
