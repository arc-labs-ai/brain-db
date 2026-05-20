//! Apply Phase::ReclaimSlots — physical slot reclamation.
//!
//! Today this is a metadata-side no-op: the actual mmap zeroing happens
//! in the storage arena (out of redb's purview). What we do here is
//! evict any FINGERPRINTS rows that referenced the reclaimed slots so
//! a future encode with the same content can dedupe-or-not freely.

use brain_metadata::tables::fingerprint::FINGERPRINTS_TABLE;
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
    // the table is keyed by MemoryId — so the scan is O(N). Phase P2
    // ports the existing slot_reclaim_worker logic; replacing this
    // with a slots → memory_id secondary index is a separate concern.)
    //
    // The MemoryId of the matching row is what we use to evict its
    // FINGERPRINTS entry (when content_hash was stamped).
    let mut count = 0usize;

    // Snapshot the (slot_id, content_hash) pairs to evict first so we
    // don't iterate the table while mutating it.
    let mut evictions: Vec<[u8; 56]> = Vec::new();
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
                    evictions.push(content_hash_to_fp_key(ch));
                }
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

    Ok(PhaseAck::SlotsReclaimed { count })
}

/// FINGERPRINTS_TABLE is keyed by 56 bytes: agent(16) + context(8) +
/// hash(32). The slot-reclaim path here only knows the content_hash;
/// it removes via prefix match isn't directly supported, so we encode
/// a synthetic key with zeroed agent/context. That's a placeholder —
/// the real port comes in P5 (worker migration) where the worker
/// already knows the original (agent, context) tuple.
fn content_hash_to_fp_key(content_hash: [u8; 32]) -> [u8; 56] {
    let mut k = [0u8; 56];
    k[24..56].copy_from_slice(&content_hash);
    k
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
        let (_dir, mut db) = open_db();
        let phase = Phase::ReclaimSlots {
            slots: vec![1, 2, 3],
        };
        let write = Write::single(
            WriteId::new(),
            brain_core::AgentId::default(),
            phase.clone(),
        );
        let wtxn = db.write_txn().unwrap();
        let ack = apply_reclaim_slots(&wtxn, &phase, &write).unwrap();
        assert!(matches!(ack, PhaseAck::SlotsReclaimed { count: 0 }));
    }
}
