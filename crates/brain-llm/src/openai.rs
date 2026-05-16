//! OpenAI Chat Completions client. Spec §22/09 §2.
//!
//! POST `https://api.openai.com/v1/chat/completions`. Reads
//! `OPENAI_API_KEY` at construction.
//!
//! ## Wire shape (request)
//!
//! ```json
//! {
//!   "model": "gpt-4o-mini",
//!   "messages": [
//!     {"role": "system",  "content": "..."},   // optional
//!     {"role": "user",    "content": "..."}
//!   ],
//!   "max_completion_tokens": 1024,
//!   "temperature": 0,
//!   "response_format": {                       // optional
//!     "type": "json_schema",
//!     "json_schema": {
//!       "name": "brain_extractor_output",
//!       "strict": true,
//!       "schema": { ... }
//!     }
//!   }
//! }
//! ```
//!
//! ## Wire shape (response)
//!
//! ```json
//! {
//!   "id": "chatcmpl-...",
//!   "model": "gpt-4o-mini-2024-07-18",
//!   "choices": [
//!     {
//!       "index": 0,
//!       "message": {"role": "assistant", "content": "..."},
//!       "finish_reason": "stop"
//!     }
//!   ],
//!   "usage": {
//!     "prompt_tokens": 123,
//!     "completion_tokens": 456,
//!     "total_tokens": 579
//!   }
//! }
//! ```

use serde::{Deserialize, Serialize};

use crate::client::{model_id_hash, LlmClient, LlmFuture};
use crate::error::LlmError;
use crate::types::{LlmRequest, LlmResponse, LlmRole};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// Pricing (dollar micro-units per token) for unknown OpenAI
/// models. Conservative; phase 22+ ships a pricing table per
/// model (§22/09 §5 + §22/07 Q-llm-3).
const PRICE_INPUT_PER_TOKEN_DEFAULT: u64 = 1;
const PRICE_OUTPUT_PER_TOKEN_DEFAULT: u64 = 4;

/// Structured-output schema name pinned for all brain LLM
/// extractor calls. OpenAI requires a non-empty name when
/// `response_format = json_schema`.
const STRUCTURED_OUTPUT_NAME: &str = "brain_extractor_output";

pub struct OpenAIClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    model_id_hash: u64,
}

impl std::fmt::Debug for OpenAIClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAIClient")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key_present", &!self.api_key.is_empty())
            .finish()
    }
}

impl OpenAIClient {
    /// Construct from the `OPENAI_API_KEY` env var. Returns
    /// `None` if unset.
    pub fn from_env(model: impl Into<String>) -> Option<Self> {
        let key = std::env::var("OPENAI_API_KEY").ok()?;
        if key.is_empty() {
            return None;
        }
        Some(Self::new(model, key, DEFAULT_BASE_URL))
    }

    /// Construct with an explicit endpoint (mock-server tests).
    pub fn with_endpoint(
        model: impl Into<String>,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::new(model, api_key.into(), base_url)
    }

    fn new(
        model: impl Into<String>,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
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

impl LlmClient for OpenAIClient {
    fn complete<'a>(&'a self, request: LlmRequest) -> LlmFuture<'a> {
        Box::pin(async move {
            if self.api_key.is_empty() {
                return Err(LlmError::Auth {
                    provider: "openai",
                });
            }

            let body = OpenAIRequestBody::from(&request);
            let url = format!("{}/v1/chat/completions", self.base_url);

            let resp = tokio::time::timeout(
                request.timeout,
                self.http
                    .post(&url)
                    .bearer_auth(&self.api_key)
                    .header("content-type", "application/json")
                    .json(&body)
                    .send(),
            )
            .await
            .map_err(|_| LlmError::Timeout)?
            .map_err(LlmError::from)?;

            let status = resp.status();
            if status == 401 || status == 403 {
                return Err(LlmError::Auth { provider: "openai" });
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

            let payload: OpenAIResponseBody =
                resp.json().await.map_err(|e| LlmError::OutputDecodeFailed {
                    reason: format!("openai response JSON decode: {e}"),
                })?;

            decode_openai_response(payload)
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
// Wire-shape types — kept private.
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
struct OpenAIRequestBody {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "is_zero_f32")]
    temperature: f32,
    max_completion_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<OpenAIResponseFormat>,
}

#[derive(Serialize, Debug)]
struct OpenAIMessage {
    role: &'static str,
    content: String,
}

#[derive(Serialize, Debug)]
struct OpenAIResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
    json_schema: OpenAIJsonSchema,
}

#[derive(Serialize, Debug)]
struct OpenAIJsonSchema {
    name: &'static str,
    strict: bool,
    schema: serde_json::Value,
}

fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}

impl From<&LlmRequest> for OpenAIRequestBody {
    fn from(req: &LlmRequest) -> Self {
        // OpenAI puts the system prompt as the first message with
        // role="system". For o-series models the role becomes
        // "developer"; phase 21 sticks with "system" for
        // simplicity (OpenAI accepts both transparently).
        let mut messages: Vec<OpenAIMessage> =
            Vec::with_capacity(req.messages.len() + req.system.as_ref().map_or(0, |_| 1));
        if let Some(sys) = &req.system {
            messages.push(OpenAIMessage {
                role: "system",
                content: sys.clone(),
            });
        }
        for m in &req.messages {
            messages.push(OpenAIMessage {
                role: match m.role {
                    LlmRole::User => "user",
                    LlmRole::Assistant => "assistant",
                },
                content: m.content.clone(),
            });
        }

        let response_format = req.response_schema.as_ref().map(|schema| OpenAIResponseFormat {
            kind: "json_schema",
            json_schema: OpenAIJsonSchema {
                name: STRUCTURED_OUTPUT_NAME,
                strict: true,
                schema: schema.clone(),
            },
        });

        Self {
            model: req.model.clone(),
            messages,
            temperature: req.temperature,
            max_completion_tokens: req.max_tokens,
            response_format,
        }
    }
}

#[derive(Deserialize, Debug)]
struct OpenAIResponseBody {
    model: String,
    choices: Vec<OpenAIChoice>,
    usage: OpenAIUsage,
}

#[derive(Deserialize, Debug)]
struct OpenAIChoice {
    message: OpenAIChoiceMessage,
}

#[derive(Deserialize, Debug)]
struct OpenAIChoiceMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize, Debug)]
struct OpenAIUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

fn decode_openai_response(payload: OpenAIResponseBody) -> Result<LlmResponse, LlmError> {
    let content = payload
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .ok_or_else(|| LlmError::OutputDecodeFailed {
            reason: "openai response had no choices[0].message.content".into(),
        })?;
    if content.is_empty() {
        return Err(LlmError::OutputDecodeFailed {
            reason: "openai response content was empty".into(),
        });
    }

    let cost_micro_usd = payload.usage.prompt_tokens * PRICE_INPUT_PER_TOKEN_DEFAULT
        + payload.usage.completion_tokens * PRICE_OUTPUT_PER_TOKEN_DEFAULT;

    Ok(LlmResponse {
        content,
        tokens_in: payload.usage.prompt_tokens,
        tokens_out: payload.usage.completion_tokens,
        cost_micro_usd,
        model_version: payload.model,
    })
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> u64 {
    // OpenAI returns `retry-after` in seconds (sometimes as a
    // float, sometimes integer). Phase 21 reads seconds-as-integer
    // and converts; non-integer / absent → 0.
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
        .map(|secs| (secs * 1000.0) as u64)
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
        let prior = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");
        let client = OpenAIClient::from_env("gpt-4o-mini");
        assert!(client.is_none());
        if let Some(p) = prior {
            std::env::set_var("OPENAI_API_KEY", p);
        }
    }

    #[test]
    fn with_endpoint_sets_fields() {
        let c = OpenAIClient::with_endpoint("gpt-4o-mini", "test-key", "http://localhost:1234");
        assert_eq!(c.model(), "gpt-4o-mini");
        assert_ne!(c.model_id_hash(), 0);
    }

    #[test]
    fn request_body_minimal_shape() {
        let req = LlmRequest::new("gpt-4o-mini", "hello");
        let body = OpenAIRequestBody::from(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"model\":\"gpt-4o-mini\""));
        assert!(json.contains("\"max_completion_tokens\":1024"));
        assert!(json.contains("\"role\":\"user\""));
        assert!(!json.contains("\"temperature\"")); // 0.0 skipped
        assert!(!json.contains("\"response_format\"")); // None skipped
    }

    #[test]
    fn request_body_prepends_system_message() {
        let mut req = LlmRequest::new("gpt-4o-mini", "user-body");
        req.system = Some("sys-instr".into());
        let body = OpenAIRequestBody::from(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"role\":\"system\",\"content\":\"sys-instr\""));
        // System message comes before the user message.
        let sys_idx = json.find("\"role\":\"system\"").unwrap();
        let user_idx = json.find("\"role\":\"user\"").unwrap();
        assert!(sys_idx < user_idx);
    }

    #[test]
    fn request_body_includes_temperature_when_nonzero() {
        let mut req = LlmRequest::new("m", "x");
        req.temperature = 0.7;
        let body = OpenAIRequestBody::from(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"temperature\":0.7"));
    }

    #[test]
    fn request_body_includes_response_format_when_schema_set() {
        let mut req = LlmRequest::new("gpt-4o-mini", "x");
        req.response_schema = Some(serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        }));
        let body = OpenAIRequestBody::from(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"response_format\""));
        assert!(json.contains("\"type\":\"json_schema\""));
        assert!(json.contains("\"strict\":true"));
        assert!(json.contains("\"name\":\"brain_extractor_output\""));
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
        let body = OpenAIRequestBody::from(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert_eq!(json.matches("\"role\":\"user\"").count(), 2);
        assert_eq!(json.matches("\"role\":\"assistant\"").count(), 1);
    }

    #[test]
    fn decode_response_extracts_first_choice_content_and_cost() {
        let payload = OpenAIResponseBody {
            model: "gpt-4o-mini-2024-07-18".into(),
            choices: vec![OpenAIChoice {
                message: OpenAIChoiceMessage {
                    content: Some("the answer".into()),
                },
            }],
            usage: OpenAIUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
            },
        };
        let resp = decode_openai_response(payload).unwrap();
        assert_eq!(resp.content, "the answer");
        assert_eq!(resp.tokens_in, 100);
        assert_eq!(resp.tokens_out, 50);
        // 100 * 1 + 50 * 4 = 300 (default pricing).
        assert_eq!(resp.cost_micro_usd, 300);
        assert_eq!(resp.model_version, "gpt-4o-mini-2024-07-18");
    }

    #[test]
    fn decode_response_errors_when_choices_empty() {
        let payload = OpenAIResponseBody {
            model: "m".into(),
            choices: vec![],
            usage: OpenAIUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
            },
        };
        let err = decode_openai_response(payload).unwrap_err();
        assert!(matches!(err, LlmError::OutputDecodeFailed { .. }));
    }

    #[test]
    fn decode_response_errors_when_content_missing() {
        let payload = OpenAIResponseBody {
            model: "m".into(),
            choices: vec![OpenAIChoice {
                message: OpenAIChoiceMessage { content: None },
            }],
            usage: OpenAIUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
            },
        };
        let err = decode_openai_response(payload).unwrap_err();
        assert!(matches!(err, LlmError::OutputDecodeFailed { .. }));
    }

    #[test]
    fn decode_response_errors_when_content_empty_string() {
        let payload = OpenAIResponseBody {
            model: "m".into(),
            choices: vec![OpenAIChoice {
                message: OpenAIChoiceMessage {
                    content: Some(String::new()),
                },
            }],
            usage: OpenAIUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
            },
        };
        let err = decode_openai_response(payload).unwrap_err();
        assert!(matches!(
            err,
            LlmError::OutputDecodeFailed { ref reason } if reason.contains("empty")
        ));
    }

    #[test]
    fn parse_retry_after_integer_seconds_to_ms() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "3".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), 3_000);
    }

    #[test]
    fn parse_retry_after_fractional_seconds_to_ms() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "2.5".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), 2_500);
    }

    #[test]
    fn parse_retry_after_absent_returns_zero() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), 0);
    }
}
