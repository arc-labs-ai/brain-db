//! Apply Phase::ReclaimSlots — physical slot reclamation.
//!
//! Today this is a metadata-side no-op: the actual mmap zeroing happens
//! in the storage arena (out of redb's purview). What we do here is
//! evict any FINGERPRINTS rows that referenced the reclaimed slots so
//! a future encode with the same content can dedupe-or-not freely.

use brain_core::{SessionId, SpaceId};
use brain_metadata::tables::fingerprint::{fingerprint_key, FINGERPRINTS_TABLE};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use redb::{ReadableTable, WriteTransaction};

use super::ApplyError;
use crate::write::{Phase, PhaseAck, Write};

/// Apply [`Phase::ReclaimSlots`].
pub fn apply_reclaim_slots(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::ReclaimSlots { slots } = phase else {
        return Err(ApplyError::PhaseMisShape("expected ReclaimSlots"));
    };

    // For each slot, walk MEMORIES_TABLE looking for a row whose
    // slot_id matches. (Slot-id is not a unique index key today —
    // the table is keyed by MemoryId — so the scan is O(N). Porting
    // the existing slot_reclaim_worker logic and replacing this
    // with a slots → memory_id secondary index is a separate concern.)
    //
    // The MemoryId of the matching row is what we use to evict its
    // FINGERPRINTS entry (when content_hash was stamped).
    let mut count = 0usize;

    // Snapshot the (slot_id, content_hash) pairs to evict first so we
    // don't iterate the table while mutating it. Also snapshot the
    // matching rows' memory ids so we can reclaim their write-artifact
    // bundles (MEMORY_INSPECT) in the same pass — the bundle's vector +
    // derived graph must not outlive the reclaimed memory.
    let mut evictions: Vec<[u8; 56]> = Vec::new();
    let mut bundle_ids: Vec<[u8; 16]> = Vec::new();
    {
        let memories_t = wtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open MEMORIES: {e:?}")))?;
        for entry in memories_t
            .iter()
            .map_err(|e| ApplyError::Storage(format!("MEMORIES iter: {e:?}")))?
        {
            let (_, v) = entry.map_err(|e| ApplyError::Storage(format!("MEMORIES item: {e:?}")))?;
            let row = v.value();
            if slots.contains(&row.slot_id) {
                if let Some(ch) = row.content_hash {
                    // Reconstruct the EXACT fingerprint key from the row's own
                    // (space, context, hash) — the same triple the encode path
                    // keyed it under. A zeroed space/context placeholder would
                    // prefix-collide: two spaces (or sessions) sharing a content
                    // hash would evict each other's fingerprint.
                    evictions.push(fingerprint_key(
                        SpaceId::from(row.space_id_bytes),
                        SessionId(row.session_id),
                        &ch,
                    ));
                }
                bundle_ids.push(row.memory_id_bytes);
                count += 1;
            }
        }
    }

    if !evictions.is_empty() {
        let mut fp_t = wtxn
            .open_table(FINGERPRINTS_TABLE)
            .map_err(|e| ApplyError::Storage(format!("open FINGERPRINTS: {e:?}")))?;
        for key in evictions {
            let _ = fp_t
                .remove(&key)
                .map_err(|e| ApplyError::Storage(format!("FINGERPRINTS remove: {e:?}")))?;
        }
    }

    for mid in bundle_ids {
        crate::memory_artifact::delete_memory_artifact(wtxn, mid)
            .map_err(|e| ApplyError::Storage(format!("artifact remove (reclaim): {e}")))?;
    }

    Ok(PhaseAck::SlotsReclaimed { count })
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    use crate::write::{Phase, Write, WriteId};

    fn open_db() -> (TempDir, MetadataDb) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        (dir, db)
    }

    #[test]
    fn reclaim_empty_table_is_a_noop() {
        let (_dir, db) = open_db();
        let phase = Phase::ReclaimSlots {
            slots: vec![1, 2, 3],
        };
        let write = Write::single(
            WriteId::new(),
            brain_core::SpaceId::default(),
            phase.clone(),
        );
        let wtxn = db.write_txn().unwrap();
        let ack = apply_reclaim_slots(&wtxn, &phase, &write).unwrap();
        assert!(matches!(ack, PhaseAck::SlotsReclaimed { count: 0 }));
    }

    /// Two memories with the SAME content hash + context but DIFFERENT
    /// spaces. Reclaiming one space's slot must evict ONLY that space's
    /// fingerprint — the other space's identical-hash fingerprint must
    /// survive. Guards the zeroed-key bug, where reclaim keyed by
    /// content-hash alone could collide across spaces/sessions.
    #[test]
    fn reclaim_evicts_only_the_matching_space_fingerprint() {
        use brain_core::{MemoryId, MemoryKind};
        use brain_metadata::tables::fingerprint::{content_hash, FingerprintEntry};
        use brain_metadata::tables::memory::MemoryMetadata;
        use uuid::Uuid;

        let (_dir, db) = open_db();
        let space_a = SpaceId(Uuid::from_bytes([1u8; 16]));
        let space_b = SpaceId(Uuid::from_bytes([2u8; 16]));
        let ctx = SessionId(7);
        let hash = content_hash("identical text stored under two spaces");

        let mem_a = MemoryMetadata::new_active(
            MemoryId::pack(0, 100, 1),
            brain_core::NamespaceId::SYSTEM,
            space_a,
            ctx,
            100,
            1,
            MemoryKind::Episodic,
            [0u8; 16],
            1.0,
            8,
            1,
        )
        .with_content_hash(hash);
        let mem_b = MemoryMetadata::new_active(
            MemoryId::pack(0, 200, 1),
            brain_core::NamespaceId::SYSTEM,
            space_b,
            ctx,
            200,
            1,
            MemoryKind::Episodic,
            [0u8; 16],
            1.0,
            8,
            1,
        )
        .with_content_hash(hash);

        let key_a = fingerprint_key(space_a, ctx, &hash);
        let key_b = fingerprint_key(space_b, ctx, &hash);

        {
            let wtxn = db.write_txn().unwrap();
            {
                let mut mt = wtxn.open_table(MEMORIES_TABLE).unwrap();
                mt.insert(&mem_a.memory_id_bytes, &mem_a).unwrap();
                mt.insert(&mem_b.memory_id_bytes, &mem_b).unwrap();
                let mut fp = wtxn.open_table(FINGERPRINTS_TABLE).unwrap();
                fp.insert(&key_a, &FingerprintEntry::new(mem_a.memory_id(), 1))
                    .unwrap();
                fp.insert(&key_b, &FingerprintEntry::new(mem_b.memory_id(), 1))
                    .unwrap();
            }
            wtxn.commit().unwrap();
        }

        let phase = Phase::ReclaimSlots { slots: vec![100] };
        let write = Write::single(WriteId::new(), SpaceId::default(), phase.clone());
        {
            let wtxn = db.write_txn().unwrap();
            let ack = apply_reclaim_slots(&wtxn, &phase, &write).unwrap();
            assert!(matches!(ack, PhaseAck::SlotsReclaimed { count: 1 }));
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let fp = rtxn.open_table(FINGERPRINTS_TABLE).unwrap();
        assert!(
            fp.get(&key_a).unwrap().is_none(),
            "reclaimed space's fingerprint must be evicted"
        );
        assert!(
            fp.get(&key_b).unwrap().is_some(),
            "other space's same-hash fingerprint must survive (no zeroed-key collision)"
        );
    }
}
