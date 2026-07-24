//! Apply functions for the space/session registry phases.
//!
//! Each mutates the registry tables under the write's `(namespace, space)`
//! scope by delegating to the idempotent `brain_metadata::registry` helpers —
//! the same helpers WAL recovery replays, so live apply and replay converge.

use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, Write};

fn scope_bytes(write: &Write) -> (u32, [u8; 16]) {
    (write.namespace.raw(), write.space_id.into())
}

/// Apply [`Phase::SpaceCreate`].
pub fn apply_space_create(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::SpaceCreate {
        created_at_unix_nanos,
        metadata,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected SpaceCreate"));
    };
    let (ns, space) = scope_bytes(write);
    let (_, created) =
        brain_metadata::space_create(wtxn, ns, space, *created_at_unix_nanos, metadata.clone())
            .map_err(|e| ApplyError::Metadata(format!("space_create: {e}")))?;
    Ok(PhaseAck::SpaceCreated { created })
}

/// Apply [`Phase::SpaceDelete`] — removes the registry rows. The underlying
/// memory/graph cascade is the handler's job (it drives FORGET tombstones).
pub fn apply_space_delete(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::SpaceDelete { .. } = phase else {
        return Err(ApplyError::PhaseMisShape("expected SpaceDelete"));
    };
    let (ns, space) = scope_bytes(write);
    let existed = brain_metadata::space_delete_registry(wtxn, ns, space)
        .map_err(|e| ApplyError::Metadata(format!("space_delete: {e}")))?;
    Ok(PhaseAck::SpaceDeleted { existed })
}

/// Apply [`Phase::SessionCreate`].
pub fn apply_session_create(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::SessionCreate {
        session_id,
        title,
        created_at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected SessionCreate"));
    };
    let (ns, space) = scope_bytes(write);
    let (_, created) = brain_metadata::session_create(
        wtxn,
        ns,
        space,
        session_id.raw(),
        *created_at_unix_nanos,
        title.clone(),
    )
    .map_err(|e| ApplyError::Metadata(format!("session_create: {e}")))?;
    Ok(PhaseAck::SessionCreated { created })
}

/// Apply [`Phase::SessionDelete`] — removes the registry row. The memory
/// cascade is the handler's job.
pub fn apply_session_delete(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::SessionDelete { session_id, .. } = phase else {
        return Err(ApplyError::PhaseMisShape("expected SessionDelete"));
    };
    let (ns, space) = scope_bytes(write);
    let existed = brain_metadata::session_delete_registry(wtxn, ns, space, session_id.raw())
        .map_err(|e| ApplyError::Metadata(format!("session_delete: {e}")))?;
    Ok(PhaseAck::SessionDeleted { existed })
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::apply::dispatch;
    use crate::write::{PhaseAck, Write, WriteId};
    use brain_core::{NamespaceId, SessionId, SpaceId};
    use brain_metadata::MetadataDb;

    fn write_for(phase: Phase, space: SpaceId, ns: NamespaceId) -> Write {
        Write::single(WriteId::new(), space, phase).with_namespace(ns)
    }

    fn apply(db: &MetadataDb, phase: Phase, space: SpaceId, ns: NamespaceId) -> PhaseAck {
        let w = write_for(phase.clone(), space, ns);
        let wtxn = db.write_txn().unwrap();
        let ack = dispatch(&wtxn, &phase, &w).unwrap();
        wtxn.commit().unwrap();
        ack
    }

    /// Full create → list → delete round-trip through the Phase/apply layer
    /// for both the space and session registries.
    #[test]
    fn space_and_session_create_list_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("m.redb")).unwrap();
        let ns = NamespaceId::from(3);
        let space = SpaceId::from([0x9A; 16]);

        // SPACE_CREATE (idempotent).
        assert!(matches!(
            apply(&db, Phase::SpaceCreate { created_at_unix_nanos: 100, metadata: None }, space, ns),
            PhaseAck::SpaceCreated { created: true }
        ));
        assert!(matches!(
            apply(&db, Phase::SpaceCreate { created_at_unix_nanos: 200, metadata: None }, space, ns),
            PhaseAck::SpaceCreated { created: false }
        ));
        // SPACE_LIST sees exactly this namespace's space.
        {
            let r = db.read_txn().unwrap();
            let spaces = brain_metadata::space_list(&r, ns.raw(), 0).unwrap();
            assert_eq!(spaces.len(), 1);
            assert_eq!(spaces[0].space_id, <[u8; 16]>::from(space));
        }

        // SESSION_CREATE + SESSION_LIST.
        apply(
            &db,
            Phase::SessionCreate {
                session_id: SessionId(7),
                title: Some("alpha".into()),
                created_at_unix_nanos: 300,
            },
            space,
            ns,
        );
        {
            let r = db.read_txn().unwrap();
            let sessions =
                brain_metadata::session_list(&r, ns.raw(), space.into(), 0).unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, 7);
        }

        // SESSION_DELETE removes the registry row.
        assert!(matches!(
            apply(
                &db,
                Phase::SessionDelete { session_id: SessionId(7), hard: false, at_unix_nanos: 400 },
                space,
                ns
            ),
            PhaseAck::SessionDeleted { existed: true }
        ));
        {
            let r = db.read_txn().unwrap();
            assert!(brain_metadata::session_list(&r, ns.raw(), space.into(), 0)
                .unwrap()
                .is_empty());
        }

        // SPACE_DELETE removes the space registry row.
        assert!(matches!(
            apply(&db, Phase::SpaceDelete { at_unix_nanos: 500 }, space, ns),
            PhaseAck::SpaceDeleted { existed: true }
        ));
        {
            let r = db.read_txn().unwrap();
            assert!(brain_metadata::space_list(&r, ns.raw(), 0).unwrap().is_empty());
        }
    }
}
