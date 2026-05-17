//! Anthropic Messages API client. Spec §22/09 §2.
//!
//! POST `https://api.anthropic.com/v1/messages`. Reads
//! `ANTHROPIC_API_KEY` at construction; deployments without the
//! key produce a `None` client that the registry materializer
//! routes through degraded extractors.
//!
//! ## Wire shape
//!
//! Request body:
//! ```json
//! {
//!   "model": "claude-haiku-4-5",
//!   "max_tokens": 1024,
//!   "system": "...",            // optional
//!   "messages": [{"role": "user", "content": "..."}]
//! }
//! ```
//!
//! Response body:
//! ```json
//! {
//!   "id": "msg_...",
//!   "model": "claude-haiku-4-5-20240307",
//!   "content": [{"type": "text", "text": "..."}],
//!   "usage": {"input_tokens": 123, "output_tokens": 456},
//!   "stop_reason": "end_turn"
//! }
//! ```

use serde::{Deserialize, Serialize};

use crate::client::{model_id_hash, LlmClient, LlmFuture};
use crate::error::LlmError;
use crate::types::{LlmRequest, LlmResponse, LlmRole};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";

/// Pricing (dollar micro-units per token) for known Anthropic
/// models. Conservative defaults for unknown models per
/// §22/09 §5.
const PRICE_INPUT_PER_TOKEN_DEFAULT: u64 = 1;
const PRICE_OUTPUT_PER_TOKEN_DEFAULT: u64 = 5;

/// Anthropic client. Construct via [`Self::from_env`] or
/// [`Self::with_endpoint`] (the latter is for tests against mock
/// servers).
pub struct AnthropicClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    model_id_hash: u64,
}

impl std::fmt::Debug for AnthropicClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicClient")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key_present", &!self.api_key.is_empty())
            .finish()
    }
}

impl AnthropicClient {
    /// Construct from the `ANTHROPIC_API_KEY` env var. Returns
    /// `None` if unset (the model router treats this as "provider
    /// not configured").
    pub fn from_env(model: impl Into<String>) -> Option<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY").ok()?;
        if key.is_empty() {
            return None;
        }
        Some(Self::new(model, key, DEFAULT_BASE_URL))
    }

    /// Construct with an explicit endpoint (for tests against
    /// mock servers).
    pub fn with_endpoint(
        model: impl Into<String>,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::new(model, api_key.into(), base_url)
    }

    fn new(model: impl Into<String>, api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let model_s = model.into();
        let hash = model_id_hash(&model_s);
        Self {
            http: reqwest::Client::builder()
                .build()
                .expect("reqwest::Client::build is infallible with defaults"),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model_id_hash: hash,
            model: model_s,
        }
    }
}

impl LlmClient for AnthropicClient {
    fn complete<'a>(&'a self, request: LlmRequest) -> LlmFuture<'a> {
        Box::pin(async move {
            if self.api_key.is_empty() {
                return Err(LlmError::Auth {
                    provider: "anthropic",
                });
            }

            let body = AnthropicRequestBody::from(&request);
            let url = format!("{}/v1/messages", self.base_url);

            let resp = tokio::time::timeout(
                request.timeout,
                self.http
                    .post(&url)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", API_VERSION)
                    .header("content-type", "application/json")
                    .json(&body)
                    .send(),
            )
            .await
            .map_err(|_| LlmError::Timeout)?
            .map_err(LlmError::from)?;

            let status = resp.status();
            if status == 401 || status == 403 {
                return Err(LlmError::Auth {
                    provider: "anthropic",
                });
            }
            if status == 429 {
                let retry_after_ms = parse_retry_after(resp.headers());
                return Err(LlmError::RateLimit { retry_after_ms });
            }
            if !status.is_success() {
                let message = resp.text().await.unwrap_or_default();
                return Err(LlmError::ProviderError {
                    status: status.as_u16(),
                    message,
                });
            }

            let payload: AnthropicResponseBody =
                resp.json().await.map_err(|e| LlmError::OutputDecodeFailed {
                    reason: format!("anthropic response JSON decode: {e}"),
                })?;

            decode_anthropic_response(payload, &request)
        })
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn model_id_hash(&self) -> u64 {
        self.model_id_hash
    }
}

// ---------------------------------------------------------------------------
// Wire-shape types — kept private to this module.
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
struct AnthropicRequestBody {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "is_zero_f32")]
    temperature: f32,
}

#[derive(Serialize, Debug)]
struct AnthropicMessage {
    role: &'static str,
    content: String,
}

fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}

impl From<&LlmRequest> for AnthropicRequestBody {
    fn from(req: &LlmRequest) -> Self {
        let messages = req
            .messages
            .iter()
            .map(|m| AnthropicMessage {
                role: match m.role {
                    LlmRole::User => "user",
                    LlmRole::Assistant => "assistant",
                },
                content: m.content.clone(),
            })
            .collect();
        Self {
            model: req.model.clone(),
            max_tokens: req.max_tokens,
            system: req.system.clone(),
            messages,
            temperature: req.temperature,
        }
    }
}

#[derive(Deserialize, Debug)]
struct AnthropicResponseBody {
    model: String,
    content: Vec<AnthropicContent>,
    usage: AnthropicUsage,
}

#[derive(Deserialize, Debug)]
struct AnthropicContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Debug)]
struct AnthropicUsage {
    input_tokens: u64,
    output_tokens: u64,
}

fn decode_anthropic_response(
    payload: AnthropicResponseBody,
    _request: &LlmRequest,
) -> Result<LlmResponse, LlmError> {
    // Concatenate all text-kind content blocks. Tool-use / thinking
    // blocks are ignored — phase 22+ may surface them.
    let mut content = String::new();
    for block in &payload.content {
        if block.kind == "text" {
            if !content.is_empty() {
                content.push_str("\n");
            }
            content.push_str(&block.text);
        }
    }
    if content.is_empty() {
        return Err(LlmError::OutputDecodeFailed {
            reason: "anthropic response had no text-kind content blocks".into(),
        });
    }

    let cost_micro_usd = payload.usage.input_tokens * PRICE_INPUT_PER_TOKEN_DEFAULT
        + payload.usage.output_tokens * PRICE_OUTPUT_PER_TOKEN_DEFAULT;

    Ok(LlmResponse {
        content,
        tokens_in: payload.usage.input_tokens,
        tokens_out: payload.usage.output_tokens,
        cost_micro_usd,
        model_version: payload.model,
    })
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> u64 {
    // Anthropic returns `retry-after` in seconds. We expose it
    // as milliseconds so callers don't have to convert.
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|secs| secs * 1000)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LlmMessage, LlmRole};

    #[test]
    fn from_env_returns_none_when_key_unset() {
        // Save + restore the env var around the test.
        let prior = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");
        let client = AnthropicClient::from_env("claude-haiku-4-5");
        assert!(client.is_none());
        if let Some(p) = prior {
            std::env::set_var("ANTHROPIC_API_KEY", p);
        }
    }

    #[test]
    fn with_endpoint_sets_fields() {
        let c = AnthropicClient::with_endpoint(
            "claude-haiku-4-5",
            "test-key",
            "http://localhost:1234",
        );
        assert_eq!(c.model(), "claude-haiku-4-5");
        assert_ne!(c.model_id_hash(), 0);
    }

    #[test]
    fn request_body_serialises_minimal_shape() {
        let req = LlmRequest::new("claude-haiku-4-5", "hello");
        let body = AnthropicRequestBody::from(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"model\":\"claude-haiku-4-5\""));
        assert!(json.contains("\"max_tokens\":1024"));
        assert!(json.contains("\"role\":\"user\""));
        // temperature 0.0 is skipped per is_zero_f32.
        assert!(!json.contains("\"temperature\""));
    }

    #[test]
    fn request_body_includes_system_when_present() {
        let mut req = LlmRequest::new("m", "u");
        req.system = Some("sys".into());
        let body = AnthropicRequestBody::from(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"system\":\"sys\""));
    }

    #[test]
    fn request_body_includes_temperature_when_nonzero() {
        let mut req = LlmRequest::new("m", "u");
        req.temperature = 0.7;
        let body = AnthropicRequestBody::from(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"temperature\":0.7"));
    }

    #[test]
    fn request_body_translates_message_roles() {
        let req = LlmRequest {
            model: "m".into(),
            system: None,
            messages: vec![
                LlmMessage {
                    role: LlmRole::User,
                    content: "u1".into(),
                },
                LlmMessage {
                    role: LlmRole::Assistant,
                    content: "a1".into(),
                },
                LlmMessage {
                    role: LlmRole::User,
                    content: "u2".into(),
                },
            ],
            response_schema: None,
            temperature: 0.0,
            max_tokens: 256,
            timeout: std::time::Duration::from_secs(5),
        };
        let body = AnthropicRequestBody::from(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.matches("\"role\":\"user\"").count() == 2);
        assert!(json.matches("\"role\":\"assistant\"").count() == 1);
    }

    #[test]
    fn decode_response_joins_text_blocks_and_computes_cost() {
        let payload = AnthropicResponseBody {
            model: "claude-haiku-4-5-20240307".into(),
            content: vec![
                AnthropicContent {
                    kind: "text".into(),
                    text: "first line".into(),
                },
                AnthropicContent {
                    kind: "text".into(),
                    text: "second line".into(),
                },
            ],
            usage: AnthropicUsage {
                input_tokens: 100,
                output_tokens: 50,
            },
        };
        let req = LlmRequest::new("claude-haiku-4-5", "x");
        let resp = decode_anthropic_response(payload, &req).unwrap();
        assert_eq!(resp.content, "first line\nsecond line");
        assert_eq!(resp.tokens_in, 100);
        assert_eq!(resp.tokens_out, 50);
        // 100 * 1 + 50 * 5 = 350 (using default pricing).
        assert_eq!(resp.cost_micro_usd, 350);
        assert_eq!(resp.model_version, "claude-haiku-4-5-20240307");
    }

    #[test]
    fn decode_response_skips_non_text_blocks() {
        let payload = AnthropicResponseBody {
            model: "m".into(),
            content: vec![
                AnthropicContent {
                    kind: "tool_use".into(),
                    text: String::new(),
                },
                AnthropicContent {
                    kind: "text".into(),
                    text: "real content".into(),
                },
            ],
            usage: AnthropicUsage {
                input_tokens: 1,
                output_tokens: 1,
            },
        };
        let req = LlmRequest::new("m", "x");
        let resp = decode_anthropic_response(payload, &req).unwrap();
        assert_eq!(resp.content, "real content");
    }

    #[test]
    fn decode_response_errors_when_no_text_blocks() {
        let payload = AnthropicResponseBody {
            model: "m".into(),
            content: vec![AnthropicContent {
                kind: "tool_use".into(),
                text: String::new(),
            }],
            usage: AnthropicUsage {
                input_tokens: 1,
                output_tokens: 0,
            },
        };
        let req = LlmRequest::new("m", "x");
        let err = decode_anthropic_response(payload, &req).unwrap_err();
        assert!(matches!(
            err,
            LlmError::OutputDecodeFailed { ref reason }
                if reason.contains("no text-kind")
        ));
    }

    #[test]
    fn parse_retry_after_returns_zero_when_absent() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), 0);
    }

    #[test]
    fn parse_retry_after_converts_seconds_to_ms() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "5".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), 5_000);
    }

    #[test]
    fn parse_retry_after_zero_on_unparseable_value() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap());
        // We only support seconds form; HTTP-date form returns 0.
        assert_eq!(parse_retry_after(&headers), 0);
    }
}
