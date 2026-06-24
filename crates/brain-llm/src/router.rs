//! Provider routing: maps a model identifier to its serving
//! provider.
//!
//! Covers Anthropic and OpenAI prefixes. Unknown prefixes return
//! [`Provider::Unknown`]; the LLM-extractor materializer treats
//! that as "no client configured" and registers the extractor in
//! degraded mode.

use std::sync::Arc;

use crate::client::LlmClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Anthropic,
    OpenAi,
    Unknown,
}

impl Provider {
    /// Classify a model identifier into its provider.
    #[must_use]
    pub fn classify(model: &str) -> Self {
        if model.starts_with("claude-") || model.starts_with("anthropic/") {
            return Self::Anthropic;
        }
        if model.starts_with("gpt-")
            || model.starts_with("openai/")
            || model.starts_with("o1-")
            || model.starts_with("o3-")
        {
            return Self::OpenAi;
        }
        Self::Unknown
    }

    /// Human-readable provider name for diagnostics.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Unknown => "unknown",
        }
    }
}

/// Routes a `model` field to one of the configured clients.
///
/// Built at shard startup from the single resolved credential
/// (`[llm] api_key` / `BRAIN__LLM__API_KEY`); the provider is derived
/// from the model id, so exactly one client slot is populated and a
/// missing key produces all-`None` slots. The extractor materializer
/// asks the router for a client at `materialize_llm_extractor` time.
#[derive(Default)]
pub struct ModelRouter {
    anthropic: Option<Arc<dyn LlmClient>>,
    openai: Option<Arc<dyn LlmClient>>,
}

impl ModelRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_anthropic(mut self, client: Arc<dyn LlmClient>) -> Self {
        self.anthropic = Some(client);
        self
    }

    #[must_use]
    pub fn with_openai(mut self, client: Arc<dyn LlmClient>) -> Self {
        self.openai = Some(client);
        self
    }

    /// Look up a client by model identifier. Returns `None` when
    /// the provider is unknown OR the matching client is
    /// unconfigured.
    pub fn resolve(&self, model: &str) -> Option<Arc<dyn LlmClient>> {
        match Provider::classify(model) {
            Provider::Anthropic => self.anthropic.clone(),
            Provider::OpenAi => self.openai.clone(),
            Provider::Unknown => None,
        }
    }

    /// True iff at least one provider is configured.
    #[must_use]
    pub fn has_any_provider(&self) -> bool {
        self.anthropic.is_some() || self.openai.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_anthropic_models() {
        assert_eq!(Provider::classify("claude-haiku-4-5"), Provider::Anthropic);
        assert_eq!(Provider::classify("claude-sonnet-4-6"), Provider::Anthropic);
        assert_eq!(
            Provider::classify("anthropic/claude-opus-4"),
            Provider::Anthropic
        );
    }

    #[test]
    fn classify_openai_models() {
        assert_eq!(Provider::classify("gpt-4o-mini"), Provider::OpenAi);
        assert_eq!(Provider::classify("gpt-4o"), Provider::OpenAi);
        assert_eq!(Provider::classify("o1-preview"), Provider::OpenAi);
        assert_eq!(Provider::classify("o3-mini"), Provider::OpenAi);
        assert_eq!(Provider::classify("openai/gpt-4o"), Provider::OpenAi);
    }

    #[test]
    fn classify_unknown_returns_unknown() {
        assert_eq!(Provider::classify("llama-3"), Provider::Unknown);
        assert_eq!(Provider::classify(""), Provider::Unknown);
        assert_eq!(Provider::classify("mistral-7b"), Provider::Unknown);
    }

    #[test]
    fn router_returns_none_when_unconfigured() {
        let r = ModelRouter::new();
        assert!(r.resolve("claude-haiku-4-5").is_none());
        assert!(!r.has_any_provider());
    }

    #[test]
    fn router_routes_to_configured_anthropic() {
        let client: Arc<dyn LlmClient> = Arc::new(crate::AnthropicClient::with_endpoint(
            "claude-haiku-4-5",
            "test-key",
            "http://localhost",
        ));
        let r = ModelRouter::new().with_anthropic(client);
        assert!(r.resolve("claude-haiku-4-5").is_some());
        assert!(r.resolve("gpt-4o").is_none()); // not configured
        assert!(r.resolve("llama-3").is_none()); // unknown provider
        assert!(r.has_any_provider());
    }

    #[test]
    fn provider_names_are_lowercase_ascii() {
        assert_eq!(Provider::Anthropic.name(), "anthropic");
        assert_eq!(Provider::OpenAi.name(), "openai");
        assert_eq!(Provider::Unknown.name(), "unknown");
    }
}
