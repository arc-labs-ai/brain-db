//! Typed CRUD + interning over the entity-type registry.
//!
//! Mirrors [`crate::predicate_ops`] (17.3) and
//! [`crate::relation_type_ops`] (18.3), introduced in 19.7 when the
//! system-schema bootstrap started flowing every entity-type
//! registration through the shared apply path.
//!
//! Entity types don't have a `(namespace, name)` qname today —
//! `Person` lives at the bare name "Person" with the implicit
//! `brain:` namespace. The on-disk row layout pre-dates phase 19's
//! namespace scheme; widening it is tracked as a §22+ migration
//! concern (per-namespace ID space). For now the registry is keyed
//! on bare `name`.

use brain_core::EntityTypeId;
use redb::{ReadableTable, WriteTransaction};

use crate::tables::knowledge::entity_type::{EntityTypeDefinition, ENTITY_TYPES_TABLE};

#[derive(thiserror::Error, Debug)]
pub enum EntityTypeOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("entity_type name {name:?} already exists with id {existing_id:?} but constraints differ")]
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

/// Intern an entity_type by name. Idempotent on identical
/// `schema_blob`; refuses to clobber a pre-existing row with a
/// diverging blob.
///
/// Allocates id = `max(existing) + 1` on first registration. Person
/// gets id `1` because it's the first item in the system schema
/// (spec §21/06 §3).
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
