//! Summarizer seam for the consolidation worker.
//!
//! Brain doesn't bundle an LLM; production deployments inject a
//! `Summarizer` impl that calls their LLM service. The default
//! [`DisabledSummarizer`] makes the consolidation worker a no-op,
//! matching: "For deployments without an LLM,
//! consolidation is disabled."

use std::future::Future;
use std::pin::Pin;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SummarizerError {
    /// no LLM configured. The consolidation worker treats
    /// this as the "disabled" state and produces zero consolidations
    /// per cycle.
    #[error("summarizer disabled")]
    Disabled,
    /// LLM service unreachable, timeout, or returned an error.
    #[error("summarizer call failed: {0}")]
    Failed(String),
}

/// Async summarization. Implementations call into an LLM and return
/// the consolidated text.
///
/// We use the `Pin<Box<Future>>` pattern (same as `WriterHandle`) to
/// avoid pulling in `async-trait`.
pub trait Summarizer: Send + Sync + 'static {
    /// Whether this deployment has a summarization backend at all.
    ///
    /// The consolidation worker checks this once per cycle and skips the
    /// whole pass when it is false, so a deployment without consolidation
    /// does no clustering work. It replaces probing with an empty slice,
    /// which forced every backend to carry a special case for a call that
    /// was not really a summarization request.
    fn enabled(&self) -> bool;

    /// Summarize `memories`, billing the call to `namespace`.
    ///
    /// The namespace is required rather than inferred: a consolidation run
    /// is paid for by the tenant whose memories it consolidates, so a
    /// backend that needs a credential resolves that tenant's. The
    /// consolidation worker clusters strictly within one
    /// `(namespace, space, session)`, so every call has exactly one owner.
    fn summarize<'a>(
        &'a self,
        namespace: &'a str,
        memories: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = Result<String, SummarizerError>> + 'a>>;
}

/// The substrate-default summarizer. Always returns
/// `SummarizerError::Disabled`: consolidation is a no-op
/// until an LLM-backed impl is injected.
pub struct DisabledSummarizer;

impl Summarizer for DisabledSummarizer {
    fn enabled(&self) -> bool {
        false
    }

    fn summarize<'a>(
        &'a self,
        _namespace: &'a str,
        _memories: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = Result<String, SummarizerError>> + 'a>> {
        Box::pin(async { Err(SummarizerError::Disabled) })
    }
}
