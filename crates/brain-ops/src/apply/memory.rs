//! Apply functions for memory-shaped phases.
//!
//! Covers: UpsertMemory, UpdateSalience, UpdateKind, UpdateContext,
//! UpdateEmbedding, and Tombstone(Memory).

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
use brain_metadata::tables::memory::{
    agent_timeline_key, MemoryMetadata, MEMORIES_BY_AGENT_TIMELINE_TABLE, MEMORIES_TABLE,
};
use brain_metadata::tables::text::TEXTS_TABLE;
use redb::{ReadableTable, WriteTransaction};

use super::ApplyError;
use crate::write::{Phase, PhaseAck, TombstoneTarget, Write};

/// Apply [`Phase::UpsertMemory`]. Inserts the memory row + writes the
/// per-agent timeline index entry inside the same wtxn.
pub fn apply_upsert_memory(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpsertMemory {
        id,
        text,
        vector: _,
        kind,
        salience,
        context,
        created_at_unix_nanos,
        occurred_at_unix_nanos,
        arena_slot,
        embedding_model_fp,
        content_hash,
        deduplicate,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UpsertMemory"));
    };

    let mut row = MemoryMetadata::new_active(
        *id,
        write.agent_id,
        *context,
        *arena_slot,
        id.version(),
        *kind,
        *embedding_model_fp,
        salience.raw(),
        text.len() as u32,
        *created_at_unix_nanos,
    )
    .with_occurred_at(*occurred_at_unix_nanos);
    if *deduplicate {
        if let Some(ch) = content_hash {
            row = row.with_content_hash(*ch);
        }
    }

    // Memory row.
    {
        let mut memories_t = wtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open MEMORIES: {e:?}")))?;
        memories_t
            .insert(&id.to_be_bytes(), row)
            .map_err(|e| ApplyError::Storage(format!("MEMORIES insert: {e:?}")))?;
    }

    // Timeline index. The TemporalEdgeStrategy walks this index in
    // descending-time order to find each new memory's predecessor.
    {
        let mut timeline_t = wtxn
            .open_table(MEMORIES_BY_AGENT_TIMELINE_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open TIMELINE: {e:?}")))?;
        let key = agent_timeline_key(
            agent_id_bytes(write.agent_id),
            *created_at_unix_nanos,
            context.raw(),
            id.to_be_bytes(),
        );
        timeline_t
            .insert(key.as_slice(), ())
            .map_err(|e| ApplyError::Storage(format!("TIMELINE insert: {e:?}")))?;
    }

    // Couple the raw text to the memory row inside the same wtxn.
    // RECALL --include-text and the lexical retriever both read here;
    // dropping the write would silently strip text from every recall.
    {
        let mut texts_t = wtxn
            .open_table(TEXTS_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open TEXTS: {e:?}")))?;
        texts_t
            .insert(&id.to_be_bytes(), text.as_bytes())
            .map_err(|e| ApplyError::Storage(format!("TEXTS insert: {e:?}")))?;
    }

    // FINGERPRINTS_TABLE entry when the encode opted into content-
    // hash dedup. The row keys (agent_id, context_id, content_hash) →
    // this memory id, so a future ENCODE with matching text/agent/ctx
    // can dedupe-to-existing without minting a fresh row.
    if *deduplicate {
        if let Some(ch) = content_hash {
            use brain_metadata::tables::fingerprint::{
                fingerprint_key, FingerprintEntry, FINGERPRINTS_TABLE,
            };
            let key = fingerprint_key(write.agent_id, *context, ch);
            let entry = FingerprintEntry::new(*id, *created_at_unix_nanos);
            let mut fp_t = wtxn
                .open_table(FINGERPRINTS_TABLE)
                .map_err(|e| ApplyError::Storage(format!("open FINGERPRINTS: {e:?}")))?;
            fp_t.insert(&key, entry)
                .map_err(|e| ApplyError::Storage(format!("FINGERPRINTS insert: {e:?}")))?;
        }
    }

    Ok(PhaseAck::UpsertedMemory(*id))
}

/// Apply [`Phase::Tombstone`] when the target is a Memory.
pub fn apply_tombstone_memory(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::Tombstone {
        target,
        reason: _,
        at_unix_nanos,
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected Tombstone"));
    };
    let TombstoneTarget::Memory { id, mode } = target else {
        return Err(ApplyError::PhaseMisShape("expected Tombstone(Memory)"));
    };

    let mut row = {
        let memories_t = wtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open MEMORIES: {e:?}")))?;
        let row_guard = memories_t
            .get(&id.to_be_bytes())
            .map_err(|e| ApplyError::Storage(format!("MEMORIES get: {e:?}")))?;
        let Some(guard) = row_guard else {
            return Err(ApplyError::NotFound {
                what: "memory",
                detail: format!("{id:?}"),
            });
        };
        guard.value()
    };

    // Stamp tombstoned_at + clear the ACTIVE flag. The actual slot
    // reclamation happens later via Phase::ReclaimSlots once the
    // grace period passes.
    row.tombstoned_at_unix_nanos = Some(*at_unix_nanos);
    row.flags &= !brain_metadata::tables::memory::flags::ACTIVE;

    let created_at = row.created_at_unix_nanos;
    let agent_bytes = row.agent_id_bytes;
    let ctx_raw = row.context_id;
    let mid_bytes = row.memory_id_bytes;
    let dedup_hash = row.content_hash;

    // Persist the tombstone-stamped row.
    {
        let mut memories_t = wtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open MEMORIES: {e:?}")))?;
        memories_t
            .insert(&id.to_be_bytes(), row)
            .map_err(|e| ApplyError::Storage(format!("MEMORIES insert (tombstone): {e:?}")))?;
    }

    // Remove the timeline-index entry — a tombstoned memory must not
    // surface as a temporal predecessor for future encodes.
    {
        let mut timeline_t = wtxn
            .open_table(MEMORIES_BY_AGENT_TIMELINE_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open TIMELINE: {e:?}")))?;
        let key = agent_timeline_key(agent_bytes, created_at, ctx_raw, mid_bytes);
        let _ = timeline_t
            .remove(key.as_slice())
            .map_err(|e| ApplyError::Storage(format!("TIMELINE remove: {e:?}")))?;
    }

    // Evict the fingerprint row in the same wtxn as the tombstone —
    // a re-encode of the same text after FORGET must miss the dedup
    // index, otherwise the new write would silently fold into a
    // dead memory. Only present when the original encode opted in.
    if let Some(hash) = dedup_hash {
        use brain_metadata::tables::fingerprint::{fingerprint_key, FINGERPRINTS_TABLE};
        let mut fp_t = wtxn
            .open_table(FINGERPRINTS_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open FINGERPRINTS: {e:?}")))?;
        let key = fingerprint_key(
            brain_core::AgentId::from(agent_bytes),
            ContextId(ctx_raw),
            &hash,
        );
        let _ = fp_t
            .remove(&key)
            .map_err(|e| ApplyError::Storage(format!("FINGERPRINTS remove: {e:?}")))?;
    }

    // Hard FORGET is the privacy escape hatch: the spec promises the
    // plaintext is "no longer recoverable from the file" once the OS
    // flushes. The text lives in the metadata (redb) TEXTS_TABLE, so
    // drop that row in the same wtxn as the tombstone — a soft forget
    // keeps it (reclamation handles it after grace), a hard forget
    // purges it now. The arena-side vector is zeroed by the storage
    // layer (live-path arena bytes are written only on WAL recovery,
    // where the recovery replay re-zeroes a hard-forgotten slot).
    if matches!(mode, crate::write::phase::TombstoneMode::Hard) {
        let mut texts_t = wtxn
            .open_table(TEXTS_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open TEXTS: {e:?}")))?;
        let _ = texts_t
            .remove(&id.to_be_bytes())
            .map_err(|e| ApplyError::Storage(format!("TEXTS remove (hard forget): {e:?}")))?;
    }

    Ok(PhaseAck::Tombstoned {
        target: *target,
        tombstoned_at_unix_nanos: *at_unix_nanos,
    })
}

/// Apply [`Phase::UpdateSalience`].
pub fn apply_update_salience(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpdateSalience { id, new_salience } = phase else {
        return Err(ApplyError::PhaseMisShape("expected UpdateSalience"));
    };
    let mut row = load_memory(wtxn, *id)?;
    row.salience = new_salience.raw();
    write_memory(wtxn, *id, row)?;
    Ok(PhaseAck::SalienceUpdated)
}

/// Apply [`Phase::UpdateKind`].
pub fn apply_update_kind(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpdateKind { id, new_kind } = phase else {
        return Err(ApplyError::PhaseMisShape("expected UpdateKind"));
    };
    let mut row = load_memory(wtxn, *id)?;
    row.kind = memory_kind_to_u8(*new_kind);
    write_memory(wtxn, *id, row)?;
    Ok(PhaseAck::KindUpdated)
}

/// Apply [`Phase::UpdateContext`].
pub fn apply_update_context(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpdateContext { id, new_context } = phase else {
        return Err(ApplyError::PhaseMisShape("expected UpdateContext"));
    };
    let mut row = load_memory(wtxn, *id)?;
    let old_context = ContextId(row.context_id);
    let old_created = row.created_at_unix_nanos;
    row.context_id = new_context.raw();
    write_memory(wtxn, *id, row.clone())?;

    // Update the timeline index — the key includes context_id, so we
    // remove the old entry and insert the new one.
    {
        let mut timeline_t = wtxn
            .open_table(MEMORIES_BY_AGENT_TIMELINE_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open TIMELINE: {e:?}")))?;
        let old_key = agent_timeline_key(
            row.agent_id_bytes,
            old_created,
            old_context.raw(),
            id.to_be_bytes(),
        );
        let _ = timeline_t
            .remove(old_key.as_slice())
            .map_err(|e| ApplyError::Storage(format!("TIMELINE remove (context change): {e:?}")))?;
        let new_key = agent_timeline_key(
            row.agent_id_bytes,
            old_created,
            new_context.raw(),
            id.to_be_bytes(),
        );
        timeline_t
            .insert(new_key.as_slice(), ())
            .map_err(|e| ApplyError::Storage(format!("TIMELINE insert (context change): {e:?}")))?;
    }

    Ok(PhaseAck::ContextUpdated)
}

/// Apply [`Phase::UpdateEmbedding`]. The actual vector lives in the
/// arena (mmap-managed by the storage layer); here we only stamp the
/// model fingerprint on the row so downstream readers know the
/// embedding's provenance.
pub fn apply_update_embedding(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpdateEmbedding { id, new_vector: _ } = phase else {
        return Err(ApplyError::PhaseMisShape("expected UpdateEmbedding"));
    };
    // The metadata row doesn't carry the vector itself; only the model
    // fingerprint, which the caller stamps via a fresh UpsertMemory if
    // they need to change it. For embedding updates we clear the STALE
    // flag (the new vector is fresh by construction).
    let mut row = load_memory(wtxn, *id)?;
    row.flags &= !brain_metadata::tables::memory::flags::STALE;
    write_memory(wtxn, *id, row)?;
    Ok(PhaseAck::EmbeddingUpdated)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_memory(wtxn: &WriteTransaction, id: MemoryId) -> Result<MemoryMetadata, ApplyError> {
    let t = wtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("open MEMORIES: {e:?}")))?;
    let row_guard = t
        .get(&id.to_be_bytes())
        .map_err(|e| ApplyError::Storage(format!("MEMORIES get: {e:?}")))?;
    let Some(guard) = row_guard else {
        return Err(ApplyError::NotFound {
            what: "memory",
            detail: format!("{id:?}"),
        });
    };
    Ok(guard.value())
}

fn write_memory(
    wtxn: &WriteTransaction,
    id: MemoryId,
    row: MemoryMetadata,
) -> Result<(), ApplyError> {
    let mut t = wtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| ApplyError::Storage(format!("open MEMORIES: {e:?}")))?;
    t.insert(&id.to_be_bytes(), row)
        .map_err(|e| ApplyError::Storage(format!("MEMORIES insert: {e:?}")))?;
    Ok(())
}

fn agent_id_bytes(a: AgentId) -> [u8; 16] {
    a.into()
}

fn memory_kind_to_u8(k: MemoryKind) -> u8 {
    match k {
        MemoryKind::Episodic => 0,
        MemoryKind::Semantic => 1,
        MemoryKind::Consolidated => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_embed::VECTOR_DIM;
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    use crate::write::{Phase, Write, WriteId};

    fn open_db() -> (TempDir, MetadataDb) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        (dir, db)
    }

    fn fixture_phase(id: MemoryId) -> Phase {
        Phase::UpsertMemory {
            id,
            text: "hello world".into(),
            vector: Box::new([0.0_f32; VECTOR_DIM]),
            kind: MemoryKind::Episodic,
            salience: brain_core::Salience::default(),
            context: ContextId(7),
            created_at_unix_nanos: 1_700_000_000_000,
            occurred_at_unix_nanos: None,
            arena_slot: 42,
            embedding_model_fp: [0xAA; 16],
            content_hash: None,
            deduplicate: false,
        }
    }

    fn fresh_write_for(agent: AgentId) -> Write {
        Write {
            write_id: WriteId::new(),
            agent_id: agent,
            started_at_unix_nanos: 0,
            phases: Vec::new(),
            request_hash: None,
        }
    }

    #[test]
    fn upsert_memory_writes_row_and_timeline() {
        let (_dir, db) = open_db();
        let id = MemoryId::pack(0, 1, 0);
        let agent = AgentId::new();
        let phase = fixture_phase(id);
        let write = fresh_write_for(agent);

        {
            let wtxn = db.write_txn().unwrap();
            let ack = apply_upsert_memory(&wtxn, &phase, &write).unwrap();
            assert!(matches!(ack, PhaseAck::UpsertedMemory(m) if m == id));
            wtxn.commit().unwrap();
        }

        // Memory row exists with the expected fields.
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let row = t.get(&id.to_be_bytes()).unwrap().unwrap().value();
        assert_eq!(row.memory_id(), id);
        assert_eq!(row.agent_id(), agent);
        assert_eq!(row.context(), ContextId(7));
        assert_eq!(row.created_at_unix_nanos, 1_700_000_000_000);
        assert!(row.flags & brain_metadata::tables::memory::flags::ACTIVE != 0);

        // Timeline index has the entry.
        let timeline_t = rtxn.open_table(MEMORIES_BY_AGENT_TIMELINE_TABLE).unwrap();
        let key = agent_timeline_key(
            agent_id_bytes(agent),
            1_700_000_000_000,
            7,
            id.to_be_bytes(),
        );
        assert!(timeline_t.get(key.as_slice()).unwrap().is_some());
    }

    #[test]
    fn tombstone_memory_clears_active_flag_and_timeline() {
        let (_dir, db) = open_db();
        let id = MemoryId::pack(0, 1, 0);
        let agent = AgentId::new();
        let write = fresh_write_for(agent);

        // Set up: upsert first.
        {
            let wtxn = db.write_txn().unwrap();
            apply_upsert_memory(&wtxn, &fixture_phase(id), &write).unwrap();
            wtxn.commit().unwrap();
        }

        // Tombstone.
        let tombstone_phase = Phase::Tombstone {
            target: TombstoneTarget::Memory {
                id,
                mode: crate::write::phase::TombstoneMode::Soft,
            },
            reason: 1,
            at_unix_nanos: 1_700_000_001_000,
        };
        {
            let wtxn = db.write_txn().unwrap();
            let ack = apply_tombstone_memory(&wtxn, &tombstone_phase, &write).unwrap();
            assert!(matches!(ack, PhaseAck::Tombstoned { .. }));
            wtxn.commit().unwrap();
        }

        // Row still exists but inactive + tombstoned_at stamped.
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let row = t.get(&id.to_be_bytes()).unwrap().unwrap().value();
        assert_eq!(row.flags & brain_metadata::tables::memory::flags::ACTIVE, 0);
        assert_eq!(row.tombstoned_at_unix_nanos, Some(1_700_000_001_000));

        // Timeline entry gone.
        let timeline_t = rtxn.open_table(MEMORIES_BY_AGENT_TIMELINE_TABLE).unwrap();
        let key = agent_timeline_key(
            agent_id_bytes(agent),
            1_700_000_000_000,
            7,
            id.to_be_bytes(),
        );
        assert!(timeline_t.get(key.as_slice()).unwrap().is_none());
    }

    #[test]
    fn hard_forget_purges_text_row_soft_keeps_it() {
        let (_dir, db) = open_db();
        let agent = AgentId::new();
        let write = fresh_write_for(agent);
        let soft_id = MemoryId::pack(0, 1, 0);
        let hard_id = MemoryId::pack(0, 2, 0);

        // Upsert both — fixture text is "hello world".
        {
            let wtxn = db.write_txn().unwrap();
            apply_upsert_memory(&wtxn, &fixture_phase(soft_id), &write).unwrap();
            let mut hard_phase = fixture_phase(hard_id);
            if let Phase::UpsertMemory { id, .. } = &mut hard_phase {
                *id = hard_id;
            }
            apply_upsert_memory(&wtxn, &hard_phase, &write).unwrap();
            wtxn.commit().unwrap();
        }

        let tombstone = |id, mode| Phase::Tombstone {
            target: TombstoneTarget::Memory { id, mode },
            reason: 1,
            at_unix_nanos: 1_700_000_001_000,
        };
        {
            let wtxn = db.write_txn().unwrap();
            apply_tombstone_memory(
                &wtxn,
                &tombstone(soft_id, crate::write::phase::TombstoneMode::Soft),
                &write,
            )
            .unwrap();
            apply_tombstone_memory(
                &wtxn,
                &tombstone(hard_id, crate::write::phase::TombstoneMode::Hard),
                &write,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let texts = rtxn.open_table(TEXTS_TABLE).unwrap();
        assert!(
            texts.get(&soft_id.to_be_bytes()).unwrap().is_some(),
            "soft forget must retain the plaintext text row"
        );
        assert!(
            texts.get(&hard_id.to_be_bytes()).unwrap().is_none(),
            "hard forget must purge the plaintext text row"
        );
    }

    #[test]
    fn update_salience_persists() {
        let (_dir, db) = open_db();
        let id = MemoryId::pack(0, 1, 0);
        let agent = AgentId::new();
        let write = fresh_write_for(agent);
        {
            let wtxn = db.write_txn().unwrap();
            apply_upsert_memory(&wtxn, &fixture_phase(id), &write).unwrap();
            wtxn.commit().unwrap();
        }
        let phase = Phase::UpdateSalience {
            id,
            new_salience: brain_core::Salience::new(0.75),
        };
        {
            let wtxn = db.write_txn().unwrap();
            apply_update_salience(&wtxn, &phase, &write).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let row = t.get(&id.to_be_bytes()).unwrap().unwrap().value();
        assert!((row.salience - 0.75).abs() < 1e-6);
    }

    #[test]
    fn update_kind_persists() {
        let (_dir, db) = open_db();
        let id = MemoryId::pack(0, 1, 0);
        let agent = AgentId::new();
        let write = fresh_write_for(agent);
        {
            let wtxn = db.write_txn().unwrap();
            apply_upsert_memory(&wtxn, &fixture_phase(id), &write).unwrap();
            wtxn.commit().unwrap();
        }
        let phase = Phase::UpdateKind {
            id,
            new_kind: MemoryKind::Semantic,
        };
        {
            let wtxn = db.write_txn().unwrap();
            apply_update_kind(&wtxn, &phase, &write).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let row = t.get(&id.to_be_bytes()).unwrap().unwrap().value();
        assert_eq!(row.kind, 1); // Semantic
    }

    #[test]
    fn tombstone_missing_memory_returns_not_found() {
        let (_dir, db) = open_db();
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Memory {
                id: MemoryId::pack(0, 999, 0),
                mode: crate::write::phase::TombstoneMode::Soft,
            },
            reason: 1,
            at_unix_nanos: 0,
        };
        let wtxn = db.write_txn().unwrap();
        let err = apply_tombstone_memory(&wtxn, &phase, &fresh_write_for(AgentId::default()))
            .unwrap_err();
        assert!(matches!(err, ApplyError::NotFound { what: "memory", .. }));
    }
}
