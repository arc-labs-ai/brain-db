//! Provider-agnostic request / response shapes. Spec §22/09 §1.

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

/// A single completion request. Provider clients translate to
/// the wire shape Anthropic / OpenAI expects.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    /// Model identifier as declared in the schema (e.g.
    /// `"claude-haiku-4-5"`). The provider client may add a
    /// provider prefix on the wire.
    pub model: String,
    /// Optional system prompt; provider-specific positioning
    /// (top-level field for Anthropic; first message for OpenAI).
    pub system: Option<String>,
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
            system: None,
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

    /// Approximate input-token count for cost estimation (spec
    /// §22/09 §5). `chars / 4` is a coarse proxy good enough for
    /// budget gating; phase 22+ swaps in real tokenizers.
    #[must_use]
    pub fn approx_input_tokens(&self) -> u64 {
        let mut chars: usize = self
            .system
            .as_deref()
            .map(|s| s.chars().count())
            .unwrap_or(0);
        for m in &self.messages {
            chars += m.content.chars().count();
        }
        (chars as u64) / 4
    }

    /// Composite prompt used by retry-on-validation-fail. Joins
    /// system + user messages with line separators.
    #[must_use]
    pub fn combined_prompt(&self) -> String {
        let mut out = String::new();
        if let Some(s) = &self.system {
            out.push_str(s);
            out.push_str("\n\n");
        }
        for (i, m) in self.messages.iter().enumerate() {
            if i > 0 {
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
    /// Input token count as reported by the provider.
    pub tokens_in: u64,
    /// Output token count as reported by the provider.
    pub tokens_out: u64,
    /// Estimated cost in dollar micro-units (1e-6 USD). Computed
    /// from token counts via the operator's pricing config; the
    /// LLM extractor writes this to the audit row.
    pub cost_micro_usd: u64,
    /// Concrete model version string from the provider (e.g.,
    /// `"claude-haiku-4-5-20240307"`). Phase 22+ uses this for
    /// drift detection.
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
    fn approx_tokens_includes_system_prompt() {
        let mut req = LlmRequest::new("claude-haiku", "x");
        req.system = Some("y".repeat(40)); // 40 chars system + 1 char user = 41 chars
        assert_eq!(req.approx_input_tokens(), 10);
    }

    #[test]
    fn approx_tokens_unicode_safe() {
        // "héllo" is 5 chars (one of them multi-byte). `chars()`
        // counts characters, not bytes.
        let req = LlmRequest::new("m", "héllo");
        assert_eq!(req.approx_input_tokens(), 5 / 4);
    }

    #[test]
    fn combined_prompt_joins_system_and_user() {
        let mut req = LlmRequest::new("m", "user-body");
        req.system = Some("system-instr".into());
        let combined = req.combined_prompt();
        assert!(combined.contains("system-instr"));
        assert!(combined.contains("user-body"));
        assert!(combined.find("system-instr").unwrap() < combined.find("user-body").unwrap());
    }

    #[test]
    fn new_defaults_match_spec() {
        let req = LlmRequest::new("m", "x");
        assert_eq!(req.temperature, 0.0);
        assert_eq!(req.max_tokens, 1024);
        assert_eq!(req.timeout.as_secs(), 30);
        assert!(req.response_schema.is_none());
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn message_role_round_trips_json() {
        let m = LlmMessage {
            role: LlmRole::User,
            content: "hi".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"role\":\"user\""));
        let back: LlmMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
    }
}
