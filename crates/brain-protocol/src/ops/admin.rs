//! Admin-surface requests.

use crate::envelope::request::{WireContextId, WireMemoryId, WireUuid};
use crate::shared::primitives::{CheckScope, ForgetMode, MemoryKindWire, StatsDetail};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminStatsRequest {
    pub detail: StatsDetail,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminSnapshotRequest {
    pub snapshot_name: String,
    pub target_path: Option<String>,
    pub include_wal: bool,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminRestoreRequest {
    pub snapshot_name: String,
    pub target_shard: Option<u8>,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminIntegrityCheckRequest {
    pub scope: CheckScope,
    pub repair_if_possible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminMigrateEmbeddingsRequest {
    pub target_model: ModelIdentifier,
    pub batch_size: u32,
    pub rate_limit_qps: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelIdentifier {
    pub name: String,
    #[serde(with = "serde_bytes")]
    pub fingerprint: [u8; 16],
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminCreateContextRequest {
    pub name: String,
    pub description: String,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminRenameContextRequest {
    pub context_id: WireContextId,
    pub new_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminMoveMemoryRequest {
    pub memory_id: WireMemoryId,
    pub new_context_id: WireContextId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminReclassifyRequest {
    pub memory_id: WireMemoryId,
    pub new_kind: MemoryKindWire,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminListTombstonedRequest {
    pub context_id: Option<WireContextId>,
    pub max_age_seconds: u32,
    pub limit: u32,
}

/// `EXTRACT_BACKFILL` admin op — re-enqueue existing memories for the
/// three-tier extractor pipeline. Used after enabling the extractor
/// worker on an already-populated shard or after a fresh schema upload.
///
/// The selector chooses what to enqueue; the handler iterates the
/// `memories` redb table (filtered by selector), reads the matching
/// `texts` row, and pushes `(memory_id, text)` onto the per-shard
/// ExtractorWorker channel via `WriterHandle::enqueue_for_extraction`.
/// Already-extracted memories are still re-enqueued — the worker's own
/// `skip_already_extracted` audit probe deduplicates downstream.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractBackfillRequest {
    pub selector: BackfillSelector,
}

/// Which memories to re-extract.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BackfillSelector {
    /// Enqueue a single memory by id. Errors if the row doesn't exist
    /// on the targeted shard.
    Memory(WireMemoryId),
    /// Enqueue every active memory with `created_at_unix_nanos >=
    /// since_unix_nanos`. Pass `0` to mean "from the beginning".
    Since { since_unix_nanos: u64 },
    /// Enqueue every active memory in the shard.
    All,
}

/// Ack for [`ExtractBackfillRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractBackfillResponse {
    /// Memories the handler successfully pushed onto the queue.
    pub enqueued: u64,
    /// Memories that were considered but skipped — channel full,
    /// missing text, tombstoned, or (for `Memory(id)`) not found.
    pub skipped: u64,
}

// ============================================================
// ADMIN_BACKFILL / ADMIN_BACKFILL_CANCEL — operator control
// surface for the per-shard backfill worker.
//
// The backfill worker (brain-workers) walks a
// `(memory_range × extractor_ids)` grid, re-running extractors
// against each memory and persisting per-pair checkpoints so
// restarts resume mid-run. These opcodes let operators:
//   - submit a new backfill run (returns a BackfillId);
//   - cancel an in-flight run by id (returns a final progress
//     snapshot).
//
// v1 ships fire-and-forget. The response carries the BackfillId
// plus an initial `BackfillProgress` snapshot; callers poll for
// detailed progress via `ADMIN_STATS` (or a future
// `ADMIN_BACKFILL_PROGRESS` opcode). Streaming progress mirrors
// `ADMIN_MIGRATE_EMBEDDINGS` and is deliberately deferred — the
// worker doesn't expose a streaming channel today and adding one
// is out of scope for the wire-allocation pass.
// ============================================================

/// Which memories a backfill request covers. Wire mirror of
/// `brain_core::BackfillRange`; structured as an enum so future
/// scope variants (e.g. namespace, schema-version) slot in
/// without reshaping callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BackfillScope {
    /// Every memory in the shard's metadata table.
    All,
    /// Inclusive memory-id range, walked in sorted order.
    /// `start <= end_inclusive`.
    MemoryRange {
        start: WireMemoryId,
        end_inclusive: WireMemoryId,
    },
}

/// Submit a backfill run. The worker enqueues the request and
/// returns immediately with a `BackfillId`; operators cancel
/// via [`AdminBackfillCancelRequest`] and poll progress out of
/// band.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminBackfillRequest {
    /// Which memories to walk.
    pub scope: BackfillScope,
    /// Extractor ids to re-run against each memory in scope.
    /// Capped at 4 by the worker — backfill against more
    /// extractors is a fresh request.
    pub extractor_ids: Vec<u32>,
    /// `true` = walk the plan + mark items completed without
    /// invoking the extractor pipeline. Used to preview cost.
    pub dry_run: bool,
    /// Idempotency key. Re-submitting the same `request_id` with
    /// matching params returns the cached response (per the
    /// standard 24h-TTL idempotency rule).
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// Ack for [`AdminBackfillRequest`]. Carries the assigned
/// `BackfillId` (used by [`AdminBackfillCancelRequest`]) plus
/// the worker's progress snapshot at enqueue time (which is
/// the idle-state snapshot if the worker isn't running yet, or
/// the live-run snapshot if a previous run is still in flight).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminBackfillResponse {
    /// Worker-assigned id for this run. Pass to
    /// [`AdminBackfillCancelRequest::backfill_id`] to cancel.
    /// Wire-shape: 16 bytes, identical to the `BackfillId` UUID.
    #[serde(with = "serde_bytes")]
    pub backfill_id: [u8; 16],
    /// Snapshot of the worker's progress at submission time.
    pub progress: BackfillProgress,
}

/// Cancel an in-flight backfill run by its id. Cancellation
/// flips a per-run flag the worker checks between items; the
/// run finalises at the next item boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminBackfillCancelRequest {
    /// The id returned by [`AdminBackfillResponse::backfill_id`].
    #[serde(with = "serde_bytes")]
    pub backfill_id: [u8; 16],
    /// Idempotency key for the cancel itself.
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// Ack for [`AdminBackfillCancelRequest`]. `cancelled` is
/// `true` iff a matching in-flight run was found; `false`
/// means no such run is active (already finished, never
/// submitted, or already cancelled). `progress` is the final
/// snapshot the worker published for the targeted run, or a
/// default-idle snapshot when no run matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminBackfillCancelResponse {
    #[serde(with = "serde_bytes")]
    pub backfill_id: [u8; 16],
    pub cancelled: bool,
    pub progress: BackfillProgress,
}

/// Wire mirror of `brain_core::BackfillProgress`. Plain
/// wire fields; the `Option`s are flattened to
/// `(bool, value)` so callers don't pay for an extra
/// `Option` wrapper on the hot path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackfillProgress {
    /// `true` iff the worker is mid-run on the targeted request.
    pub running: bool,
    /// Items the worker has marked completed during this run.
    pub completed: u64,
    /// Items that hit the worker's per-item attempt cap and
    /// were marked failed.
    pub failed: u64,
    /// Items whose checkpoint was already `Completed` from a
    /// prior run (resume path).
    pub skipped_already_completed: u64,
    /// `last_processed_memory_id` is `Some` once the worker
    /// has advanced past one item; flattened to
    /// `(has_value, value)` for wire compactness.
    pub last_processed_memory_id_present: bool,
    pub last_processed_memory_id: WireMemoryId,
}

impl BackfillProgress {
    /// Default idle snapshot. Returned when the targeted run
    /// has no published progress yet.
    #[must_use]
    pub const fn idle() -> Self {
        Self {
            running: false,
            completed: 0,
            failed: 0,
            skipped_already_completed: 0,
            last_processed_memory_id_present: false,
            last_processed_memory_id: 0,
        }
    }
}

// ============================================================
// Response payloads
// ============================================================

use crate::shared::enums::{IntegrityIssueType, MigrationStatus};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AdminStatsResponse {
    pub summary: StatsSummary,
    pub per_shard: Option<Vec<ShardStats>>,
    pub per_context: Option<Vec<ContextStats>>,
    pub server_uptime_seconds: u64,
    pub server_version: String,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatsSummary {
    pub total_memories: u64,
    pub total_active_memories: u64,
    pub total_tombstoned_memories: u64,
    pub total_contexts: u32,
    pub encode_qps: f32,
    pub recall_qps: f32,
    pub p99_encode_latency_ms: f32,
    pub p99_recall_latency_ms: f32,
    pub resident_memory_bytes: u64,
    pub disk_used_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShardStats {
    pub shard_id: u16,
    pub memory_count: u64,
    pub salience_distribution: SalienceHistogram,
    pub wal_segment_count: u32,
    pub last_checkpoint_lsn: u64,
    pub arena_used_bytes: u64,
}

/// — fixed 10-bucket histogram.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SalienceHistogram {
    pub buckets: [u32; 10],
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextStats {
    pub context_id: WireContextId,
    pub name: String,
    pub memory_count: u64,
    pub last_encoded_at_unix_nanos: u64,
    pub last_recalled_at_unix_nanos: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminSnapshotResponse {
    #[serde(with = "serde_bytes")]
    pub snapshot_id: [u8; 16],
    pub snapshot_name: String,
    pub snapshot_path: String,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
    pub bytes_written: u64,
    pub used_reflink: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminRestoreResponse {
    pub snapshot_name: String,
    pub shards_restored: Vec<u8>,
    pub completed_at_unix_nanos: u64,
    pub memories_restored: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminIntegrityCheckResponse {
    pub scope: crate::envelope::request::CheckScope,
    pub issues_found: Vec<IntegrityIssue>,
    pub issues_repaired: u32,
    pub completed_at_unix_nanos: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IntegrityIssue {
    pub issue_type: IntegrityIssueType,
    pub affected_memory_id: Option<WireMemoryId>,
    pub affected_shard_id: Option<u16>,
    pub description: String,
    pub repaired: bool,
}

/// — one streaming migration frame.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AdminMigrateEmbeddingsResponseFrame {
    pub is_final: bool,
    pub progress: MigrationProgress,
    pub status: Option<MigrationStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MigrationProgress {
    pub total_memories: u64,
    pub migrated_so_far: u64,
    pub failed_so_far: u64,
    pub current_qps: f32,
    pub estimated_remaining_seconds: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminCreateContextResponse {
    pub context_id: WireContextId,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminRenameContextResponse {
    pub context_id: WireContextId,
    pub new_name: String,
    pub old_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminMoveMemoryResponse {
    pub memory_id: WireMemoryId,
    pub new_context_id: WireContextId,
    pub old_context_id: WireContextId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminReclassifyResponse {
    pub memory_id: WireMemoryId,
    pub new_kind: MemoryKindWire,
    pub old_kind: MemoryKindWire,
}

/// — one streaming tombstoned-list frame.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdminListTombstonedResponseFrame {
    pub memory: TombstonedMemoryInfo,
    pub is_final: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TombstonedMemoryInfo {
    pub memory_id: WireMemoryId,
    pub text: String,
    pub forgot_at_unix_nanos: u64,
    pub forget_mode: ForgetMode,
    pub age_seconds: u32,
    pub eligible_for_reclaim: bool,
}
