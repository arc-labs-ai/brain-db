//! Admin-surface requests (spec §07/15 – §07/25).

use rkyv::{Archive, Deserialize, Serialize};

use super::types::{CheckScope, MemoryKindWire, StatsDetail};
use crate::request::{WireContextId, WireMemoryId, WireUuid};

/// Spec §07/16.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminStatsRequest {
    pub detail: StatsDetail,
}

/// Spec §07/17.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminSnapshotRequest {
    pub snapshot_name: String,
    pub target_path: Option<String>,
    pub include_wal: bool,
    pub request_id: WireUuid,
}

/// Spec §07/18.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminRestoreRequest {
    pub snapshot_name: String,
    pub target_shard: Option<u8>,
    pub request_id: WireUuid,
}

/// Spec §07/19.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminIntegrityCheckRequest {
    pub scope: CheckScope,
    pub repair_if_possible: bool,
}

/// Spec §07/20.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminMigrateEmbeddingsRequest {
    pub target_model: ModelIdentifier,
    pub batch_size: u32,
    pub rate_limit_qps: u32,
}

/// Spec §07/20.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ModelIdentifier {
    pub name: String,
    pub fingerprint: [u8; 16],
}

/// Spec §07/21.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminCreateContextRequest {
    pub name: String,
    pub description: String,
    pub request_id: WireUuid,
}

/// Spec §07/22.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminRenameContextRequest {
    pub context_id: WireContextId,
    pub new_name: String,
}

/// Spec §07/23.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminMoveMemoryRequest {
    pub memory_id: WireMemoryId,
    pub new_context_id: WireContextId,
}

/// Spec §07/24.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminReclassifyRequest {
    pub memory_id: WireMemoryId,
    pub new_kind: MemoryKindWire,
}

/// Spec §07/25.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct AdminListTombstonedRequest {
    pub context_id: Option<WireContextId>,
    pub max_age_seconds: u32,
    pub limit: u32,
}
