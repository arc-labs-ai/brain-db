//! Apply functions for relation-shaped phases.
//!
//! Full P2c coverage: UpsertRelation, Tombstone(Relation),
//! Supersede(Relation) all real.

use brain_core::Relation;
use brain_metadata::relation::ops::{relation_create, relation_supersede, relation_tombstone};
use brain_metadata::relation::types::relation_type_intern_or_get;
use brain_metadata::tables::relation_type::{RelationTypeDefinition, RELATION_TYPES_TABLE};
use redb::{ReadableTable, WriteTransaction};

use super::ApplyError;
use crate::write::{
    Phase, PhaseAck, SupersedeReplacement, SupersedeTarget, TombstoneTarget, Write,
};

pub fn apply_upsert_relation(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpsertRelation {
        id,
        ty,
        from,
        to,
        confidence,
        evidence_memories,
        is_symmetric,
        extractor,
        extracted_at_unix_nanos,
        properties_blob,
        valid_from_unix_nanos,
        valid_to_unix_nanos,
        relation_type_intern_hint,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UpsertRelation"));
    };

    // Schemaless path: intern the relation_type inside this wtxn so the
    // schemaless RELATION_CREATE costs one fsync instead of two.
    // `relation_type_intern_or_get` is idempotent — concurrent writers
    // converge on the same id without conflict.
    //
    // `is_symmetric` is encoded into the relation row itself; the
    // handler reads it from the resolved row before submit when the
    // hint is `None` (strict mode). For the hint path we have to look
    // up the row's is_symmetric here because intern may have allocated
    // a fresh row with the (open-vocab) default — see the lookup
    // immediately below.
    let resolved_ty = match relation_type_intern_hint {
        None => *ty,
        Some((namespace, name)) => {
            relation_type_intern_or_get(
                wtxn,
                namespace,
                name,
                /* first_seen_lsn */ 0,
                *extracted_at_unix_nanos,
            )
            .map_err(|e| ApplyError::Metadata(format!("relation_type_intern_or_get: {e}")))?
        }
    };
    // For the schemaless hint path, the canonical `is_symmetric` lives
    // on the (possibly just-allocated) relation_type row. Re-read it so
    // a concurrent SCHEMA_UPLOAD that already adopted the qname with a
    // declared symmetry wins over the handler's open-vocab default.
    let effective_is_symmetric = if relation_type_intern_hint.is_some() {
        lookup_is_symmetric_in_wtxn(wtxn, resolved_ty)?
    } else {
        *is_symmetric
    };

    let mut r = Relation::new_root(
        *id,
        resolved_ty,
        *from,
        *to,
        *confidence,
        evidence_memories.clone(),
        *extractor,
        *extracted_at_unix_nanos,
        effective_is_symmetric,
    );
    r.properties_blob = properties_blob.clone();
    r.valid_from_unix_nanos = *valid_from_unix_nanos;
    r.valid_to_unix_nanos = *valid_to_unix_nanos;
    relation_create(wtxn, &r, *extracted_at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("relation_create: {e}")))?;
    Ok(PhaseAck::UpsertedRelation(*id, 1))
}

/// Read `is_symmetric` for a `RelationTypeId` inside a write txn.
/// Used when the schemaless intern path didn't have a pre-resolved
/// relation_type row.
fn lookup_is_symmetric_in_wtxn(
    wtxn: &WriteTransaction,
    ty: brain_core::RelationTypeId,
) -> Result<bool, ApplyError> {
    let t = wtxn
        .open_table(RELATION_TYPES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("open relation_types: {e}")))?;
    let row = t
        .get(&ty.raw())
        .map_err(|e| ApplyError::Storage(format!("relation_types lookup: {e}")))?;
    let row: RelationTypeDefinition = row
        .ok_or_else(|| ApplyError::Invariant(format!("relation_type {ty:?} missing after intern")))?
        .value();
    Ok(row.to_relation_type().is_symmetric)
}

pub fn apply_supersede_relation(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::Supersede {
        target,
        replacement,
        at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected Supersede"));
    };
    let SupersedeTarget::Relation(old_id) = target else {
        return Err(ApplyError::PhaseMisShape("expected Supersede(Relation)"));
    };
    let SupersedeReplacement::Relation(new_relation) = replacement else {
        return Err(ApplyError::PhaseMisShape(
            "expected Supersede with Relation replacement",
        ));
    };
    relation_supersede(wtxn, *old_id, new_relation.as_ref(), *at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("relation_supersede: {e}")))?;
    Ok(PhaseAck::Superseded(*target, replacement.id()))
}

pub fn apply_tombstone_relation(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::Tombstone {
        target,
        at_unix_nanos,
        ..
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected Tombstone"));
    };
    let TombstoneTarget::Relation(id) = target else {
        return Err(ApplyError::PhaseMisShape("expected Tombstone(Relation)"));
    };
    relation_tombstone(wtxn, *id, *at_unix_nanos)
        .map_err(|e| ApplyError::Metadata(format!("relation_tombstone: {e}")))?;
    Ok(PhaseAck::Tombstoned {
        target: *target,
        tombstoned_at_unix_nanos: *at_unix_nanos,
    })
}
