//! Wire-protocol error taxonomy.
//!
//! Three layers, in order of generality:
//!
//! - [`ErrorCategory`] — the 9 broad classes from spec §03/10 §2 that drive
//!   client retry behavior.
//! - [`ErrorCode`] — every named code in spec §03/10 §3, faithfully one
//!   variant per row. Used both for ERROR-frame encoding/decoding (later
//!   sub-tasks) and as a stable mapping target for [`ProtocolError`].
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
// ErrorCategory — spec §03/10 §2.
// ---------------------------------------------------------------------------

/// Broad error class. Drives the SDK's default retry policy
/// (spec §03/10 §6).
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
    /// Whether the SDK should retry by default. Mirrors the §6 retry table.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::ResourceExhausted | Self::Internal | Self::Unavailable
        )
    }
}

// ---------------------------------------------------------------------------
// ErrorCode — every named code in spec §03/10 §3.
// ---------------------------------------------------------------------------

/// Wire-level error code. One variant per row in spec §03/10 §3.1–3.9.
///
/// Numeric / on-wire encoding for these is owned by the ERROR-frame codec
/// (a later sub-task in Phase 1); this enum is the in-memory representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    // §3.1 Protocol
    BadMagic,
    BadHeaderCrc,
    BadPayloadCrc,
    BadOpcode,
    BadVersion,
    BadFrame,
    OversizePayload,
    ReservedFieldNonZero,
    BadFlagCombination,
    MalformedRkyv,
    MalformedVector,

    // §3.2 Connection / handshake
    VersionNotSupported,
    NoSuchAuthMethod,
    Unauthenticated,
    NotAuthenticated,
    AuthBackendUnavailable,
    SessionExpired,

    // §3.3 Authorization
    PermissionDenied,
    AdminPermissionRequired,
    WrongShard,

    // §3.4 Validation
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

    // §3.5 Not found
    MemoryNotFound,
    ContextNotFound,
    SubscriptionNotFound,
    SnapshotNotFound,
    TxnNotFound,

    // §3.6 Conflict
    IdempotencyConflict,
    TransactionConflict,
    TransactionTimeout,
    StreamIdInUse,
    SubscriptionLsnTooOld,

    // §3.7 Resource exhausted
    OutOfSlots,
    OutOfDisk,
    OutOfMemory,
    RateLimited,
    StreamLimitExceeded,
    ConnectionLimitExceeded,
    TransactionLimitExceeded,

    // §3.8 Internal
    Internal,
    StorageError,
    IndexError,
    EmbeddingError,
    MetadataError,

    // §3.9 Unavailable
    ShardUnavailable,
    Overloaded,
    Restarting,
    Maintenance,
}

impl ErrorCode {
    /// Map this code to its spec category (§3.1–3.9).
    #[must_use]
    pub fn category(self) -> ErrorCategory {
        match self {
            // §3.1 — all Protocol.
            Self::BadMagic
            | Self::BadHeaderCrc
            | Self::BadPayloadCrc
            | Self::BadOpcode
            | Self::BadVersion
            | Self::BadFrame
            | Self::OversizePayload
            | Self::ReservedFieldNonZero
            | Self::BadFlagCombination
            | Self::MalformedRkyv
            | Self::MalformedVector => ErrorCategory::Protocol,

            // §3.2 — version/auth-method failures are Protocol; credential
            // failures are Authentication.
            Self::VersionNotSupported | Self::NoSuchAuthMethod => ErrorCategory::Protocol,
            Self::Unauthenticated
            | Self::NotAuthenticated
            | Self::AuthBackendUnavailable
            | Self::SessionExpired => ErrorCategory::Authentication,

            // §3.3
            Self::PermissionDenied | Self::AdminPermissionRequired | Self::WrongShard => {
                ErrorCategory::Authorization
            }

            // §3.4
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
            | Self::BadModelFingerprint => ErrorCategory::Validation,

            // §3.5
            Self::MemoryNotFound
            | Self::ContextNotFound
            | Self::SubscriptionNotFound
            | Self::SnapshotNotFound
            | Self::TxnNotFound => ErrorCategory::NotFound,

            // §3.6
            Self::IdempotencyConflict
            | Self::TransactionConflict
            | Self::TransactionTimeout
            | Self::StreamIdInUse
            | Self::SubscriptionLsnTooOld => ErrorCategory::Conflict,

            // §3.7
            Self::OutOfSlots
            | Self::OutOfDisk
            | Self::OutOfMemory
            | Self::RateLimited
            | Self::StreamLimitExceeded
            | Self::ConnectionLimitExceeded
            | Self::TransactionLimitExceeded => ErrorCategory::ResourceExhausted,

            // §3.8
            Self::Internal
            | Self::StorageError
            | Self::IndexError
            | Self::EmbeddingError
            | Self::MetadataError => ErrorCategory::Internal,

            // §3.9
            Self::ShardUnavailable | Self::Overloaded | Self::Restarting | Self::Maintenance => {
                ErrorCategory::Unavailable
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
    /// Frame's magic bytes aren't `b"BRN0"` (spec §03/10 §3.1).
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
    /// An opcode value didn't match any known opcode (per spec §03/05 +
    /// §28/00). The u16 is the offending wire value.
    #[error("unknown opcode: 0x{0:04X}")]
    UnknownOpcode(u16),
    /// Input ran out before a full frame could be decoded.
    #[error("truncated frame: have {have} bytes, need {need}")]
    Truncated { have: usize, need: usize },
    /// Generic malformed-frame error for cases not covered by a more
    /// specific variant (`BadFrame` in spec §03/10 §3.1).
    #[error("bad frame: {0}")]
    BadFrame(String),
    /// Frame flags are mutually inconsistent (spec §03/10 §3.1).
    #[error("bad flag combination: {0}")]
    BadFlagCombination(String),
    /// Payload failed structural validation (rkyv / vector layout).
    #[error("malformed payload: {0}")]
    MalformedPayload(String),
}

impl ProtocolError {
    /// Map the variant to its spec [`ErrorCode`].
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
            Self::MalformedPayload(_) => ErrorCode::MalformedRkyv,
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

    /// Spot-check the category mapping against spec §03/10 §3.1–3.9 with
    /// at least one code per category.
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
        // §6: retryable categories are ResourceExhausted, Internal, Unavailable.
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
            ProtocolError::MalformedPayload("rkyv check failed".into()),
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
            ErrorCode::MalformedRkyv
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
