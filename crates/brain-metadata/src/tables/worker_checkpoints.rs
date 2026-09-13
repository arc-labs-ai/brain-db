//! Per-(worker, item) checkpoint table for state-carrying workers.
//!
//! ## Layout
//!
//! Key: `(worker_id: &str, item_key: &[u8])`.
//!
//! - `worker_id` is a stable string constant per worker:
//!   `"backfill"`, `"forget_cascade"`, `"schema_migration"`.
//! - `item_key` is the worker-specific item identifier (e.g.
//!   `memory_id.to_be_bytes() ‖ extractor_id.to_le_bytes()` for
//!   backfill).
//!
//! The composite key lets a single redb table host all three
//! state-carrying workers' checkpoints without name collisions.
//!
//! Value: [`WorkerCheckpointRow`] — status enum + retry / timing
//! fields + last error.
//!
//! ## Status transitions
//!
//!
//! ```text
//! Pending  ──started──> Started
//! Started  ──ok─────>  Completed
//! Started  ──err────>  Failed (attempts++; retry if < max)
//! Failed   ──retry──>  Started
//! Completed            (terminal)
//! ```

use redb::{ReadableTable, TableDefinition};

use crate::impl_redb_rkyv_value;

/// Composite-key table.
pub const WORKER_CHECKPOINTS_TABLE: TableDefinition<
    'static,
    (&'static str, &'static [u8]),
    WorkerCheckpointRow,
> = TableDefinition::new("worker_checkpoints");

// ---------------------------------------------------------------------------
// Status byte mapping. Wire-stable.
// ---------------------------------------------------------------------------

pub mod status {
    pub const PENDING: u8 = 0;
    pub const STARTED: u8 = 1;
    pub const COMPLETED: u8 = 2;
    pub const FAILED: u8 = 3;
}

/// Per-item checkpoint row.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct WorkerCheckpointRow {
    /// One of [`status::PENDING`] / `STARTED` / `COMPLETED` / `FAILED`.
    pub status: u8,
    /// Number of times this item has transitioned `Started → Failed`.
    /// `Started → Completed` does not increment.
    pub attempts: u32,
    /// Timestamp of the most-recent transition into `Started`.
    pub started_at_unix_nanos: u64,
    /// Timestamp of the transition into `Completed`. Zero for non-
    /// completed rows.
    pub completed_at_unix_nanos: u64,
    /// Human-readable error message from the most recent failure.
    /// `None` for non-failed rows.
    pub last_error: Option<String>,
}

impl WorkerCheckpointRow {
    /// Construct a fresh `Pending` row.
    #[must_use]
    pub fn pending() -> Self {
        Self {
            status: status::PENDING,
            attempts: 0,
            started_at_unix_nanos: 0,
            completed_at_unix_nanos: 0,
            last_error: None,
        }
    }

    /// `true` iff this row's status is `Completed` (terminal success).
    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.status == status::COMPLETED
    }

    /// `true` iff this row's status is `Failed`.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.status == status::FAILED
    }
}

impl_redb_rkyv_value!(WorkerCheckpointRow, "brain_metadata::WorkerCheckpointRow");

// ---------------------------------------------------------------------------
// Pure ops over a transaction.
// ---------------------------------------------------------------------------

/// Read the checkpoint for `(worker_id, item_key)`. Returns `None` if
/// no row exists.
pub fn get(
    rtxn: &redb::ReadTransaction,
    worker_id: &'static str,
    item_key: &[u8],
) -> Result<Option<WorkerCheckpointRow>, redb::Error> {
    let table = rtxn.open_table(WORKER_CHECKPOINTS_TABLE)?;
    Ok(table.get(&(worker_id, item_key))?.map(|g| g.value()))
}

/// Transition `(worker_id, item_key)` to `Started`. Idempotent: a
/// `Started` row stays `Started` with the new timestamp; a `Failed`
/// row keeps its `attempts` counter and transitions back.
pub fn mark_started(
    wtxn: &redb::WriteTransaction,
    worker_id: &'static str,
    item_key: &[u8],
    now_unix_nanos: u64,
) -> Result<(), redb::Error> {
    let mut table = wtxn.open_table(WORKER_CHECKPOINTS_TABLE)?;
    let existing = table.get(&(worker_id, item_key))?.map(|g| g.value());
    let row = match existing {
        Some(mut r) => {
            r.status = status::STARTED;
            r.started_at_unix_nanos = now_unix_nanos;
            r
        }
        None => WorkerCheckpointRow {
            status: status::STARTED,
            attempts: 0,
            started_at_unix_nanos: now_unix_nanos,
            completed_at_unix_nanos: 0,
            last_error: None,
        },
    };
    table.insert(&(worker_id, item_key), &row)?;
    Ok(())
}

/// Transition to `Completed`. Clears `last_error` on success.
pub fn mark_completed(
    wtxn: &redb::WriteTransaction,
    worker_id: &'static str,
    item_key: &[u8],
    now_unix_nanos: u64,
) -> Result<(), redb::Error> {
    let mut table = wtxn.open_table(WORKER_CHECKPOINTS_TABLE)?;
    let existing = table.get(&(worker_id, item_key))?.map(|g| g.value());
    let row = match existing {
        Some(mut r) => {
            r.status = status::COMPLETED;
            r.completed_at_unix_nanos = now_unix_nanos;
            r.last_error = None;
            r
        }
        None => WorkerCheckpointRow {
            status: status::COMPLETED,
            attempts: 0,
            started_at_unix_nanos: now_unix_nanos,
            completed_at_unix_nanos: now_unix_nanos,
            last_error: None,
        },
    };
    table.insert(&(worker_id, item_key), &row)?;
    Ok(())
}

/// Transition to `Failed`. Increments the `attempts` counter
/// monotonically.
pub fn mark_failed(
    wtxn: &redb::WriteTransaction,
    worker_id: &'static str,
    item_key: &[u8],
    error: impl Into<String>,
    now_unix_nanos: u64,
) -> Result<(), redb::Error> {
    let mut table = wtxn.open_table(WORKER_CHECKPOINTS_TABLE)?;
    let existing = table.get(&(worker_id, item_key))?.map(|g| g.value());
    let row = match existing {
        Some(mut r) => {
            r.status = status::FAILED;
            r.attempts = r.attempts.saturating_add(1);
            r.started_at_unix_nanos = now_unix_nanos;
            r.last_error = Some(error.into());
            r
        }
        None => WorkerCheckpointRow {
            status: status::FAILED,
            attempts: 1,
            started_at_unix_nanos: now_unix_nanos,
            completed_at_unix_nanos: 0,
            last_error: Some(error.into()),
        },
    };
    table.insert(&(worker_id, item_key), &row)?;
    Ok(())
}

/// Scan up to `limit` rows for `worker_id` whose status is `Pending`,
/// `Started` (stale), or `Failed` (retryable).
///
/// Useful for the worker's restart path: drain the table on shard
/// startup and re-attach to the first non-`Completed` item.
pub fn list_non_terminal(
    rtxn: &redb::ReadTransaction,
    worker_id: &'static str,
    limit: usize,
) -> Result<Vec<(Vec<u8>, WorkerCheckpointRow)>, redb::Error> {
    let table = rtxn.open_table(WORKER_CHECKPOINTS_TABLE)?;
    // redb's range over `(&str, &[u8])` keys: scan all rows for the
    // worker_id prefix by iterating the full table and filtering.
    // Optimisation (range-by-prefix) deferred.
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (k, v) = entry?;
        let (w, item_key) = k.value();
        if w != worker_id {
            continue;
        }
        let row = v.value();
        if row.is_completed() {
            continue;
        }
        out.push((item_key.to_vec(), row));
        if out.len() == limit {
            break;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::ReadableDatabase;
    use tempfile::TempDir;

    fn fresh_db() -> (TempDir, redb::Database) {
        let dir = TempDir::new().expect("tempdir");
        let db = redb::Database::create(dir.path().join("test.redb")).expect("create");
        let wtxn = db.begin_write().expect("begin_write");
        crate::tables::materialize_all_tables(&wtxn).expect("materialize");
        wtxn.commit().expect("commit");
        (dir, db)
    }

    #[test]
    fn get_returns_none_when_absent() {
        let (_dir, db) = fresh_db();
        let rtxn = db.begin_read().unwrap();
        let got = get(&rtxn, "backfill", b"absent").unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn mark_started_writes_row() {
        let (_dir, db) = fresh_db();
        {
            let wtxn = db.begin_write().unwrap();
            mark_started(&wtxn, "backfill", b"key1", 1_000).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let row = get(&rtxn, "backfill", b"key1").unwrap().expect("row");
        assert_eq!(row.status, status::STARTED);
        assert_eq!(row.attempts, 0);
        assert_eq!(row.started_at_unix_nanos, 1_000);
    }

    #[test]
    fn mark_completed_clears_last_error() {
        let (_dir, db) = fresh_db();
        {
            let wtxn = db.begin_write().unwrap();
            mark_failed(&wtxn, "backfill", b"key", "boom", 1_000).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let wtxn = db.begin_write().unwrap();
            mark_completed(&wtxn, "backfill", b"key", 2_000).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let row = get(&rtxn, "backfill", b"key").unwrap().expect("row");
        assert_eq!(row.status, status::COMPLETED);
        assert_eq!(row.last_error, None);
        assert_eq!(row.completed_at_unix_nanos, 2_000);
    }

    #[test]
    fn mark_failed_increments_attempts_monotonically() {
        let (_dir, db) = fresh_db();
        for now in [1_000, 2_000, 3_000] {
            let wtxn = db.begin_write().unwrap();
            mark_failed(&wtxn, "backfill", b"key", "boom", now).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let row = get(&rtxn, "backfill", b"key").unwrap().expect("row");
        assert_eq!(row.attempts, 3);
        assert_eq!(row.status, status::FAILED);
    }

    #[test]
    fn list_non_terminal_excludes_completed_rows() {
        let (_dir, db) = fresh_db();
        {
            let wtxn = db.begin_write().unwrap();
            mark_completed(&wtxn, "backfill", b"done", 1_000).unwrap();
            mark_started(&wtxn, "backfill", b"todo", 2_000).unwrap();
            mark_failed(&wtxn, "backfill", b"oops", "x", 3_000).unwrap();
            // Another worker id — must not appear in the list.
            mark_started(&wtxn, "schema_migration", b"todo2", 4_000).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let items = list_non_terminal(&rtxn, "backfill", 10).unwrap();
        let keys: Vec<&[u8]> = items.iter().map(|(k, _)| k.as_slice()).collect();
        assert!(keys.contains(&&b"todo"[..]));
        assert!(keys.contains(&&b"oops"[..]));
        assert!(!keys.contains(&&b"done"[..]));
        assert_eq!(items.len(), 2);
    }
}
