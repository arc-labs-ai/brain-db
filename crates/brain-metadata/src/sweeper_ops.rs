//! Shared sweeper-side primitives (sub-tasks 24.3–24.7).
//! Spec §27/03 §1 — common discipline (dry-run, batch cap,
//! SweepSummary).
//!
//! Individual sweepers live in `brain-workers::workers::*` and
//! call into this module for the metadata-side scan-and-delete.

use redb::{ReadableTable, WriteTransaction};

use crate::statement_ops::StatementOpError;
use crate::tables::knowledge::audit::EXTRACTOR_AUDIT_TABLE;
use crate::tables::knowledge::statement::STATEMENTS_TABLE;

/// Shared summary returned by every sweeper. Spec §27/03 §1.4.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepSummary {
    pub scanned: u64,
    pub deleted: u64,
    pub dry_run_would_delete: u64,
    pub skipped: u64,
}

// ---------------------------------------------------------------------------
// 24.3 — Supersession sweeper.
// ---------------------------------------------------------------------------

/// Hard-delete superseded statements past `retention_seconds`.
/// Spec §27/03 §2.
///
/// `retention_seconds == 0` means "disabled" — the caller is
/// expected to short-circuit before invoking, but we double-check.
pub fn sweep_superseded_statements(
    wtxn: &WriteTransaction,
    retention_seconds: u64,
    now_unix_nanos: u64,
    batch_cap: usize,
    dry_run: bool,
) -> Result<SweepSummary, StatementOpError> {
    let mut summary = SweepSummary::default();
    if retention_seconds == 0 {
        return Ok(summary);
    }
    let cutoff_ns = now_unix_nanos.saturating_sub(retention_seconds * 1_000_000_000);

    // Collect victim ids first (statements with `superseded_by` set
    // AND retired earlier than cutoff). Two-phase scan keeps the
    // scan-side immutable.
    //
    // Retired-at proxy: `valid_to_unix_nanos`. `statement_supersede`
    // (statement_ops.rs §"Supersede non-Event statements") sets
    // `valid_to_unix_nanos = Some(new.extracted_at_unix_nanos)` on
    // the old row when its existing valid_to was None — which is
    // the universal case for supersession-driven retirement. Events
    // cannot be superseded, so their None valid_to here is correct:
    // the loop skips them via the `superseded_by_bytes.is_none()`
    // guard before this check.
    //
    // Operators who explicitly set a Statement's `valid_to` in the
    // future and THEN supersede it will see this preserved
    // valid_to drive the sweeper's cutoff — i.e. the sweeper waits
    // until the operator-declared end-of-validity. That matches
    // the user's intent (the row is "valid until X"; sweep when
    // retention past X expires).
    let victims: Vec<[u8; 16]> = {
        let table = wtxn.open_table(STATEMENTS_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v) = entry?;
            let row = v.value();
            summary.scanned += 1;
            if row.superseded_by_bytes.is_none() {
                continue;
            }
            let Some(retired_at) = row.valid_to_unix_nanos else {
                continue;
            };
            if retired_at > cutoff_ns {
                continue;
            }
            out.push(row.statement_id_bytes);
            if out.len() == batch_cap {
                break;
            }
        }
        out
    };

    if dry_run {
        summary.dry_run_would_delete = victims.len() as u64;
    } else {
        let mut t = wtxn.open_table(STATEMENTS_TABLE)?;
        for key in &victims {
            t.remove(key)?;
            summary.deleted += 1;
        }
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// 24.7 — Audit log sweeper.
// ---------------------------------------------------------------------------

/// Hard-delete audit rows older than `retention_seconds`. Spec
/// §27/03 §3. Merge/Unmerge audit rows are exempt (kept forever
/// per §25/00) — v1 audit table stores extraction events only,
/// so the merge-exemption is a no-op until merge audits land on
/// this table.
pub fn sweep_audit_log(
    wtxn: &WriteTransaction,
    retention_seconds: u64,
    now_unix_nanos: u64,
    batch_cap: usize,
    dry_run: bool,
) -> Result<SweepSummary, redb::Error> {
    let mut summary = SweepSummary::default();
    if retention_seconds == 0 {
        return Ok(summary);
    }
    let cutoff_ns = now_unix_nanos.saturating_sub(retention_seconds * 1_000_000_000);

    let victims: Vec<[u8; 16]> = {
        let table = match wtxn.open_table(EXTRACTOR_AUDIT_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(summary),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            let row = v.value();
            summary.scanned += 1;
            if row.started_at_unix_nanos > cutoff_ns {
                continue;
            }
            out.push(k.value());
            if out.len() == batch_cap {
                break;
            }
        }
        out
    };

    if dry_run {
        summary.dry_run_would_delete = victims.len() as u64;
    } else {
        let mut t = wtxn.open_table(EXTRACTOR_AUDIT_TABLE)?;
        for key in &victims {
            t.remove(key)?;
            summary.deleted += 1;
        }
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// 24.4 — Stale extraction detector.
// ---------------------------------------------------------------------------

/// Stale-extraction flag bit on `StatementMetadata` is not yet
/// in the row layout. v1 surfaces staleness via row inspection
/// at query time (cheap: schema_version comparison). The
/// dedicated flag bit lands as a post-v1 schema bump.
///
/// This sweeper enumerates statements whose `schema_version` is
/// behind the current value and returns the count. The flag-write
/// side is deferred; admin / query layer can consult the same
/// predicate on-demand.
pub fn scan_stale_statements(
    rtxn: &redb::ReadTransaction,
    current_schema_version: u32,
    batch_cap: usize,
) -> Result<SweepSummary, StatementOpError> {
    let mut summary = SweepSummary::default();
    let table = match rtxn.open_table(STATEMENTS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(summary),
        Err(e) => return Err(StatementOpError::from(e)),
    };
    for entry in table.iter()? {
        let (_, v) = entry?;
        let row = v.value();
        summary.scanned += 1;
        if row.tombstoned != 0 {
            continue;
        }
        if row.schema_version < current_schema_version {
            // Count under `dry_run_would_delete` to reuse the field.
            // A future schema bump adds STATEMENT_FLAG_STALE_EXTRACTION
            // and converts this to a real mutation.
            summary.dry_run_would_delete += 1;
        }
        if summary.scanned >= batch_cap as u64 {
            break;
        }
    }
    Ok(summary)
}
