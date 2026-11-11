//! Typed CRUD + interning over the relation-type registry.
//! Mirrors [`crate::schema::predicate`].

use std::collections::{HashMap, HashSet};

use brain_core::RelationType;
use brain_core::{Cardinality, EntityTypeId, RelationTypeId};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::relation::RELATION_TYPE_EMBEDDINGS_TABLE;
use crate::tables::relation_type::{
    encode_entity_type_id, RelationTypeDefinition, RelationTypeOrigin,
    RELATION_TYPES_BY_SCOPE_TABLE, RELATION_TYPES_TABLE,
};
use crate::tables::scope::{space_chain, DeclScope, SPACE_BASE};

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum RelationTypeOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("invalid relation-type identifier: {reason}")]
    InvalidIdentifier { reason: &'static str },

    #[error(
        "relation type {qname:?} already exists with id {existing_id:?} but constraints differ"
    )]
    AlreadyExists {
        qname: String,
        existing_id: RelationTypeId,
    },
}

// ---------------------------------------------------------------------------
// Identifier validation.
// ---------------------------------------------------------------------------

pub const NAMESPACE_MAX_LEN: usize = 32;
pub const NAME_MAX_LEN: usize = 64;

fn validate_namespace(s: &str) -> Result<(), RelationTypeOpError> {
    validate_identifier(s, NAMESPACE_MAX_LEN, "namespace")
}

fn validate_name(s: &str) -> Result<(), RelationTypeOpError> {
    validate_identifier(s, NAME_MAX_LEN, "name")
}

fn validate_identifier(
    s: &str,
    max: usize,
    label: &'static str,
) -> Result<(), RelationTypeOpError> {
    if s.is_empty() {
        return Err(RelationTypeOpError::InvalidIdentifier {
            reason: match label {
                "namespace" => "namespace must not be empty",
                _ => "name must not be empty",
            },
        });
    }
    // Bounded by Unicode code points, not bytes, so a multibyte name
    // (作用于, wirkt_gegen) isn't clipped far below the stated char limit —
    // matching the predicate-name validator. 64 code points is ≤ 256 bytes,
    // so it stays within the wire identifier bound.
    if s.chars().count() > max {
        return Err(RelationTypeOpError::InvalidIdentifier {
            reason: match label {
                "namespace" => "namespace exceeds 32 chars",
                _ => "name exceeds 64 chars",
            },
        });
    }
    let mut chars = s.chars();
    let first = chars.next().expect("non-empty checked above");
    if !first.is_ascii_lowercase() {
        return Err(RelationTypeOpError::InvalidIdentifier {
            reason: match label {
                "namespace" => "namespace must start with [a-z]",
                _ => "name must start with [a-z]",
            },
        });
    }
    for c in std::iter::once(first).chain(chars) {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_';
        if !ok {
            return Err(RelationTypeOpError::InvalidIdentifier {
                reason: match label {
                    "namespace" => "namespace must match [a-z][a-z0-9_]*",
                    _ => "name must match [a-z][a-z0-9_]*",
                },
            });
        }
    }
    Ok(())
}

#[must_use]
fn qname(namespace: &str, name: &str) -> String {
    format!("{namespace}:{name}")
}

// ---------------------------------------------------------------------------
// Read paths.
// ---------------------------------------------------------------------------

/// Fetch a relation type by id. `None` if absent.
pub fn relation_type_get(
    rtxn: &ReadTransaction,
    id: RelationTypeId,
) -> Result<Option<RelationType>, RelationTypeOpError> {
    let t = rtxn.open_table(RELATION_TYPES_TABLE)?;
    let row: Option<RelationTypeDefinition> = t.get(&id.raw())?.map(|g| g.value());
    Ok(row.as_ref().map(RelationTypeDefinition::to_relation_type))
}

/// Resolve `qname` through the scope chain, read txn.
fn scope_get_rtxn(
    rtxn: &ReadTransaction,
    qname: &str,
    space: [u8; 16],
) -> Result<Option<u32>, RelationTypeOpError> {
    let idx = rtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE)?;
    for sp in space_chain(space) {
        if let Some(id) = idx.get(&(qname, sp))?.map(|g| g.value()) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Resolve `qname` through the scope chain, write txn.
fn scope_get_wtxn(
    wtxn: &WriteTransaction,
    qname: &str,
    space: [u8; 16],
) -> Result<Option<u32>, RelationTypeOpError> {
    let idx = wtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE)?;
    for sp in space_chain(space) {
        if let Some(id) = idx.get(&(qname, sp))?.map(|g| g.value()) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Resolve `qname` in exactly one scope, write txn.
///
/// Writes probe exactly; only reads chain. A write that chained would find
/// the namespace base and hand it back, so declaring an overlay would
/// silently return the base row and no space could ever have its own.
fn scope_get_exact_wtxn(
    wtxn: &WriteTransaction,
    qname: &str,
    space: [u8; 16],
) -> Result<Option<u32>, RelationTypeOpError> {
    let idx = wtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE)?;
    let got = idx.get(&(qname, space))?.map(|g| g.value());
    Ok(got)
}

/// Remove one scope's entry for `qname` — a targeted drop takes away
/// exactly the declaration it names.
fn scope_remove(
    wtxn: &WriteTransaction,
    qname: &str,
    space: [u8; 16],
) -> Result<(), RelationTypeOpError> {
    let mut idx = wtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE)?;
    idx.remove(&(qname, space))?;
    Ok(())
}

/// Remove every scope's entry for `qname` — the row itself is gone, so an
/// Remove the index entry that points at `id`.
///
/// The row is gone, so the entry naming it must go — and ONLY that entry.
/// One qname can have SEVERAL rows, one per scope that declared it, so
/// clearing every `(qname, *)` entry would orphan a sibling scope's row: the
/// row would survive with nothing resolving to it, and that scope's
/// declaration would silently disappear.
fn scope_remove_entry(
    wtxn: &WriteTransaction,
    qname: &str,
    id: u32,
) -> Result<(), RelationTypeOpError> {
    let owned: Vec<[u8; 16]> = {
        let idx = wtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE)?;
        let mut out = Vec::new();
        for entry in idx.range((qname, [0u8; 16])..=(qname, [0xFFu8; 16]))? {
            let (k, v) = entry?;
            if v.value() == id {
                out.push(k.value().1);
            }
        }
        out
    };
    let mut idx = wtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE)?;
    for sp in owned {
        idx.remove(&(qname, sp))?;
    }
    Ok(())
}

/// Resolve a relation type as `space` sees it: the space's own declaration
/// when it has one, otherwise the namespace-wide one.
///
/// # Errors
/// `InvalidIdentifier` for a malformed namespace/name, and redb storage
/// failures.
pub fn relation_type_lookup_by_qname(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
    name: &str,
) -> Result<Option<RelationType>, RelationTypeOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);
    let Some(id_raw) = scope_get_rtxn(rtxn, q.as_str(), space)? else {
        return Ok(None);
    };

    let t = rtxn.open_table(RELATION_TYPES_TABLE)?;
    let row: Option<RelationTypeDefinition> = t.get(&id_raw)?.map(|g| g.value());
    Ok(row.as_ref().map(RelationTypeDefinition::to_relation_type))
}

/// Write-transaction counterpart to [`relation_type_lookup_by_qname`].
///
/// A write path resolves vocabulary inside its own txn so the resolution
/// and the row it gates cannot straddle two snapshots.
///
/// # Errors
/// `InvalidIdentifier` for a malformed namespace/name, and redb storage
/// failures.
pub fn relation_type_lookup_by_qname_wtxn(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    name: &str,
) -> Result<Option<RelationType>, RelationTypeOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);
    let Some(id_raw) = scope_get_wtxn(wtxn, q.as_str(), space)? else {
        return Ok(None);
    };

    let t = wtxn.open_table(RELATION_TYPES_TABLE)?;
    let row: Option<RelationTypeDefinition> = t.get(&id_raw)?.map(|g| g.value());
    Ok(row.as_ref().map(RelationTypeDefinition::to_relation_type))
}

/// Write-transaction counterpart to
/// [`relation_types_active_for_schema`].
///
/// # Errors
/// `InvalidIdentifier` for a malformed namespace, and redb storage failures.
pub fn relation_types_active_for_schema_wtxn(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    version: u32,
) -> Result<HashSet<RelationTypeId>, RelationTypeOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    let resolved = {
        let idx = wtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE)?;
        let mut out: HashMap<String, u32> = HashMap::new();
        for entry in idx.iter()? {
            let (k, v) = entry?;
            let (qname, sp) = k.value();
            if sp == space {
                out.insert(qname.to_string(), v.value());
            } else if sp == SPACE_BASE {
                out.entry(qname.to_string()).or_insert(v.value());
            }
        }
        out
    };
    let t = wtxn.open_table(RELATION_TYPES_TABLE)?;
    let mut out = HashSet::new();
    for id in resolved.into_values() {
        let Some(row) = t.get(&id)?.map(|g| g.value()) else {
            continue;
        };
        if row.namespace != namespace {
            continue;
        }
        if let RelationTypeOrigin::SchemaDeclared { version: v_decl } = row.origin() {
            if v_decl == version {
                out.insert(RelationTypeId::from(row.relation_type_id));
            }
        }
    }
    Ok(out)
}

/// The relation-type ids `space` resolves that version `version` declares.
///
/// Membership is judged over what the scope RESOLVES, for the reason given
/// on [`crate::schema::predicate::predicates_active_for_schema`].
///
/// # Errors
/// `InvalidIdentifier` for a malformed namespace, and redb storage failures.
pub fn relation_types_active_for_schema(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
    version: u32,
) -> Result<HashSet<RelationTypeId>, RelationTypeOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    let resolved = resolved_relation_type_ids(rtxn, space)?;
    let t = rtxn.open_table(RELATION_TYPES_TABLE)?;
    let mut out = HashSet::new();
    for id in resolved.into_values() {
        let Some(row) = t.get(&id)?.map(|g| g.value()) else {
            continue;
        };
        if row.namespace != namespace {
            continue;
        }
        if let RelationTypeOrigin::SchemaDeclared { version: v_decl } = row.origin() {
            if v_decl == version {
                out.insert(RelationTypeId::from(row.relation_type_id));
            }
        }
    }
    Ok(out)
}

/// Every qname `space` resolves, mapped to the id it resolves to. Base
/// before overlay, for the key-order reason given on
/// [`crate::schema::predicate::predicates_active_for_schema`]'s helper.
fn resolved_relation_type_ids(
    rtxn: &ReadTransaction,
    space: [u8; 16],
) -> Result<HashMap<String, u32>, RelationTypeOpError> {
    let idx = rtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE)?;
    let mut out: HashMap<String, u32> = HashMap::new();
    for entry in idx.iter()? {
        let (k, v) = entry?;
        let (qname, sp) = k.value();
        if sp == space {
            out.insert(qname.to_string(), v.value());
        } else if sp == SPACE_BASE {
            out.entry(qname.to_string()).or_insert(v.value());
        }
    }
    Ok(out)
}

/// List relation types. `None` → all; `Some(ns)` → filtered.
pub fn relation_type_list(
    rtxn: &ReadTransaction,
    namespace_filter: Option<&str>,
) -> Result<Vec<RelationType>, RelationTypeOpError> {
    if let Some(ns) = namespace_filter {
        validate_namespace(ns)?;
    }
    let t = rtxn.open_table(RELATION_TYPES_TABLE)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        let row = v.value();
        if let Some(ns) = namespace_filter {
            if row.namespace != ns {
                continue;
            }
        }
        out.push(row.to_relation_type());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Write path.
// ---------------------------------------------------------------------------

/// Intern (or look up) a relation type by its qname, in `scope`.
///
/// [`DeclScope::base`] is the namespace-wide declaration; any other space is
/// an overlay a lookup from that space prefers over the base.
///
/// Semantics mirror `predicate_intern`:
///
/// - No prior row: allocate fresh id, write row + scope index entry.
/// - Prior row with identical constraints: return existing id.
/// - Prior row with diverging constraints: error.
///
/// # Errors
/// Propagates validation and storage failures, and `AlreadyExists` when the
/// scope holds a diverging definition of the same name.
#[allow(clippy::too_many_arguments)]
pub fn relation_type_intern(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    name: &str,
    from_type: Option<EntityTypeId>,
    to_type: Option<EntityTypeId>,
    cardinality: Cardinality,
    is_symmetric: bool,
    schema_version: u32,
    description: &str,
    now_unix_nanos: u64,
) -> Result<RelationTypeId, RelationTypeOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);

    // Idempotency / adoption probe — same three-case dispatch as
    // `crate::schema::predicate::predicate_intern`: exact match returns
    // existing id, implicit-from-write row gets adopted at the new
    // schema_version, schema-declared row with diverging constraints
    // errors.
    let probed: Option<(u32, RelationTypeDefinition)> = {
        let id_raw: Option<u32> = scope_get_exact_wtxn(wtxn, q.as_str(), space)?;
        match id_raw {
            None => None,
            Some(id_raw) => {
                let t = wtxn.open_table(RELATION_TYPES_TABLE)?;
                let row = t
                    .get(&id_raw)?
                    .map(|g| g.value())
                    .expect("qname index points to a missing row — file is corrupt");
                Some((id_raw, row))
            }
        }
    };
    if let Some((id_raw, row)) = probed {
        // Constraint-only equality (excludes schema_version) so an
        // unchanged relation type carried forward into a new schema
        // version is a no-op rather than a conflict.
        let constraints_match = row.cardinality == cardinality.as_u8()
            && (row.is_symmetric != 0) == is_symmetric
            && row.from_entity_type_id == encode_entity_type_id(from_type)
            && row.to_entity_type_id == encode_entity_type_id(to_type)
            && row.description == description;

        if constraints_match && row.schema_version == schema_version {
            return Ok(RelationTypeId::from(id_raw));
        }

        // Constraints match but schema_version advanced — bump the
        // row's version in-place. Existing id is reused so relations
        // already pointing at it stay valid.
        if constraints_match && row.origin().is_schema_declared() {
            let rt = RelationType {
                id: RelationTypeId::from(id_raw),
                namespace: namespace.to_string(),
                name: name.to_string(),
                from_type,
                to_type,
                cardinality,
                is_symmetric,
                schema_version,
                description: description.to_string(),
            };
            let new_row = RelationTypeDefinition::from_relation_type_with_origin(
                &rt,
                now_unix_nanos,
                RelationTypeOrigin::SchemaDeclared {
                    version: schema_version,
                },
            );
            let mut t = wtxn.open_table(RELATION_TYPES_TABLE)?;
            t.insert(&id_raw, &new_row)?;
            return Ok(RelationTypeId::from(id_raw));
        }

        if !row.origin().is_schema_declared() {
            let rt = RelationType {
                id: RelationTypeId::from(id_raw),
                namespace: namespace.to_string(),
                name: name.to_string(),
                from_type,
                to_type,
                cardinality,
                is_symmetric,
                schema_version,
                description: description.to_string(),
            };
            let new_row = RelationTypeDefinition::from_relation_type_with_origin(
                &rt,
                now_unix_nanos,
                RelationTypeOrigin::SchemaDeclared {
                    version: schema_version,
                },
            );
            let mut t = wtxn.open_table(RELATION_TYPES_TABLE)?;
            t.insert(&id_raw, &new_row)?;
            return Ok(RelationTypeId::from(id_raw));
        }
        return Err(RelationTypeOpError::AlreadyExists {
            qname: q,
            existing_id: RelationTypeId::from(id_raw),
        });
    }

    // Fresh registration. Allocate id = max(existing) + 1.
    let next_id_raw: u32 = {
        let t = wtxn.open_table(RELATION_TYPES_TABLE)?;
        let mut max: u32 = 0;
        for entry in t.iter()? {
            let (k, _) = entry?;
            let id = k.value();
            if id > max {
                max = id;
            }
        }
        max.checked_add(1).expect("RelationTypeId space exhausted")
    };

    let rt = RelationType {
        id: RelationTypeId::from(next_id_raw),
        namespace: namespace.to_string(),
        name: name.to_string(),
        from_type,
        to_type,
        cardinality,
        is_symmetric,
        schema_version,
        description: description.to_string(),
    };
    let row = RelationTypeDefinition::from_relation_type(&rt, now_unix_nanos);

    {
        let mut t = wtxn.open_table(RELATION_TYPES_TABLE)?;
        t.insert(&row.relation_type_id, &row)?;
    }
    {
        let mut idx = wtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE)?;
        idx.insert(&(q.as_str(), space), &row.relation_type_id)?;
    }

    Ok(RelationTypeId::from(next_id_raw))
}

/// Resolve a relation type in `scope`, coining it if nothing resolves.
///
/// The open-vocabulary path. COINING ADOPTS, IT DOES NOT OVERRIDE — the
/// chain is walked for the reason given on
/// [`crate::schema::predicate::predicate_intern_or_get`].
///
/// # Errors
/// Propagates validation and storage failures.
pub fn relation_type_intern_or_get(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    name: &str,
    first_seen_lsn: u64,
    now_unix_nanos: u64,
) -> Result<RelationTypeId, RelationTypeOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;
    let q = qname(namespace, name);

    let existing_id: Option<u32> = scope_get_wtxn(wtxn, q.as_str(), space)?;
    if let Some(id_raw) = existing_id {
        return Ok(RelationTypeId::from(id_raw));
    }

    let next_id_raw: u32 = {
        let t = wtxn.open_table(RELATION_TYPES_TABLE)?;
        let mut max: u32 = 0;
        for entry in t.iter()? {
            let (k, _) = entry?;
            let id = k.value();
            if id > max {
                max = id;
            }
        }
        max.checked_add(1).expect("RelationTypeId space exhausted")
    };

    let rt = RelationType {
        id: RelationTypeId::from(next_id_raw),
        namespace: namespace.to_string(),
        name: name.to_string(),
        from_type: None,
        to_type: None,
        // Schemaless writers don't pick a cardinality. ManyToMany is
        // the only choice that doesn't risk an automatic supersession
        // on a future create — the writer must call
        // RELATION_SUPERSEDE explicitly to retire old edges.
        cardinality: Cardinality::ManyToMany,
        is_symmetric: false,
        schema_version: 0,
        description: String::new(),
    };
    let row = RelationTypeDefinition::from_relation_type_with_origin(
        &rt,
        now_unix_nanos,
        RelationTypeOrigin::ImplicitFromWrite { first_seen_lsn },
    );

    {
        let mut t = wtxn.open_table(RELATION_TYPES_TABLE)?;
        t.insert(&row.relation_type_id, &row)?;
    }
    {
        let mut idx = wtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE)?;
        idx.insert(&(q.as_str(), space), &row.relation_type_id)?;
    }
    Ok(RelationTypeId::from(next_id_raw))
}

/// Drop every schema-declared relation_type row in `namespace`.
/// Implicit-from-write rows are preserved. Counterpart to
/// [`crate::schema::predicate::predicate_drop_schema_declared`]; used
/// by `SCHEMA_REPLACE` to clear the existing declared vocabulary
/// before re-running apply.
pub fn relation_type_drop_schema_declared(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
) -> Result<usize, RelationTypeOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;

    // Victims are the declarations THIS SCOPE made, for the reason given on
    // [`crate::schema::predicate::predicate_drop_schema_declared`].
    let victims: Vec<(u32, String)> = {
        let idx = wtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE)?;
        let t = wtxn.open_table(RELATION_TYPES_TABLE)?;
        let mut out = Vec::new();
        for entry in idx.iter()? {
            let (k, v) = entry?;
            let (q, sp) = k.value();
            if sp != space {
                continue;
            }
            let id = v.value();
            let Some(row) = t.get(&id)?.map(|g| g.value()) else {
                continue;
            };
            if row.namespace == namespace && row.origin().is_schema_declared() {
                out.push((id, q.to_string()));
            }
        }
        out
    };
    let count = victims.len();
    {
        let mut t = wtxn.open_table(RELATION_TYPES_TABLE)?;
        for (id, _) in &victims {
            t.remove(id)?;
        }
    }
    {
        for (id, q) in &victims {
            scope_remove_entry(wtxn, q.as_str(), *id)?;
        }
    }
    Ok(count)
}

/// Resolve a relation_type id by `(namespace, name)` inside a write
/// txn. The write-txn counterpart to [`relation_type_lookup_by_qname`],
/// mirroring [`crate::schema::predicate::predicate_id_by_qname`]; lets a
/// composing handler resolve the id and mutate in one transaction.
pub fn relation_type_id_by_qname(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    name: &str,
) -> Result<Option<RelationTypeId>, RelationTypeOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;
    let q = qname(namespace, name);
    let found = scope_get_wtxn(wtxn, q.as_str(), space)?.map(RelationTypeId::from);
    Ok(found)
}

/// Drop a single schema-declared relation_type row identified by
/// `(namespace, name)`. The scoped counterpart to
/// [`relation_type_drop_schema_declared`], used by `SCHEMA_DROP` to
/// narrow one declaration instead of wiping the namespace.
///
/// Returns `Some(id)` when a schema-declared relation_type with that
/// qname existed and was removed, `None` otherwise. An
/// implicit-from-write row sharing the qname is left untouched.
pub fn relation_type_drop_one(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    name: &str,
) -> Result<Option<RelationTypeId>, RelationTypeOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);
    let victim: Option<u32> = {
        // Exact, for the reason given on
        // [`crate::schema::predicate::predicate_drop_one`].
        let id = scope_get_exact_wtxn(wtxn, q.as_str(), space)?;
        match id {
            Some(id) => {
                let t = wtxn.open_table(RELATION_TYPES_TABLE)?;
                let row: Option<RelationTypeDefinition> = t.get(&id)?.map(|g| g.value());
                match row {
                    Some(r) if r.origin().is_schema_declared() => Some(id),
                    _ => None,
                }
            }
            None => None,
        }
    };
    if let Some(id) = victim {
        {
            let mut t = wtxn.open_table(RELATION_TYPES_TABLE)?;
            t.remove(&id)?;
        }
        scope_remove(wtxn, q.as_str(), space)?;
    }
    Ok(victim.map(RelationTypeId::from))
}

// ---------------------------------------------------------------------------
// Embeddings.
// ---------------------------------------------------------------------------

/// Store the semantic embedding for a relation type. Called when a
/// relation type is first interned at extraction time. `vec` is the
/// BGE-small output (384 dims); stored as little-endian `f32` bytes.
/// Idempotent overwrite. Mirrors
/// [`crate::schema::predicate::predicate_embedding_put`].
pub fn relation_type_embedding_put(
    wtxn: &WriteTransaction,
    id: RelationTypeId,
    vec: &[f32],
) -> Result<(), RelationTypeOpError> {
    let mut bytes = Vec::with_capacity(vec.len() * 4);
    for f in vec {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    let mut t = wtxn.open_table(RELATION_TYPE_EMBEDDINGS_TABLE)?;
    t.insert(id.raw(), bytes.as_slice())?;
    Ok(())
}

/// Load a relation type's embedding, decoding the little-endian `f32`
/// bytes. Returns `None` when no vector was stored (e.g. relation types
/// interned before the embedding pass, or user-authored relations with no
/// extractor embedding). Mirrors
/// [`crate::schema::predicate::predicate_embedding_get`].
pub fn relation_type_embedding_get(
    rtxn: &ReadTransaction,
    id: RelationTypeId,
) -> Result<Option<Vec<f32>>, RelationTypeOpError> {
    let t = rtxn.open_table(RELATION_TYPE_EMBEDDINGS_TABLE)?;
    let Some(g) = t.get(id.raw())? else {
        return Ok(None);
    };
    let bytes = g.value();
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.as_chunks::<4>().0.iter() {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(Some(out))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    fn open_db() -> (tempfile::TempDir, redb::Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        (dir, db)
    }

    // ── The space overlay ─────────────────────────────────────────────────
    //
    // Same shape as predicates: the qname carries the namespace, so `space`
    // is the one dimension the index lacked. Two applications inside a
    // tenant can mean different things by the same relation.

    const RT_NOW: u64 = 1_700_000_000_000_000_000;
    const SP_A: [u8; 16] = [0xA1; 16];
    const SP_B: [u8; 16] = [0xB2; 16];

    fn rt_at(db: &redb::Database, space: [u8; 16], ns: &str, name: &str) -> RelationTypeId {
        let wtxn = db.begin_write().unwrap();
        let id = relation_type_intern(
            &wtxn,
            DeclScope::space(ns, space),
            name,
            None,
            None,
            Cardinality::ManyToMany,
            false,
            1,
            "",
            RT_NOW,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn rt_resolve(db: &redb::Database, space: [u8; 16], ns: &str, name: &str) -> Option<u32> {
        let wtxn = db.begin_write().unwrap();
        let got = scope_get_wtxn(&wtxn, qname(ns, name).as_str(), space).unwrap();
        wtxn.commit().unwrap();
        got
    }

    /// Resolution CHAINS, so `rt_resolve` cannot prove a row is gone — it
    /// would report the declaration the scope inherits. This asks the exact
    /// question: does THIS scope declare the name itself?
    fn rt_declares_exactly(
        db: &redb::Database,
        space: [u8; 16],
        ns: &str,
        name: &str,
    ) -> Option<u32> {
        let wtxn = db.begin_write().unwrap();
        let got = scope_get_exact_wtxn(&wtxn, qname(ns, name).as_str(), space).unwrap();
        wtxn.commit().unwrap();
        got
    }

    #[test]
    fn a_spaces_relation_type_wins_over_the_namespace_base() {
        let (_dir, db) = open_db();
        let base = rt_at(&db, SPACE_BASE, "acme", "owns");
        let overlay = rt_at(&db, SP_A, "acme", "owns");

        assert_ne!(base, overlay, "an overlay is its own row");
        assert_eq!(rt_resolve(&db, SP_A, "acme", "owns"), Some(overlay.raw()));
        assert_eq!(
            rt_resolve(&db, SPACE_BASE, "acme", "owns"),
            Some(base.raw())
        );
    }

    #[test]
    fn a_space_without_its_own_relation_type_falls_back_to_the_base() {
        let (_dir, db) = open_db();
        let base = rt_at(&db, SPACE_BASE, "acme", "owns");
        assert_eq!(rt_resolve(&db, SP_B, "acme", "owns"), Some(base.raw()));
    }

    /// Sibling spaces stay isolated — the reason the dimension exists.
    #[test]
    fn one_space_cannot_see_anothers_relation_type() {
        let (_dir, db) = open_db();
        let only_a = rt_at(&db, SP_A, "acme", "escalates_to");
        assert_eq!(
            rt_resolve(&db, SP_A, "acme", "escalates_to"),
            Some(only_a.raw())
        );
        assert_eq!(rt_resolve(&db, SP_B, "acme", "escalates_to"), None);
        assert_eq!(rt_resolve(&db, SPACE_BASE, "acme", "escalates_to"), None);
    }

    /// A targeted drop takes away the declaration it names and no other.
    #[test]
    fn dropping_the_base_leaves_a_spaces_relation_type_standing() {
        let (_dir, db) = open_db();
        rt_at(&db, SPACE_BASE, "acme", "owns");
        let overlay = rt_at(&db, SP_A, "acme", "owns");

        let wtxn = db.begin_write().unwrap();
        scope_remove(&wtxn, qname("acme", "owns").as_str(), SPACE_BASE).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(rt_resolve(&db, SPACE_BASE, "acme", "owns"), None);
        assert_eq!(rt_resolve(&db, SP_A, "acme", "owns"), Some(overlay.raw()));
    }

    /// When a ROW goes, the entry naming it goes — and no other.
    ///
    /// This replaced an assertion that every space's entry went with it.
    /// That premise held only while a qname meant one row; a relation type now has
    /// one row PER SCOPE that declared it, so clearing them all orphaned the
    /// sibling scopes' rows — each survived with nothing resolving to it, and
    /// the declaration silently vanished.
    #[test]
    fn removing_a_row_clears_only_its_own_entry() {
        let (_dir, db) = open_db();
        let base = rt_at(&db, SPACE_BASE, "acme", "owns");
        let in_a = rt_at(&db, SP_A, "acme", "owns");
        let in_b = rt_at(&db, SP_B, "acme", "owns");
        assert_ne!(base, in_a, "each scope declared its own row");
        assert_ne!(in_a, in_b);

        let wtxn = db.begin_write().unwrap();
        scope_remove_entry(&wtxn, qname("acme", "owns").as_str(), in_a.raw()).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(
            rt_declares_exactly(&db, SP_A, "acme", "owns"),
            None,
            "the removed row's entry is gone"
        );
        assert_eq!(
            rt_declares_exactly(&db, SPACE_BASE, "acme", "owns"),
            Some(base.raw()),
            "the base's entry is untouched"
        );
        assert_eq!(
            rt_declares_exactly(&db, SP_B, "acme", "owns"),
            Some(in_b.raw()),
            "and the sibling space's entry is untouched"
        );
    }

    /// The base entry point means `SPACE_BASE`, so existing callers resolve
    /// exactly as they did before the dimension existed.
    #[test]
    fn the_base_relation_type_entry_point_declares_at_the_base() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id = relation_type_intern_or_get(&wtxn, DeclScope::base("acme"), "coined", 5, RT_NOW)
            .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(
            rt_resolve(&db, SPACE_BASE, "acme", "coined"),
            Some(id.raw())
        );
        assert_eq!(rt_resolve(&db, SP_A, "acme", "coined"), Some(id.raw()));
    }

    #[test]
    fn intern_fresh_allocates_id_one() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id = relation_type_intern(
            &wtxn,
            DeclScope::base("brain"),
            "related_to",
            None,
            None,
            Cardinality::ManyToMany,
            false,
            1,
            "Generic relation",
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(id, RelationTypeId::from(1));

        let rtxn = db.begin_read().unwrap();
        let got = relation_type_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(got.canonical(), "brain:related_to");
        assert_eq!(got.cardinality, Cardinality::ManyToMany);
        assert!(!got.is_symmetric);

        let by_qname = relation_type_lookup_by_qname(&rtxn, DeclScope::base("brain"), "related_to")
            .unwrap()
            .unwrap();
        assert_eq!(by_qname.id, id);
    }

    #[test]
    fn intern_idempotent_returns_same_id() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id1 = relation_type_intern(
            &wtxn,
            DeclScope::base("brain"),
            "related_to",
            None,
            None,
            Cardinality::ManyToMany,
            false,
            1,
            "x",
            0,
        )
        .unwrap();
        let id2 = relation_type_intern(
            &wtxn,
            DeclScope::base("brain"),
            "related_to",
            None,
            None,
            Cardinality::ManyToMany,
            false,
            1,
            "x",
            999,
        )
        .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(id1, id2);

        let rtxn = db.begin_read().unwrap();
        let all = relation_type_list(&rtxn, None).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn intern_conflict_on_cardinality_mismatch() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let _ = relation_type_intern(
            &wtxn,
            DeclScope::base("brain"),
            "related_to",
            None,
            None,
            Cardinality::ManyToMany,
            false,
            1,
            "",
            0,
        )
        .unwrap();
        let err = relation_type_intern(
            &wtxn,
            DeclScope::base("brain"),
            "related_to",
            None,
            None,
            Cardinality::OneToOne, // ← changed
            false,
            1,
            "",
            0,
        )
        .unwrap_err();
        matches!(err, RelationTypeOpError::AlreadyExists { .. })
            .then_some(())
            .expect("expected AlreadyExists");
    }

    #[test]
    fn intern_conflict_on_symmetric_mismatch() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let _ = relation_type_intern(
            &wtxn,
            DeclScope::base("brain"),
            "discussed_with",
            None,
            None,
            Cardinality::ManyToMany,
            false,
            1,
            "",
            0,
        )
        .unwrap();
        let err = relation_type_intern(
            &wtxn,
            DeclScope::base("brain"),
            "discussed_with",
            None,
            None,
            Cardinality::ManyToMany,
            true, // ← changed
            1,
            "",
            0,
        )
        .unwrap_err();
        matches!(err, RelationTypeOpError::AlreadyExists { .. })
            .then_some(())
            .expect("expected AlreadyExists");
    }

    #[test]
    fn list_by_namespace_filters() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let _ = relation_type_intern(
            &wtxn,
            DeclScope::base("brain"),
            "related_to",
            None,
            None,
            Cardinality::ManyToMany,
            false,
            1,
            "",
            0,
        )
        .unwrap();
        let _ = relation_type_intern(
            &wtxn,
            DeclScope::base("acme"),
            "reports_to",
            Some(EntityTypeId(1)),
            Some(EntityTypeId(1)),
            Cardinality::ManyToOne,
            false,
            1,
            "",
            0,
        )
        .unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let brain_only = relation_type_list(&rtxn, Some("brain")).unwrap();
        assert_eq!(brain_only.len(), 1);
        let acme_only = relation_type_list(&rtxn, Some("acme")).unwrap();
        assert_eq!(acme_only.len(), 1);
        let all = relation_type_list(&rtxn, None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn lookup_missing_returns_none() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let _ = wtxn.open_table(RELATION_TYPES_TABLE).unwrap();
        let _ = wtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = relation_type_lookup_by_qname(&rtxn, DeclScope::base("brain"), "nope").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn entity_type_constraints_round_trip() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id = relation_type_intern(
            &wtxn,
            DeclScope::base("acme"),
            "reports_to",
            Some(EntityTypeId(1)),
            Some(EntityTypeId(1)),
            Cardinality::ManyToOne,
            false,
            1,
            "",
            0,
        )
        .unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let rt = relation_type_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(rt.from_type, Some(EntityTypeId(1)));
        assert_eq!(rt.to_type, Some(EntityTypeId(1)));
    }

    // ----- Open-vocabulary intern path. -----

    #[test]
    fn relation_type_intern_or_get_allocates_then_returns_same_id() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let a = relation_type_intern_or_get(&wtxn, DeclScope::base("acme"), "knows", 0, 0).unwrap();
        let b = relation_type_intern_or_get(&wtxn, DeclScope::base("acme"), "knows", 0, 0).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(a, b);

        let rtxn = db.begin_read().unwrap();
        let rt = relation_type_get(&rtxn, a).unwrap().unwrap();
        // Implicit relation types default to ManyToMany.
        assert_eq!(rt.cardinality, Cardinality::ManyToMany);
        assert!(!rt.is_symmetric);
        assert_eq!(rt.from_type, None);
        assert_eq!(rt.to_type, None);
    }

    #[test]
    fn relation_type_intern_or_get_marks_origin_implicit() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id = relation_type_intern_or_get(&wtxn, DeclScope::base("acme"), "y", 42, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(RELATION_TYPES_TABLE).unwrap();
        let row = t.get(&id.raw()).unwrap().unwrap().value();
        assert_eq!(row.origin_tag, 1);
        assert_eq!(row.origin_payload, 42);
    }

    #[test]
    fn relation_type_intern_at_higher_version_with_same_constraints_bumps_version() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let v1_id = relation_type_intern(
            &wtxn,
            DeclScope::base("acme"),
            "reports_to",
            Some(EntityTypeId(1)),
            Some(EntityTypeId(1)),
            Cardinality::ManyToOne,
            false,
            1,
            "org chart edge",
            0,
        )
        .unwrap();
        let v2_id = relation_type_intern(
            &wtxn,
            DeclScope::base("acme"),
            "reports_to",
            Some(EntityTypeId(1)),
            Some(EntityTypeId(1)),
            Cardinality::ManyToOne,
            false,
            2,
            "org chart edge",
            99,
        )
        .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(v1_id, v2_id, "id must be preserved across version bump");

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(RELATION_TYPES_TABLE).unwrap();
        let row = t.get(&v1_id.raw()).unwrap().unwrap().value();
        assert_eq!(row.schema_version, 2);
        assert_eq!(
            row.origin(),
            RelationTypeOrigin::SchemaDeclared { version: 2 }
        );

        let all = relation_type_list(&rtxn, Some("acme")).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn relation_type_intern_at_higher_version_with_different_constraints_errors() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let _ = relation_type_intern(
            &wtxn,
            DeclScope::base("acme"),
            "reports_to",
            Some(EntityTypeId(1)),
            Some(EntityTypeId(1)),
            Cardinality::ManyToOne,
            false,
            1,
            "",
            0,
        )
        .unwrap();
        let err = relation_type_intern(
            &wtxn,
            DeclScope::base("acme"),
            "reports_to",
            Some(EntityTypeId(1)),
            Some(EntityTypeId(1)),
            Cardinality::OneToOne, // changed
            false,
            2,
            "",
            0,
        )
        .unwrap_err();
        matches!(err, RelationTypeOpError::AlreadyExists { .. })
            .then_some(())
            .expect("expected AlreadyExists");
    }

    #[test]
    fn relation_types_active_for_schema_excludes_implicit_rows() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let declared = relation_type_intern(
            &wtxn,
            DeclScope::base("acme"),
            "in_schema",
            None,
            None,
            Cardinality::ManyToOne,
            false,
            5,
            "",
            0,
        )
        .unwrap();
        let implicit =
            relation_type_intern_or_get(&wtxn, DeclScope::base("acme"), "implicit", 0, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let active = relation_types_active_for_schema(&rtxn, DeclScope::base("acme"), 5).unwrap();
        assert!(active.contains(&declared));
        assert!(!active.contains(&implicit));
    }

    #[test]
    fn relation_type_embedding_round_trips_and_missing_is_none() {
        let (_dir, db) = open_db();
        let id = RelationTypeId::from(7);
        let vec = vec![0.5_f32, -1.25, 3.0, 0.0];

        let wtxn = db.begin_write().unwrap();
        relation_type_embedding_put(&wtxn, id, &vec).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = relation_type_embedding_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(got, vec);

        // An id with no stored embedding returns None.
        let missing = relation_type_embedding_get(&rtxn, RelationTypeId::from(99)).unwrap();
        assert!(missing.is_none());
    }
}
