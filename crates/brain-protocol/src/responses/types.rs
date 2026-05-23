//! Foundational wire-domain enums for response bodies plus the
//! ErrorCategoryWire / ErrorCodeWire mirrors of `crate::error`'s
//! `#[non_exhaustive]` types.

use rkyv::{Archive, Deserialize, Serialize};

use crate::error::{ErrorCategory, ErrorCode};

/// — `PlanResponseFrame::TransitionKind`.
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

// Bridge from the typed-graph namespace's hybrid-query retriever
// enum to the cognitive RECALL response field. Lives here so the
// cognitive response module stays free of typed-graph imports beyond
// this single conversion.
impl From<crate::requests::RetrieverWire> for RetrieverNameWire {
    fn from(w: crate::requests::RetrieverWire) -> Self {
        match w {
            crate::requests::RetrieverWire::Semantic => Self::Semantic,
            crate::requests::RetrieverWire::Lexical => Self::Lexical,
            crate::requests::RetrieverWire::Graph => Self::Graph,
        }
    }
}

/// Names the retriever family that surfaced a memory in a
/// `MemoryResult`. Populated when `RECALL_REQ` routes through the
/// hybrid query engine (schema-declared deployments).
///
/// This is the cognitive-side wire enum. The typed-graph namespace
/// has its own `RetrieverWire` for hybrid-query opcodes;
/// `From<RetrieverWire>` bridges the two so the cognitive response
/// type doesn't depend on the typed-graph namespace.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum RetrieverNameWire {
    Semantic = 0,
    Lexical = 1,
    Graph = 2,
}

/// — `PlanResponseFrame::PlanStatus` (set on the final frame
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

/// — `ReasonResponseFrame::InferenceKind`.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum InferenceKind {
    CausalExplanation,
    EvidenceAccumulation,
    AnalogicalInference,
    Other(String),
}

/// — `ReasonResponseFrame::ReasonStatus`.
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

/// `SubscriptionEvent::EventType`.
///
/// Carries 14 typed-graph event variants ([`Self::EntityCreated`]
/// through [`Self::SchemaUpdated`]). For typed-graph events the
/// cognitive fields on `SubscriptionEvent` (`memory_id`, `context_id`,
/// `kind`, `salience`, `text`) are zero-filled and `knowledge_payload`
/// carries the typed body.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum EventType {
    // Cognitive events.
    Encoded = 0,
    Forgotten = 1,
    Reclaimed = 2,
    KindChanged = 3,

    // Typed-graph events. knowledge_payload is populated.
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
    /// One *stage* of a write's pipeline completed. The same envelope
    /// is published by every background worker (auto-edge, temporal-
    /// edge, extractor) once it has committed its derived phases for
    /// a memory. Subscribers waiting on a write's completion count
    /// down their `pending_stages` checklist as `StageCompleted`
    /// events arrive. `stage_payload` carries the per-stage detail
    /// (extractor counts + audit status, edge stages: the count of
    /// edges written, etc.).
    StageCompleted = 27,
    SchemaUpdated = 29,
    /// Appended after `SchemaUpdated` to preserve the stable
    /// discriminants of prior variants.
    RelationTombstoned = 30,

    // Unified-edge change feed. Cognitive Link / Unlink and typed-
    // relation create / supersede / tombstone all flow through these
    // three variants; the per-event `edge_payload` sidecar carries
    // `from`, `to`, kind discriminator, relation id when applicable.
    EdgeAdded = 31,
    EdgeRemoved = 32,
    EdgeSuperseded = 33,
}

/// Background stage a write triggers. A write submission runs N
/// foreground phases inside `submit()`'s wtxn (caller blocks) plus M
/// background stages that workers drain (caller doesn't block, but
/// may opt in via `--wait`). The ack lists the stages this write
/// queued so a client knows which `StageCompleted` events to wait
/// for.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum StageKind {
    /// `SimilarTo` edges derived from HNSW k-NN of the new memory.
    AutoEdge = 0,
    /// `FollowedBy` edges derived from session adjacency.
    TemporalEdge = 1,
    /// Entities, statements, relations extracted from memory text via
    /// the three-tier pipeline (pattern → classifier → LLM).
    Extractor = 2,
}

/// Verdict of a completed stage. Carried on every `StageCompleted`
/// event so a client can distinguish "ran and produced output" from
/// "ran but had nothing to produce" from "failed."
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum StageOutcome {
    /// Stage produced at least one derived item.
    Ok = 0,
    /// Stage ran cleanly but produced nothing (e.g. zero-vector input
    /// for auto-edge, no extractable content for extractor). Distinct
    /// from `Ok` so wait helpers can show "completed, nothing to add."
    Empty = 1,
    /// Stage errored. The wait helper still unblocks; the failure
    /// reason lives on the per-stage payload (`StagePayload::*`).
    Failed = 2,
}

/// Per-stage detail sidecar on `StageCompleted` events. Discriminated
/// by [`StageKind`] but kept as a flat enum so subscribers can
/// destructure without first reading the parent stage_kind byte.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum StagePayload {
    AutoEdge(StageAutoEdgePayload),
    TemporalEdge(StageTemporalEdgePayload),
    Extractor(StageExtractorPayload),
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StageAutoEdgePayload {
    /// How many `SimilarTo` rows the worker wrote.
    pub edges_written: u32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StageTemporalEdgePayload {
    /// How many `FollowedBy` rows the worker wrote.
    pub edges_written: u32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StageExtractorPayload {
    pub entity_count: u32,
    pub statement_count: u32,
    pub relation_count: u32,
    pub audit_status: StageAuditStatus,
    /// Populated only when `audit_status == Failed`. Empty otherwise.
    pub error_message: String,
}

/// Audit verdict for an extractor stage. Mirrors the worker's
/// internal `pipeline_status` byte. Distinct from [`StageOutcome`]
/// because the extractor pipeline has more granularity than the
/// generic three-state outcome — `PartiallyApplied` is a per-tier
/// concern that doesn't make sense for the edge stages.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum StageAuditStatus {
    /// Every enabled tier ran cleanly and writes committed.
    Succeeded = 0,
    /// One or more tiers failed; surviving tiers still committed
    /// their items. Counts reflect what landed.
    PartiallyApplied = 1,
    /// All tiers failed or the apply path errored; nothing landed.
    Failed = 2,
    /// Worker ran but had nothing to commit (no extractors
    /// registered, or every tier returned zero items).
    Skipped = 3,
}

/// — `IntegrityIssue::IntegrityIssueType`.
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

/// — `AdminMigrateEmbeddingsResponseFrame::MigrationStatus`.
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

/// rkyv-archivable mirror of [`crate::error::ErrorCode`]. Numeric repr
/// is stable; this is the *wire* representation.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u16)]
pub enum ErrorCodeWire {
    // Protocol
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
    // Connection / handshake
    VersionNotSupported = 0x0020,
    NoSuchAuthMethod = 0x0021,
    Unauthenticated = 0x0022,
    NotAuthenticated = 0x0023,
    AuthBackendUnavailable = 0x0024,
    SessionExpired = 0x0025,
    // Authorization
    PermissionDenied = 0x0030,
    AdminPermissionRequired = 0x0031,
    WrongShard = 0x0032,
    // Validation
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
    PredicateNotInSchema = 0x004B,
    RelationTypeNotInSchema = 0x004C,
    // Not found
    MemoryNotFound = 0x0050,
    ContextNotFound = 0x0051,
    SubscriptionNotFound = 0x0052,
    SnapshotNotFound = 0x0053,
    TxnNotFound = 0x0054,
    // Conflict
    IdempotencyConflict = 0x0060,
    TransactionConflict = 0x0061,
    TransactionTimeout = 0x0062,
    StreamIdInUse = 0x0063,
    SubscriptionLsnTooOld = 0x0064,
    CardinalityViolation = 0x0065,
    // Resource exhausted
    OutOfSlots = 0x0070,
    OutOfDisk = 0x0071,
    OutOfMemory = 0x0072,
    RateLimited = 0x0073,
    StreamLimitExceeded = 0x0074,
    ConnectionLimitExceeded = 0x0075,
    TransactionLimitExceeded = 0x0076,
    // Internal
    Internal = 0x0080,
    StorageError = 0x0081,
    IndexError = 0x0082,
    EmbeddingError = 0x0083,
    MetadataError = 0x0084,
    // Unavailable
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
            ErrorCode::PredicateNotInSchema => Self::PredicateNotInSchema,
            ErrorCode::RelationTypeNotInSchema => Self::RelationTypeNotInSchema,
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
            ErrorCode::CardinalityViolation => Self::CardinalityViolation,
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
            ErrorCodeWire::PredicateNotInSchema => Self::PredicateNotInSchema,
            ErrorCodeWire::RelationTypeNotInSchema => Self::RelationTypeNotInSchema,
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
            ErrorCodeWire::CardinalityViolation => Self::CardinalityViolation,
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
