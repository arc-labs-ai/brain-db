//! Summarizer seam for the consolidation worker. Spec §11/03 §6.
//!
//! Brain doesn't bundle an LLM; production deployments inject a
//! `Summarizer` impl that calls their LLM service. The default
//! [`DisabledSummarizer`] makes the consolidation worker a no-op,
//! matching spec §6 / §16: "For deployments without an LLM,
//! consolidation is disabled."

use std::future::Future;
use std::pin::Pin;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SummarizerError {
    /// Spec §16: no LLM configured. The consolidation worker treats
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
    fn summarize<'a>(
        &'a self,
        memories: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = Result<String, SummarizerError>> + 'a>>;
}

/// The substrate-default summarizer. Always returns
/// `SummarizerError::Disabled`. Spec §16: consolidation is a no-op
/// until an LLM-backed impl is injected.
pub struct DisabledSummarizer;

impl Summarizer for DisabledSummarizer {
    fn summarize<'a>(
        &'a self,
        _memories: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = Result<String, SummarizerError>> + 'a>> {
        Box::pin(async { Err(SummarizerError::Disabled) })
    }
}
