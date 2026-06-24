//! Provider-agnostic request / response shapes.

use serde::{Deserialize, Serialize};

/// One message in an LLM completion request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: LlmRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmRole {
    User,
    Assistant,
}

/// A system content block that may be marked for server-side prompt
/// caching.
///
/// Anthropic caches blocks tagged `cache_control: ephemeral` for 5
/// minutes (rolling) and returns `cache_creation_input_tokens` +
/// `cache_read_input_tokens` on the response. Steady-state target
/// for repeated extractor / judge calls is a read ratio ≥ 0.7.
///
/// Up to 4 cache breakpoints per request — typically used for a
/// stable role block + a stable schema block, leaving the per-call
/// user message uncached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemBlock {
    pub text: String,
    pub cache: bool,
}

impl SystemBlock {
    /// Block whose text is stable across calls — mark it for server-side
    /// caching so repeated requests amortise its input-token cost.
    pub fn cached(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cache: true,
        }
    }

    /// Block whose text changes call-to-call. Not eligible for caching;
    /// counts toward the live input-token tally every time.
    pub fn live(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cache: false,
        }
    }
}

/// A single completion request. Provider clients translate to
/// the wire shape Anthropic / OpenAI expects.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    /// Model identifier as declared in the schema (e.g.
    /// `"claude-haiku-4-5"`). The provider client may add a
    /// provider prefix on the wire.
    pub model: String,
    /// Ordered system content blocks. Anthropic emits these as the
    /// top-level `system` array (with per-block `cache_control`
    /// honoured); OpenAI flattens them into one system message.
    /// Empty vec → no system context.
    pub system_blocks: Vec<SystemBlock>,
    /// Conversation turns. The LLM extractor typically sends one
    /// `User` message with the rendered prompt + memory text.
    pub messages: Vec<LlmMessage>,
    /// Optional response schema (JSON Schema draft-7).
    /// Provider clients pass this through their structured-output
    /// mode where supported; absent → free-form text.
    pub response_schema: Option<serde_json::Value>,
    /// Sampling temperature. Defaults to 0.0 for determinism.
    pub temperature: f32,
    /// Hard cap on response tokens.
    pub max_tokens: u32,
    /// Per-call HTTP timeout.
    pub timeout: std::time::Duration,
}

impl LlmRequest {
    /// New request with `temperature = 0.0`, `max_tokens = 1024`,
    /// 30-second timeout. Override fields with field assignment.
    pub fn new(model: impl Into<String>, user_text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system_blocks: Vec::new(),
            messages: vec![LlmMessage {
                role: LlmRole::User,
                content: user_text.into(),
            }],
            response_schema: None,
            temperature: 0.0,
            max_tokens: 1024,
            timeout: std::time::Duration::from_secs(30),
        }
    }

    /// Convenience: set a single uncached system block. Replaces any
    /// existing system blocks. Use field assignment on `system_blocks`
    /// directly for the role + schema split that gets prompt caching.
    pub fn with_system(mut self, text: impl Into<String>) -> Self {
        self.system_blocks = vec![SystemBlock::live(text)];
        self
    }

    /// Approximate input-token count for cost estimation.
    /// `chars / 4` is a coarse proxy good enough for budget gating.
    #[must_use]
    pub fn approx_input_tokens(&self) -> u64 {
        let mut chars: usize = self
            .system_blocks
            .iter()
            .map(|b| b.text.chars().count())
            .sum();
        for m in &self.messages {
            chars += m.content.chars().count();
        }
        (chars as u64) / 4
    }

    /// Composite prompt used by retry-on-validation-fail. Joins
    /// system blocks + user messages with line separators.
    #[must_use]
    pub fn combined_prompt(&self) -> String {
        let mut out = String::new();
        for b in &self.system_blocks {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&b.text);
        }
        for m in &self.messages {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&m.content);
        }
        out
    }
}

/// LLM completion response. Provider clients normalise the
/// provider's reply shape into this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmResponse {
    /// The model's generated text content. For structured-output
    /// requests this is the JSON string ready for schema
    /// validation.
    pub content: String,
    /// Input token count as reported by the provider. Excludes
    /// cached-read and cache-creation tokens (Anthropic reports
    /// them separately).
    pub tokens_in: u64,
    /// Output token count as reported by the provider.
    pub tokens_out: u64,
    /// Tokens billed at the cache-write rate (Anthropic returns this
    /// on the first call that populates a cache breakpoint).
    /// Zero when the provider doesn't expose prompt caching or no
    /// cache hit / write happened.
    pub cache_creation_input_tokens: u64,
    /// Tokens served from the server-side cache (Anthropic). These
    /// are billed at a discount and don't count toward `tokens_in`.
    pub cache_read_input_tokens: u64,
    /// Estimated cost in dollar micro-units (1e-6 USD). Computed
    /// from token counts via the operator's pricing config; the
    /// LLM extractor writes this to the audit row.
    pub cost_micro_usd: u64,
    /// Concrete model version string from the provider (e.g.,
    /// `"claude-haiku-4-5-20240307"`). Used downstream for drift
    /// detection.
    pub model_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approx_tokens_uses_char_count() {
        let req = LlmRequest::new("claude-haiku", "hello world"); // 11 chars
        assert_eq!(req.approx_input_tokens(), 2);
    }

    #[test]
    fn approx_tokens_includes_system_blocks() {
        let mut req = LlmRequest::new("claude-haiku", "x");
        req.system_blocks = vec![SystemBlock::cached("y".repeat(40))]; // 40 + 1 = 41 chars
        assert_eq!(req.approx_input_tokens(), 10);
    }

    #[test]
    fn approx_tokens_sums_multiple_system_blocks() {
        let mut req = LlmRequest::new("m", "");
        req.system_blocks = vec![
            SystemBlock::cached("a".repeat(20)),
            SystemBlock::live("b".repeat(20)),
        ];
        assert_eq!(req.approx_input_tokens(), 40 / 4);
    }

    #[test]
    fn approx_tokens_unicode_safe() {
        let req = LlmRequest::new("m", "héllo");
        assert_eq!(req.approx_input_tokens(), 5 / 4);
    }

    #[test]
    fn combined_prompt_joins_system_blocks_and_user() {
        let mut req = LlmRequest::new("m", "user-body");
        req.system_blocks = vec![
            SystemBlock::cached("role-block"),
            SystemBlock::cached("schema-block"),
        ];
        let combined = req.combined_prompt();
        assert!(combined.contains("role-block"));
        assert!(combined.contains("schema-block"));
        assert!(combined.contains("user-body"));
        assert!(combined.find("role-block").unwrap() < combined.find("schema-block").unwrap());
        assert!(combined.find("schema-block").unwrap() < combined.find("user-body").unwrap());
    }

    #[test]
    fn with_system_replaces_blocks_uncached() {
        let req = LlmRequest::new("m", "u").with_system("sys");
        assert_eq!(req.system_blocks.len(), 1);
        assert!(!req.system_blocks[0].cache);
        assert_eq!(req.system_blocks[0].text, "sys");
    }

    #[test]
    fn system_block_constructors_set_cache_flag() {
        assert!(SystemBlock::cached("x").cache);
        assert!(!SystemBlock::live("x").cache);
    }
}
