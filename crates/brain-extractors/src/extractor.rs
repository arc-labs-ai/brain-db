//! `Extractor` trait + execution context + result types. Spec §22/01,
//! §22/02, §22/05.

use std::future::Future;
use std::pin::Pin;

use brain_core::knowledge::ExtractorKind;
use brain_core::ExtractorId;
use brain_core::Memory;

use crate::item::ExtractedItem;
use crate::registry::ExtractorRegistry;

// ---------------------------------------------------------------------------
// Trait.
// ---------------------------------------------------------------------------

/// Boxed-future return type for [`Extractor::run`]. Pattern +
/// classifier impls wrap their sync bodies via
/// `Box::pin(async move { ... })`; LLM impls use the async path
/// natively for HTTP calls.
pub type ExtractionFuture<'a> =
    Pin<Box<dyn Future<Output = ExtractionResult> + Send + 'a>>;

/// Object-safe extractor interface. Pattern / classifier / LLM
/// impls live in `pattern.rs`, `classifier.rs`, and `llm.rs`.
/// The registry stores `Arc<dyn Extractor>`.
///
/// `run` returns a boxed future to let LLM extractors call out
/// to HTTP providers without blocking the executor thread. Sync
/// impls trivially wrap their bodies in `Box::pin(async move
/// { ... })`.
pub trait Extractor: Send + Sync {
    fn id(&self) -> ExtractorId;
    fn kind(&self) -> ExtractorKind;
    /// Canonical qname, e.g. `"acme:person_mentions"`.
    fn name(&self) -> &str;
    fn extractor_version(&self) -> u32;
    /// Run over `mem`. Returns a populated [`ExtractionResult`]
    /// including `started_at` / `completed_at` timestamps; the
    /// caller writes the audit row from these.
    fn run<'a>(
        &'a self,
        ctx: &'a ExtractionContext<'a>,
        mem: &'a Memory,
    ) -> ExtractionFuture<'a>;
}

// ---------------------------------------------------------------------------
// Context.
// ---------------------------------------------------------------------------

/// Read-only context handed to every `run` call. Carries the
/// schema version stamped onto outputs and a registry reference
/// for dep lookups.
pub struct ExtractionContext<'a> {
    pub schema_version: u32,
    pub now_unix_nanos: u64,
    pub registry: &'a ExtractorRegistry,
}

// ---------------------------------------------------------------------------
// Result.
// ---------------------------------------------------------------------------

/// One extractor invocation's full output. Whether `items` is
/// populated depends on `status`: `Success` always carries items
/// (possibly empty); the various `Skipped*` and `Failure` variants
/// carry an empty `items` vec.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractionResult {
    pub items: Vec<ExtractedItem>,
    pub status: ExtractionStatus,
    pub status_reason: String,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
}

impl ExtractionResult {
    /// Convenience: an empty result with `Success`. Wall time is
    /// expected to be filled in by callers that wrap `run`.
    pub fn success(items: Vec<ExtractedItem>, started_at: u64, completed_at: u64) -> Self {
        Self {
            items,
            status: ExtractionStatus::Success,
            status_reason: String::new(),
            started_at_unix_nanos: started_at,
            completed_at_unix_nanos: completed_at,
        }
    }

    pub fn skipped(status: ExtractionStatus, reason: impl Into<String>, at: u64) -> Self {
        Self {
            items: Vec::new(),
            status,
            status_reason: reason.into(),
            started_at_unix_nanos: at,
            completed_at_unix_nanos: at,
        }
    }

    pub fn failure(reason: impl Into<String>, started_at: u64, completed_at: u64) -> Self {
        Self {
            items: Vec::new(),
            status: ExtractionStatus::Failure,
            status_reason: reason.into(),
            started_at_unix_nanos: started_at,
            completed_at_unix_nanos: completed_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Status.
// ---------------------------------------------------------------------------

/// Audit-row status discriminant. Bytes match spec §22/05 §3 — never
/// reassigned; new variants append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ExtractionStatus {
    Success = 1,
    Failure = 2,
    SkippedBudget = 3,
    SkippedFilter = 4,
    SkippedDuplicate = 5,
    SkippedDisabled = 6,
}

impl ExtractionStatus {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Success,
            2 => Self::Failure,
            3 => Self::SkippedBudget,
            4 => Self::SkippedFilter,
            5 => Self::SkippedDuplicate,
            6 => Self::SkippedDisabled,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum ExtractorError {
    #[error("regex compilation failed at index {index}: {message}")]
    RegexCompile { index: usize, message: String },

    #[error("resource limit exceeded at pattern {index}: {limit}")]
    ResourceLimit { index: usize, limit: &'static str },

    #[error("extractor declared `pattern` kind but has no patterns")]
    EmptyPatterns,

    #[error("classifier model not found: {id:?}")]
    ModelNotFound { id: String },

    #[error("feature extraction failed: {reason}")]
    FeatureExtractionFailed { reason: String },

    #[error("inference failed: {reason}")]
    InferenceFailed { reason: String },

    #[error("output decode failed: {reason}")]
    OutputDecodeFailed { reason: String },

    #[error("trigger evaluation error: {reason}")]
    TriggerEval { reason: String },
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_enum_discriminants_match_spec() {
        // Bytes from spec §22/05 §3 — never change.
        assert_eq!(ExtractionStatus::Success.as_u8(), 1);
        assert_eq!(ExtractionStatus::Failure.as_u8(), 2);
        assert_eq!(ExtractionStatus::SkippedBudget.as_u8(), 3);
        assert_eq!(ExtractionStatus::SkippedFilter.as_u8(), 4);
        assert_eq!(ExtractionStatus::SkippedDuplicate.as_u8(), 5);
        assert_eq!(ExtractionStatus::SkippedDisabled.as_u8(), 6);
    }

    #[test]
    fn status_from_u8_round_trip() {
        for s in [
            ExtractionStatus::Success,
            ExtractionStatus::Failure,
            ExtractionStatus::SkippedBudget,
            ExtractionStatus::SkippedFilter,
            ExtractionStatus::SkippedDuplicate,
            ExtractionStatus::SkippedDisabled,
        ] {
            assert_eq!(ExtractionStatus::from_u8(s.as_u8()), Some(s));
        }
        assert_eq!(ExtractionStatus::from_u8(0), None);
        assert_eq!(ExtractionStatus::from_u8(255), None);
    }

    #[test]
    fn extraction_result_success_builder() {
        let r = ExtractionResult::success(Vec::new(), 10, 20);
        assert_eq!(r.status, ExtractionStatus::Success);
        assert!(r.items.is_empty());
        assert_eq!(r.completed_at_unix_nanos - r.started_at_unix_nanos, 10);
    }

    #[test]
    fn extraction_result_skipped_filter_builder() {
        let r = ExtractionResult::skipped(ExtractionStatus::SkippedFilter, "no match", 42);
        assert_eq!(r.status, ExtractionStatus::SkippedFilter);
        assert_eq!(r.status_reason, "no match");
        assert_eq!(r.started_at_unix_nanos, r.completed_at_unix_nanos);
    }

    #[test]
    fn extraction_result_failure_builder() {
        let r = ExtractionResult::failure("inference oom", 5, 7);
        assert_eq!(r.status, ExtractionStatus::Failure);
        assert!(r.items.is_empty());
    }

    #[test]
    fn extractor_error_messages_carry_position() {
        let e = ExtractorError::RegexCompile {
            index: 2,
            message: "bad".into(),
        };
        let s = e.to_string();
        assert!(s.contains("index 2"));
        assert!(s.contains("bad"));
    }
}
