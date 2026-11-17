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

/// Default row budget for [`extraction_queue_backlog`]'s oldest-row
/// scan. Chosen so a pathologically deep queue cannot stall the caller:
/// the scan is bounded work, the depth it reports is not.
pub const EXTRACTION_BACKLOG_SCAN_CAP: usize = 65_536;

/// How much derivation work a shard owes, and how long the oldest debt
/// has been waiting.
///
/// Derivation drains far slower than writes arrive, so a shard can fall
/// arbitrarily far behind while every request still succeeds. Depth is
/// the leading indicator of that; age is what turns "behind" into "how
/// badly".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExtractionBacklog {
    /// Exact number of memories queued for extraction.
    pub depth: u64,
    /// Enqueue time of the oldest row seen by the scan, unix nanos.
    /// `None` when the queue is empty.
    pub oldest_enqueued_at_unix_nanos: Option<u64>,
}

impl ExtractionBacklog {
    /// Wait time of the oldest queued memory, in whole seconds. Zero
    /// for an empty queue, and zero rather than a wrapped huge number
    /// if the stored timestamp is ahead of `now_unix_nanos` (clock
    /// skew across a restart must not fabricate an alert).
    #[must_use]
    pub fn oldest_age_secs(&self, now_unix_nanos: u64) -> u64 {
        match self.oldest_enqueued_at_unix_nanos {
            Some(enqueued_at) => now_unix_nanos.saturating_sub(enqueued_at) / 1_000_000_000,
            None => 0,
        }
    }
}

/// Measure the extraction backlog: queue depth plus the enqueue time of
/// the oldest pending row.
///
/// Depth is exact and free — redb tracks table length. The oldest row
/// costs a scan, because the table is keyed by `MemoryId`, not by time,
/// so `scan_cap` bounds how many rows that scan reads. Pass
/// [`EXTRACTION_BACKLOG_SCAN_CAP`] unless you have a reason not to.
///
/// What the cap costs: key order is `(shard, session, sequence)`, so
/// rows from one session are already in enqueue order and the true
/// oldest is the first row scanned. Only a queue that is both deeper
/// than the cap *and* spread across many sessions can hide an older row
/// past the window; the age then under-reports, while depth — the
/// number an operator alerts on — stays exact.
///
/// Never call this from the write path: it is a diagnostic read, sized
/// for scrape cadence.
pub fn extraction_queue_backlog(
    rtxn: &ReadTransaction,
    scan_cap: usize,
) -> Result<ExtractionBacklog, ExtractionQueueError> {
    let t = rtxn.open_table(EXTRACTION_QUEUE_TABLE)?;
    let depth = t.len()?;
    if depth == 0 || scan_cap == 0 {
        return Ok(ExtractionBacklog {
            depth,
            oldest_enqueued_at_unix_nanos: None,
        });
    }
    let mut oldest: Option<u64> = None;
    for (scanned, entry) in t.iter()?.enumerate() {
        if scanned >= scan_cap {
            break;
        }
        let (_k, v) = entry?;
        let enqueued_at = v.value();
        if oldest.is_none_or(|current| enqueued_at < current) {
            oldest = Some(enqueued_at);
        }
    }
    Ok(ExtractionBacklog {
        depth,
        oldest_enqueued_at_unix_nanos: oldest,
    })
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use brain_core::SessionId;
    use redb::ReadableDatabase;

    fn mem(byte: u16) -> MemoryId {
        MemoryId::pack(byte, SessionId::DEFAULT.into(), 0)
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
    fn backlog_is_empty_on_an_empty_queue() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let rtxn = db.begin_read().unwrap();
        let backlog = extraction_queue_backlog(&rtxn, EXTRACTION_BACKLOG_SCAN_CAP).unwrap();
        assert_eq!(backlog, ExtractionBacklog::default());
        assert_eq!(backlog.oldest_age_secs(1_000), 0);
    }

    #[test]
    fn backlog_reports_depth_and_the_oldest_enqueue() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        // Enqueue times deliberately out of key order: the oldest row is
        // the last key, so a scan that trusted key order would miss it.
        extraction_queue_enqueue(&wtxn, mem(1), 900).unwrap();
        extraction_queue_enqueue(&wtxn, mem(2), 700).unwrap();
        extraction_queue_enqueue(&wtxn, mem(3), 500).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let backlog = extraction_queue_backlog(&rtxn, EXTRACTION_BACKLOG_SCAN_CAP).unwrap();
        assert_eq!(backlog.depth, 3);
        assert_eq!(backlog.oldest_enqueued_at_unix_nanos, Some(500));
        assert_eq!(backlog.oldest_age_secs(3_500_000_000), 3);
    }

    #[test]
    fn backlog_depth_stays_exact_when_the_scan_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        for i in 0..8u16 {
            extraction_queue_enqueue(&wtxn, mem(i), 1_000 - u64::from(i)).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let capped = extraction_queue_backlog(&rtxn, 2).unwrap();
        // Depth comes from table length, so the cap never truncates it.
        assert_eq!(capped.depth, 8);
        // Age is the oldest within the window only — here the window
        // holds the two newest rows, so it under-reports.
        assert_eq!(capped.oldest_enqueued_at_unix_nanos, Some(999));

        let full = extraction_queue_backlog(&rtxn, EXTRACTION_BACKLOG_SCAN_CAP).unwrap();
        assert_eq!(full.oldest_enqueued_at_unix_nanos, Some(993));

        // A zero cap still answers the depth question.
        let no_scan = extraction_queue_backlog(&rtxn, 0).unwrap();
        assert_eq!(no_scan.depth, 8);
        assert_eq!(no_scan.oldest_enqueued_at_unix_nanos, None);
    }

    #[test]
    fn backlog_age_does_not_wrap_when_the_clock_runs_backwards() {
        let backlog = ExtractionBacklog {
            depth: 1,
            oldest_enqueued_at_unix_nanos: Some(5_000_000_000),
        };
        assert_eq!(backlog.oldest_age_secs(1_000_000_000), 0);
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
