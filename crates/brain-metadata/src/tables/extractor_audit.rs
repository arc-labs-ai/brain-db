//! Per-memory pipeline audit table — `extractor_pipeline_audit`.
//!
//! Captures one row per ENCODE the worker processed: which tiers fired,
//! what they emitted, how much the LLM tier cost. Used for two purposes:
//!
//! 1. **Idempotency.** Before re-running extraction on a memory, the
//!    worker probes `has_extracted` to skip memories already audited.
//!    Queue replay on worker restart therefore can't double-write
//!    entities / statements / mention edges.
//! 2. **Observability.** Operators can ask "how many memories has the
//!    extractor pipeline visited?", "how much did the LLM tier cost
//!    last week?", "which memories partial-failed?".
//!
//! This is distinct from `extractor_audit` (per-extractor, per-run)
//! in [`crate::tables::knowledge::audit`]. That table records each
//! individual `Extractor::run` invocation; this one records the
//! per-memory dispatch outcome the worker decided to commit.

use crate::impl_redb_rkyv_value;
use brain_core::MemoryId;
use redb::{ReadTransaction, ReadableTableMetadata, TableDefinition, WriteTransaction};

// ---------------------------------------------------------------------------
// Table.
// ---------------------------------------------------------------------------

/// `memory_id_be_bytes → ExtractorPipelineAuditEntry`.
pub const EXTRACTOR_PIPELINE_AUDIT_TABLE: TableDefinition<
    'static,
    [u8; 16],
    ExtractorPipelineAuditEntry,
> = TableDefinition::new("extractor_pipeline_audit_v1");

// ---------------------------------------------------------------------------
// Status discriminants.
// ---------------------------------------------------------------------------

/// `ExtractorPipelineAuditEntry.status` byte values. Stable; never
/// reassigned.
pub mod pipeline_status {
    /// All enabled tiers ran successfully and at least one tier produced
    /// items (or all tiers produced none, but none errored either).
    pub const SUCCESS: u8 = 1;
    /// One or more tiers failed but the worker committed what other
    /// tiers produced. Memory is still audited so it won't be retried.
    pub const PARTIAL_FAILURE: u8 = 2;
    /// All tiers failed or the apply path errored; nothing was
    /// committed downstream. Auditing prevents infinite retry; an
    /// operator-triggered backfill is the recovery path.
    pub const FAILURE: u8 = 3;
    /// Worker decided not to run any tier (e.g., the registry was
    /// empty for this deployment).
    pub const SKIPPED: u8 = 4;
}

/// `TierStatus` byte values. Mirrors per-tier outcome inside one
/// audit row. Stable; never reassigned.
pub mod tier_status {
    /// Tier executed and produced (possibly zero) items.
    pub const RAN: u8 = 1;
    /// Tier was skipped — either disabled, budget exhausted, or
    /// schema-gated.
    pub const SKIPPED: u8 = 2;
    /// Tier executed but returned an error or malformed output.
    pub const FAILED: u8 = 3;
    /// Tier not present in the registry (e.g., LLM extractor not
    /// configured for this deployment).
    pub const ABSENT: u8 = 4;
}

// ---------------------------------------------------------------------------
// Value struct.
// ---------------------------------------------------------------------------

/// Counts of items the worker resolved + committed for one memory.
/// Mirrors the `ExtractedItem` discriminants in `brain_extractors`.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[archive(check_bytes)]
pub struct ExtractorItemCounts {
    pub entities: u32,
    pub statements: u32,
    pub relations: u32,
    pub mention_edges: u32,
}

impl ExtractorItemCounts {
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            entities: 0,
            statements: 0,
            relations: 0,
            mention_edges: 0,
        }
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.entities == 0 && self.statements == 0 && self.relations == 0 && self.mention_edges == 0
    }
}

/// Per-memory dispatch audit row.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ExtractorPipelineAuditEntry {
    pub memory_id_bytes: [u8; 16],
    pub extracted_at_unix_nanos: u64,
    /// One of the [`pipeline_status`] constants.
    pub status: u8,
    /// Free-form reason; empty on `SUCCESS`. Populated with the first
    /// tier's error message on `PARTIAL_FAILURE` / `FAILURE`.
    pub status_reason: String,
    /// One of the [`tier_status`] constants.
    pub tier_pattern: u8,
    pub tier_classifier: u8,
    pub tier_llm: u8,
    pub item_counts: ExtractorItemCounts,
    /// LLM cost in dollar-micro-units (1e-6 USD). 0 when the LLM tier
    /// didn't run.
    pub llm_micro_usd_spent: u64,
}

impl ExtractorPipelineAuditEntry {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        memory_id: MemoryId,
        extracted_at_unix_nanos: u64,
        status: u8,
        status_reason: String,
        tier_pattern: u8,
        tier_classifier: u8,
        tier_llm: u8,
        item_counts: ExtractorItemCounts,
        llm_micro_usd_spent: u64,
    ) -> Self {
        Self {
            memory_id_bytes: memory_id.to_be_bytes(),
            extracted_at_unix_nanos,
            status,
            status_reason,
            tier_pattern,
            tier_classifier,
            tier_llm,
            item_counts,
            llm_micro_usd_spent,
        }
    }

    #[must_use]
    pub fn memory_id(&self) -> MemoryId {
        MemoryId::from_be_bytes(self.memory_id_bytes)
    }
}

impl_redb_rkyv_value!(
    ExtractorPipelineAuditEntry,
    "brain_metadata::ExtractorPipelineAuditEntry::v1"
);

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Errors produced by [`has_extracted`] / [`record_extracted`].
#[derive(thiserror::Error, Debug)]
pub enum ExtractorPipelineAuditError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
}

/// Probe whether the worker has already processed `memory_id`. Used to
/// skip already-extracted memories on queue replay.
pub fn has_extracted(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
) -> Result<bool, ExtractorPipelineAuditError> {
    let table = match rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE) {
        Ok(t) => t,
        // Table not yet materialised (no extraction has ever run on
        // this shard). That's a definitive "not extracted".
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(ExtractorPipelineAuditError::Table(e)),
    };
    Ok(table.get(&memory_id.to_be_bytes())?.is_some())
}

/// Commit the pipeline outcome for one memory. Idempotent under
/// repeated calls (later write overwrites prior).
pub fn record_extracted(
    wtxn: &WriteTransaction,
    entry: &ExtractorPipelineAuditEntry,
) -> Result<(), ExtractorPipelineAuditError> {
    let mut t = wtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE)?;
    t.insert(&entry.memory_id_bytes, entry)?;
    Ok(())
}

/// Total number of pipeline audit rows present in the shard. Used by
/// observability + tests.
pub fn audit_count(rtxn: &ReadTransaction) -> Result<u64, ExtractorPipelineAuditError> {
    let table = match rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(ExtractorPipelineAuditError::Table(e)),
    };
    Ok(table.len()?)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::MetadataDb;
    use tempfile::TempDir;

    fn db(dir: &TempDir) -> MetadataDb {
        MetadataDb::open(dir.path().join("metadata.redb")).expect("open")
    }

    fn make_entry(memory_id: MemoryId) -> ExtractorPipelineAuditEntry {
        ExtractorPipelineAuditEntry::new(
            memory_id,
            1_700_000_000_000_000_000,
            pipeline_status::SUCCESS,
            String::new(),
            tier_status::RAN,
            tier_status::ABSENT,
            tier_status::ABSENT,
            ExtractorItemCounts {
                entities: 2,
                statements: 1,
                relations: 0,
                mention_edges: 2,
            },
            0,
        )
    }

    #[test]
    fn has_extracted_returns_false_on_empty_db() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let rtxn = d.read_txn().unwrap();
        let id = MemoryId::pack(0, 1, 1);
        assert!(!has_extracted(&rtxn, id).unwrap());
    }

    #[test]
    fn record_then_has_extracted_returns_true() {
        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let id = MemoryId::pack(0, 42, 1);
        let entry = make_entry(id);
        {
            let wtxn = d.write_txn().unwrap();
            record_extracted(&wtxn, &entry).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = d.read_txn().unwrap();
        assert!(has_extracted(&rtxn, id).unwrap());
        // A different id is still untouched.
        assert!(!has_extracted(&rtxn, MemoryId::pack(0, 43, 1)).unwrap());
    }

    #[test]
    fn record_overwrites_prior_entry() {
        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let id = MemoryId::pack(0, 7, 1);
        let mut first = make_entry(id);
        first.item_counts.entities = 1;
        {
            let wtxn = d.write_txn().unwrap();
            record_extracted(&wtxn, &first).unwrap();
            wtxn.commit().unwrap();
        }
        let mut second = make_entry(id);
        second.item_counts.entities = 9;
        {
            let wtxn = d.write_txn().unwrap();
            record_extracted(&wtxn, &second).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = d.read_txn().unwrap();
        let table = rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE).unwrap();
        let got = table.get(&id.to_be_bytes()).unwrap().unwrap().value();
        assert_eq!(got.item_counts.entities, 9);
    }

    #[test]
    fn audit_count_grows_with_records() {
        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        {
            let rtxn = d.read_txn().unwrap();
            assert_eq!(audit_count(&rtxn).unwrap(), 0);
        }
        {
            let wtxn = d.write_txn().unwrap();
            record_extracted(&wtxn, &make_entry(MemoryId::pack(0, 1, 1))).unwrap();
            record_extracted(&wtxn, &make_entry(MemoryId::pack(0, 2, 1))).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = d.read_txn().unwrap();
        assert_eq!(audit_count(&rtxn).unwrap(), 2);
    }

    #[test]
    fn entry_round_trips_through_rkyv() {
        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let id = MemoryId::pack(2, 99, 5);
        let entry = ExtractorPipelineAuditEntry::new(
            id,
            42,
            pipeline_status::PARTIAL_FAILURE,
            "LLM 5xx".into(),
            tier_status::RAN,
            tier_status::RAN,
            tier_status::FAILED,
            ExtractorItemCounts {
                entities: 4,
                statements: 2,
                relations: 1,
                mention_edges: 4,
            },
            12_345,
        );
        {
            let wtxn = d.write_txn().unwrap();
            record_extracted(&wtxn, &entry).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = d.read_txn().unwrap();
        let table = rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE).unwrap();
        let got = table.get(&id.to_be_bytes()).unwrap().unwrap().value();
        assert_eq!(got, entry);
        assert_eq!(got.memory_id(), id);
    }
}
