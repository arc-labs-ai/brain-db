//! Apply Link / Unlink phases.
//!
//! Same code path regardless of whether `from` / `to` are memories,
//! entities, or statements — `brain_metadata::tables::edge::link` is
//! polymorphic over `NodeRef` and handles the auto-mirror for builtin
//! symmetric kinds. Typed-relation disambiguation rides on the
//! `disambiguator` field of the phase.

use brain_core::{MemoryId, NodeRef};
use brain_metadata::tables::edge::{self, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use redb::{ReadableTable, WriteTransaction};

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

    // Detect "already-existed" inside the wtxn so we don't double-
    // count the denormalised edge counters. edge::link is upsert
    // (overwrites weight) — bumping the count again would corrupt
    // the denorm. The forward-key encoding matches edge::link's,
    // so a direct table.get is enough.
    let fwd_key = edge::EdgeKey {
        from: *from,
        kind: *kind,
        to: *to,
        disambiguator: *disambiguator,
    }
    .encode();
    let already_existed = edges_t
        .get(fwd_key.as_slice())
        .map_err(|e| ApplyError::Storage(format!("EDGES get: {e:?}")))?
        .is_some();

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

    // Drop the table borrows before bump_edge_count opens MEMORIES.
    drop(edges_t);
    drop(edges_rev_t);

    // Maintain the denormalised edge counters on Memory endpoints.
    // Entity / Statement endpoints don't carry these counters yet.
    if !already_existed {
        if let (NodeRef::Memory(src), NodeRef::Memory(tgt)) = (*from, *to) {
            bump_edge_count(wtxn, src, true, 1)?;
            bump_edge_count(wtxn, tgt, false, 1)?;
        }
    }

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

    let removed = {
        let mut edges_t = wtxn
            .open_table(EDGES_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open EDGES: {e:?}")))?;
        let mut edges_rev_t = wtxn
            .open_table(EDGES_REVERSE_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open EDGES_REVERSE: {e:?}")))?;

        edge::unlink(
            &mut edges_t,
            &mut edges_rev_t,
            *from,
            *kind,
            *to,
            *disambiguator,
        )
        .map_err(|e| ApplyError::Metadata(format!("unlink: {e:?}")))?
        // Drop borrows on scope exit so bump_edge_count can open MEMORIES.
    };

    // Decrement counters when we actually removed an edge between memories.
    if removed {
        if let (NodeRef::Memory(src), NodeRef::Memory(tgt)) = (*from, *to) {
            bump_edge_count(wtxn, src, true, -1)?;
            bump_edge_count(wtxn, tgt, false, -1)?;
        }
    }

    Ok(PhaseAck::Unlinked)
}

/// Adjust `edges_out_count` (`out=true`) or `edges_in_count` on
/// `memory_id` by `delta`. No-op when the memory row doesn't exist —
/// the apply path validates target existence before queuing the
/// phase; a stale phase racing reclamation just doesn't update the
/// gone row.
fn bump_edge_count(
    wtxn: &WriteTransaction,
    memory_id: MemoryId,
    out: bool,
    delta: i32,
) -> Result<(), ApplyError> {
    let key = memory_id.to_be_bytes();
    let mut row: MemoryMetadata = {
        let t = wtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open MEMORIES: {e:?}")))?;
        let Some(g) = t
            .get(key)
            .map_err(|e| ApplyError::Storage(format!("MEMORIES get: {e:?}")))?
        else {
            return Ok(());
        };
        g.value()
    };

    let cur = if out {
        row.edges_out_count
    } else {
        row.edges_in_count
    };
    let new = if delta >= 0 {
        cur.saturating_add(delta as u32)
    } else {
        cur.saturating_sub((-delta) as u32)
    };
    if out {
        row.edges_out_count = new;
    } else {
        row.edges_in_count = new;
    }

    let mut t = wtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("open MEMORIES: {e:?}")))?;
    t.insert(key, row)
        .map_err(|e| ApplyError::Storage(format!("MEMORIES insert (edge count): {e:?}")))?;
    Ok(())
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
            brain_core::SpaceId::default(),
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
        let (_dir, db) = open_db();
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
        let (_dir, db) = open_db();
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
