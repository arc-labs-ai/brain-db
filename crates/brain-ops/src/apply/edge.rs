//! Apply Link / Unlink phases.
//!
//! Same code path regardless of whether `from` / `to` are memories,
//! entities, or statements — `brain_metadata::tables::edge::link` is
//! polymorphic over `NodeRef` and handles the auto-mirror for builtin
//! symmetric kinds. Typed-relation disambiguation rides on the
//! `disambiguator` field of the phase.

use brain_metadata::tables::edge::{self, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE};
use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, Write};

/// Apply [`Phase::Link`].
pub fn apply_link(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::Link {
        from,
        to,
        kind,
        weight,
        origin,
        derived_by,
        disambiguator,
        created_at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected Link"));
    };

    let data = EdgeData::new(*weight, *origin, *derived_by, *created_at_unix_nanos);
    let mut edges_t = wtxn
        .open_table(EDGES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("open EDGES: {e:?}")))?;
    let mut edges_rev_t = wtxn
        .open_table(EDGES_REVERSE_TABLE)
        .map_err(|e| ApplyError::Storage(format!("open EDGES_REVERSE: {e:?}")))?;

    edge::link(
        &mut edges_t,
        &mut edges_rev_t,
        *from,
        *kind,
        *to,
        *disambiguator,
        &data,
    )
    .map_err(|e| ApplyError::Metadata(format!("link: {e:?}")))?;

    Ok(PhaseAck::Linked)
}

/// Apply [`Phase::Unlink`].
pub fn apply_unlink(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::Unlink {
        from,
        to,
        kind,
        disambiguator,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected Unlink"));
    };

    let mut edges_t = wtxn
        .open_table(EDGES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("open EDGES: {e:?}")))?;
    let mut edges_rev_t = wtxn
        .open_table(EDGES_REVERSE_TABLE)
        .map_err(|e| ApplyError::Storage(format!("open EDGES_REVERSE: {e:?}")))?;

    let _removed = edge::unlink(
        &mut edges_t,
        &mut edges_rev_t,
        *from,
        *kind,
        *to,
        *disambiguator,
    )
    .map_err(|e| ApplyError::Metadata(format!("unlink: {e:?}")))?;

    Ok(PhaseAck::Unlinked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::{EdgeKind, EdgeKindRef, MemoryId, NodeRef};
    use brain_metadata::tables::edge::zero_disambiguator;
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    use crate::write::{Phase, Write, WriteId};

    fn open_db() -> (TempDir, MetadataDb) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        (dir, db)
    }

    fn empty_write() -> Write {
        Write::single(
            WriteId::new(),
            brain_core::AgentId::default(),
            Phase::Link {
                from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
                to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
                kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
                weight: 0.5,
                origin: 0,
                derived_by: 0,
                disambiguator: zero_disambiguator(),
                created_at_unix_nanos: 0,
            },
        )
    }

    #[test]
    fn link_writes_a_row_then_unlink_removes_it() {
        let (_dir, mut db) = open_db();
        let phase_link = Phase::Link {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.42,
            origin: 1,
            derived_by: 2,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 1_700_000_000_000,
        };
        let phase_unlink = Phase::Unlink {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            disambiguator: zero_disambiguator(),
        };
        let write = empty_write();

        {
            let wtxn = db.write_txn().unwrap();
            let ack = apply_link(&wtxn, &phase_link, &write).unwrap();
            assert!(matches!(ack, PhaseAck::Linked));
            wtxn.commit().unwrap();
        }

        // Confirm the edge exists.
        {
            let rtxn = db.read_txn().unwrap();
            use brain_metadata::tables::edge::edge_get;
            let got = edge_get(
                &rtxn,
                NodeRef::Memory(MemoryId::pack(0, 1, 0)),
                EdgeKindRef::Builtin(EdgeKind::SimilarTo),
                NodeRef::Memory(MemoryId::pack(0, 2, 0)),
                zero_disambiguator(),
            )
            .unwrap();
            assert!(got.is_some(), "edge must exist after link");
        }

        {
            let wtxn = db.write_txn().unwrap();
            let ack = apply_unlink(&wtxn, &phase_unlink, &write).unwrap();
            assert!(matches!(ack, PhaseAck::Unlinked));
            wtxn.commit().unwrap();
        }

        // Confirm the edge is gone.
        {
            let rtxn = db.read_txn().unwrap();
            use brain_metadata::tables::edge::edge_get;
            let got = edge_get(
                &rtxn,
                NodeRef::Memory(MemoryId::pack(0, 1, 0)),
                EdgeKindRef::Builtin(EdgeKind::SimilarTo),
                NodeRef::Memory(MemoryId::pack(0, 2, 0)),
                zero_disambiguator(),
            )
            .unwrap();
            assert!(got.is_none(), "edge must be gone after unlink");
        }
    }

    #[test]
    fn link_rejects_mis_shape() {
        let (_dir, mut db) = open_db();
        let wtxn = db.write_txn().unwrap();
        let phase = Phase::Unlink {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            disambiguator: zero_disambiguator(),
        };
        let err = apply_link(&wtxn, &phase, &empty_write()).unwrap_err();
        assert!(matches!(err, ApplyError::PhaseMisShape(_)));
    }
}
