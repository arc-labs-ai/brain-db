//! Typed CRUD + interning over the relation-type registry.
//! Mirrors [`crate::schema::predicate`].

use std::collections::HashSet;

use brain_core::RelationType;
use brain_core::{Cardinality, EntityTypeId, RelationTypeId};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::relation::RELATION_TYPE_EMBEDDINGS_TABLE;
use crate::tables::relation_type::{
    encode_entity_type_id, RelationTypeDefinition, RelationTypeOrigin,
    RELATION_TYPES_BY_QNAME_TABLE, RELATION_TYPES_TABLE,
};

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
    if s.len() > max {
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

/// Look up a relation type by qname. Identifier validation is
/// enforced — invalid namespace/name yields `InvalidIdentifier`
/// instead of `Ok(None)`.
pub fn relation_type_lookup_by_qname(
    rtxn: &ReadTransaction,
    namespace: &str,
    name: &str,
) -> Result<Option<RelationType>, RelationTypeOpError> {
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);
    let idx = rtxn.open_table(RELATION_TYPES_BY_QNAME_TABLE)?;
    let id_raw: Option<u32> = idx.get(q.as_str())?.map(|g| g.value());
    let Some(id_raw) = id_raw else {
        return Ok(None);
    };

    let t = rtxn.open_table(RELATION_TYPES_TABLE)?;
    let row: Option<RelationTypeDefinition> = t.get(&id_raw)?.map(|g| g.value());
    Ok(row.as_ref().map(RelationTypeDefinition::to_relation_type))
}

/// Set of `RelationTypeId`s the active schema version of `namespace`
/// declares. Companion to
/// [`crate::schema::predicate::predicates_active_for_schema`].
pub fn relation_types_active_for_schema(
    rtxn: &ReadTransaction,
    namespace: &str,
    version: u32,
) -> Result<HashSet<RelationTypeId>, RelationTypeOpError> {
    validate_namespace(namespace)?;
    let t = rtxn.open_table(RELATION_TYPES_TABLE)?;
    let mut out = HashSet::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        let row = v.value();
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

/// Intern (or look up) a relation type by its qname.
///
/// Semantics mirror `predicate_intern`:
/// - No prior row: allocate fresh id, write row + qname index entry.
/// - Prior row with identical constraints: return existing id.
/// - Prior row with diverging constraints: error.
#[allow(clippy::too_many_arguments)]
pub fn relation_type_intern(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
    from_type: Option<EntityTypeId>,
    to_type: Option<EntityTypeId>,
    cardinality: Cardinality,
    is_symmetric: bool,
    schema_version: u32,
    description: &str,
    now_unix_nanos: u64,
) -> Result<RelationTypeId, RelationTypeOpError> {
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);

    // Idempotency / adoption probe — same three-case dispatch as
    // `crate::schema::predicate::predicate_intern`: exact match returns
    // existing id, implicit-from-write row gets adopted at the new
    // schema_version, schema-declared row with diverging constraints
    // errors.
    let probed: Option<(u32, RelationTypeDefinition)> = {
        let idx = wtxn.open_table(RELATION_TYPES_BY_QNAME_TABLE)?;
        let id_raw: Option<u32> = idx.get(q.as_str())?.map(|g| g.value());
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
        let mut idx = wtxn.open_table(RELATION_TYPES_BY_QNAME_TABLE)?;
        idx.insert(q.as_str(), &row.relation_type_id)?;
    }

    Ok(RelationTypeId::from(next_id_raw))
}

/// Open-vocabulary intern: look up `(namespace, name)` and return its
/// `RelationTypeId` if present, else allocate a fresh row with
/// [`RelationTypeOrigin::ImplicitFromWrite`] and
/// [`Cardinality::ManyToMany`] (no constraint — schemaless callers
/// have no cardinality contract).
///
/// Counterpart to
/// [`crate::schema::predicate::predicate_intern_or_get`].
pub fn relation_type_intern_or_get(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
    first_seen_lsn: u64,
    now_unix_nanos: u64,
) -> Result<RelationTypeId, RelationTypeOpError> {
    validate_namespace(namespace)?;
    validate_name(name)?;
    let q = qname(namespace, name);

    let existing_id: Option<u32> = {
        let idx = wtxn.open_table(RELATION_TYPES_BY_QNAME_TABLE)?;
        let got = idx.get(q.as_str())?.map(|g| g.value());
        got
    };
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
        let mut idx = wtxn.open_table(RELATION_TYPES_BY_QNAME_TABLE)?;
        idx.insert(q.as_str(), &row.relation_type_id)?;
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
    namespace: &str,
) -> Result<usize, RelationTypeOpError> {
    validate_namespace(namespace)?;

    let victims: Vec<(u32, String)> = {
        let t = wtxn.open_table(RELATION_TYPES_TABLE)?;
        let mut out = Vec::new();
        for entry in t.iter()? {
            let (k, v) = entry?;
            let row: RelationTypeDefinition = v.value();
            if row.namespace == namespace && row.origin().is_schema_declared() {
                out.push((k.value(), qname(&row.namespace, &row.name)));
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
        let mut idx = wtxn.open_table(RELATION_TYPES_BY_QNAME_TABLE)?;
        for (_, q) in &victims {
            idx.remove(q.as_str())?;
        }
    }
    Ok(count)
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
    for chunk in bytes.chunks_exact(4) {
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

    #[test]
    fn intern_fresh_allocates_id_one() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id = relation_type_intern(
            &wtxn,
            "brain",
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

        let by_qname = relation_type_lookup_by_qname(&rtxn, "brain", "related_to")
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
            "brain",
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
            "brain",
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
            "brain",
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
            "brain",
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
            "brain",
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
            "brain",
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
            "brain",
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
            "acme",
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
        let _ = wtxn.open_table(RELATION_TYPES_BY_QNAME_TABLE).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = relation_type_lookup_by_qname(&rtxn, "brain", "nope").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn entity_type_constraints_round_trip() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id = relation_type_intern(
            &wtxn,
            "acme",
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
        let a = relation_type_intern_or_get(&wtxn, "acme", "knows", 0, 0).unwrap();
        let b = relation_type_intern_or_get(&wtxn, "acme", "knows", 0, 0).unwrap();
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
        let id = relation_type_intern_or_get(&wtxn, "acme", "y", 42, 0).unwrap();
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
            "acme",
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
            "acme",
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
            "acme",
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
            "acme",
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
            "acme",
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
        let implicit = relation_type_intern_or_get(&wtxn, "acme", "implicit", 0, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let active = relation_types_active_for_schema(&rtxn, "acme", 5).unwrap();
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
