//! Admin-surface requests.

use rkyv::{Archive, Deserialize, Serialize};

use crate::shared::primitives::{CheckScope, ForgetMode, MemoryKindWire, StatsDetail};
use crate::envelope::request::{WireContextId, WireMemoryId, WireUuid};

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminStatsRequest {
    pub detail: StatsDetail,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminSnapshotRequest {
    pub snapshot_name: String,
    pub target_path: Option<String>,
    pub include_wal: bool,
    pub request_id: WireUuid,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminRestoreRequest {
    pub snapshot_name: String,
    pub target_shard: Option<u8>,
    pub request_id: WireUuid,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminIntegrityCheckRequest {
    pub scope: CheckScope,
    pub repair_if_possible: bool,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminMigrateEmbeddingsRequest {
    pub target_model: ModelIdentifier,
    pub batch_size: u32,
    pub rate_limit_qps: u32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ModelIdentifier {
    pub name: String,
    pub fingerprint: [u8; 16],
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminCreateContextRequest {
    pub name: String,
    pub description: String,
    pub request_id: WireUuid,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminRenameContextRequest {
    pub context_id: WireContextId,
    pub new_name: String,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminMoveMemoryRequest {
    pub memory_id: WireMemoryId,
    pub new_context_id: WireContextId,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminReclassifyRequest {
    pub memory_id: WireMemoryId,
    pub new_kind: MemoryKindWire,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
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
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ExtractBackfillRequest {
    pub selector: BackfillSelector,
}

/// Which memories to re-extract.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
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
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ExtractBackfillResponse {
    /// Memories the handler successfully pushed onto the queue.
    pub enqueued: u64,
    /// Memories that were considered but skipped — channel full,
    /// missing text, tombstoned, or (for `Memory(id)`) not found.
    pub skipped: u64,
}

// ============================================================
// Response payloads
// ============================================================


use crate::shared::enums::{IntegrityIssueType, MigrationStatus};

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminStatsResponse {
    pub summary: StatsSummary,
    pub per_shard: Option<Vec<ShardStats>>,
    pub per_context: Option<Vec<ContextStats>>,
    pub server_uptime_seconds: u64,
    pub server_version: String,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
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

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ShardStats {
    pub shard_id: u16,
    pub memory_count: u64,
    pub salience_distribution: SalienceHistogram,
    pub wal_segment_count: u32,
    pub last_checkpoint_lsn: u64,
    pub arena_used_bytes: u64,
}

/// — fixed 10-bucket histogram.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SalienceHistogram {
    pub buckets: [u32; 10],
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ContextStats {
    pub context_id: WireContextId,
    pub name: String,
    pub memory_count: u64,
    pub last_encoded_at_unix_nanos: u64,
    pub last_recalled_at_unix_nanos: u64,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminSnapshotResponse {
    pub snapshot_id: [u8; 16],
    pub snapshot_name: String,
    pub snapshot_path: String,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
    pub bytes_written: u64,
    pub used_reflink: bool,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminRestoreResponse {
    pub snapshot_name: String,
    pub shards_restored: Vec<u8>,
    pub completed_at_unix_nanos: u64,
    pub memories_restored: u64,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminIntegrityCheckResponse {
    pub scope: crate::envelope::request::CheckScope,
    pub issues_found: Vec<IntegrityIssue>,
    pub issues_repaired: u32,
    pub completed_at_unix_nanos: u64,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct IntegrityIssue {
    pub issue_type: IntegrityIssueType,
    pub affected_memory_id: Option<WireMemoryId>,
    pub affected_shard_id: Option<u16>,
    pub description: String,
    pub repaired: bool,
}

/// — one streaming migration frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminMigrateEmbeddingsResponseFrame {
    pub is_final: bool,
    pub progress: MigrationProgress,
    pub status: Option<MigrationStatus>,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct MigrationProgress {
    pub total_memories: u64,
    pub migrated_so_far: u64,
    pub failed_so_far: u64,
    pub current_qps: f32,
    pub estimated_remaining_seconds: u32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminCreateContextResponse {
    pub context_id: WireContextId,
    pub name: String,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminRenameContextResponse {
    pub context_id: WireContextId,
    pub new_name: String,
    pub old_name: String,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminMoveMemoryResponse {
    pub memory_id: WireMemoryId,
    pub new_context_id: WireContextId,
    pub old_context_id: WireContextId,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminReclassifyResponse {
    pub memory_id: WireMemoryId,
    pub new_kind: MemoryKindWire,
    pub old_kind: MemoryKindWire,
}

/// — one streaming tombstoned-list frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminListTombstonedResponseFrame {
    pub memory: TombstonedMemoryInfo,
    pub is_final: bool,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct TombstonedMemoryInfo {
    pub memory_id: WireMemoryId,
    pub text: String,
    pub forgot_at_unix_nanos: u64,
    pub forget_mode: ForgetMode,
    pub age_seconds: u32,
    pub eligible_for_reclaim: bool,
}
