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
//! in [`crate::tables::audit`]. That table records each
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
> = TableDefinition::new("extractor_pipeline_audit");

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

/// `ExtractorPipelineAuditEntry.failure_class` byte values — whether a
/// failed LLM tier is worth retrying. Stable; never reassigned.
pub mod failure_class {
    /// Not a failure, or a failure carrying no retry signal. Treated as
    /// retryable (a transient blip is the safe default — retry-with-backoff
    /// is bounded and cheap; dropping grounding on an unclassified failure
    /// is not).
    pub const UNCLASSIFIED: u8 = 0;
    /// The provider blipped (timeout / rate-limit / network / 5xx) — the
    /// same call can succeed later. Retried with backoff, never terminal.
    pub const TRANSIENT: u8 = 1;
    /// The call can't succeed as-is (bad/absent key, no balance, malformed
    /// request, schema mismatch). Terminal immediately so the operator sees
    /// it instead of a silent retry loop.
    pub const PERMANENT: u8 = 2;
}

/// Backoff before re-attempting a transient extraction failure:
/// `min(RETRY_BACKOFF_CAP, RETRY_BACKOFF_BASE * 2^(attempts-1))`. Keeps a
/// passing provider outage from hot-looping while still recovering on its
/// own once the provider returns — no permanent grounding loss.
pub const RETRY_BACKOFF_BASE_NANOS: u64 = 1_000_000_000; // 1s
pub const RETRY_BACKOFF_CAP_NANOS: u64 = 3_600_000_000_000; // 1h

/// Nanos to wait before the `attempts`-th retry of a transient failure.
#[must_use]
pub fn retry_backoff_nanos(attempts: u8) -> u64 {
    let shift = attempts.saturating_sub(1).min(63);
    RETRY_BACKOFF_BASE_NANOS
        .saturating_mul(1u64 << shift)
        .min(RETRY_BACKOFF_CAP_NANOS)
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
    /// How many extraction attempts this memory has had. Starts at 1 on
    /// the first run; incremented each time a retryable failure re-records
    /// the row. Used to compute the transient-retry backoff
    /// ([`retry_backoff_nanos`]); not a terminal cap any more — a transient
    /// failure retries until it succeeds (the read path's grounding must
    /// not be permanently lost to a passing provider outage), while a
    /// permanent failure is terminal on the first attempt regardless.
    pub attempts: u8,
    /// One of the [`failure_class`] constants. Set on a failed LLM tier so
    /// the retry decision can keep transient failures reprocessable and
    /// terminate permanent ones. `UNCLASSIFIED` for success / skip / non-LLM
    /// outcomes.
    pub failure_class: u8,
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
            // First attempt by default; the worker bumps this via
            // `with_attempts` when re-recording a retryable LLM failure.
            attempts: 1,
            failure_class: failure_class::UNCLASSIFIED,
        }
    }

    /// Override the attempt counter (the worker sets `prior + 1` when
    /// re-recording a retryable failure so the backoff schedule advances).
    #[must_use]
    pub fn with_attempts(mut self, attempts: u8) -> Self {
        self.attempts = attempts;
        self
    }

    /// Tag the failure class (one of [`failure_class`]) the worker derived
    /// from the LLM error, so the retry gate can distinguish a transient
    /// blip (retry with backoff) from a permanent failure (terminal).
    #[must_use]
    pub fn with_failure_class(mut self, class: u8) -> Self {
        self.failure_class = class;
        self
    }

    #[must_use]
    pub fn memory_id(&self) -> MemoryId {
        MemoryId::from_be_bytes(self.memory_id_bytes)
    }
}

impl_redb_rkyv_value!(
    ExtractorPipelineAuditEntry,
    "brain_metadata::ExtractorPipelineAuditEntry"
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

/// Whether a failed audit `entry` is a retryable transient LLM failure.
///
/// Retryable ONLY when the LLM tier itself failed (statements are LLM-only,
/// and a failed LLM call wrote zero, so a retry can't duplicate them) AND
/// the failure isn't classified permanent. A pattern/classifier-only
/// failure is never retryable: its rows already committed, so re-running
/// would duplicate. A permanent LLM failure (bad key / no balance /
/// malformed) is terminal — retrying can't help and only hides the problem.
fn is_retryable_transient(entry: &ExtractorPipelineAuditEntry) -> bool {
    entry.tier_llm == tier_status::FAILED && entry.failure_class != failure_class::PERMANENT
}

/// Probe whether `memory_id`'s extraction is **terminal** — i.e. the
/// worker should skip it on queue replay. Terminal means: it succeeded,
/// there was nothing to extract, or it failed permanently / in a non-LLM
/// tier. A retryable transient LLM failure returns `false` so the worker
/// re-runs it (subject to backoff, see [`extraction_retry_due`]) rather
/// than permanently abandoning the memory's typed graph. Absent row →
/// `false`.
pub fn has_extracted(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
) -> Result<bool, ExtractorPipelineAuditError> {
    let table = rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE)?;
    let Some(row) = table.get(&memory_id.to_be_bytes())? else {
        return Ok(false);
    };
    let entry = row.value();
    let terminal = match entry.status {
        pipeline_status::SUCCESS | pipeline_status::SKIPPED => true,
        _ => !is_retryable_transient(&entry),
    };
    Ok(terminal)
}

/// Whether a memory queued for extraction is **due** to (re)run at `now`.
///
/// A memory with no audit row, or a non-failure / terminal row, is due
/// (the terminal case is filtered separately by [`has_extracted`] and
/// removed from the queue). A retryable transient failure is due only once
/// its exponential backoff has elapsed since the last attempt — so a
/// provider outage retries on a widening interval instead of every cycle,
/// recovering on its own without burning calls. Absent row → due.
pub fn extraction_retry_due(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
    now_unix_nanos: u64,
) -> Result<bool, ExtractorPipelineAuditError> {
    let table = rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE)?;
    let Some(row) = table.get(&memory_id.to_be_bytes())? else {
        return Ok(true);
    };
    let entry = row.value();
    if !is_retryable_transient(&entry) {
        return Ok(true);
    }
    let due_at = entry
        .extracted_at_unix_nanos
        .saturating_add(retry_backoff_nanos(entry.attempts));
    Ok(now_unix_nanos >= due_at)
}

/// Read a memory's prior extraction-attempt count (0 if no audit row
/// exists yet). Used by the worker to increment `attempts` when
/// re-recording a retryable failure.
pub fn extraction_attempts(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
) -> Result<u8, ExtractorPipelineAuditError> {
    let table = rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE)?;
    Ok(table
        .get(&memory_id.to_be_bytes())?
        .map(|r| r.value().attempts)
        .unwrap_or(0))
}

/// Read the full pipeline audit entry for `memory_id`, if one exists.
/// Returns the owned [`ExtractorPipelineAuditEntry`] so callers can inspect
/// per-tier status and item counts (e.g. to confirm which tiers ran and what
/// they produced). Absent row → `None`.
pub fn pipeline_audit_entry(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
) -> Result<Option<ExtractorPipelineAuditEntry>, ExtractorPipelineAuditError> {
    let table = rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE)?;
    Ok(table.get(&memory_id.to_be_bytes())?.map(|r| r.value()))
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
    let table = rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE)?;
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
        let d = db(&dir);
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

    /// A transient LLM-tier failure stays retryable (never terminal) so a
    /// passing provider outage can't permanently drop a memory's typed
    /// graph — but it is rate-limited by backoff. A permanent LLM failure
    /// (bad key / no balance) and a non-LLM failure are terminal at once.
    #[test]
    fn transient_llm_failure_retries_with_backoff_permanent_is_terminal() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let id = MemoryId::pack(0, 100, 1);
        let base = 1_700_000_000_000_000_000u64;

        let llm_fail = |attempts: u8, class: u8| {
            ExtractorPipelineAuditEntry::new(
                id,
                base,
                pipeline_status::PARTIAL_FAILURE,
                "provider request timed out".into(),
                tier_status::RAN,    // pattern ran
                tier_status::RAN,    // classifier ran
                tier_status::FAILED, // llm failed
                ExtractorItemCounts::zero(),
                0,
            )
            .with_attempts(attempts)
            .with_failure_class(class)
        };
        let put = |entry: &ExtractorPipelineAuditEntry| {
            let wtxn = d.write_txn().unwrap();
            record_extracted(&wtxn, entry).unwrap();
            wtxn.commit().unwrap();
        };

        // Transient → never terminal, even after many attempts.
        put(&llm_fail(7, failure_class::TRANSIENT));
        {
            let rtxn = d.read_txn().unwrap();
            assert!(!has_extracted(&rtxn, id).unwrap());
            // Not due immediately after the attempt (backoff in effect)…
            assert!(!extraction_retry_due(&rtxn, id, base + 1).unwrap());
            // …but due once the backoff window elapses.
            let due_at = base + retry_backoff_nanos(7);
            assert!(extraction_retry_due(&rtxn, id, due_at).unwrap());
        }

        // Unclassified LLM failure is treated as transient (retryable).
        put(&llm_fail(1, failure_class::UNCLASSIFIED));
        {
            let rtxn = d.read_txn().unwrap();
            assert!(!has_extracted(&rtxn, id).unwrap());
        }

        // Permanent → terminal immediately (no retry loop).
        put(&llm_fail(1, failure_class::PERMANENT));
        {
            let rtxn = d.read_txn().unwrap();
            assert!(has_extracted(&rtxn, id).unwrap());
            // Terminal rows report "due" (the queue removes them separately).
            assert!(extraction_retry_due(&rtxn, id, base + 1).unwrap());
        }

        // A non-LLM failure (classifier failed, llm ok) is terminal — its
        // statements committed; a retry would duplicate.
        let classifier_fail = ExtractorPipelineAuditEntry::new(
            id,
            base,
            pipeline_status::PARTIAL_FAILURE,
            "classifier failed".into(),
            tier_status::RAN,
            tier_status::FAILED, // classifier failed
            tier_status::RAN,    // llm ok
            ExtractorItemCounts::zero(),
            0,
        )
        .with_attempts(1);
        put(&classifier_fail);
        {
            let rtxn = d.read_txn().unwrap();
            assert!(has_extracted(&rtxn, id).unwrap());
        }
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(retry_backoff_nanos(1), RETRY_BACKOFF_BASE_NANOS);
        assert_eq!(retry_backoff_nanos(2), RETRY_BACKOFF_BASE_NANOS * 2);
        assert_eq!(retry_backoff_nanos(3), RETRY_BACKOFF_BASE_NANOS * 4);
        // Far out, it saturates at the cap rather than overflowing.
        assert_eq!(retry_backoff_nanos(200), RETRY_BACKOFF_CAP_NANOS);
    }

    #[test]
    fn record_overwrites_prior_entry() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
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
        let d = db(&dir);
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
        let d = db(&dir);
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
