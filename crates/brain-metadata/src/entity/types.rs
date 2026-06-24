//! Typed CRUD + interning over the entity-type registry.
//!
//! Mirrors [`crate::schema::predicate`] and
//! [`crate::relation::types`]; every entity-type registration flows
//! through the shared apply path.
//!
//! Entity types don't have a `(namespace, name)` qname today —
//! `Person` lives at the bare name "Person" with the implicit
//! `brain:` namespace. The on-disk row layout pre-dates the namespace
//! scheme; widening it to a per-namespace ID space is a future
//! migration concern. For now the registry is keyed on bare `name`.

use brain_core::EntityTypeId;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::entity_type::{EntityTypeDefinition, ENTITY_TYPES_TABLE};

#[derive(thiserror::Error, Debug)]
pub enum EntityTypeOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error(
        "entity_type name {name:?} already exists with id {existing_id:?} but constraints differ"
    )]
    AlreadyExists {
        name: String,
        existing_id: EntityTypeId,
    },
}

/// Look up an entity_type by its bare `name`. Linear scan; the
/// registry is small (≤ a few hundred entries in any v1
/// deployment). Returns `Ok(None)` if not found.
pub fn entity_type_lookup_by_name(
    wtxn: &WriteTransaction,
    name: &str,
) -> Result<Option<EntityTypeDefinition>, EntityTypeOpError> {
    let t = wtxn.open_table(ENTITY_TYPES_TABLE)?;
    for entry in t.iter()? {
        let (_k, v) = entry?;
        let row = v.value();
        if row.name == name {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

/// Read-only counterpart to [`entity_type_lookup_by_name`]. Used by
/// the schema-upload pre-flight to classify each declared entity_type
/// as new/idempotent/conflict without opening a write transaction.
pub fn entity_type_lookup_by_name_rtxn(
    rtxn: &ReadTransaction,
    name: &str,
) -> Result<Option<EntityTypeDefinition>, EntityTypeOpError> {
    let t = rtxn.open_table(ENTITY_TYPES_TABLE)?;
    for entry in t.iter()? {
        let (_k, v) = entry?;
        let row = v.value();
        if row.name == name {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

/// Snapshot the active entity-type names as zero-shot classifier labels —
/// `brain:<Name>` in stable id-order. The GLiNER tier reads this each drain
/// cycle so a user's `SCHEMA_UPLOAD` adding entity types reaches the classifier
/// on the next batch without a shard restart.
///
/// Entity types are a flat, namespaceless registry (see the module note), so
/// the `brain:` prefix is a cosmetic convention the resolver strips before
/// lookup (`resolve_entity_type`); a user-uploaded type ("Drug") is labeled
/// `brain:Drug` and still resolves to the bare row. The prefix/format matches
/// what the classifier was tuned against — don't change it without re-measuring
/// GLiNER zero-shot recall.
pub fn entity_type_label_qnames(rtxn: &ReadTransaction) -> Result<Vec<String>, EntityTypeOpError> {
    let t = rtxn.open_table(ENTITY_TYPES_TABLE)?;
    let mut rows: Vec<(u32, String)> = Vec::new();
    for entry in t.iter()? {
        let (k, v) = entry?;
        rows.push((k.value(), v.value().name));
    }
    rows.sort_by_key(|(id, _)| *id);
    Ok(rows
        .into_iter()
        .map(|(_, name)| format!("brain:{name}"))
        .collect())
}

/// Intern an entity_type by name. Idempotent on identical
/// `schema_blob`; refuses to clobber a pre-existing row with a
/// diverging blob.
///
/// Allocates id = `max(existing) + 1` on first registration. Person
/// gets id `1` because it's the first item in the system schema
pub fn entity_type_intern(
    wtxn: &WriteTransaction,
    name: &str,
    schema_blob: Vec<u8>,
    now_unix_nanos: u64,
) -> Result<EntityTypeId, EntityTypeOpError> {
    if let Some(existing) = entity_type_lookup_by_name(wtxn, name)? {
        if existing.schema_blob == schema_blob {
            return Ok(existing.id());
        }
        return Err(EntityTypeOpError::AlreadyExists {
            name: name.to_string(),
            existing_id: existing.id(),
        });
    }

    // Fresh registration.
    let next_id_raw: u32 = {
        let t = wtxn.open_table(ENTITY_TYPES_TABLE)?;
        let mut max: u32 = 0;
        for entry in t.iter()? {
            let (k, _v) = entry?;
            let id = k.value();
            if id > max {
                max = id;
            }
        }
        max.checked_add(1).expect("EntityTypeId space exhausted")
    };

    let row = EntityTypeDefinition::new(
        EntityTypeId::from(next_id_raw),
        name.to_string(),
        schema_blob,
        now_unix_nanos,
    );
    {
        let mut t = wtxn.open_table(ENTITY_TYPES_TABLE)?;
        t.insert(&row.entity_type_id, &row)?;
    }
    Ok(EntityTypeId::from(next_id_raw))
}
