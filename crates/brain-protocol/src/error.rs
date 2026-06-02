//! Wire-protocol error taxonomy.
//!
//! Three layers, in order of generality:
//!
//! - [`ErrorCategory`] — nine broad classes that drive client retry
//!   behavior (Protocol, Authentication, Authorization, Validation,
//!   NotFound, Conflict, ResourceExhausted, Internal, Unavailable).
//! - [`ErrorCode`] — one variant per named code in the wire-error table.
//!   Used both for ERROR-frame encoding/decoding and as a stable mapping
//!   target for [`ProtocolError`].
//! - [`ProtocolError`] — the Rust error type emitted by this crate's codec
//!   (frame parse / validate / encode failures). Each variant maps to an
//!   [`ErrorCode`] via [`ProtocolError::code`].
//!
//! `ProtocolError` deliberately covers only what the codec itself produces.
//! Higher-level error codes (validation, not-found, resource-exhausted, etc.)
//! arrive as ERROR-frame payloads from the server; they're represented as
//! [`ErrorCode`] there, not as `ProtocolError` variants.

use thiserror::Error;

// ---------------------------------------------------------------------------
// ErrorCategory.
// ---------------------------------------------------------------------------

/// Broad error class. Drives the client's default retry policy: only
/// `ResourceExhausted`, `Internal`, and `Unavailable` retry by default
/// (see [`ErrorCategory::is_retryable`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ErrorCategory {
    /// Bad frame, version mismatch, malformed message — client bug.
    Protocol,
    /// Authentication failed; refresh credentials may help.
    Authentication,
    /// Permission denied — not retryable.
    Authorization,
    /// Invalid request argument.
    Validation,
    /// Referenced entity doesn't exist.
    NotFound,
    /// Idempotency or transaction conflict.
    Conflict,
    /// Out of slots / disk / rate / quota — retryable after backoff.
    ResourceExhausted,
    /// Server bug; retry once with backoff, report if persistent.
    Internal,
    /// Server temporarily unavailable; retry per `retry_after_ms`.
    Unavailable,
}

impl ErrorCategory {
    /// Whether the client should retry by default. Three retryable
    /// categories: `ResourceExhausted`, `Internal`, `Unavailable`.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::ResourceExhausted | Self::Internal | Self::Unavailable
        )
    }
}

// ---------------------------------------------------------------------------
// ErrorCode — every named code in the wire error table.
// ---------------------------------------------------------------------------

/// Wire-level error code. One variant per named code in the wire error
/// table.
///
/// Numeric / on-wire encoding for these is owned by the ERROR-frame codec
/// (see [`crate::shared::enums::ErrorCodeWire`]); this enum is the
/// in-memory representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    // Protocol
    BadMagic,
    BadHeaderCrc,
    BadPayloadCrc,
    BadOpcode,
    BadVersion,
    BadFrame,
    OversizePayload,
    ReservedFieldNonZero,
    BadFlagCombination,
    MalformedPayload,
    MalformedVector,

    // Connection / handshake
    VersionNotSupported,
    NoSuchAuthMethod,
    Unauthenticated,
    NotAuthenticated,
    AuthBackendUnavailable,
    SessionExpired,

    // Authorization
    PermissionDenied,
    AdminPermissionRequired,
    WrongShard,

    // Validation
    InvalidArgument,
    MissingRequiredField,
    TextTooLarge,
    TextEmpty,
    BadContextId,
    BadMemoryKind,
    BadEdgeKind,
    BadStrategyHint,
    TopKOutOfRange,
    BudgetTooLarge,
    BadModelFingerprint,
    /// Schema-strict mode rejected a STATEMENT_CREATE / QUERY because
    /// the requested predicate qname is not declared in the namespace's
    /// active schema version. In schemaless mode this never fires —
    /// unknown predicates are interned on demand.
    PredicateNotInSchema,
    /// Schema-strict mode rejected a RELATION_CREATE because the
    /// requested relation type qname is not declared. Same shape as
    /// `PredicateNotInSchema` for the relation registry.
    RelationTypeNotInSchema,

    // Not found
    MemoryNotFound,
    ContextNotFound,
    SubscriptionNotFound,
    SnapshotNotFound,
    TxnNotFound,

    // Conflict
    IdempotencyConflict,
    TransactionConflict,
    TransactionTimeout,
    StreamIdInUse,
    SubscriptionLsnTooOld,
    /// A schema-declared relation type's cardinality (OneToOne /
    /// OneToMany / ManyToOne) is violated by the requested write.
    /// Implicit relation types never trigger this — they always
    /// behave as ManyToMany.
    CardinalityViolation,

    // Resource exhausted
    OutOfSlots,
    OutOfDisk,
    OutOfMemory,
    RateLimited,
    StreamLimitExceeded,
    ConnectionLimitExceeded,
    TransactionLimitExceeded,
    /// Transaction buffer exceeded the per-transaction op cap (1000 ops).
    /// Surfaced both at append-time (so the agent
    /// learns immediately when the 1001st op is buffered) and at commit
    /// time (defense-in-depth for any buffer mutation that slipped past
    /// the append guard). The client should split the work into multiple
    /// transactions.
    TransactionTooLarge,

    // Internal
    Internal,
    StorageError,
    IndexError,
    EmbeddingError,
    MetadataError,
    /// Operation cancelled (client `CANCEL_STREAM`, server shutdown
    /// mid-stream, or a parent stream aborted). Carried in the cancelled
    /// stream's terminal ERROR frame.
    Cancelled,

    // Unavailable
    ShardUnavailable,
    Overloaded,
    Restarting,
    Maintenance,
    /// Reserved for admin / diagnostic surfaces (`/health`, `ADMIN_STATUS`)
    /// when a shard reports a degraded retriever set — e.g. tantivy
    /// segment corruption or a graph-store `pwritev2` failure observed
    /// after spawn. Never returned to a normal RECALL: shards refuse to
    /// spawn if a required retriever is unwired.
    RetrieverDegraded,

    // Typed-graph error codes (`0x01xx` namespace). Low-byte family
    // mirrors the typed-graph opcode ranges.
    /// Schema upload failed validation (syntax, type-ref, attribute rule, etc.).
    SchemaInvalid,
    /// Schema upload requires a migration plan that hasn't been executed.
    SchemaMigrationRequired,
    /// `ENTITY_GET` / `ENTITY_RESOLVE` / dependent ops referenced a
    /// nonexistent entity.
    EntityNotFound,
    /// `ENTITY_CREATE` / `ENTITY_UPDATE` supplied an entity-type id that
    /// is not in the active schema (or whose declared attribute types
    /// don't match the supplied attribute blob).
    EntityTypeMismatch,
    /// `ENTITY_RESOLVE` returned multiple high-confidence candidates;
    /// the disambiguation is a human/admin action.
    EntityAmbiguous,
    /// `ENTITY_MERGE` rejected the merge (grace period expired, already
    /// merged, etc.).
    EntityMergeConflict,
    /// Statement op referenced a `StatementId` that doesn't exist.
    StatementNotFound,
    /// `StatementObject` doesn't match the predicate's declared object type.
    StatementObjectTypeMismatch,
    /// New Fact contradicts an existing active Fact for the same
    /// (subject, predicate). Resolution: client picks `STATEMENT_SUPERSEDE`
    /// or leaves both active.
    StatementContradictsExisting,
    /// `QUERY` / `RECALL_HYBRID` exceeded its wall-time budget.
    QueryTimeout,
    /// `QUERY` exceeded its declared cost budget (top_k × retrievers ×
    /// per-hit cost).
    QueryOverBudget,
    /// Extractor governance op (`EXTRACTOR_DISABLE` / `_ENABLE`) refused —
    /// extractor is disabled by the operator or unreachable.
    ExtractorDisabled,
    /// LLM extractor's per-call cost budget exceeded.
    ExtractorBudgetExceeded,
    /// LLM extractor failed (provider error, schema-invalid output after
    /// retry, projection failed).
    ExtractionFailed,
}

impl ErrorCode {
    /// Map this code to its broad category.
    #[must_use]
    pub fn category(self) -> ErrorCategory {
        match self {
            // Protocol codes.
            Self::BadMagic
            | Self::BadHeaderCrc
            | Self::BadPayloadCrc
            | Self::BadOpcode
            | Self::BadVersion
            | Self::BadFrame
            | Self::OversizePayload
            | Self::ReservedFieldNonZero
            | Self::BadFlagCombination
            | Self::MalformedPayload
            | Self::MalformedVector => ErrorCategory::Protocol,

            // Connection / handshake — version + auth-method failures are
            // Protocol; credential failures are Authentication.
            Self::VersionNotSupported | Self::NoSuchAuthMethod => ErrorCategory::Protocol,
            Self::Unauthenticated
            | Self::NotAuthenticated
            | Self::AuthBackendUnavailable
            | Self::SessionExpired => ErrorCategory::Authentication,

            // Authorization codes.
            Self::PermissionDenied | Self::AdminPermissionRequired | Self::WrongShard => {
                ErrorCategory::Authorization
            }

            // Validation codes.
            Self::InvalidArgument
            | Self::MissingRequiredField
            | Self::TextTooLarge
            | Self::TextEmpty
            | Self::BadContextId
            | Self::BadMemoryKind
            | Self::BadEdgeKind
            | Self::BadStrategyHint
            | Self::TopKOutOfRange
            | Self::BudgetTooLarge
            | Self::BadModelFingerprint
            | Self::PredicateNotInSchema
            | Self::RelationTypeNotInSchema => ErrorCategory::Validation,

            // Not-found codes.
            Self::MemoryNotFound
            | Self::ContextNotFound
            | Self::SubscriptionNotFound
            | Self::SnapshotNotFound
            | Self::TxnNotFound => ErrorCategory::NotFound,

            // Conflict codes.
            Self::IdempotencyConflict
            | Self::TransactionConflict
            | Self::TransactionTimeout
            | Self::StreamIdInUse
            | Self::SubscriptionLsnTooOld
            | Self::CardinalityViolation => ErrorCategory::Conflict,

            // Resource-exhausted codes.
            Self::OutOfSlots
            | Self::OutOfDisk
            | Self::OutOfMemory
            | Self::RateLimited
            | Self::StreamLimitExceeded
            | Self::ConnectionLimitExceeded
            | Self::TransactionLimitExceeded
            | Self::TransactionTooLarge => ErrorCategory::ResourceExhausted,

            // Internal codes.
            Self::Internal
            | Self::StorageError
            | Self::IndexError
            | Self::EmbeddingError
            | Self::MetadataError
            | Self::Cancelled
            | Self::ExtractionFailed => ErrorCategory::Internal,

            // Unavailable codes.
            Self::ShardUnavailable
            | Self::Overloaded
            | Self::Restarting
            | Self::Maintenance
            | Self::RetrieverDegraded
            | Self::QueryTimeout => ErrorCategory::Unavailable,

            // Typed-graph validation codes.
            Self::SchemaInvalid | Self::EntityTypeMismatch | Self::StatementObjectTypeMismatch => {
                ErrorCategory::Validation
            }

            // Typed-graph not-found codes.
            Self::EntityNotFound | Self::StatementNotFound => ErrorCategory::NotFound,

            // Typed-graph conflict codes.
            Self::SchemaMigrationRequired
            | Self::EntityAmbiguous
            | Self::EntityMergeConflict
            | Self::StatementContradictsExisting
            | Self::ExtractorDisabled => ErrorCategory::Conflict,

            // Typed-graph resource-exhausted codes.
            Self::QueryOverBudget | Self::ExtractorBudgetExceeded => {
                ErrorCategory::ResourceExhausted
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ProtocolError — codec-level failures.
// ---------------------------------------------------------------------------

/// Errors raised by this crate's frame codec (parse / validate / encode).
///
/// Each variant maps to an [`ErrorCode`] via [`ProtocolError::code`]; the
/// corresponding [`ErrorCategory`] is reachable via [`ProtocolError::category`].
#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum ProtocolError {
    /// Frame's magic bytes aren't `b"BRN0"`.
    #[error("bad magic: expected b\"BRN0\"")]
    BadMagic,
    /// Frame's version doesn't match the negotiated/supported version.
    #[error("bad version: got {got}, expected {expected}")]
    BadVersion { got: u8, expected: u8 },
    /// Stored header CRC32C doesn't match the recomputed value.
    #[error("bad header crc32c")]
    BadHeaderCrc,
    /// Stored payload CRC32C doesn't match the recomputed value.
    #[error("bad payload crc32c")]
    BadPayloadCrc,
    /// `payload_len` exceeds the configured / 24-bit max.
    #[error("oversize payload: {len} > {max}")]
    OversizePayload { len: u32, max: u32 },
    /// A reserved header field was non-zero.
    #[error("reserved field non-zero")]
    ReservedFieldNonZero,
    /// An opcode value didn't match any known opcode. The u16 is the
    /// offending wire value.
    #[error("unknown opcode: 0x{0:04X}")]
    UnknownOpcode(u16),
    /// Input ran out before a full frame could be decoded.
    #[error("truncated frame: have {have} bytes, need {need}")]
    Truncated { have: usize, need: usize },
    /// Generic malformed-frame error for cases not covered by a more
    /// specific variant (maps to `BadFrame` in the wire error table).
    #[error("bad frame: {0}")]
    BadFrame(String),
    /// Frame flags are mutually inconsistent.
    #[error("bad flag combination: {0}")]
    BadFlagCombination(String),
    /// Payload failed structural validation (CBOR / vector layout).
    #[error("malformed payload: {0}")]
    MalformedPayload(String),
}

impl ProtocolError {
    /// Map the variant to its [`ErrorCode`].
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::BadMagic => ErrorCode::BadMagic,
            Self::BadVersion { .. } => ErrorCode::BadVersion,
            Self::BadHeaderCrc => ErrorCode::BadHeaderCrc,
            Self::BadPayloadCrc => ErrorCode::BadPayloadCrc,
            Self::OversizePayload { .. } => ErrorCode::OversizePayload,
            Self::ReservedFieldNonZero => ErrorCode::ReservedFieldNonZero,
            Self::UnknownOpcode(_) => ErrorCode::BadOpcode,
            Self::Truncated { .. } => ErrorCode::BadFrame,
            Self::BadFrame(_) => ErrorCode::BadFrame,
            Self::BadFlagCombination(_) => ErrorCode::BadFlagCombination,
            Self::MalformedPayload(_) => ErrorCode::MalformedPayload,
        }
    }

    /// Convenience: the [`ErrorCategory`] of this error's [`ErrorCode`].
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        self.code().category()
    }
}

// `ProtocolError` is always a Protocol-category error (client-side
// framing/parsing problem). Surface it as `InvalidArgument` to higher
// layers, with the `Internal` fallback for the rare cases an `Internal`
// code slips in (e.g., future variants).
impl From<ProtocolError> for brain_core::Error {
    fn from(e: ProtocolError) -> Self {
        match e.category() {
            ErrorCategory::Internal => brain_core::Error::Internal(e.to_string()),
            _ => brain_core::Error::InvalidArgument(e.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Spot-check the category mapping with at least one code per
    /// category.
    #[test]
    fn error_code_categories_match_spec() {
        assert_eq!(ErrorCode::BadMagic.category(), ErrorCategory::Protocol);
        assert_eq!(
            ErrorCode::VersionNotSupported.category(),
            ErrorCategory::Protocol
        );
        assert_eq!(
            ErrorCode::Unauthenticated.category(),
            ErrorCategory::Authentication
        );
        assert_eq!(
            ErrorCode::PermissionDenied.category(),
            ErrorCategory::Authorization
        );
        assert_eq!(
            ErrorCode::InvalidArgument.category(),
            ErrorCategory::Validation
        );
        assert_eq!(
            ErrorCode::MemoryNotFound.category(),
            ErrorCategory::NotFound
        );
        assert_eq!(
            ErrorCode::IdempotencyConflict.category(),
            ErrorCategory::Conflict
        );
        assert_eq!(
            ErrorCode::OutOfSlots.category(),
            ErrorCategory::ResourceExhausted
        );
        assert_eq!(ErrorCode::StorageError.category(), ErrorCategory::Internal);
        assert_eq!(
            ErrorCode::ShardUnavailable.category(),
            ErrorCategory::Unavailable
        );
    }

    #[test]
    fn retryability_matches_spec_table() {
        // Retryable categories: ResourceExhausted, Internal, Unavailable.
        assert!(ErrorCategory::ResourceExhausted.is_retryable());
        assert!(ErrorCategory::Internal.is_retryable());
        assert!(ErrorCategory::Unavailable.is_retryable());
        // Non-retryable.
        for cat in [
            ErrorCategory::Protocol,
            ErrorCategory::Authentication,
            ErrorCategory::Authorization,
            ErrorCategory::Validation,
            ErrorCategory::NotFound,
            ErrorCategory::Conflict,
        ] {
            assert!(!cat.is_retryable(), "{cat:?} must not be retryable");
        }
    }

    #[test]
    fn protocol_error_codes_are_in_protocol_category() {
        let samples = [
            ProtocolError::BadMagic,
            ProtocolError::BadVersion {
                got: 9,
                expected: 1,
            },
            ProtocolError::BadHeaderCrc,
            ProtocolError::BadPayloadCrc,
            ProtocolError::OversizePayload { len: 100, max: 50 },
            ProtocolError::ReservedFieldNonZero,
            ProtocolError::UnknownOpcode(0x0077),
            ProtocolError::Truncated { have: 0, need: 32 },
            ProtocolError::BadFrame("x".into()),
            ProtocolError::BadFlagCombination("EOS+MPL".into()),
            ProtocolError::MalformedPayload("CBOR decode failed".into()),
        ];
        for e in samples {
            assert_eq!(
                e.category(),
                ErrorCategory::Protocol,
                "{e:?} must be Protocol-category"
            );
        }
    }

    #[test]
    fn protocol_error_code_mapping_is_stable() {
        assert_eq!(ProtocolError::BadMagic.code(), ErrorCode::BadMagic);
        assert_eq!(
            ProtocolError::BadVersion {
                got: 9,
                expected: 1,
            }
            .code(),
            ErrorCode::BadVersion
        );
        assert_eq!(ProtocolError::BadHeaderCrc.code(), ErrorCode::BadHeaderCrc);
        assert_eq!(
            ProtocolError::BadPayloadCrc.code(),
            ErrorCode::BadPayloadCrc
        );
        assert_eq!(
            ProtocolError::OversizePayload { len: 1, max: 0 }.code(),
            ErrorCode::OversizePayload
        );
        assert_eq!(
            ProtocolError::ReservedFieldNonZero.code(),
            ErrorCode::ReservedFieldNonZero
        );
        assert_eq!(
            ProtocolError::UnknownOpcode(0x0077).code(),
            ErrorCode::BadOpcode
        );
        assert_eq!(
            ProtocolError::Truncated { have: 0, need: 32 }.code(),
            ErrorCode::BadFrame
        );
        assert_eq!(
            ProtocolError::BadFrame("x".into()).code(),
            ErrorCode::BadFrame
        );
        assert_eq!(
            ProtocolError::BadFlagCombination("y".into()).code(),
            ErrorCode::BadFlagCombination
        );
        assert_eq!(
            ProtocolError::MalformedPayload("z".into()).code(),
            ErrorCode::MalformedPayload
        );
    }

    /// Pins every typed-graph code plus the Cancelled and
    /// RetrieverDegraded codes to their assigned categories.
    /// Drift guard: if a category changes, this fails and forces
    /// an explicit decision rather than silent drift.
    #[test]
    fn typed_graph_and_new_codes_match_spec_categories() {
        // Internal additions.
        assert_eq!(ErrorCode::Cancelled.category(), ErrorCategory::Internal);

        // Unavailable additions.
        assert_eq!(
            ErrorCode::RetrieverDegraded.category(),
            ErrorCategory::Unavailable
        );

        // Typed-graph codes.
        assert_eq!(
            ErrorCode::SchemaInvalid.category(),
            ErrorCategory::Validation
        );
        assert_eq!(
            ErrorCode::SchemaMigrationRequired.category(),
            ErrorCategory::Conflict
        );
        assert_eq!(
            ErrorCode::EntityNotFound.category(),
            ErrorCategory::NotFound
        );
        assert_eq!(
            ErrorCode::EntityTypeMismatch.category(),
            ErrorCategory::Validation
        );
        assert_eq!(
            ErrorCode::EntityAmbiguous.category(),
            ErrorCategory::Conflict
        );
        assert_eq!(
            ErrorCode::EntityMergeConflict.category(),
            ErrorCategory::Conflict
        );
        assert_eq!(
            ErrorCode::StatementNotFound.category(),
            ErrorCategory::NotFound
        );
        assert_eq!(
            ErrorCode::StatementObjectTypeMismatch.category(),
            ErrorCategory::Validation
        );
        assert_eq!(
            ErrorCode::StatementContradictsExisting.category(),
            ErrorCategory::Conflict
        );
        assert_eq!(
            ErrorCode::QueryTimeout.category(),
            ErrorCategory::Unavailable
        );
        assert_eq!(
            ErrorCode::QueryOverBudget.category(),
            ErrorCategory::ResourceExhausted
        );
        assert_eq!(
            ErrorCode::ExtractorDisabled.category(),
            ErrorCategory::Conflict
        );
        assert_eq!(
            ErrorCode::ExtractorBudgetExceeded.category(),
            ErrorCategory::ResourceExhausted
        );
        assert_eq!(
            ErrorCode::ExtractionFailed.category(),
            ErrorCategory::Internal
        );
    }

    #[test]
    fn protocol_error_converts_to_brain_core_invalid_argument() {
        let core_err: brain_core::Error = ProtocolError::BadMagic.into();
        assert!(matches!(core_err, brain_core::Error::InvalidArgument(_)));

        // Display message round-trips through the conversion.
        let pe = ProtocolError::OversizePayload { len: 10, max: 5 };
        let msg = pe.to_string();
        let core_err: brain_core::Error = pe.into();
        match core_err {
            brain_core::Error::InvalidArgument(s) => assert_eq!(s, msg),
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
