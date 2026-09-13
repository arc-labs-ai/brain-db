//! Statement embed-queue helpers.
//!
//! Read-side scan and write-side removal for the
//! `STATEMENT_EMBED_QUEUE_TABLE` table. The per-shard
//! `StatementEmbedWorker` calls these to find which Statement rows
//! still need a vector in the Statement HNSW.
//!
//! Population happens in `statement::crud::insert_new_statement`
//! (covers both `statement_create` and `statement_supersede`) and
//! removal in `statement::tombstone::statement_tombstone`. The worker
//! removes a row only after a successful HNSW write so a crash between
//! embed and queue-delete just costs an idempotent re-embed on restart.

use brain_core::StatementId;
use redb::{ReadTransaction, ReadableTable, ReadableTableMetadata, WriteTransaction};

use crate::tables::statement::{STATEMENTS_TABLE, STATEMENT_EMBED_QUEUE_TABLE};

use super::StatementOpError;

/// Read up to `limit` pending statement ids from the embed queue.
/// Returns `(statement_id, enqueued_at_unix_nanos)` pairs in redb's
/// natural byte order — not strictly time-ordered, but the queue is a
/// best-effort surface, not a priority queue.
///
/// Returns an empty `Vec` if the table doesn't exist yet (a redb table
/// is created on first write; a freshly-opened shard with no statements
/// hasn't seen one). Mirrors the same defensive behaviour
/// [`crate::statement::list::statement_list`] takes against unknown
/// tables.
pub fn statement_embed_queue_peek(
    rtxn: &ReadTransaction,
    limit: usize,
) -> Result<Vec<(StatementId, u64)>, StatementOpError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let t = rtxn.open_table(STATEMENT_EMBED_QUEUE_TABLE)?;
    let mut out = Vec::with_capacity(limit.min(1024));
    for entry in t.iter()? {
        let (k, v) = entry?;
        out.push((StatementId::from(k.value()), v.value()));
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Total queued statement count. Used by metrics + the worker's
/// "is there work?" check.
pub fn statement_embed_queue_len(rtxn: &ReadTransaction) -> Result<u64, StatementOpError> {
    let t = rtxn.open_table(STATEMENT_EMBED_QUEUE_TABLE)?;
    Ok(t.len()?)
}

/// Remove the queue row for `id`. No-op if the row is absent (already
/// drained, or the statement was tombstoned after the worker peeked).
pub fn statement_embed_queue_remove(
    wtxn: &WriteTransaction,
    id: StatementId,
) -> Result<(), StatementOpError> {
    let mut t = wtxn.open_table(STATEMENT_EMBED_QUEUE_TABLE)?;
    t.remove(&id.to_bytes())?;
    Ok(())
}

/// Bulk-remove convenience used by the worker after a successful
/// batch embed. Returns the number of rows actually removed.
pub fn statement_embed_queue_remove_many(
    wtxn: &WriteTransaction,
    ids: &[StatementId],
) -> Result<usize, StatementOpError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut t = wtxn.open_table(STATEMENT_EMBED_QUEUE_TABLE)?;
    let mut removed = 0usize;
    for id in ids {
        if t.remove(&id.to_bytes())?.is_some() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Re-enqueue every live (non-tombstoned) statement for embedding.
///
/// The Statement HNSW is in-RAM only and not persisted — on restart it
/// is empty, and the embed queue (which survives in redb) holds only the
/// statements that hadn't been embedded yet at crash time. That leaves
/// already-embedded statements absent from the rebuilt HNSW. This helper
/// seeds the queue with *all* live statements so the `StatementEmbedWorker`
/// repopulates the index after a restart.
///
/// Idempotent: re-seeding an id already in the queue upserts the same row,
/// and the worker skips any statement already present in the HNSW. The
/// queue value is the statement's `extracted_at_unix_nanos`, matching what
/// `insert_new_statement` writes. Returns the number of statements seeded.
pub fn statement_embed_queue_seed_all_live(
    wtxn: &WriteTransaction,
) -> Result<u64, StatementOpError> {
    // Collect live (id, extracted_at) first so the STATEMENTS_TABLE read
    // borrow is released before the queue table is opened for writes.
    let seeds: Vec<([u8; 16], u64)> = {
        let s = wtxn.open_table(STATEMENTS_TABLE)?;
        let mut v = Vec::new();
        for entry in s.iter()? {
            let (k, val) = entry?;
            let m = val.value();
            if m.is_tombstoned() {
                continue;
            }
            v.push((k.value(), m.extracted_at_unix_nanos));
        }
        v
    };
    let mut q = wtxn.open_table(STATEMENT_EMBED_QUEUE_TABLE)?;
    for (id, ts) in &seeds {
        q.insert(id, ts)?;
    }
    Ok(seeds.len() as u64)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn peek_empty_table_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let rtxn = db.begin_read().unwrap();
        let pending = statement_embed_queue_peek(&rtxn, 16).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn len_empty_table_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let rtxn = db.begin_read().unwrap();
        assert_eq!(statement_embed_queue_len(&rtxn).unwrap(), 0);
    }

    #[test]
    fn insert_peek_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let ids: Vec<StatementId> = (0..5).map(|_| StatementId::new()).collect();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(STATEMENT_EMBED_QUEUE_TABLE).unwrap();
            for id in &ids {
                t.insert(&id.to_bytes(), &42u64).unwrap();
            }
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert_eq!(statement_embed_queue_len(&rtxn).unwrap(), 5);
        let pending = statement_embed_queue_peek(&rtxn, 3).unwrap();
        assert_eq!(pending.len(), 3, "limit honoured");
        for (_, ts) in &pending {
            assert_eq!(*ts, 42, "enqueue timestamp round-trips");
        }
        drop(rtxn);

        let wtxn = db.begin_write().unwrap();
        let n = statement_embed_queue_remove_many(&wtxn, &ids[..3]).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(n, 3);

        let rtxn = db.begin_read().unwrap();
        assert_eq!(statement_embed_queue_len(&rtxn).unwrap(), 2);
    }

    #[test]
    fn remove_absent_row_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = StatementId::new();
        let wtxn = db.begin_write().unwrap();
        statement_embed_queue_remove(&wtxn, id).unwrap();
        wtxn.commit().unwrap();
    }
}
