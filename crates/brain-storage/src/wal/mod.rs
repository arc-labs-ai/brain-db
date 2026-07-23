//! Write-Ahead Log: per-shard, append-only, fsync-coordinated.
//!
//! This module exposes the record-level framing, segment
//! writer/reader, and recovery.

pub mod checkpoint;
pub mod group_commit;
pub mod kinds;
pub mod payload;
pub mod reader;
pub mod record;
pub mod segment;
#[allow(clippy::module_inception)]
pub mod wal;

pub use group_commit::{AppendHandle, CommitError, GroupCommitConfig, GroupCommitter};
pub use kinds::{WalRecordKind, ALL_KINDS};
pub use payload::{
    CheckpointBeginPayload, CheckpointEndPayload, ConsolidatePayload, EdgePayload,
    EmbeddingModelFp, EncodePayload, ForgetMode, ForgetPayload, ForgetReason, LinkPayload,
    MigrateEmbeddingPayload, ReclaimPayload, RelationLinkPayload, RelationSupersedePayload,
    RelationTombstonePayload, SalienceReason, SalienceUpdate, TxnAbortPayload, TxnBeginPayload,
    TxnCommitPayload, UnlinkPayload, UpdateSessionPayload, UpdateKindPayload,
    UpdateSaliencePayload, WalPayload, WalPayloadError, VECTOR_DIMS_MAX,
};
pub use reader::{SegmentInfo, WalReadError, WalReader};
pub use record::{
    DecodeOutcome, Lsn, WalRecord, WalRecordError, FOOTER_LEN, HEADER_LEN, MAX_PAYLOAD,
};
pub use segment::{
    WalSegment, WalSegmentError, WAL_SEGMENT_FORMAT_VERSION_V1,
    WAL_SEGMENT_HEADER_CRC_COVERAGE_END, WAL_SEGMENT_HEADER_LEN, WAL_SEGMENT_MAGIC,
};
pub use wal::{Wal, WalConfig, WalError};
