//! Extractor audit-log query API.
//!
//! Single-writer-per-shard discipline applies — `audit_write` takes
//! the caller's `wtxn`. Reads via
//! `audit_by_*` / `audit_recent_*` are MVCC-safe (`&ReadTransaction`).

use brain_core::{AuditId, MemoryId};
use redb::{ReadTransaction, WriteTransaction};

use crate::tables::audit::{
    extraction_status, ExtractionAudit, EXTRACTOR_AUDIT_BY_EXTRACTOR_TABLE,
    EXTRACTOR_AUDIT_BY_MEMORY_TABLE, EXTRACTOR_AUDIT_BY_TIME_TABLE, EXTRACTOR_AUDIT_TABLE,
    OUTPUTS_CAP,
};

#[derive(thiserror::Error, Debug)]
pub enum AuditOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("audit_write: outputs exceeds cap of {cap} (got {got})")]
    OutputsOverCap { cap: usize, got: usize },
}

// ---------------------------------------------------------------------------
// Writes.
// ---------------------------------------------------------------------------

/// Write one audit row + populate the three secondary indexes.
/// Caller commits the same `wtxn` that wrote the produced outputs
/// (entities / statements / relations); audit and outputs commit
/// atomically.
///
/// Rejects `audit.outputs.len() > OUTPUTS_CAP` with
/// `OutputsOverCap` before any write touches the txn.
pub fn audit_write(wtxn: &WriteTransaction, audit: &ExtractionAudit) -> Result<(), AuditOpError> {
    if audit.outputs.len() > OUTPUTS_CAP {
        return Err(AuditOpError::OutputsOverCap {
            cap: OUTPUTS_CAP,
            got: audit.outputs.len(),
        });
    }
    let key = audit.audit_id_bytes;
    {
        let mut t = wtxn.open_table(EXTRACTOR_AUDIT_TABLE)?;
        t.insert(&key, audit)?;
    }
    {
        let mut t = wtxn.open_table(EXTRACTOR_AUDIT_BY_MEMORY_TABLE)?;
        t.insert(&(audit.memory_id_bytes, key), &())?;
    }
    {
        let mut t = wtxn.open_table(EXTRACTOR_AUDIT_BY_EXTRACTOR_TABLE)?;
        t.insert(&(audit.extractor_id, key), &())?;
    }
    {
        let mut t = wtxn.open_table(EXTRACTOR_AUDIT_BY_TIME_TABLE)?;
        t.insert(&(audit.started_at_unix_nanos, key), &())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reads.
// ---------------------------------------------------------------------------

/// Fetch a specific audit row.
pub fn audit_get(
    rtxn: &ReadTransaction,
    audit_id: AuditId,
) -> Result<Option<ExtractionAudit>, AuditOpError> {
    let t = rtxn.open_table(EXTRACTOR_AUDIT_TABLE)?;
    let guard = t.get(&audit_id.to_bytes())?;
    Ok(guard.map(|g| g.value()))
}

/// Audits for one memory, **newest-first**, capped at `limit`.
pub fn audit_by_memory(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError> {
    let idx = rtxn.open_table(EXTRACTOR_AUDIT_BY_MEMORY_TABLE)?;
    let mid = memory_id.to_be_bytes();
    let lo = (mid, [0u8; 16]);
    let hi = (mid, [0xffu8; 16]);
    let mut ids: Vec<[u8; 16]> = Vec::new();
    for entry in idx.range(lo..=hi)? {
        let (k, _) = entry?;
        ids.push(k.value().1);
    }
    drop(idx);
    ids.sort();
    ids.reverse(); // newest-first (UUIDv7 → time-ordered)
    fetch_rows(rtxn, &ids, limit)
}

/// Audits for one extractor, newest-first.
pub fn audit_by_extractor(
    rtxn: &ReadTransaction,
    extractor_id: u32,
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError> {
    let idx = rtxn.open_table(EXTRACTOR_AUDIT_BY_EXTRACTOR_TABLE)?;
    let lo = (extractor_id, [0u8; 16]);
    let hi = (extractor_id, [0xffu8; 16]);
    let mut ids: Vec<[u8; 16]> = Vec::new();
    for entry in idx.range(lo..=hi)? {
        let (k, _) = entry?;
        ids.push(k.value().1);
    }
    drop(idx);
    ids.sort();
    ids.reverse();
    fetch_rows(rtxn, &ids, limit)
}

/// All audits started ≥ `since_unix_nanos`, newest-first.
pub fn audit_recent(
    rtxn: &ReadTransaction,
    since_unix_nanos: u64,
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError> {
    let idx = rtxn.open_table(EXTRACTOR_AUDIT_BY_TIME_TABLE)?;
    let lo = (since_unix_nanos, [0u8; 16]);
    let hi = (u64::MAX, [0xffu8; 16]);
    let mut ids: Vec<[u8; 16]> = Vec::new();
    for entry in idx.range(lo..=hi)? {
        let (k, _) = entry?;
        ids.push(k.value().1);
    }
    drop(idx);
    ids.sort();
    ids.reverse();
    fetch_rows(rtxn, &ids, limit)
}

/// [`audit_recent`] filtered to `status == FAILURE`.
pub fn audit_recent_failures(
    rtxn: &ReadTransaction,
    since_unix_nanos: u64,
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError> {
    let mut out = Vec::new();
    // Fan out wide; filter; stop at limit. Worst case scans more rows
    // than necessary; there's no status-indexed secondary table yet —
    // operators with very-failure-heavy workloads should raise the
    // issue.
    let fanout = limit.saturating_mul(8).max(limit + 16);
    for row in audit_recent(rtxn, since_unix_nanos, fanout)? {
        if row.status == extraction_status::FAILURE {
            out.push(row);
            if out.len() == limit {
                break;
            }
        }
    }
    Ok(out)
}

fn fetch_rows(
    rtxn: &ReadTransaction,
    ids: &[[u8; 16]],
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError> {
    let primary = rtxn.open_table(EXTRACTOR_AUDIT_TABLE)?;
    let mut out = Vec::with_capacity(limit.min(ids.len()));
    for id in ids.iter().take(limit) {
        if let Some(g) = primary.get(id)? {
            out.push(g.value());
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
    use crate::tables::audit::{output_kind, ExtractionAudit, OutputRef};
    use brain_core::{AuditId, MemoryId};
    use redb::{Database, ReadableDatabase};

    fn open_db(dir: &tempfile::TempDir) -> Database {
        let db = Database::create(dir.path().join("test.redb")).unwrap();
        let wtxn = db.begin_write().unwrap();
        crate::tables::materialize_all_tables(&wtxn).unwrap();
        wtxn.commit().unwrap();
        db
    }

    fn success_row(memory: MemoryId, extractor_id: u32, started_at: u64) -> ExtractionAudit {
        ExtractionAudit::success(
            AuditId::new(),
            memory,
            extractor_id,
            1,
            1,
            started_at,
            started_at + 100,
            vec![OutputRef {
                kind: output_kind::ENTITY,
                id: [1u8; 16],
            }],
            [0u8; 32],
        )
    }

    fn failure_row(memory: MemoryId, extractor_id: u32, started_at: u64) -> ExtractionAudit {
        ExtractionAudit::non_success(
            AuditId::new(),
            memory,
            extractor_id,
            1,
            1,
            started_at,
            started_at + 100,
            extraction_status::FAILURE,
            "boom".into(),
            [0u8; 32],
        )
    }

    #[test]
    fn write_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let row = success_row(MemoryId::pack(1, 0, 0), 7, 1_000);
        let id = row.audit_id();

        let wtxn = db.begin_write().unwrap();
        audit_write(&wtxn, &row).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = audit_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(got, row);
    }

    #[test]
    fn write_populates_three_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let mem = MemoryId::pack(1, 0, 0);
        let row = success_row(mem, 7, 1_000);

        let wtxn = db.begin_write().unwrap();
        audit_write(&wtxn, &row).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let by_mem = audit_by_memory(&rtxn, mem, 10).unwrap();
        assert_eq!(by_mem.len(), 1);
        let by_ext = audit_by_extractor(&rtxn, 7, 10).unwrap();
        assert_eq!(by_ext.len(), 1);
        let recent = audit_recent(&rtxn, 0, 10).unwrap();
        assert_eq!(recent.len(), 1);
    }

    #[test]
    fn by_memory_returns_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let mem = MemoryId::pack(1, 0, 0);
        let r1 = success_row(mem, 7, 1_000);
        let r2 = success_row(mem, 7, 2_000);
        let r3 = success_row(mem, 7, 3_000);

        let wtxn = db.begin_write().unwrap();
        audit_write(&wtxn, &r1).unwrap();
        audit_write(&wtxn, &r2).unwrap();
        audit_write(&wtxn, &r3).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = audit_by_memory(&rtxn, mem, 10).unwrap();
        assert_eq!(got.len(), 3);
        // UUIDv7 → newest started_at gets the highest audit_id; we
        // sort by audit_id descending. r3 should be first.
        assert_eq!(got[0].started_at_unix_nanos, r3.started_at_unix_nanos);
        assert_eq!(got[1].started_at_unix_nanos, r2.started_at_unix_nanos);
        assert_eq!(got[2].started_at_unix_nanos, r1.started_at_unix_nanos);
    }

    #[test]
    fn by_extractor_isolates_extractor_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let mem = MemoryId::pack(1, 0, 0);
        let r1 = success_row(mem, 7, 1_000);
        let r2 = success_row(mem, 8, 2_000);

        let wtxn = db.begin_write().unwrap();
        audit_write(&wtxn, &r1).unwrap();
        audit_write(&wtxn, &r2).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got_7 = audit_by_extractor(&rtxn, 7, 10).unwrap();
        let got_8 = audit_by_extractor(&rtxn, 8, 10).unwrap();
        assert_eq!(got_7.len(), 1);
        assert_eq!(got_7[0].extractor_id, 7);
        assert_eq!(got_8.len(), 1);
        assert_eq!(got_8[0].extractor_id, 8);
    }

    #[test]
    fn audit_recent_filters_by_time() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let mem = MemoryId::pack(1, 0, 0);
        let r1 = success_row(mem, 7, 100);
        let r2 = success_row(mem, 7, 500);
        let r3 = success_row(mem, 7, 900);

        let wtxn = db.begin_write().unwrap();
        audit_write(&wtxn, &r1).unwrap();
        audit_write(&wtxn, &r2).unwrap();
        audit_write(&wtxn, &r3).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = audit_recent(&rtxn, 400, 10).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|r| r.started_at_unix_nanos >= 400));
    }

    #[test]
    fn audit_recent_failures_filters_status() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let mem = MemoryId::pack(1, 0, 0);
        let r1 = success_row(mem, 7, 100);
        let r2 = failure_row(mem, 7, 200);
        let r3 = failure_row(mem, 7, 300);
        let r4 = success_row(mem, 7, 400);

        let wtxn = db.begin_write().unwrap();
        audit_write(&wtxn, &r1).unwrap();
        audit_write(&wtxn, &r2).unwrap();
        audit_write(&wtxn, &r3).unwrap();
        audit_write(&wtxn, &r4).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = audit_recent_failures(&rtxn, 0, 10).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|r| r.is_failure()));
    }

    #[test]
    fn outputs_over_cap_rejected_before_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let mem = MemoryId::pack(1, 0, 0);
        let mut row = success_row(mem, 7, 1_000);
        row.outputs = (0..OUTPUTS_CAP + 1)
            .map(|i| OutputRef {
                kind: output_kind::ENTITY,
                id: [(i & 0xff) as u8; 16],
            })
            .collect();

        let wtxn = db.begin_write().unwrap();
        let err = audit_write(&wtxn, &row).unwrap_err();
        assert!(matches!(
            err,
            AuditOpError::OutputsOverCap { cap, got } if cap == OUTPUTS_CAP && got == OUTPUTS_CAP + 1
        ));
        wtxn.commit().unwrap();
        // The wtxn was committed cleanly because the over-cap call
        // returned early without writing — no audit row exists.
        let rtxn = db.begin_read().unwrap();
        let by_mem = audit_by_memory(&rtxn, mem, 10).unwrap();
        assert!(by_mem.is_empty());
    }

    #[test]
    fn fresh_db_queries_return_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let rtxn = db.begin_read().unwrap();
        assert!(audit_get(&rtxn, AuditId::new()).unwrap().is_none());
        assert!(audit_by_memory(&rtxn, MemoryId::pack(1, 0, 0), 10)
            .unwrap()
            .is_empty());
        assert!(audit_by_extractor(&rtxn, 7, 10).unwrap().is_empty());
        assert!(audit_recent(&rtxn, 0, 10).unwrap().is_empty());
        assert!(audit_recent_failures(&rtxn, 0, 10).unwrap().is_empty());
    }

    #[test]
    fn audit_by_memory_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let mem = MemoryId::pack(1, 0, 0);

        let wtxn = db.begin_write().unwrap();
        for i in 0..5u64 {
            let r = success_row(mem, 7, 1_000 + i);
            audit_write(&wtxn, &r).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = audit_by_memory(&rtxn, mem, 2).unwrap();
        assert_eq!(got.len(), 2);
    }
}
