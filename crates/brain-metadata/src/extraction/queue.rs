//! Extraction-queue ops.
//!
//! Enqueue (inside the ENCODE write txn), drain (worker read side), and
//! remove (worker, after a memory's extraction commits) over the
//! [`EXTRACTION_QUEUE_TABLE`]. The table is keyed by `MemoryId`, not by
//! time, so the drain order is redb's natural byte order — a best-effort
//! surface, not a strict priority queue.

use brain_core::MemoryId;
use redb::{ReadTransaction, ReadableTable, ReadableTableMetadata, WriteTransaction};

use crate::tables::extraction_queue::EXTRACTION_QUEUE_TABLE;

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum ExtractionQueueError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
}

// ---------------------------------------------------------------------------
// Write path.
// ---------------------------------------------------------------------------

/// Enqueue `memory_id` for asynchronous extraction. Composed inside the
/// ENCODE write txn so the trigger commits atomically with the memory.
/// Idempotent: re-enqueuing an id already present upserts the same row.
pub fn extraction_queue_enqueue(
    wtxn: &WriteTransaction,
    memory_id: MemoryId,
    now_unix_nanos: u64,
) -> Result<(), ExtractionQueueError> {
    let mut t = wtxn.open_table(EXTRACTION_QUEUE_TABLE)?;
    t.insert(&memory_id.to_be_bytes(), &now_unix_nanos)?;
    Ok(())
}

/// Remove the queue row for `memory_id`. Called by the worker after that
/// memory's extraction has committed. No-op if the row is absent.
pub fn extraction_queue_remove(
    wtxn: &WriteTransaction,
    memory_id: MemoryId,
) -> Result<(), ExtractionQueueError> {
    let mut t = wtxn.open_table(EXTRACTION_QUEUE_TABLE)?;
    t.remove(&memory_id.to_be_bytes())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Read path.
// ---------------------------------------------------------------------------

/// Read up to `limit` pending memory ids from the extraction queue.
/// Returns `(memory_id, enqueued_at_unix_nanos)` pairs in redb's natural
/// byte order — the table is keyed by `MemoryId`, not time, so this is
/// not strictly oldest-first; the queue is a best-effort work surface.
pub fn extraction_queue_drain(
    rtxn: &ReadTransaction,
    limit: usize,
) -> Result<Vec<(MemoryId, u64)>, ExtractionQueueError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let t = rtxn.open_table(EXTRACTION_QUEUE_TABLE)?;
    let mut out = Vec::with_capacity(limit.min(1024));
    for entry in t.iter()? {
        let (k, v) = entry?;
        out.push((MemoryId::from_be_bytes(k.value()), v.value()));
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Total queued memory count. Used by metrics + the worker's "is there
/// work?" check.
pub fn extraction_queue_len(rtxn: &ReadTransaction) -> Result<u64, ExtractionQueueError> {
    let t = rtxn.open_table(EXTRACTION_QUEUE_TABLE)?;
    Ok(t.len()?)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use brain_core::ContextId;
    use redb::ReadableDatabase;

    fn mem(byte: u16) -> MemoryId {
        MemoryId::pack(byte, ContextId::DEFAULT.into(), 0)
    }

    #[test]
    fn enqueue_drain_remove_len_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let ids = [mem(1), mem(2), mem(3)];

        let wtxn = db.begin_write().unwrap();
        for (i, id) in ids.iter().enumerate() {
            extraction_queue_enqueue(&wtxn, *id, 100 + i as u64).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert_eq!(extraction_queue_len(&rtxn).unwrap(), 3);
        let drained = extraction_queue_drain(&rtxn, 16).unwrap();
        assert_eq!(drained.len(), 3);
        drop(rtxn);

        let wtxn = db.begin_write().unwrap();
        extraction_queue_remove(&wtxn, ids[0]).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert_eq!(extraction_queue_drain(&rtxn, 16).unwrap().len(), 2);
        assert_eq!(extraction_queue_len(&rtxn).unwrap(), 2);
    }

    #[test]
    fn drain_honours_limit() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        for i in 0..5u16 {
            extraction_queue_enqueue(&wtxn, mem(i), 42).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert_eq!(extraction_queue_drain(&rtxn, 3).unwrap().len(), 3);
        assert_eq!(extraction_queue_drain(&rtxn, 0).unwrap().len(), 0);
    }

    #[test]
    fn remove_absent_row_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        extraction_queue_remove(&wtxn, mem(7)).unwrap();
        wtxn.commit().unwrap();
    }
}
