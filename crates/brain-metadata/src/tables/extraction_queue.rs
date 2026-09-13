//! `extraction_queue` table — durable trigger for the async extractor.
//!
//! When an ENCODE write commits, the memory is enqueued here inside the
//! same write txn that persists the memory metadata. The per-shard
//! extractor worker drains the queue, runs the extraction tiers, and
//! removes the row only after that memory's extraction has committed.
//!
//! A redb table rather than an in-memory channel for crash safety: a
//! shard that restarts after ENCODE committed but before extraction ran
//! still finds the row, so no memory is silently left un-extracted. The
//! enqueue + remove discipline makes re-runs idempotent — re-draining a
//! row already extracted is harmless because the worker removes it only
//! on success.
//!
//! Key is the `MemoryId` big-endian bytes; value is the enqueue time in
//! unix nanos. The value is observability-only (worker logs "oldest
//! pending" age); the table is not time-ordered.

use redb::TableDefinition;

/// Queue of memory ids awaiting asynchronous extraction.
///
/// Populated inside the ENCODE write txn and drained by the per-shard
/// extractor worker. See the module docs for the crash-safety rationale.
pub const EXTRACTION_QUEUE_TABLE: TableDefinition<'static, [u8; 16], u64> =
    TableDefinition::new("extraction_queue");
