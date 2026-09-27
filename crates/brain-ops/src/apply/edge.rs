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
    write: &Write,
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

    // Tenant wall (authoritative, inside the write txn). Memory endpoints are
    // enumerable packed u128s, so without this guard a caller could forge
    // cross-tenant edges (or probe foreign-id existence) by id. Both memory
    // endpoints must live in the caller's space; a foreign/missing endpoint
    // reads as absent, so the edge is silently not created — the same
    // NotFound-shaped leniency LINK already applies to a missing endpoint,
    // with no foreign-exists vs absent distinction. The space id folds the
    // namespace (disjoint per tenant) and is the scope field threaded
    // reliably through every write path. Entity / Statement endpoints are
    // produced only by scoped extractor / typed-graph writes, so only Memory
    // endpoints need the by-id wall here.
    let caller_space = <[u8; 16]>::from(write.space_id);
    for endpoint in [from, to] {
        if let NodeRef::Memory(mem_id) = endpoint {
            if !memory_in_space(wtxn, *mem_id, caller_space)? {
                return Ok(PhaseAck::Linked);
            }
        }
    }

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

/// `true` when `memory_id` exists and its row is owned by `space`. A
/// missing row and a foreign-space row both return `false` — the LINK
/// apply guard treats them identically (no edge created, no existence
/// oracle). The space id folds the namespace, so this is a complete
/// tenant-isolation check.
fn memory_in_space(
    wtxn: &WriteTransaction,
    memory_id: MemoryId,
    space: [u8; 16],
) -> Result<bool, ApplyError> {
    let t = wtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("open MEMORIES: {e:?}")))?;
    let Some(g) = t
        .get(memory_id.to_be_bytes())
        .map_err(|e| ApplyError::Storage(format!("MEMORIES get: {e:?}")))?
    else {
        return Ok(false);
    };
    Ok(g.value().space_id_bytes == space)
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

    /// Insert an ACTIVE memory row owned by `space` so the LINK apply
    /// tenant-wall (`memory_in_space`) admits it as an endpoint.
    fn seed_memory(db: &MetadataDb, id: MemoryId, space: brain_core::SpaceId) {
        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            let row = MemoryMetadata::new_active(
                id,
                brain_core::NamespaceId::SYSTEM,
                space,
                brain_core::SessionId(0),
                0,
                id.version(),
                brain_core::MemoryKind::Episodic,
                [0u8; 16],
                0.5,
                0,
                0,
            );
            t.insert(&id.to_be_bytes(), row).unwrap();
        }
        wtxn.commit().unwrap();
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

        // Both endpoints must exist in the write's space for the LINK
        // apply tenant-wall to admit them.
        seed_memory(&db, MemoryId::pack(0, 1, 0), write.space_id);
        seed_memory(&db, MemoryId::pack(0, 2, 0), write.space_id);

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

    fn link_phase(src: MemoryId, tgt: MemoryId) -> Phase {
        Phase::Link {
            from: NodeRef::Memory(src),
            to: NodeRef::Memory(tgt),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.5,
            origin: 1,
            derived_by: 2,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 1,
        }
    }

    fn edge_present(db: &MetadataDb, src: MemoryId, tgt: MemoryId) -> bool {
        let rtxn = db.read_txn().unwrap();
        brain_metadata::tables::edge::edge_get(
            &rtxn,
            NodeRef::Memory(src),
            EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            NodeRef::Memory(tgt),
            zero_disambiguator(),
        )
        .unwrap()
        .is_some()
    }

    /// Tenant B linking two of tenant A's memories must NOT create an
    /// edge — the apply tenant-wall reads foreign endpoints as absent and
    /// no-ops (LINK leniency), so nothing is written to A's graph.
    #[test]
    fn link_apply_refuses_cross_tenant_endpoints() {
        let (_dir, db) = open_db();
        let space_a = brain_core::SpaceId::new();
        let space_b = brain_core::SpaceId::new();
        let a1 = MemoryId::pack(0, 1, 0);
        let a2 = MemoryId::pack(0, 2, 0);
        seed_memory(&db, a1, space_a);
        seed_memory(&db, a2, space_a);

        // A write scoped to tenant B links A's ids.
        let write_b = Write::single(WriteId::new(), space_b, link_phase(a1, a2));
        {
            let wtxn = db.write_txn().unwrap();
            let ack = apply_link(&wtxn, &link_phase(a1, a2), &write_b).unwrap();
            assert!(matches!(ack, PhaseAck::Linked)); // lenient no-op
            wtxn.commit().unwrap();
        }
        assert!(
            !edge_present(&db, a1, a2),
            "cross-tenant LINK must not create an edge in A's graph"
        );

        // Tenant A's own LINK of the same ids DOES create the edge.
        let write_a = Write::single(WriteId::new(), space_a, link_phase(a1, a2));
        {
            let wtxn = db.write_txn().unwrap();
            apply_link(&wtxn, &link_phase(a1, a2), &write_a).unwrap();
            wtxn.commit().unwrap();
        }
        assert!(
            edge_present(&db, a1, a2),
            "same-tenant LINK must create the edge"
        );
    }

    /// A LINK where only one endpoint is foreign is still a no-op — both
    /// endpoints must be in the caller's space.
    #[test]
    fn link_apply_refuses_one_foreign_endpoint() {
        let (_dir, db) = open_db();
        let space_a = brain_core::SpaceId::new();
        let space_b = brain_core::SpaceId::new();
        let a1 = MemoryId::pack(0, 1, 0);
        let b1 = MemoryId::pack(0, 9, 0);
        seed_memory(&db, a1, space_a);
        seed_memory(&db, b1, space_b);

        // B owns b1 but not a1; linking b1 -> a1 must no-op.
        let write_b = Write::single(WriteId::new(), space_b, link_phase(b1, a1));
        {
            let wtxn = db.write_txn().unwrap();
            apply_link(&wtxn, &link_phase(b1, a1), &write_b).unwrap();
            wtxn.commit().unwrap();
        }
        assert!(
            !edge_present(&db, b1, a1),
            "LINK with one foreign endpoint must not create an edge"
        );
    }
}
