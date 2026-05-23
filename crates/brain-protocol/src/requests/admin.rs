//! Admin-surface requests.

use rkyv::{Archive, Deserialize, Serialize};

use super::types::{CheckScope, MemoryKindWire, StatsDetail};
use crate::request::{WireContextId, WireMemoryId, WireUuid};

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
