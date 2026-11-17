//! Apply functions for relation-shaped phases.
//!
//! Covers UpsertRelation, Tombstone(Relation), and
//! Supersede(Relation).

use brain_core::{Relation, RelationId};
use brain_metadata::relation::ops::{
    relation_create, relation_supersede, relation_tombstone, RelationOpError,
};
use brain_metadata::relation::types::{relation_type_intern_or_get, RelationTypeOpError};
use brain_metadata::tables::relation::{RelationMetadata, RELATION_METADATA_TABLE};
use brain_metadata::tables::relation_type::{RelationTypeDefinition, RELATION_TYPES_TABLE};
use brain_metadata::DeclScope;
use redb::{ReadableTable, WriteTransaction};

use super::{ApplyError, RedbFault};
use crate::write::{
    Phase, PhaseAck, SupersedeReplacement, SupersedeTarget, TombstoneTarget, Write,
};

/// Tenant wall for a relation mutation. Loads the sidecar row inside the
/// wtxn and confirms it belongs to the caller's `(namespace, space)`
/// scope; a row owned by another tenant (or a missing one) reads as
/// NotFound — no existence leak. Mirrors the memory-layer wall in
/// `apply_tombstone_memory`: `relation::ops` mutators look the sidecar up
/// by global id alone, so the apply layer is the atomic last line of
/// defense against a cross-tenant `RelationId`.
fn relation_scope_guard(
    wtxn: &WriteTransaction,
    id: RelationId,
    write: &Write,
) -> Result<(), ApplyError> {
    let row: Option<RelationMetadata> = {
        let t = wtxn
            .open_table(RELATION_METADATA_TABLE)
            .map_err(|e| ApplyError::storage("open relation metadata", &e))?;
        let got = t
            .get(&id.to_bytes())
            .map_err(|e| ApplyError::storage("relation lookup", &e))?;
        got.map(|g| g.value())
    };
    let caller = brain_metadata::RowScope::new(write.namespace, write.space_id);
    match row {
        Some(m) if m.scope() == caller => Ok(()),
        _ => Err(ApplyError::NotFound {
            what: "relation",
            detail: format!("{id:?}"),
        }),
    }
}

/// Classify a [`RelationTypeOpError`] for the wire. A qname already taken by
/// a differently-constrained relation type is the caller racing committed state;
/// everything else is the engine's own fault.
fn relation_type_op_err(ctx: &'static str, e: RelationTypeOpError) -> ApplyError {
    use brain_metadata::relation::types::RelationTypeOpError as E;
    let detail = format!("{ctx}: {e}");
    match e {
        E::AlreadyExists { .. } => ApplyError::Conflict(detail),
        _ => ApplyError::Metadata(detail),
    }
}

/// Classify a [`RelationOpError`] for the wire. A missing relation or a missing
/// endpoint entity is the caller pointing at something that is not there, and a
/// tombstoned, superseded, already-existing or cardinality-blocked target is the
/// caller racing committed state — neither can succeed on a retry of the same
/// request. Redb failures retain their retry classification. Everything else is
/// the engine's own fault. An undeclared relation type, a malformed argument,
/// and a supersede or endpoint whose type does not match are the caller's
/// input to fix.
///
/// The classification per variant is the one the relation read path already
/// publishes in `handlers::relation::map_relation_op_error`, so a client sees
/// the same verdict whichever side of the write boundary produced it.
fn relation_op_err(ctx: &'static str, e: RelationOpError) -> ApplyError {
    use brain_metadata::relation::ops::RelationOpError as E;
    let detail = format!("{ctx}: {e}");
    match e {
        E::NotFound(_) => ApplyError::NotFound {
            what: "relation",
            detail,
        },
        E::UnknownEntity(_) => ApplyError::NotFound {
            what: "entity",
            detail,
        },
        E::AlreadyExists(_)
        | E::AlreadySuperseded(..)
        | E::AlreadyTombstoned(_)
        | E::CardinalityViolation { .. } => ApplyError::Conflict(detail),
        E::Storage(ref se) => ApplyError::Storage {
            fault: se.storage_fault(),
            detail,
        },
        E::UnknownRelationType(_)
        | E::InvalidArgument(_)
        | E::TypeMismatch { .. }
        | E::EndpointMismatch
        | E::EndpointTypeViolation { .. } => ApplyError::InvalidRequest(detail),
        E::RelationTypeOp(inner) => relation_type_op_err(ctx, inner),
        _ => ApplyError::Metadata(detail),
    }
}

pub fn apply_upsert_relation(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let scope = brain_metadata::RowScope::new(write.namespace, write.space_id);
    let Phase::UpsertRelation {
        id,
        ty,
        session,
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
                DeclScope::base(namespace),
                name,
                /* first_seen_lsn */ 0,
                *extracted_at_unix_nanos,
            )
            .map_err(|e| relation_type_op_err("relation_type_intern_or_get", e))?
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
    let stored_id = relation_create(wtxn, scope, *session, &r, *extracted_at_unix_nanos)
        .map_err(|e| relation_op_err("relation_create", e))?;
    // Ack the row `relation_create` actually settled on, not the phase's
    // minted id: the function has three exits — a fresh insert at the
    // minted id, a cardinality supersession, and a content-dedup no-op
    // that keeps a pre-existing identical active `(from, type, to)` edge
    // and returns ITS id. Acking the minted id on the dedup path would
    // hand the client a relation_id that names no stored row.
    Ok(PhaseAck::UpsertedRelation(stored_id, 1))
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
        .map_err(|e| ApplyError::storage("open relation_types", &e))?;
    let row = t
        .get(&ty.raw())
        .map_err(|e| ApplyError::storage("relation_types lookup", &e))?;
    let row: RelationTypeDefinition = row
        .ok_or_else(|| ApplyError::Invariant(format!("relation_type {ty:?} missing after intern")))?
        .value();
    Ok(row.to_relation_type().is_symmetric)
}

pub fn apply_supersede_relation(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let scope = brain_metadata::RowScope::new(write.namespace, write.space_id);
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
    // Wall: the caller must own the row it supersedes. `relation_supersede`
    // re-checks this too (defense in depth); guarding here keeps the write
    // from touching any table when the target is foreign / absent.
    relation_scope_guard(wtxn, *old_id, write)?;
    // Explicit RELATION_SUPERSEDE carries no session on the phase; the
    // replacement row lands in the default session.
    relation_supersede(
        wtxn,
        scope,
        brain_core::SessionId::DEFAULT,
        *old_id,
        new_relation.as_ref(),
        *at_unix_nanos,
    )
    .map_err(|e| relation_op_err("relation_supersede", e))?;
    Ok(PhaseAck::Superseded(*target, replacement.id()))
}

pub fn apply_tombstone_relation(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
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
    relation_scope_guard(wtxn, *id, write)?;
    relation_tombstone(wtxn, *id, *at_unix_nanos)
        .map_err(|e| relation_op_err("relation_tombstone", e))?;
    Ok(PhaseAck::Tombstoned {
        target: *target,
        tombstoned_at_unix_nanos: *at_unix_nanos,
    })
}

#[cfg(all(test, not(miri)))]
mod tenant_wall_tests {
    //! Cross-tenant write isolation for the relation apply path. A caller
    //! in tenant B must not tombstone / supersede a relation owned by
    //! tenant A; the mutation reads as NotFound and A's row is untouched.
    use super::*;
    use brain_core::{
        Cardinality, Entity, EntityId, EntityType, ExtractorId, RelationTypeId, SessionId,
    };
    use brain_metadata::entity::ops::{entity_put, normalize_name};
    use brain_metadata::relation::ops::{relation_create, relation_get};
    use brain_metadata::relation::types::relation_type_intern;
    use brain_metadata::{MetadataDb, RowScope};
    use tempfile::TempDir;

    use crate::write::{
        Phase, SupersedeReplacement, SupersedeTarget, TombstoneTarget, Write, WriteId,
    };

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn scope_a() -> RowScope {
        RowScope::from_bytes(1, [0xA1; 16])
    }
    fn scope_b() -> RowScope {
        RowScope::from_bytes(2, [0xB2; 16])
    }

    fn open_db() -> (TempDir, MetadataDb) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        (dir, db)
    }

    fn write_for(scope: RowScope, phase: Phase) -> Write {
        Write::single(WriteId::new(), scope.space(), phase).with_namespace(scope.namespace())
    }

    fn make_entity(db: &MetadataDb, scope: RowScope, name: &str) -> EntityId {
        let id = EntityId::new();
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            normalize_name(name),
            NOW,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, scope, SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_type(db: &MetadataDb, name: &str) -> RelationTypeId {
        let wtxn = db.write_txn().unwrap();
        let id = relation_type_intern(
            &wtxn,
            DeclScope::base("test"),
            name,
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
    }

    fn fresh_rel(ty: RelationTypeId, from: EntityId, to: EntityId) -> Relation {
        Relation::new_root(
            RelationId::new(),
            ty,
            from,
            to,
            0.9,
            vec![],
            ExtractorId::from(0),
            NOW,
            false,
        )
    }

    /// Seed a current relation owned by tenant A. Returns (id, type, from, to).
    fn seed_a(db: &MetadataDb) -> (RelationId, RelationTypeId, EntityId, EntityId) {
        let from = make_entity(db, scope_a(), "ada");
        let to = make_entity(db, scope_a(), "charles");
        let ty = intern_type(db, "knows");
        let r = fresh_rel(ty, from, to);
        let wtxn = db.write_txn().unwrap();
        relation_create(&wtxn, scope_a(), SessionId::DEFAULT, &r, NOW).unwrap();
        wtxn.commit().unwrap();
        (r.id, ty, from, to)
    }

    #[test]
    fn cross_tenant_tombstone_denied_and_row_untouched() {
        let (_dir, db) = open_db();
        let (id, _, _, _) = seed_a(&db);
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Relation(id),
            reason: 0,
            at_unix_nanos: NOW + 1_000,
        };
        let wtxn = db.write_txn().unwrap();
        let err = apply_tombstone_relation(&wtxn, &phase, &write_for(scope_b(), phase.clone()))
            .expect_err("tenant B must not tombstone tenant A's relation");
        assert!(matches!(
            err,
            ApplyError::NotFound {
                what: "relation",
                ..
            }
        ));
        drop(wtxn);

        let rtxn = db.read_txn().unwrap();
        let got = relation_get(&rtxn, id)
            .unwrap()
            .expect("A's relation present");
        assert!(!got.tombstoned, "A's relation must not be tombstoned");
    }

    #[test]
    fn cross_tenant_supersede_denied_no_row_in_victim_scope() {
        let (_dir, db) = open_db();
        let (old_id, ty, from, to) = seed_a(&db);
        let replacement = fresh_rel(ty, from, to);
        let new_id = replacement.id;
        let phase = Phase::Supersede {
            target: SupersedeTarget::Relation(old_id),
            replacement: SupersedeReplacement::Relation(Box::new(replacement)),
            at_unix_nanos: NOW + 1_000,
        };
        let wtxn = db.write_txn().unwrap();
        let err = apply_supersede_relation(&wtxn, &phase, &write_for(scope_b(), phase.clone()))
            .expect_err("tenant B must not supersede tenant A's relation");
        assert!(matches!(
            err,
            ApplyError::NotFound {
                what: "relation",
                ..
            }
        ));
        drop(wtxn);

        let rtxn = db.read_txn().unwrap();
        let old_got = relation_get(&rtxn, old_id).unwrap().unwrap();
        assert!(
            old_got.superseded_by.is_none() && !old_got.tombstoned,
            "A's relation must stay current"
        );
        assert!(
            relation_get(&rtxn, new_id).unwrap().is_none(),
            "no B-authored replacement may land in A's scope"
        );
    }

    #[test]
    fn same_tenant_tombstone_succeeds() {
        let (_dir, db) = open_db();
        let (id, _, _, _) = seed_a(&db);
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Relation(id),
            reason: 0,
            at_unix_nanos: NOW + 1_000,
        };
        let wtxn = db.write_txn().unwrap();
        apply_tombstone_relation(&wtxn, &phase, &write_for(scope_a(), phase.clone()))
            .expect("same-tenant tombstone must succeed");
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = relation_get(&rtxn, id).unwrap().unwrap();
        assert!(got.tombstoned, "same-tenant tombstone must apply");
    }
}

#[cfg(all(test, not(miri)))]
mod content_dedup_tests {
    //! Re-asserting a byte-identical `(from, type, to)` edge is
    //! idempotent: `relation_create` keeps the edge it already has and
    //! writes nothing. The ack has to carry that pre-existing edge's
    //! id. Acking the phase's own minted id instead hands the client a
    //! `relation_id` that names no stored row — and because the
    //! relation handler does no post-commit read-back, the write still
    //! reports success, so the phantom id only surfaces much later on a
    //! RELATION_GET that cannot find it.
    use super::*;
    use brain_core::{Entity, EntityId, EntityType, ExtractorId, RelationTypeId, SessionId};
    use brain_metadata::entity::ops::{entity_put, normalize_name};
    use brain_metadata::relation::ops::relation_get;
    use brain_metadata::{MetadataDb, RowScope};
    use tempfile::TempDir;

    use crate::write::{Phase, PhaseAck, Write, WriteId};

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn scope() -> RowScope {
        RowScope::from_bytes(1, [0xD4; 16])
    }

    /// A fresh phase for the same `(from, knows, to)` edge. Each call
    /// mints its own phase id, exactly as a repeated client request
    /// does.
    fn upsert_phase(ty: RelationTypeId, from: EntityId, to: EntityId) -> Phase {
        Phase::UpsertRelation {
            id: RelationId::new(),
            ty,
            session: SessionId::DEFAULT,
            from,
            to,
            confidence: 0.9,
            evidence_memories: Vec::new(),
            is_symmetric: false,
            extractor: ExtractorId::from(0),
            extracted_at_unix_nanos: NOW,
            properties_blob: Vec::new(),
            valid_from_unix_nanos: None,
            valid_to_unix_nanos: None,
            relation_type_intern_hint: None,
        }
    }

    fn minted_id(phase: &Phase) -> RelationId {
        match phase {
            Phase::UpsertRelation { id, .. } => *id,
            _ => unreachable!("built as UpsertRelation"),
        }
    }

    #[test]
    fn repeating_an_identical_edge_acks_the_relation_that_already_exists() {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        let scope = scope();

        let put_entity = |name: &str| {
            let id = EntityId::new();
            let wtxn = db.write_txn().unwrap();
            let e = Entity::new_active(
                id,
                EntityType::PERSON_ID,
                name.into(),
                normalize_name(name),
                NOW,
            );
            entity_put(&wtxn, scope, SessionId::DEFAULT, &e).unwrap();
            wtxn.commit().unwrap();
            id
        };
        let from = put_entity("ada");
        let to = put_entity("grace");

        // An interned (open-vocab) relation type is ManyToMany, so no
        // cardinality conflict catches the repeat and content dedup is
        // the only thing that collapses it.
        let ty = {
            let wtxn = db.write_txn().unwrap();
            let id = relation_type_intern_or_get(&wtxn, DeclScope::base("test"), "knows", 0, NOW)
                .unwrap();
            wtxn.commit().unwrap();
            id
        };

        let apply = |phase: &Phase| -> RelationId {
            let write = Write::single(WriteId::new(), scope.space(), phase.clone())
                .with_namespace(scope.namespace());
            let wtxn = db.write_txn().unwrap();
            let ack = apply_upsert_relation(&wtxn, phase, &write).expect("apply must succeed");
            wtxn.commit().unwrap();
            match ack {
                PhaseAck::UpsertedRelation(id, _) => id,
                other => panic!("expected an UpsertedRelation ack, got {other:?}"),
            }
        };

        let first_phase = upsert_phase(ty, from, to);
        let second_phase = upsert_phase(ty, from, to);
        assert_ne!(
            minted_id(&first_phase),
            minted_id(&second_phase),
            "the two attempts must mint distinct phase ids"
        );

        let first = apply(&first_phase);
        let second = apply(&second_phase);

        assert_eq!(
            first, second,
            "the repeat must ack the existing edge's id, not its own minted id"
        );
        assert_ne!(
            second,
            minted_id(&second_phase),
            "the dedup no-op wrote no row at the second phase's minted id"
        );
        let rtxn = db.read_txn().unwrap();
        assert!(
            relation_get(&rtxn, second).unwrap().is_some(),
            "the acked id must resolve to a stored relation"
        );
    }
}

#[cfg(all(test, not(miri)))]
mod missing_endpoint_tests {
    //! A relation create naming an endpoint entity that does not exist is the
    //! caller's mistake and can never succeed on a retry, so it must leave the
    //! apply path as a not-found. Classifying it as `ApplyError::Metadata`
    //! makes it `WriterError::Internal`, so the client sees a retryable 5xx
    //! and retries forever.
    use super::*;
    use brain_core::{Entity, EntityId, EntityType, ExtractorId, RelationTypeId, SessionId};
    use brain_metadata::entity::ops::{entity_put, normalize_name};
    use brain_metadata::{MetadataDb, RowScope};
    use tempfile::TempDir;

    use crate::write::{Phase, Write, WriteId};

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn scope() -> RowScope {
        RowScope::from_bytes(1, [0xD4; 16])
    }

    fn upsert_phase(ty: RelationTypeId, from: EntityId, to: EntityId) -> Phase {
        Phase::UpsertRelation {
            id: RelationId::new(),
            ty,
            session: SessionId::DEFAULT,
            from,
            to,
            confidence: 0.9,
            evidence_memories: Vec::new(),
            is_symmetric: false,
            extractor: ExtractorId::from(0),
            extracted_at_unix_nanos: NOW,
            properties_blob: Vec::new(),
            valid_from_unix_nanos: None,
            valid_to_unix_nanos: None,
            relation_type_intern_hint: None,
        }
    }

    fn put_entity(db: &MetadataDb, name: &str) -> EntityId {
        let scope = scope();
        let id = EntityId::new();
        let wtxn = db.write_txn().unwrap();
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            normalize_name(name),
            NOW,
        );
        entity_put(&wtxn, scope, SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_type(db: &MetadataDb) -> RelationTypeId {
        let wtxn = db.write_txn().unwrap();
        let id =
            relation_type_intern_or_get(&wtxn, DeclScope::base("test"), "knows", 0, NOW).unwrap();
        wtxn.commit().unwrap();
        id
    }

    #[test]
    fn missing_from_entity_is_not_found() {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        let scope = scope();
        let to = put_entity(&db, "grace");
        let from = EntityId::new();
        let ty = intern_type(&db);
        let phase = upsert_phase(ty, from, to);
        let write = Write::single(WriteId::new(), scope.space(), phase.clone())
            .with_namespace(scope.namespace());
        let wtxn = db.write_txn().unwrap();
        let err = apply_upsert_relation(&wtxn, &phase, &write)
            .expect_err("a missing from entity must be rejected as not found");
        assert!(
            matches!(err, ApplyError::NotFound { what: "entity", .. }),
            "a missing from entity must surface as a not-found so the client does not retry"
        );
        assert!(
            !matches!(err, ApplyError::Metadata(_)),
            "a missing from entity must not become an internal retryable error"
        );
    }

    #[test]
    fn missing_to_entity_is_not_found() {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        let scope = scope();
        let from = put_entity(&db, "ada");
        let to = EntityId::new();
        let ty = intern_type(&db);
        let phase = upsert_phase(ty, from, to);
        let write = Write::single(WriteId::new(), scope.space(), phase.clone())
            .with_namespace(scope.namespace());
        let wtxn = db.write_txn().unwrap();
        let err = apply_upsert_relation(&wtxn, &phase, &write)
            .expect_err("a missing to entity must be rejected as not found");
        assert!(
            matches!(err, ApplyError::NotFound { what: "entity", .. }),
            "a missing to entity must surface as a not-found so the client does not retry"
        );
        assert!(
            !matches!(err, ApplyError::Metadata(_)),
            "a missing to entity must not become an internal retryable error"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::{EntityId, EntityTypeId, RelationTypeId};

    /// Every relation refusal the caller can fix must leave the apply layer as
    /// an invalid request. Flattened into `Metadata` it becomes an engine
    /// fault on the wire, in a category clients retry — against a write that
    /// is refused identically every time.
    #[test]
    fn caller_fixable_relation_faults_are_invalid_requests() {
        let cases = [
            RelationOpError::UnknownRelationType(RelationTypeId::from(7)),
            RelationOpError::InvalidArgument("confidence out of range"),
            RelationOpError::TypeMismatch {
                old: RelationTypeId::from(1),
                new: RelationTypeId::from(2),
            },
            RelationOpError::EndpointMismatch,
            RelationOpError::EndpointTypeViolation {
                side: "from",
                entity: EntityId::from_bytes([0; 16]),
                expected: EntityTypeId::from(4),
                actual: EntityTypeId::from(5),
            },
        ];
        for case in cases {
            let rendered = case.to_string();
            let mapped = relation_op_err("relation_create", case);
            assert!(
                matches!(mapped, ApplyError::InvalidRequest(_)),
                "{rendered} must classify as an invalid request, got {mapped:?}"
            );
        }
    }
}
