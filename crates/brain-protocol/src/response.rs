//! Response-frame payload codecs.
//!
//! One variant of [`ResponseBody`] per client-bound opcode in spec §03/08.
//! Mirrors `crate::request` exactly: rkyv-archivable structs for the
//! structured fields, raw vector blobs (where applicable) appended at
//! the [`crate::Frame`] layer.
//!
//! ## Streaming
//!
//! Several opcodes stream multiple response frames over a single stream
//! (`RECALL_RESP`, `PLAN_RESP`, `REASON_RESP`, `SUBSCRIBE_EVENT`,
//! `ADMIN_MIGRATE_EMBEDDINGS_RESP`, `ADMIN_LIST_TOMBSTONED_RESP`). Each
//! emitted frame is one variant payload; the *last* frame of a stream
//! sets the header's `EOS` flag and (per spec §08 §3) the body's
//! `is_final = true`. [`ResponseBody::is_final`] surfaces the body-side
//! signal so a Frame-layer dispatcher can cross-check against the
//! header (Phase 9).
//!
//! ## ERROR-frame mirror enums
//!
//! Spec §08 §25 ties the ERROR body to `ErrorCode` / `ErrorCategory`
//! from §10. Those enums live in [`crate::error`] and are intentionally
//! `#[non_exhaustive]` for forward-compat. We mirror them here as
//! plain rkyv-archivable enums so wire encoding/decoding is closed,
//! and convert at the boundary via `From` impls.

use rkyv::{Archive, Deserialize, Serialize};

use crate::error::{ErrorCategory, ErrorCode, ProtocolError};
use crate::handshake::{AuthOkPayload, WelcomePayload};
use crate::opcode::Opcode;
use crate::request::{
    EdgeKindWire, ForgetMode, MemoryKindWire, WireContextId, WireMemoryId, WireUuid,
};
use crate::rkyv_codec::{from_rkyv_bytes, to_rkyv_bytes};

// ---------------------------------------------------------------------------
// Helper enums shared by multiple response bodies (spec §08).
// ---------------------------------------------------------------------------

/// Spec §08 §4 — `PlanResponseFrame::TransitionKind`.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum TransitionKind {
    Initial,
    Causal,
    Temporal,
    Similarity,
    Other(String),
}

/// Spec §08 §4 — `PlanResponseFrame::PlanStatus` (set on the final frame
/// only).
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum PlanStatus {
    GoalReached = 0,
    BudgetExhausted = 1,
    NoPathFound = 2,
    Cancelled = 3,
}

/// Spec §08 §5 — `ReasonResponseFrame::InferenceKind`.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum InferenceKind {
    CausalExplanation,
    EvidenceAccumulation,
    AnalogicalInference,
    Other(String),
}

/// Spec §08 §5 — `ReasonResponseFrame::ReasonStatus`.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum ReasonStatus {
    Complete = 0,
    BudgetExhausted = 1,
    DepthLimitReached = 2,
    Cancelled = 3,
}

/// Spec §08 §7 — `SubscriptionEvent::EventType`.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum EventType {
    Encoded = 0,
    Forgotten = 1,
    Reclaimed = 2,
    KindChanged = 3,
}

/// Spec §08 §18 — `IntegrityIssue::IntegrityIssueType`.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum IntegrityIssueType {
    VectorCorruption,
    TextCorruption,
    StaleEdge,
    OrphanIndex,
    SchemaVersionMismatch,
    Other(String),
}

/// Spec §08 §19 — `AdminMigrateEmbeddingsResponseFrame::MigrationStatus`.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum MigrationStatus {
    InProgress,
    Completed,
    Failed(String),
    Cancelled,
}

// ---------------------------------------------------------------------------
// ERROR-frame mirror enums (closed; convert from the open `error::*` enums).
// ---------------------------------------------------------------------------

/// rkyv-archivable mirror of [`crate::error::ErrorCategory`]. The
/// canonical type is intentionally `#[non_exhaustive]` for forward-
/// compatibility, which is incompatible with rkyv's closed-world derive.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum ErrorCategoryWire {
    Protocol = 0,
    Authentication = 1,
    Authorization = 2,
    Validation = 3,
    NotFound = 4,
    Conflict = 5,
    ResourceExhausted = 6,
    Internal = 7,
    Unavailable = 8,
}

impl From<ErrorCategory> for ErrorCategoryWire {
    fn from(c: ErrorCategory) -> Self {
        match c {
            ErrorCategory::Protocol => Self::Protocol,
            ErrorCategory::Authentication => Self::Authentication,
            ErrorCategory::Authorization => Self::Authorization,
            ErrorCategory::Validation => Self::Validation,
            ErrorCategory::NotFound => Self::NotFound,
            ErrorCategory::Conflict => Self::Conflict,
            ErrorCategory::ResourceExhausted => Self::ResourceExhausted,
            ErrorCategory::Internal => Self::Internal,
            ErrorCategory::Unavailable => Self::Unavailable,
        }
    }
}

impl From<ErrorCategoryWire> for ErrorCategory {
    fn from(c: ErrorCategoryWire) -> Self {
        match c {
            ErrorCategoryWire::Protocol => Self::Protocol,
            ErrorCategoryWire::Authentication => Self::Authentication,
            ErrorCategoryWire::Authorization => Self::Authorization,
            ErrorCategoryWire::Validation => Self::Validation,
            ErrorCategoryWire::NotFound => Self::NotFound,
            ErrorCategoryWire::Conflict => Self::Conflict,
            ErrorCategoryWire::ResourceExhausted => Self::ResourceExhausted,
            ErrorCategoryWire::Internal => Self::Internal,
            ErrorCategoryWire::Unavailable => Self::Unavailable,
        }
    }
}

/// rkyv-archivable mirror of [`crate::error::ErrorCode`]. Variants are in
/// the same order as the spec table (§10 §3.1–§3.9). Numeric repr is
/// stable; this is the *wire* representation.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u16)]
pub enum ErrorCodeWire {
    // §3.1 Protocol
    BadMagic = 0x0001,
    BadHeaderCrc = 0x0002,
    BadPayloadCrc = 0x0003,
    BadOpcode = 0x0004,
    BadVersion = 0x0005,
    BadFrame = 0x0006,
    OversizePayload = 0x0007,
    ReservedFieldNonZero = 0x0008,
    BadFlagCombination = 0x0009,
    MalformedRkyv = 0x000A,
    MalformedVector = 0x000B,
    // §3.2 Connection / handshake
    VersionNotSupported = 0x0020,
    NoSuchAuthMethod = 0x0021,
    Unauthenticated = 0x0022,
    NotAuthenticated = 0x0023,
    AuthBackendUnavailable = 0x0024,
    SessionExpired = 0x0025,
    // §3.3 Authorization
    PermissionDenied = 0x0030,
    AdminPermissionRequired = 0x0031,
    WrongShard = 0x0032,
    // §3.4 Validation
    InvalidArgument = 0x0040,
    MissingRequiredField = 0x0041,
    TextTooLarge = 0x0042,
    TextEmpty = 0x0043,
    BadContextId = 0x0044,
    BadMemoryKind = 0x0045,
    BadEdgeKind = 0x0046,
    BadStrategyHint = 0x0047,
    TopKOutOfRange = 0x0048,
    BudgetTooLarge = 0x0049,
    BadModelFingerprint = 0x004A,
    // §3.5 Not found
    MemoryNotFound = 0x0050,
    ContextNotFound = 0x0051,
    SubscriptionNotFound = 0x0052,
    SnapshotNotFound = 0x0053,
    TxnNotFound = 0x0054,
    // §3.6 Conflict
    IdempotencyConflict = 0x0060,
    TransactionConflict = 0x0061,
    TransactionTimeout = 0x0062,
    StreamIdInUse = 0x0063,
    SubscriptionLsnTooOld = 0x0064,
    // §3.7 Resource exhausted
    OutOfSlots = 0x0070,
    OutOfDisk = 0x0071,
    OutOfMemory = 0x0072,
    RateLimited = 0x0073,
    StreamLimitExceeded = 0x0074,
    ConnectionLimitExceeded = 0x0075,
    TransactionLimitExceeded = 0x0076,
    // §3.8 Internal
    Internal = 0x0080,
    StorageError = 0x0081,
    IndexError = 0x0082,
    EmbeddingError = 0x0083,
    MetadataError = 0x0084,
    // §3.9 Unavailable
    ShardUnavailable = 0x0090,
    Overloaded = 0x0091,
    Restarting = 0x0092,
    Maintenance = 0x0093,
}

impl From<ErrorCode> for ErrorCodeWire {
    #[allow(clippy::too_many_lines)]
    fn from(c: ErrorCode) -> Self {
        match c {
            ErrorCode::BadMagic => Self::BadMagic,
            ErrorCode::BadHeaderCrc => Self::BadHeaderCrc,
            ErrorCode::BadPayloadCrc => Self::BadPayloadCrc,
            ErrorCode::BadOpcode => Self::BadOpcode,
            ErrorCode::BadVersion => Self::BadVersion,
            ErrorCode::BadFrame => Self::BadFrame,
            ErrorCode::OversizePayload => Self::OversizePayload,
            ErrorCode::ReservedFieldNonZero => Self::ReservedFieldNonZero,
            ErrorCode::BadFlagCombination => Self::BadFlagCombination,
            ErrorCode::MalformedRkyv => Self::MalformedRkyv,
            ErrorCode::MalformedVector => Self::MalformedVector,
            ErrorCode::VersionNotSupported => Self::VersionNotSupported,
            ErrorCode::NoSuchAuthMethod => Self::NoSuchAuthMethod,
            ErrorCode::Unauthenticated => Self::Unauthenticated,
            ErrorCode::NotAuthenticated => Self::NotAuthenticated,
            ErrorCode::AuthBackendUnavailable => Self::AuthBackendUnavailable,
            ErrorCode::SessionExpired => Self::SessionExpired,
            ErrorCode::PermissionDenied => Self::PermissionDenied,
            ErrorCode::AdminPermissionRequired => Self::AdminPermissionRequired,
            ErrorCode::WrongShard => Self::WrongShard,
            ErrorCode::InvalidArgument => Self::InvalidArgument,
            ErrorCode::MissingRequiredField => Self::MissingRequiredField,
            ErrorCode::TextTooLarge => Self::TextTooLarge,
            ErrorCode::TextEmpty => Self::TextEmpty,
            ErrorCode::BadContextId => Self::BadContextId,
            ErrorCode::BadMemoryKind => Self::BadMemoryKind,
            ErrorCode::BadEdgeKind => Self::BadEdgeKind,
            ErrorCode::BadStrategyHint => Self::BadStrategyHint,
            ErrorCode::TopKOutOfRange => Self::TopKOutOfRange,
            ErrorCode::BudgetTooLarge => Self::BudgetTooLarge,
            ErrorCode::BadModelFingerprint => Self::BadModelFingerprint,
            ErrorCode::MemoryNotFound => Self::MemoryNotFound,
            ErrorCode::ContextNotFound => Self::ContextNotFound,
            ErrorCode::SubscriptionNotFound => Self::SubscriptionNotFound,
            ErrorCode::SnapshotNotFound => Self::SnapshotNotFound,
            ErrorCode::TxnNotFound => Self::TxnNotFound,
            ErrorCode::IdempotencyConflict => Self::IdempotencyConflict,
            ErrorCode::TransactionConflict => Self::TransactionConflict,
            ErrorCode::TransactionTimeout => Self::TransactionTimeout,
            ErrorCode::StreamIdInUse => Self::StreamIdInUse,
            ErrorCode::SubscriptionLsnTooOld => Self::SubscriptionLsnTooOld,
            ErrorCode::OutOfSlots => Self::OutOfSlots,
            ErrorCode::OutOfDisk => Self::OutOfDisk,
            ErrorCode::OutOfMemory => Self::OutOfMemory,
            ErrorCode::RateLimited => Self::RateLimited,
            ErrorCode::StreamLimitExceeded => Self::StreamLimitExceeded,
            ErrorCode::ConnectionLimitExceeded => Self::ConnectionLimitExceeded,
            ErrorCode::TransactionLimitExceeded => Self::TransactionLimitExceeded,
            ErrorCode::Internal => Self::Internal,
            ErrorCode::StorageError => Self::StorageError,
            ErrorCode::IndexError => Self::IndexError,
            ErrorCode::EmbeddingError => Self::EmbeddingError,
            ErrorCode::MetadataError => Self::MetadataError,
            ErrorCode::ShardUnavailable => Self::ShardUnavailable,
            ErrorCode::Overloaded => Self::Overloaded,
            ErrorCode::Restarting => Self::Restarting,
            ErrorCode::Maintenance => Self::Maintenance,
        }
    }
}

impl From<ErrorCodeWire> for ErrorCode {
    #[allow(clippy::too_many_lines)]
    fn from(c: ErrorCodeWire) -> Self {
        match c {
            ErrorCodeWire::BadMagic => Self::BadMagic,
            ErrorCodeWire::BadHeaderCrc => Self::BadHeaderCrc,
            ErrorCodeWire::BadPayloadCrc => Self::BadPayloadCrc,
            ErrorCodeWire::BadOpcode => Self::BadOpcode,
            ErrorCodeWire::BadVersion => Self::BadVersion,
            ErrorCodeWire::BadFrame => Self::BadFrame,
            ErrorCodeWire::OversizePayload => Self::OversizePayload,
            ErrorCodeWire::ReservedFieldNonZero => Self::ReservedFieldNonZero,
            ErrorCodeWire::BadFlagCombination => Self::BadFlagCombination,
            ErrorCodeWire::MalformedRkyv => Self::MalformedRkyv,
            ErrorCodeWire::MalformedVector => Self::MalformedVector,
            ErrorCodeWire::VersionNotSupported => Self::VersionNotSupported,
            ErrorCodeWire::NoSuchAuthMethod => Self::NoSuchAuthMethod,
            ErrorCodeWire::Unauthenticated => Self::Unauthenticated,
            ErrorCodeWire::NotAuthenticated => Self::NotAuthenticated,
            ErrorCodeWire::AuthBackendUnavailable => Self::AuthBackendUnavailable,
            ErrorCodeWire::SessionExpired => Self::SessionExpired,
            ErrorCodeWire::PermissionDenied => Self::PermissionDenied,
            ErrorCodeWire::AdminPermissionRequired => Self::AdminPermissionRequired,
            ErrorCodeWire::WrongShard => Self::WrongShard,
            ErrorCodeWire::InvalidArgument => Self::InvalidArgument,
            ErrorCodeWire::MissingRequiredField => Self::MissingRequiredField,
            ErrorCodeWire::TextTooLarge => Self::TextTooLarge,
            ErrorCodeWire::TextEmpty => Self::TextEmpty,
            ErrorCodeWire::BadContextId => Self::BadContextId,
            ErrorCodeWire::BadMemoryKind => Self::BadMemoryKind,
            ErrorCodeWire::BadEdgeKind => Self::BadEdgeKind,
            ErrorCodeWire::BadStrategyHint => Self::BadStrategyHint,
            ErrorCodeWire::TopKOutOfRange => Self::TopKOutOfRange,
            ErrorCodeWire::BudgetTooLarge => Self::BudgetTooLarge,
            ErrorCodeWire::BadModelFingerprint => Self::BadModelFingerprint,
            ErrorCodeWire::MemoryNotFound => Self::MemoryNotFound,
            ErrorCodeWire::ContextNotFound => Self::ContextNotFound,
            ErrorCodeWire::SubscriptionNotFound => Self::SubscriptionNotFound,
            ErrorCodeWire::SnapshotNotFound => Self::SnapshotNotFound,
            ErrorCodeWire::TxnNotFound => Self::TxnNotFound,
            ErrorCodeWire::IdempotencyConflict => Self::IdempotencyConflict,
            ErrorCodeWire::TransactionConflict => Self::TransactionConflict,
            ErrorCodeWire::TransactionTimeout => Self::TransactionTimeout,
            ErrorCodeWire::StreamIdInUse => Self::StreamIdInUse,
            ErrorCodeWire::SubscriptionLsnTooOld => Self::SubscriptionLsnTooOld,
            ErrorCodeWire::OutOfSlots => Self::OutOfSlots,
            ErrorCodeWire::OutOfDisk => Self::OutOfDisk,
            ErrorCodeWire::OutOfMemory => Self::OutOfMemory,
            ErrorCodeWire::RateLimited => Self::RateLimited,
            ErrorCodeWire::StreamLimitExceeded => Self::StreamLimitExceeded,
            ErrorCodeWire::ConnectionLimitExceeded => Self::ConnectionLimitExceeded,
            ErrorCodeWire::TransactionLimitExceeded => Self::TransactionLimitExceeded,
            ErrorCodeWire::Internal => Self::Internal,
            ErrorCodeWire::StorageError => Self::StorageError,
            ErrorCodeWire::IndexError => Self::IndexError,
            ErrorCodeWire::EmbeddingError => Self::EmbeddingError,
            ErrorCodeWire::MetadataError => Self::MetadataError,
            ErrorCodeWire::ShardUnavailable => Self::ShardUnavailable,
            ErrorCodeWire::Overloaded => Self::Overloaded,
            ErrorCodeWire::Restarting => Self::Restarting,
            ErrorCodeWire::Maintenance => Self::Maintenance,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-opcode response structs (spec §03/08 §1–§25).
// ---------------------------------------------------------------------------

/// Spec §08 §1 `ENCODE_RESP`. Same shape used for §08 §2
/// `ENCODE_VECTOR_DIRECT_RESP`.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EncodeResponse {
    pub memory_id: WireMemoryId,
    pub was_deduplicated: bool,
    pub salience: f32,
    pub auto_edges_added: u32,
}

/// Spec §08 §3 — one streaming RECALL frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RecallResponseFrame {
    pub results: Vec<MemoryResult>,
    pub is_final: bool,
    pub cumulative_count: u32,
    pub estimated_remaining: Option<u32>,
}

/// Spec §08 §3.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct MemoryResult {
    pub memory_id: WireMemoryId,
    pub text: String,
    pub similarity_score: f32,
    pub confidence: f32,
    pub salience: f32,
    pub kind: MemoryKindWire,
    pub context_id: WireContextId,
    pub created_at_unix_nanos: u64,
    pub last_accessed_at_unix_nanos: u64,
    pub vector_offset: u32,
    pub vector_dim: u16,
    pub edges: Option<Vec<EdgeView>>,
}

/// Spec §08 §3.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EdgeView {
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    pub weight: f32,
}

/// Spec §08 §4 — one streaming PLAN frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct PlanResponseFrame {
    pub steps: Vec<PlanStep>,
    pub is_final: bool,
    pub plan_status: Option<PlanStatus>,
}

/// Spec §08 §4.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct PlanStep {
    pub step_index: u32,
    pub memory_id: WireMemoryId,
    pub text: String,
    pub transition_kind: TransitionKind,
    pub confidence: f32,
    pub estimated_distance_to_goal: f32,
}

/// Spec §08 §5 — one streaming REASON frame.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ReasonResponseFrame {
    pub inferences: Vec<InferenceStep>,
    pub is_final: bool,
    pub reason_status: Option<ReasonStatus>,
}

/// Spec §08 §5.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct InferenceStep {
    pub step_index: u32,
    pub claim: String,
    pub supporting_memories: Vec<WireMemoryId>,
    pub contradicting_memories: Vec<WireMemoryId>,
    pub confidence: f32,
    pub inference_kind: InferenceKind,
}

/// Spec §08 §6.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ForgetResponse {
    pub memory_id: WireMemoryId,
    pub was_already_forgotten: bool,
    pub edges_removed: u32,
}

/// Spec §08 §7 — push event for a subscription.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SubscriptionEvent {
    pub event_type: EventType,
    pub memory_id: WireMemoryId,
    pub context_id: WireContextId,
    pub text: String,
    pub kind: MemoryKindWire,
    pub salience: f32,
    pub timestamp_unix_nanos: u64,
    pub lsn: u64,
}

/// Spec §08 §8.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnsubscribeResponse {
    pub target_stream_id: u32,
    pub final_lsn: u64,
}

/// Spec §08 §9.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct TxnBeginResponse {
    pub txn_id: WireUuid,
    pub timeout_seconds: u32,
    pub started_at_unix_nanos: u64,
}

/// Spec §08 §10.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct TxnCommitResponse {
    pub txn_id: WireUuid,
    pub committed_at_unix_nanos: u64,
    pub operations_applied: u32,
}

/// Spec §08 §11.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct TxnAbortResponse {
    pub txn_id: WireUuid,
    pub operations_discarded: u32,
}

/// Spec §08 §12.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct CancelStreamAck {
    pub target_stream_id: u32,
    pub cancelled_at_unix_nanos: u64,
}

/// Spec §08 §13.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct PongResponse {
    pub client_timestamp_unix_nanos: u64,
    pub server_timestamp_unix_nanos: u64,
}

/// Spec §08 §14 — server-initiated keepalive (despite "Request" in the
/// spec name, this is a server→client frame).
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ServerPingResponse {
    pub server_timestamp_unix_nanos: u64,
}

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

/// Spec §08 §25 — error frame body.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ErrorResponse {
    pub code: ErrorCodeWire,
    pub category: ErrorCategoryWire,
    pub message: String,
    pub details: Option<ErrorDetails>,
    pub retry_after_ms: Option<u32>,
}

/// Spec §08 §25.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ErrorDetails {
    pub field: Option<String>,
    pub expected: Option<String>,
    pub actual: Option<String>,
}

// ---------------------------------------------------------------------------
// ResponseBody dispatch enum.
// ---------------------------------------------------------------------------

/// One variant per client-bound opcode in spec §03/08. Mirrors
/// [`crate::request::RequestBody`]; raw vector blobs (where applicable)
/// live in the trailing section of [`crate::Frame::payload`] and are
/// not part of the rkyv-encoded bytes this module produces.
#[derive(Clone, Debug, PartialEq)]
pub enum ResponseBody {
    /// Spec §06 §3 — server reply to HELLO (connection-level, stream 0).
    Welcome(WelcomePayload),
    /// Spec §06 §5 — server confirmation of authentication.
    AuthOk(AuthOkPayload),
    Encode(EncodeResponse),
    EncodeVectorDirect(EncodeResponse),
    Recall(RecallResponseFrame),
    Plan(PlanResponseFrame),
    Reason(ReasonResponseFrame),
    Forget(ForgetResponse),
    SubscribeEvent(SubscriptionEvent),
    Unsubscribe(UnsubscribeResponse),
    TxnBegin(TxnBeginResponse),
    TxnCommit(TxnCommitResponse),
    TxnAbort(TxnAbortResponse),
    CancelStreamAck(CancelStreamAck),
    Pong(PongResponse),
    ServerPing(ServerPingResponse),
    AdminStats(AdminStatsResponse),
    AdminSnapshot(AdminSnapshotResponse),
    AdminRestore(AdminRestoreResponse),
    AdminIntegrityCheck(AdminIntegrityCheckResponse),
    AdminMigrateEmbeddings(AdminMigrateEmbeddingsResponseFrame),
    AdminCreateContext(AdminCreateContextResponse),
    AdminRenameContext(AdminRenameContextResponse),
    AdminMoveMemory(AdminMoveMemoryResponse),
    AdminReclassify(AdminReclassifyResponse),
    AdminListTombstoned(AdminListTombstonedResponseFrame),
    Error(ErrorResponse),
}

impl ResponseBody {
    /// The opcode this body corresponds to.
    #[must_use]
    pub fn opcode(&self) -> Opcode {
        match self {
            Self::Welcome(_) => Opcode::Welcome,
            Self::AuthOk(_) => Opcode::AuthOk,
            Self::Encode(_) => Opcode::EncodeResp,
            Self::EncodeVectorDirect(_) => Opcode::EncodeVectorDirectResp,
            Self::Recall(_) => Opcode::RecallResp,
            Self::Plan(_) => Opcode::PlanResp,
            Self::Reason(_) => Opcode::ReasonResp,
            Self::Forget(_) => Opcode::ForgetResp,
            Self::SubscribeEvent(_) => Opcode::SubscribeEvent,
            Self::Unsubscribe(_) => Opcode::UnsubscribeResp,
            Self::TxnBegin(_) => Opcode::TxnBeginResp,
            Self::TxnCommit(_) => Opcode::TxnCommitResp,
            Self::TxnAbort(_) => Opcode::TxnAbortResp,
            Self::CancelStreamAck(_) => Opcode::CancelStreamAck,
            Self::Pong(_) => Opcode::Pong,
            Self::ServerPing(_) => Opcode::ServerPing,
            Self::AdminStats(_) => Opcode::AdminStatsResp,
            Self::AdminSnapshot(_) => Opcode::AdminSnapshotResp,
            Self::AdminRestore(_) => Opcode::AdminRestoreResp,
            Self::AdminIntegrityCheck(_) => Opcode::AdminIntegrityCheckResp,
            Self::AdminMigrateEmbeddings(_) => Opcode::AdminMigrateEmbeddingsResp,
            Self::AdminCreateContext(_) => Opcode::AdminCreateContextResp,
            Self::AdminRenameContext(_) => Opcode::AdminRenameContextResp,
            Self::AdminMoveMemory(_) => Opcode::AdminMoveMemoryResp,
            Self::AdminReclassify(_) => Opcode::AdminReclassifyResp,
            Self::AdminListTombstoned(_) => Opcode::AdminListTombstonedResp,
            Self::Error(_) => Opcode::Error,
        }
    }

    /// `Some(is_final)` for streaming variants (recall / plan / reason /
    /// admin-migrate / admin-list-tombstoned). `None` for unary or
    /// open-ended variants — subscription events have no body-side
    /// `is_final` (the EOS flag in the frame header carries it).
    #[must_use]
    pub fn is_final(&self) -> Option<bool> {
        match self {
            Self::Recall(r) => Some(r.is_final),
            Self::Plan(r) => Some(r.is_final),
            Self::Reason(r) => Some(r.is_final),
            Self::AdminMigrateEmbeddings(r) => Some(r.is_final),
            Self::AdminListTombstoned(r) => Some(r.is_final),
            _ => None,
        }
    }

    /// Encode the structured body to bytes via rkyv. Vector blobs (where
    /// supported) are appended by callers at the [`crate::Frame`] layer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Welcome(r) => to_rkyv_bytes(r),
            Self::AuthOk(r) => to_rkyv_bytes(r),
            Self::Encode(r) | Self::EncodeVectorDirect(r) => to_rkyv_bytes(r),
            Self::Recall(r) => to_rkyv_bytes(r),
            Self::Plan(r) => to_rkyv_bytes(r),
            Self::Reason(r) => to_rkyv_bytes(r),
            Self::Forget(r) => to_rkyv_bytes(r),
            Self::SubscribeEvent(r) => to_rkyv_bytes(r),
            Self::Unsubscribe(r) => to_rkyv_bytes(r),
            Self::TxnBegin(r) => to_rkyv_bytes(r),
            Self::TxnCommit(r) => to_rkyv_bytes(r),
            Self::TxnAbort(r) => to_rkyv_bytes(r),
            Self::CancelStreamAck(r) => to_rkyv_bytes(r),
            Self::Pong(r) => to_rkyv_bytes(r),
            Self::ServerPing(r) => to_rkyv_bytes(r),
            Self::AdminStats(r) => to_rkyv_bytes(r),
            Self::AdminSnapshot(r) => to_rkyv_bytes(r),
            Self::AdminRestore(r) => to_rkyv_bytes(r),
            Self::AdminIntegrityCheck(r) => to_rkyv_bytes(r),
            Self::AdminMigrateEmbeddings(r) => to_rkyv_bytes(r),
            Self::AdminCreateContext(r) => to_rkyv_bytes(r),
            Self::AdminRenameContext(r) => to_rkyv_bytes(r),
            Self::AdminMoveMemory(r) => to_rkyv_bytes(r),
            Self::AdminReclassify(r) => to_rkyv_bytes(r),
            Self::AdminListTombstoned(r) => to_rkyv_bytes(r),
            Self::Error(r) => to_rkyv_bytes(r),
        }
    }

    /// Decode `bytes` as the response body for `opcode`. Returns
    /// [`ProtocolError::UnknownOpcode`] if `opcode` doesn't carry a
    /// response body (request opcodes).
    pub fn decode(opcode: Opcode, bytes: &[u8]) -> Result<Self, ProtocolError> {
        Ok(match opcode {
            Opcode::Welcome => Self::Welcome(from_rkyv_bytes(bytes)?),
            Opcode::AuthOk => Self::AuthOk(from_rkyv_bytes(bytes)?),
            Opcode::EncodeResp => Self::Encode(from_rkyv_bytes(bytes)?),
            Opcode::EncodeVectorDirectResp => Self::EncodeVectorDirect(from_rkyv_bytes(bytes)?),
            Opcode::RecallResp => Self::Recall(from_rkyv_bytes(bytes)?),
            Opcode::PlanResp => Self::Plan(from_rkyv_bytes(bytes)?),
            Opcode::ReasonResp => Self::Reason(from_rkyv_bytes(bytes)?),
            Opcode::ForgetResp => Self::Forget(from_rkyv_bytes(bytes)?),
            Opcode::SubscribeEvent => Self::SubscribeEvent(from_rkyv_bytes(bytes)?),
            Opcode::UnsubscribeResp => Self::Unsubscribe(from_rkyv_bytes(bytes)?),
            Opcode::TxnBeginResp => Self::TxnBegin(from_rkyv_bytes(bytes)?),
            Opcode::TxnCommitResp => Self::TxnCommit(from_rkyv_bytes(bytes)?),
            Opcode::TxnAbortResp => Self::TxnAbort(from_rkyv_bytes(bytes)?),
            Opcode::CancelStreamAck => Self::CancelStreamAck(from_rkyv_bytes(bytes)?),
            Opcode::Pong => Self::Pong(from_rkyv_bytes(bytes)?),
            Opcode::ServerPing => Self::ServerPing(from_rkyv_bytes(bytes)?),
            Opcode::AdminStatsResp => Self::AdminStats(from_rkyv_bytes(bytes)?),
            Opcode::AdminSnapshotResp => Self::AdminSnapshot(from_rkyv_bytes(bytes)?),
            Opcode::AdminRestoreResp => Self::AdminRestore(from_rkyv_bytes(bytes)?),
            Opcode::AdminIntegrityCheckResp => Self::AdminIntegrityCheck(from_rkyv_bytes(bytes)?),
            Opcode::AdminMigrateEmbeddingsResp => {
                Self::AdminMigrateEmbeddings(from_rkyv_bytes(bytes)?)
            }
            Opcode::AdminCreateContextResp => Self::AdminCreateContext(from_rkyv_bytes(bytes)?),
            Opcode::AdminRenameContextResp => Self::AdminRenameContext(from_rkyv_bytes(bytes)?),
            Opcode::AdminMoveMemoryResp => Self::AdminMoveMemory(from_rkyv_bytes(bytes)?),
            Opcode::AdminReclassifyResp => Self::AdminReclassify(from_rkyv_bytes(bytes)?),
            Opcode::AdminListTombstonedResp => Self::AdminListTombstoned(from_rkyv_bytes(bytes)?),
            Opcode::Error => Self::Error(from_rkyv_bytes(bytes)?),
            other => return Err(ProtocolError::UnknownOpcode(other.as_u8())),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(body: ResponseBody) {
        let bytes = body.encode();
        let decoded = ResponseBody::decode(body.opcode(), &bytes)
            .unwrap_or_else(|e| panic!("decode failed for {:?}: {e}", body.opcode()));
        assert_eq!(decoded, body);
    }

    fn sample_uuid(seed: u8) -> WireUuid {
        let mut u = [0u8; 16];
        for (i, b) in u.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        u
    }

    fn sample_memory_id() -> WireMemoryId {
        ((7u128) << 72) | ((42u128) << 56) | 0x12_3456_u128
    }

    #[test]
    fn encode_response_round_trips() {
        round_trip(ResponseBody::Encode(EncodeResponse {
            memory_id: sample_memory_id(),
            was_deduplicated: false,
            salience: 0.5,
            auto_edges_added: 3,
        }));
        round_trip(ResponseBody::EncodeVectorDirect(EncodeResponse {
            memory_id: sample_memory_id(),
            was_deduplicated: true,
            salience: 0.8,
            auto_edges_added: 0,
        }));
    }

    #[test]
    fn recall_response_round_trips() {
        round_trip(ResponseBody::Recall(RecallResponseFrame {
            results: vec![MemoryResult {
                memory_id: sample_memory_id(),
                text: "first result".into(),
                similarity_score: 0.92,
                confidence: 0.85,
                salience: 0.5,
                kind: MemoryKindWire::Episodic,
                context_id: 1_u64,
                created_at_unix_nanos: 1_700_000_000_000_000_000,
                last_accessed_at_unix_nanos: 1_700_000_001_000_000_000,
                vector_offset: 0,
                vector_dim: 0,
                edges: Some(vec![EdgeView {
                    target: sample_memory_id(),
                    kind: EdgeKindWire::Caused,
                    weight: 0.9,
                }]),
            }],
            is_final: false,
            cumulative_count: 1,
            estimated_remaining: Some(9),
        }));
    }

    #[test]
    fn plan_response_round_trips_each_transition() {
        for transition in [
            TransitionKind::Initial,
            TransitionKind::Causal,
            TransitionKind::Temporal,
            TransitionKind::Similarity,
            TransitionKind::Other("custom".into()),
        ] {
            round_trip(ResponseBody::Plan(PlanResponseFrame {
                steps: vec![PlanStep {
                    step_index: 0,
                    memory_id: sample_memory_id(),
                    text: "first step".into(),
                    transition_kind: transition.clone(),
                    confidence: 0.7,
                    estimated_distance_to_goal: 1.0,
                }],
                is_final: false,
                plan_status: None,
            }));
        }
        round_trip(ResponseBody::Plan(PlanResponseFrame {
            steps: vec![],
            is_final: true,
            plan_status: Some(PlanStatus::GoalReached),
        }));
    }

    #[test]
    fn reason_response_round_trips() {
        round_trip(ResponseBody::Reason(ReasonResponseFrame {
            inferences: vec![InferenceStep {
                step_index: 0,
                claim: "A causes B".into(),
                supporting_memories: vec![sample_memory_id()],
                contradicting_memories: vec![],
                confidence: 0.8,
                inference_kind: InferenceKind::CausalExplanation,
            }],
            is_final: false,
            reason_status: None,
        }));
        round_trip(ResponseBody::Reason(ReasonResponseFrame {
            inferences: vec![],
            is_final: true,
            reason_status: Some(ReasonStatus::Complete),
        }));
    }

    #[test]
    fn forget_response_round_trips() {
        round_trip(ResponseBody::Forget(ForgetResponse {
            memory_id: sample_memory_id(),
            was_already_forgotten: false,
            edges_removed: 4,
        }));
    }

    #[test]
    fn subscribe_event_round_trips() {
        round_trip(ResponseBody::SubscribeEvent(SubscriptionEvent {
            event_type: EventType::Encoded,
            memory_id: sample_memory_id(),
            context_id: 2_u64,
            text: "new memory".into(),
            kind: MemoryKindWire::Episodic,
            salience: 0.5,
            timestamp_unix_nanos: 1_700_000_000_000_000_000,
            lsn: 1234,
        }));
    }

    #[test]
    fn unsubscribe_response_round_trips() {
        round_trip(ResponseBody::Unsubscribe(UnsubscribeResponse {
            target_stream_id: 7,
            final_lsn: 9999,
        }));
    }

    #[test]
    fn txn_responses_round_trip() {
        let id = sample_uuid(3);
        round_trip(ResponseBody::TxnBegin(TxnBeginResponse {
            txn_id: id,
            timeout_seconds: 60,
            started_at_unix_nanos: 1,
        }));
        round_trip(ResponseBody::TxnCommit(TxnCommitResponse {
            txn_id: id,
            committed_at_unix_nanos: 2,
            operations_applied: 5,
        }));
        round_trip(ResponseBody::TxnAbort(TxnAbortResponse {
            txn_id: id,
            operations_discarded: 5,
        }));
    }

    #[test]
    fn cancel_stream_ack_round_trips() {
        round_trip(ResponseBody::CancelStreamAck(CancelStreamAck {
            target_stream_id: 1,
            cancelled_at_unix_nanos: 99,
        }));
    }

    #[test]
    fn keepalive_responses_round_trip() {
        round_trip(ResponseBody::Pong(PongResponse {
            client_timestamp_unix_nanos: 1,
            server_timestamp_unix_nanos: 2,
        }));
        round_trip(ResponseBody::ServerPing(ServerPingResponse {
            server_timestamp_unix_nanos: 3,
        }));
    }

    #[test]
    fn admin_responses_round_trip() {
        round_trip(ResponseBody::AdminStats(AdminStatsResponse {
            summary: StatsSummary {
                total_memories: 1_000_000,
                total_active_memories: 999_000,
                total_tombstoned_memories: 1_000,
                total_contexts: 10,
                encode_qps: 100.5,
                recall_qps: 50.25,
                p99_encode_latency_ms: 2.0,
                p99_recall_latency_ms: 5.0,
                resident_memory_bytes: 1024 * 1024 * 1024,
                disk_used_bytes: 10_u64.pow(10),
            },
            per_shard: Some(vec![ShardStats {
                shard_id: 0,
                memory_count: 100_000,
                salience_distribution: SalienceHistogram { buckets: [10; 10] },
                wal_segment_count: 5,
                last_checkpoint_lsn: 1_000_000,
                arena_used_bytes: 1024 * 1024,
            }]),
            per_context: Some(vec![ContextStats {
                context_id: 4_u64,
                name: "default".into(),
                memory_count: 100,
                last_encoded_at_unix_nanos: 1,
                last_recalled_at_unix_nanos: 2,
            }]),
            server_uptime_seconds: 3600,
            server_version: "0.1.0".into(),
        }));
        round_trip(ResponseBody::AdminSnapshot(AdminSnapshotResponse {
            snapshot_id: sample_uuid(5),
            snapshot_name: "nightly".into(),
            snapshot_path: "/var/brain/snapshots/2026-05-10".into(),
            started_at_unix_nanos: 1,
            completed_at_unix_nanos: 2,
            bytes_written: 1_000_000,
            used_reflink: true,
        }));
        round_trip(ResponseBody::AdminRestore(AdminRestoreResponse {
            snapshot_name: "nightly".into(),
            shards_restored: vec![0, 1, 2],
            completed_at_unix_nanos: 3,
            memories_restored: 1_000_000,
        }));
        round_trip(ResponseBody::AdminIntegrityCheck(
            AdminIntegrityCheckResponse {
                scope: crate::request::CheckScope::Full,
                issues_found: vec![IntegrityIssue {
                    issue_type: IntegrityIssueType::VectorCorruption,
                    affected_memory_id: Some(sample_memory_id()),
                    affected_shard_id: Some(0),
                    description: "vector failed norm check".into(),
                    repaired: false,
                }],
                issues_repaired: 0,
                completed_at_unix_nanos: 4,
            },
        ));
        round_trip(ResponseBody::AdminMigrateEmbeddings(
            AdminMigrateEmbeddingsResponseFrame {
                is_final: false,
                progress: MigrationProgress {
                    total_memories: 100_000,
                    migrated_so_far: 25_000,
                    failed_so_far: 0,
                    current_qps: 1000.0,
                    estimated_remaining_seconds: 75,
                },
                status: None,
            },
        ));
        round_trip(ResponseBody::AdminMigrateEmbeddings(
            AdminMigrateEmbeddingsResponseFrame {
                is_final: true,
                progress: MigrationProgress {
                    total_memories: 100_000,
                    migrated_so_far: 100_000,
                    failed_so_far: 0,
                    current_qps: 0.0,
                    estimated_remaining_seconds: 0,
                },
                status: Some(MigrationStatus::Completed),
            },
        ));
        round_trip(ResponseBody::AdminCreateContext(
            AdminCreateContextResponse {
                context_id: 6_u64,
                name: "personal".into(),
            },
        ));
        round_trip(ResponseBody::AdminRenameContext(
            AdminRenameContextResponse {
                context_id: 7_u64,
                new_name: "renamed".into(),
                old_name: "original".into(),
            },
        ));
        round_trip(ResponseBody::AdminMoveMemory(AdminMoveMemoryResponse {
            memory_id: sample_memory_id(),
            new_context_id: 8_u64,
            old_context_id: 9_u64,
        }));
        round_trip(ResponseBody::AdminReclassify(AdminReclassifyResponse {
            memory_id: sample_memory_id(),
            new_kind: MemoryKindWire::Consolidated,
            old_kind: MemoryKindWire::Episodic,
        }));
        round_trip(ResponseBody::AdminListTombstoned(
            AdminListTombstonedResponseFrame {
                memory: TombstonedMemoryInfo {
                    memory_id: sample_memory_id(),
                    text: "forgotten".into(),
                    forgot_at_unix_nanos: 5,
                    forget_mode: ForgetMode::Soft,
                    age_seconds: 3600,
                    eligible_for_reclaim: false,
                },
                is_final: false,
            },
        ));
    }

    #[test]
    fn error_response_round_trips() {
        round_trip(ResponseBody::Error(ErrorResponse {
            code: ErrorCodeWire::InvalidArgument,
            category: ErrorCategoryWire::Validation,
            message: "field 'top_k' out of range".into(),
            details: Some(ErrorDetails {
                field: Some("top_k".into()),
                expected: Some("[1, 1000]".into()),
                actual: Some("5000".into()),
            }),
            retry_after_ms: None,
        }));
    }

    #[test]
    fn streaming_sequence_round_trips() {
        // Spec §08 §3 + §09 §3.2: a streaming response is a sequence of
        // frames, only the last of which has is_final=true. Round-trip a
        // 3-frame sequence and verify ordering survives.
        let seq: Vec<ResponseBody> = vec![
            ResponseBody::Recall(RecallResponseFrame {
                results: vec![],
                is_final: false,
                cumulative_count: 0,
                estimated_remaining: Some(10),
            }),
            ResponseBody::Recall(RecallResponseFrame {
                results: vec![],
                is_final: false,
                cumulative_count: 5,
                estimated_remaining: Some(5),
            }),
            ResponseBody::Recall(RecallResponseFrame {
                results: vec![],
                is_final: true,
                cumulative_count: 10,
                estimated_remaining: Some(0),
            }),
        ];
        let encoded: Vec<Vec<u8>> = seq.iter().map(ResponseBody::encode).collect();
        let decoded: Vec<ResponseBody> = encoded
            .iter()
            .map(|b| ResponseBody::decode(Opcode::RecallResp, b).expect("decode streaming frame"))
            .collect();
        assert_eq!(decoded, seq);
        // Ordering: only the third frame is final.
        assert_eq!(
            decoded
                .iter()
                .map(ResponseBody::is_final)
                .collect::<Vec<_>>(),
            vec![Some(false), Some(false), Some(true)],
        );
    }

    #[test]
    fn is_final_signals_streaming_variants() {
        // Streaming variants report Some(...).
        assert_eq!(
            ResponseBody::Recall(RecallResponseFrame {
                results: vec![],
                is_final: true,
                cumulative_count: 0,
                estimated_remaining: None,
            })
            .is_final(),
            Some(true)
        );
        // Unary variants report None.
        assert_eq!(
            ResponseBody::Pong(PongResponse {
                client_timestamp_unix_nanos: 0,
                server_timestamp_unix_nanos: 0,
            })
            .is_final(),
            None
        );
        // Subscription events are open-ended; body has no is_final field.
        assert_eq!(
            ResponseBody::SubscribeEvent(SubscriptionEvent {
                event_type: EventType::Encoded,
                memory_id: 0,
                context_id: 0,
                text: String::new(),
                kind: MemoryKindWire::Episodic,
                salience: 0.0,
                timestamp_unix_nanos: 0,
                lsn: 0,
            })
            .is_final(),
            None
        );
    }

    #[test]
    fn decode_with_request_opcode_returns_unknown() {
        let any_bytes = vec![0u8; 8];
        let err = ResponseBody::decode(Opcode::EncodeReq, &any_bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::UnknownOpcode(_)));
    }

    #[test]
    fn decode_garbage_returns_malformed() {
        let garbage = vec![0xAAu8; 64];
        let err = ResponseBody::decode(Opcode::EncodeResp, &garbage).unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedPayload(_)));
    }

    #[test]
    fn handshake_response_bodies_round_trip() {
        use crate::handshake::{
            AgentPermissions, AuthMethod, AuthOkPayload, HelloCapabilities, ServerFeatures,
            WelcomePayload,
        };

        let welcome = ResponseBody::Welcome(WelcomePayload {
            server_id: "brain-server/0.5.0".into(),
            chosen_version: 1,
            session_id: sample_uuid(20),
            capabilities: HelloCapabilities {
                streaming: true,
                compression_zstd: false,
                server_push: false,
            },
            server_features: ServerFeatures {
                max_payload_size: 16 * 1024 * 1024 - 1,
                max_concurrent_streams: 1024,
                idle_timeout_seconds: 300,
                auth_methods: vec![AuthMethod::Token, AuthMethod::None],
            },
        });
        let auth_ok = ResponseBody::AuthOk(AuthOkPayload {
            agent_id: sample_uuid(21),
            bound_shard_id: 5,
            permissions: AgentPermissions {
                can_encode: true,
                can_recall: true,
                can_plan: true,
                can_reason: true,
                can_forget: true,
                can_admin: false,
            },
            server_time_unix_nanos: 1_700_000_000_000_000_000,
        });

        for body in [welcome, auth_ok] {
            let bytes = body.encode();
            let decoded = ResponseBody::decode(body.opcode(), &bytes).unwrap();
            assert_eq!(decoded, body);
        }
    }

    #[test]
    fn error_code_wire_round_trips_through_canonical() {
        // ErrorCode → ErrorCodeWire → ErrorCode is the identity for every
        // code (sanity-check on the From mappings).
        for code in [
            ErrorCode::BadMagic,
            ErrorCode::Unauthenticated,
            ErrorCode::PermissionDenied,
            ErrorCode::InvalidArgument,
            ErrorCode::MemoryNotFound,
            ErrorCode::IdempotencyConflict,
            ErrorCode::OutOfSlots,
            ErrorCode::Internal,
            ErrorCode::ShardUnavailable,
        ] {
            let wire: ErrorCodeWire = code.into();
            let back: ErrorCode = wire.into();
            assert_eq!(back, code);
        }
        for cat in [
            ErrorCategory::Protocol,
            ErrorCategory::Authentication,
            ErrorCategory::Authorization,
            ErrorCategory::Validation,
            ErrorCategory::NotFound,
            ErrorCategory::Conflict,
            ErrorCategory::ResourceExhausted,
            ErrorCategory::Internal,
            ErrorCategory::Unavailable,
        ] {
            let wire: ErrorCategoryWire = cat.into();
            let back: ErrorCategory = wire.into();
            assert_eq!(back, cat);
        }
    }
}
