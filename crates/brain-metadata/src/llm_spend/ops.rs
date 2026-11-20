//! Read-modify-write operations for the durable LLM spend ledger.

use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::audit::EXTRACTOR_AUDIT_TABLE;
use crate::tables::llm_spend::LLM_SPEND_TABLE;

/// Errors from the LLM spend ledger.
#[derive(thiserror::Error, Debug)]
pub enum LlmSpendError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
}

/// Add `micro_usd` to the running total for `(namespace, stage)` and return the
/// new total.
///
/// Read-modify-write inside the caller's transaction; the caller commits. The
/// add saturates: an implausible figure pins the counter at the ceiling rather
/// than wrapping it to a small number and reporting a tenant as having spent
/// almost nothing.
pub fn llm_spend_add(
    wtxn: &WriteTransaction,
    namespace: &str,
    stage: u8,
    micro_usd: u64,
) -> Result<u64, LlmSpendError> {
    let mut table = wtxn.open_table(LLM_SPEND_TABLE)?;
    let key = (namespace, stage);
    let total = table
        .get(&key)?
        .map_or(0, |v| v.value())
        .saturating_add(micro_usd);
    table.insert(&key, &total)?;
    Ok(total)
}

/// Running total for one `(namespace, stage)` pair, in dollar micro-units.
///
/// Zero when nothing has been recorded, including on a redb file written before
/// this table existed — the table is created lazily by the first
/// [`llm_spend_add`], so its absence means no spend, not an error.
pub fn llm_spend_get(
    rtxn: &ReadTransaction,
    namespace: &str,
    stage: u8,
) -> Result<u64, LlmSpendError> {
    let table = match rtxn.open_table(LLM_SPEND_TABLE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    Ok(table.get(&(namespace, stage))?.map_or(0, |v| v.value()))
}

/// Every recorded `(namespace, stage, micro_usd)` triple, in key order — the
/// reconciliation read an operator runs against a provider invoice. Empty when
/// nothing has ever been recorded.
pub fn llm_spend_iter(rtxn: &ReadTransaction) -> Result<Vec<(String, u8, u64)>, LlmSpendError> {
    let table = match rtxn.open_table(LLM_SPEND_TABLE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut rows = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        let (namespace, stage) = key.value();
        rows.push((namespace.to_owned(), stage, value.value()));
    }
    Ok(rows)
}

/// Every micro-USD of model spend durably recorded for `namespace`, across both
/// stores that hold any.
///
/// The extraction tiers record their cost on their own audit row in
/// `EXTRACTOR_AUDIT_TABLE`; every other model-backed stage records it in
/// `LLM_SPEND_TABLE`. Neither half is the tenant's bill on its own, so this sums
/// them. Rows whose `namespace` is empty were written without attribution and
/// are counted toward no namespace.
///
/// Scans the extraction audit table, which holds one row per tier call per
/// memory: this is a reporting read, not a request-path one.
pub fn namespace_total_micro_usd(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<u64, LlmSpendError> {
    // An unattributed audit row carries the empty namespace, so the empty
    // namespace is the one name that owns nothing: answering with those rows'
    // costs would report spend for a tenant that does not exist.
    if namespace.is_empty() {
        return Ok(0);
    }

    let mut total: u64 = 0;

    match rtxn.open_table(LLM_SPEND_TABLE) {
        Ok(table) => {
            for entry in table.iter()? {
                let (key, value) = entry?;
                if key.value().0 == namespace {
                    total = total.saturating_add(value.value());
                }
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => return Err(e.into()),
    }

    match rtxn.open_table(EXTRACTOR_AUDIT_TABLE) {
        Ok(table) => {
            for entry in table.iter()? {
                let (_, value) = entry?;
                let row = value.value();
                if row.namespace == namespace {
                    total = total.saturating_add(row.cost_micro_usd);
                }
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => return Err(e.into()),
    }

    Ok(total)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;
    use tempfile::TempDir;

    #[test]
    fn add_accumulates_committed_stage_totals_without_collisions() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        assert_eq!(llm_spend_add(&wtxn, "acme", 1, 7).unwrap(), 7);
        assert_eq!(llm_spend_add(&wtxn, "acme", 1, 5).unwrap(), 12);
        assert_eq!(llm_spend_add(&wtxn, "acme", 2, 3).unwrap(), 3);
        wtxn.commit().unwrap();
        let wtxn = db.begin_write().unwrap();
        assert_eq!(llm_spend_add(&wtxn, "acme", 1, 8).unwrap(), 20);
        wtxn.commit().unwrap();
        let rtxn = db.begin_read().unwrap();
        assert_eq!(llm_spend_get(&rtxn, "acme", 1).unwrap(), 20);
        assert_eq!(llm_spend_get(&rtxn, "acme", 2).unwrap(), 3);
    }

    #[test]
    fn empty_database_reads_as_zero() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let rtxn = db.begin_read().unwrap();
        assert_eq!(llm_spend_get(&rtxn, "missing", 1).unwrap(), 0);
        assert!(llm_spend_iter(&rtxn).unwrap().is_empty());
    }

    /// A namespace's durable spend is both stores added together: the ledger
    /// rows for its non-extraction stages plus the cost on its extraction audit
    /// rows. Another tenant's rows and unattributed rows are not its spend.
    #[test]
    fn namespace_total_sums_ledger_and_extraction_audit() {
        use crate::tables::audit::{ExtractionAudit, EXTRACTOR_AUDIT_TABLE};
        use brain_core::{AuditId, MemoryId};

        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);

        let audit_row = |namespace: &str, cost: u64| {
            let mut row = ExtractionAudit::success(
                AuditId::new(),
                MemoryId::pack(1, 42, 1),
                7,
                1,
                1,
                1_700_000_000_000_000_000,
                1_700_000_000_000_000_500,
                Vec::new(),
                [0u8; 32],
            )
            .with_namespace(namespace.to_string());
            row.cost_micro_usd = cost;
            row
        };

        let wtxn = db.begin_write().unwrap();
        llm_spend_add(&wtxn, "acme", 1, 100).unwrap();
        llm_spend_add(&wtxn, "acme", 3, 20).unwrap();
        llm_spend_add(&wtxn, "globex", 1, 7_000).unwrap();
        {
            let mut t = wtxn.open_table(EXTRACTOR_AUDIT_TABLE).unwrap();
            for (ns, cost) in [("acme", 5u64), ("acme", 3), ("globex", 900), ("", 11)] {
                let row = audit_row(ns, cost);
                t.insert(&row.audit_id_bytes, &row).unwrap();
            }
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        // 100 + 20 from the ledger, 5 + 3 from the audit rows.
        assert_eq!(namespace_total_micro_usd(&rtxn, "acme").unwrap(), 128);
        // 7_000 from the ledger, 900 from the audit row.
        assert_eq!(namespace_total_micro_usd(&rtxn, "globex").unwrap(), 7_900);
        // An unattributed audit row belongs to no namespace.
        assert_eq!(namespace_total_micro_usd(&rtxn, "").unwrap(), 0);
        // A namespace that has spent nothing reads as zero, not as an error.
        assert_eq!(namespace_total_micro_usd(&rtxn, "initech").unwrap(), 0);
    }
}
