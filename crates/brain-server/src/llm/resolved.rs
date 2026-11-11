//! Consolidation summarization through the namespace's own LLM credential.
//!
//! A consolidation run condenses one tenant's memories, so it is paid for by
//! that tenant: the summarizer resolves a credential for the namespace whose
//! cluster it is summarizing, exactly as extraction, HyPE and entity
//! disambiguation do. There is no operator credential for consolidation and
//! no second provider stack — the resolved client is the same one every other
//! model-backed stage uses, already bridged onto the runtime where its HTTP
//! is valid.
//!
//! The provider follows from what the namespace registered. A tenant on
//! Anthropic consolidates through Anthropic without configuring anything,
//! which is why this backend is named for the resolution rather than for a
//! provider.

use std::sync::Arc;
use std::time::Duration;

use brain_llm::{CredentialOutcome, LlmCredentialResolver, LlmMessage, LlmRequest, LlmRole};
use brain_workers::{Summarizer, SummarizerError};

use crate::llm::prompt::build_consolidation_prompt;

/// Summarizes through whichever credential the namespace registered.
pub(crate) struct ResolvedSummarizer {
    resolver: Arc<dyn LlmCredentialResolver>,
    temperature: f32,
    max_tokens: u32,
    timeout: Duration,
}

impl ResolvedSummarizer {
    pub(crate) fn new(
        resolver: Arc<dyn LlmCredentialResolver>,
        temperature: f32,
        max_summary_chars: u32,
        timeout: Duration,
    ) -> Self {
        // Roughly four characters per token, rounded up so a short
        // completion still fits.
        let max_tokens = (max_summary_chars / 4).max(64);
        Self {
            resolver,
            temperature,
            max_tokens,
            timeout,
        }
    }
}

impl Summarizer for ResolvedSummarizer {
    fn enabled(&self) -> bool {
        true
    }

    fn summarize<'a>(
        &'a self,
        namespace: &'a str,
        memories: &'a [&'a str],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, SummarizerError>> + 'a>>
    {
        Box::pin(async move {
            let credential = match self.resolver.client_for(namespace, None).await {
                CredentialOutcome::Ready(c) => c,
                // A tenant who has registered no key is not a failure to
                // report upstream: their clusters are simply left
                // unconsolidated until they do.
                CredentialOutcome::Missing => {
                    return Err(SummarizerError::Failed(
                        self.resolver.missing_reason(namespace),
                    ));
                }
                CredentialOutcome::Unavailable => {
                    return Err(SummarizerError::Failed(format!(
                        "llm credential for namespace {namespace} unavailable"
                    )));
                }
            };

            let (system, user) = build_consolidation_prompt(memories);
            let request = LlmRequest {
                model: credential.client.model().to_string(),
                system_blocks: vec![brain_llm::types::SystemBlock::cached(system.to_owned())],
                messages: vec![LlmMessage {
                    role: LlmRole::User,
                    content: user,
                }],
                response_schema: None,
                temperature: self.temperature,
                max_tokens: self.max_tokens,
                timeout: self.timeout,
            };

            credential
                .client
                .complete(request)
                .await
                .map(|response| response.content)
                .map_err(|e| SummarizerError::Failed(format!("summarizer: {e}")))
        })
    }
}
