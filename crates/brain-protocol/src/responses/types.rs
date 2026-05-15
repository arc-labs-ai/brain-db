//! Foundational wire-domain enums for response bodies plus the
//! ErrorCategoryWire / ErrorCodeWire mirrors of `crate::error`'s
//! `#[non_exhaustive]` types.

use rkyv::{Archive, Deserialize, Serialize};

use crate::error::{ErrorCategory, ErrorCode};

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

/// Spec §03/08 §7 — `SubscriptionEvent::EventType`.
///
/// Phase 16.7 extended this enum with the 14 knowledge-layer event
/// variants ([`Self::EntityCreated`] through [`Self::SchemaUpdated`]).
/// For knowledge events the substrate fields on `SubscriptionEvent`
/// (`memory_id`, `context_id`, `kind`, `salience`, `text`) are
/// zero-filled and `knowledge_payload` carries the typed body. See
/// `spec/28_knowledge_wire_protocol/02_subscribe_events.md`.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum EventType {
    // Substrate events.
    Encoded = 0,
    Forgotten = 1,
    Reclaimed = 2,
    KindChanged = 3,

    // Knowledge-layer events (§28/02). knowledge_payload is populated.
    EntityCreated = 16,
    EntityUpdated = 17,
    EntityRenamed = 18,
    EntityMerged = 19,
    EntityUnmerged = 20,
    EntityTombstoned = 21,
    StatementCreated = 22,
    StatementSuperseded = 23,
    StatementTombstoned = 24,
    RelationCreated = 25,
    RelationSuperseded = 26,
    ExtractionCompleted = 27,
    ExtractionFailed = 28,
    SchemaUpdated = 29,
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
