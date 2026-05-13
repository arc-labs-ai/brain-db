//! Admin-surface responses (spec §07/15 – §07/25).

use rkyv::{Archive, Deserialize, Serialize};

use super::types::{IntegrityIssueType, MigrationStatus};
use crate::request::{ForgetMode, MemoryKindWire, WireContextId, WireMemoryId};

/// Spec §08 §15.
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

/// Spec §08 §15.
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

/// Spec §08 §15.
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

/// Spec §08 §15 — fixed 10-bucket histogram.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SalienceHistogram {
    pub buckets: [u32; 10],
}

/// Spec §08 §15.
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

/// Spec §08 §16.
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

/// Spec §08 §17.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminRestoreResponse {
    pub snapshot_name: String,
    pub shards_restored: Vec<u8>,
    pub completed_at_unix_nanos: u64,
    pub memories_restored: u64,
}

/// Spec §08 §18.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminIntegrityCheckResponse {
    pub scope: crate::request::CheckScope,
    pub issues_found: Vec<IntegrityIssue>,
    pub issues_repaired: u32,
    pub completed_at_unix_nanos: u64,
}

/// Spec §08 §18.
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

/// Spec §08 §19 — one streaming migration frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminMigrateEmbeddingsResponseFrame {
    pub is_final: bool,
    pub progress: MigrationProgress,
    pub status: Option<MigrationStatus>,
}

/// Spec §08 §19.
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

/// Spec §08 §20.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminCreateContextResponse {
    pub context_id: WireContextId,
    pub name: String,
}

/// Spec §08 §21.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminRenameContextResponse {
    pub context_id: WireContextId,
    pub new_name: String,
    pub old_name: String,
}

/// Spec §08 §22.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminMoveMemoryResponse {
    pub memory_id: WireMemoryId,
    pub new_context_id: WireContextId,
    pub old_context_id: WireContextId,
}

/// Spec §08 §23.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminReclassifyResponse {
    pub memory_id: WireMemoryId,
    pub new_kind: MemoryKindWire,
    pub old_kind: MemoryKindWire,
}

/// Spec §08 §24 — one streaming tombstoned-list frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminListTombstonedResponseFrame {
    pub memory: TombstonedMemoryInfo,
    pub is_final: bool,
}

/// Spec §08 §24.
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
