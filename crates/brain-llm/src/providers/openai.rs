//! OpenAI Chat Completions client.
//!
//! POST `https://api.openai.com/v1/chat/completions`. Constructed with
//! the credential the caller's namespace registered.
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
use crate::pricing::ModelPricing;
use crate::providers::runtime::run_on_provider_runtime;
use crate::types::{LlmRequest, LlmResponse, LlmRole};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

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
    /// Construct with an explicit API key against the default endpoint.
    /// The key comes from the single resolved credential (`[llm] api_key`
    /// from the credential store). Returns `None` for an empty key so
    /// callers can fall back uniformly.
    pub fn with_key(model: impl Into<String>, api_key: impl Into<String>) -> Option<Self> {
        let key = api_key.into();
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
                .expect("invariant: reqwest::Client::build is infallible with defaults"),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model_id_hash: hash,
            model: model_s,
        }
    }
}

impl LlmClient for OpenAIClient {
    fn complete<'a>(&'a self, request: LlmRequest) -> LlmFuture<'a> {
        // Owned clones so the task is `'static`. `reqwest::Client` is an Arc
        // inside, so this shares one connection pool rather than building one
        // per completion.
        let http = self.http.clone();
        let api_key = self.api_key.clone();
        let url = format!("{}/v1/chat/completions", self.base_url);
        // Weighed before the request is consumed below; the guard lives
        // inside the async block so occupancy spans the whole round trip
        // and is released even if the call is cancelled.
        let weight = crate::inflight::request_bytes(&request);
        let body = OpenAIRequestBody::from(&request);
        let timeout = request.timeout;

        Box::pin(async move {
            // Admitted by BYTES against the machine's budget, not by a count.
            // Parking here is what backpressure looks like: the durable queue
            // keeps the work, so this defers a call rather than losing it.
            let _admitted = crate::admission::global().admit_bytes(weight).await;
            if api_key.is_empty() {
                return Err(LlmError::Auth {
                    provider: crate::router::PROVIDER_OPENAI,
                });
            }

            // Every line below runs on the provider runtime, not on the
            // caller's executor, which is a glommio shard thread with no Tokio
            // reactor. See `providers::runtime`.
            run_on_provider_runtime(
                async move {
                    let mut body = body;
                    let mut resp = send_chat(&http, &url, &api_key, &body, timeout).await?;

                    // Some models accept only their default temperature and
                    // reject any explicit value with a 400. Retry once without
                    // the field rather than fail the call: the caller's
                    // temperature is a preference, and for HyPE — which is
                    // mandatory and always-on — losing the whole generation
                    // over a sampling nudge is the wrong trade.
                    //
                    // Dropping it is expressible because `temperature` is
                    // skipped when zero, which is also why every caller that
                    // leaves it at the default already works on these models.
                    if resp.status() == 400 && body.temperature != 0.0 {
                        let detail = resp.text().await.unwrap_or_default();
                        if rejects_temperature(&detail) {
                            tracing::debug!(
                                target: "brain_llm::openai",
                                model = %body.model,
                                "model rejected an explicit temperature; retrying with its default",
                            );
                            body.temperature = 0.0;
                            resp = send_chat(&http, &url, &api_key, &body, timeout).await?;
                        } else {
                            return Err(LlmError::ProviderError {
                                status: 400,
                                message: detail,
                            });
                        }
                    }

                    let status = resp.status();
                    if status == 401 || status == 403 {
                        return Err(LlmError::Auth {
                            provider: crate::router::PROVIDER_OPENAI,
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

                    let payload: OpenAIResponseBody =
                        resp.json()
                            .await
                            .map_err(|e| LlmError::OutputDecodeFailed {
                                reason: format!("openai response JSON decode: {e}"),
                            })?;

                    decode_openai_response(payload, &request)
                },
                || LlmError::ProviderRuntimeUnavailable,
                |e| LlmError::RequestTaskFailed {
                    reason: format!("openai completion task failed: {e}"),
                },
            )
            .await
        })
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn model_id_hash(&self) -> u64 {
        self.model_id_hash
    }
}

/// One chat-completions POST. Split out so the temperature retry can repeat it
/// without duplicating the request construction.
async fn send_chat(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &OpenAIRequestBody,
    timeout: std::time::Duration,
) -> Result<reqwest::Response, LlmError> {
    tokio::time::timeout(
        timeout,
        http.post(url)
            .bearer_auth(api_key)
            .header("content-type", "application/json")
            .json(body)
            .send(),
    )
    .await
    .map_err(|_| LlmError::Timeout)?
    .map_err(LlmError::from)
}

/// Whether a 400 body is the provider refusing an explicit temperature.
///
/// Matched on the error's own `param`/`code` fields rather than its prose,
/// which is not a stable interface. Both must be present: a 400 that merely
/// mentions temperature somewhere else is a different fault and must keep its
/// own error.
fn rejects_temperature(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body).is_ok_and(|v| {
        let err = &v["error"];
        err["param"] == "temperature" && err["code"] == "unsupported_value"
    })
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
        // "developer"; we stick with "system" — OpenAI accepts
        // both transparently. Anthropic-style prompt caching has no
        // direct equivalent on the Chat Completions API, so cached
        // and live blocks both fold into a single concatenated system
        // message here; the cache flag is ignored on this provider.
        let extra = if req.system_blocks.is_empty() { 0 } else { 1 };
        let mut messages: Vec<OpenAIMessage> = Vec::with_capacity(req.messages.len() + extra);
        if !req.system_blocks.is_empty() {
            let mut combined = String::new();
            for b in &req.system_blocks {
                if !combined.is_empty() {
                    combined.push_str("\n\n");
                }
                combined.push_str(&b.text);
            }
            messages.push(OpenAIMessage {
                role: "system",
                content: combined,
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

        let response_format = req
            .response_schema
            .as_ref()
            .map(|schema| OpenAIResponseFormat {
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
    /// `"stop"` on a clean finish, `"length"` when the output hit
    /// `max_tokens`. Absent on some proxies, so it defaults to `None`
    /// and an unknown reason is treated as NOT truncated — a false
    /// "truncated" would mask a real schema failure.
    #[serde(default)]
    finish_reason: Option<String>,
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
    #[serde(default)]
    prompt_tokens_details: OpenAIPromptTokensDetails,
}

#[derive(Deserialize, Debug, Default)]
struct OpenAIPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

fn decode_openai_response(
    payload: OpenAIResponseBody,
    request: &LlmRequest,
) -> Result<LlmResponse, LlmError> {
    let first_choice = payload.choices.first();
    let finish_reason = first_choice.and_then(|c| c.finish_reason.as_deref());
    let truncated = finish_reason == Some("length");
    let (content, content_was_null, choice_was_absent) = match first_choice {
        Some(choice) => match choice.message.content.clone() {
            Some(content) => (content, false, false),
            None => (String::new(), true, false),
        },
        None => (String::new(), false, true),
    };
    let model = if payload.model.is_empty() {
        request.model.clone()
    } else {
        payload.model.clone()
    };
    let cached = payload
        .usage
        .prompt_tokens_details
        .cached_tokens
        .min(payload.usage.prompt_tokens);
    let live_in = payload.usage.prompt_tokens - cached;
    let cost_micro_usd = match ModelPricing::for_model(&model) {
        Some(pricing) => {
            pricing.cost_micro_usd(live_in, payload.usage.completion_tokens, 0, cached)
        }
        // Zero with a warning is deliberate: an invented rate silently corrupts every downstream total.
        None => {
            tracing::warn!(target: "brain_llm::pricing", model = %model, tokens_in = live_in, tokens_out = payload.usage.completion_tokens, "no published pricing for this model; token counts are recorded but its cost is reported as zero — add it to brain_llm::pricing::ModelPricing");
            0
        }
    };
    let resp = LlmResponse {
        content,
        tokens_in: live_in,
        tokens_out: payload.usage.completion_tokens,
        // OpenAI reports cached input inside prompt_tokens; subtract it so
        // tokens_in remains the live-input count while cached tokens are billed
        // at the provider's cached-input rate.
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
        cost_micro_usd,
        model_version: payload.model,
        truncated,
    };
    if resp.content.is_empty() {
        // The priced response travels with the error because the call cost real money and this
        // is the only record of that spend. The finish reasons stay distinct because their
        // remedies differ: a spent completion budget, provider-side filtering, a tool-call-only
        // reply and a response carrying no choices each call for a different operator action.
        let reason = if truncated {
            "openai returned no content: the completion budget was spent \
                 before any output (finish_reason=length). On a reasoning \
                 model the budget must cover the reasoning tokens too — raise \
                 max_tokens for this call, or configure a non-reasoning model"
                .into()
        } else if choice_was_absent {
            "openai response carried no choices".into()
        } else if content_was_null {
            match finish_reason {
                Some("content_filter") => {
                    "openai stopped on its content filter and returned no text".into()
                }
                Some("tool_calls" | "function_call") => {
                    "openai reply carried only tool calls and no text content".into()
                }
                Some(reason) => format!(
                    "openai response choices[0].message.content was null (finish_reason={reason})"
                ),
                None => {
                    "openai response choices[0].message.content was null (finish_reason absent)"
                        .into()
                }
            }
        } else {
            "openai response content was empty".into()
        };
        return Err(LlmError::EmptyContent {
            reason,
            billed: resp,
        });
    }
    Ok(resp)
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> u64 {
    // OpenAI returns `retry-after` in seconds (sometimes as a
    // float, sometimes integer). We read seconds-as-integer
    // and convert; non-integer / absent → 0.
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
        let req = LlmRequest::new("gpt-4o-mini", "user-body").with_system("sys-instr");
        let body = OpenAIRequestBody::from(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"role\":\"system\",\"content\":\"sys-instr\""));
        // System message comes before the user message.
        let sys_idx = json.find("\"role\":\"system\"").unwrap();
        let user_idx = json.find("\"role\":\"user\"").unwrap();
        assert!(sys_idx < user_idx);
    }

    #[test]
    fn request_body_concatenates_multiple_system_blocks() {
        use crate::types::SystemBlock;
        let mut req = LlmRequest::new("gpt-4o-mini", "u");
        req.system_blocks = vec![
            SystemBlock::cached("role-text"),
            SystemBlock::cached("schema-text"),
        ];
        let body = OpenAIRequestBody::from(&req);
        let json = serde_json::to_string(&body).unwrap();
        // OpenAI has no prompt cache directive in the same shape — both
        // cached blocks fold into one system message, blank-line joined.
        assert!(json.contains("\"role\":\"system\""));
        assert!(json.contains("role-text"));
        assert!(json.contains("schema-text"));
        // Exactly one system message — the cache flag is ignored here.
        assert_eq!(json.matches("\"role\":\"system\"").count(), 1);
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
            system_blocks: Vec::new(),
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
                finish_reason: None,
                message: OpenAIChoiceMessage {
                    content: Some("the answer".into()),
                },
            }],
            usage: OpenAIUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                prompt_tokens_details: OpenAIPromptTokensDetails::default(),
            },
        };
        let resp = decode_openai_response(payload, &LlmRequest::new("gpt-4o-mini", "x")).unwrap();
        assert_eq!(resp.content, "the answer");
        assert_eq!(resp.tokens_in, 100);
        assert_eq!(resp.tokens_out, 50);
        // (100 * 150 + 50 * 600) nano / 1000 = 45 micro-USD.
        assert_eq!(resp.cost_micro_usd, 45);
        assert_eq!(resp.model_version, "gpt-4o-mini-2024-07-18");
    }

    /// A successful response with no choices is billed and retains its priced usage.
    #[test]
    fn decode_response_errors_when_choices_empty() {
        let payload = OpenAIResponseBody {
            model: "gpt-4.1-mini".into(),
            choices: vec![],
            usage: OpenAIUsage {
                prompt_tokens: 1000,
                completion_tokens: 0,
                prompt_tokens_details: OpenAIPromptTokensDetails::default(),
            },
        };
        let err =
            decode_openai_response(payload, &LlmRequest::new("gpt-4.1-mini", "x")).unwrap_err();
        match err {
            LlmError::EmptyContent { billed, reason } => {
                assert!(reason.contains("no choices"));
                assert!(billed.cost_micro_usd > 0);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// A null message content is billed and preserves its provider provenance.
    #[test]
    fn decode_response_errors_when_content_missing() {
        let payload = OpenAIResponseBody {
            model: "gpt-4.1-mini".into(),
            choices: vec![OpenAIChoice {
                finish_reason: None,
                message: OpenAIChoiceMessage { content: None },
            }],
            usage: OpenAIUsage {
                prompt_tokens: 1000,
                completion_tokens: 0,
                prompt_tokens_details: OpenAIPromptTokensDetails::default(),
            },
        };
        let err =
            decode_openai_response(payload, &LlmRequest::new("gpt-4.1-mini", "x")).unwrap_err();
        match err {
            LlmError::EmptyContent { billed, reason } => {
                assert!(reason.contains("was null"));
                assert!(billed.cost_micro_usd > 0);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    fn null_content_error(finish_reason: Option<&str>) -> LlmError {
        decode_openai_response(
            OpenAIResponseBody {
                model: "gpt-4.1-mini".into(),
                choices: vec![OpenAIChoice {
                    finish_reason: finish_reason.map(str::to_string),
                    message: OpenAIChoiceMessage { content: None },
                }],
                usage: OpenAIUsage {
                    prompt_tokens: 1000,
                    completion_tokens: 0,
                    prompt_tokens_details: OpenAIPromptTokensDetails::default(),
                },
            },
            &LlmRequest::new("gpt-4.1-mini", "x"),
        )
        .unwrap_err()
    }

    /// Tool-only replies retain billed usage and identify the operator remedy.
    #[test]
    fn null_content_tool_calls_is_billed_empty_content() {
        match null_content_error(Some("tool_calls")) {
            LlmError::EmptyContent { billed, reason } => {
                assert!(reason.contains("tool calls"));
                assert!(billed.cost_micro_usd > 0);
                assert_eq!(
                    LlmError::EmptyContent { reason, billed }.failure_class(),
                    crate::FailureClass::Permanent
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// Content-filtered null replies retain billed usage and identify the provider filter.
    #[test]
    fn null_content_filter_is_billed_empty_content() {
        match null_content_error(Some("content_filter")) {
            LlmError::EmptyContent { billed, reason } => {
                assert!(reason.contains("content filter"));
                assert!(billed.cost_micro_usd > 0);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// Truncated null replies retain the actionable reasoning-budget diagnostic.
    #[test]
    fn null_content_length_keeps_budget_reason() {
        match null_content_error(Some("length")) {
            LlmError::EmptyContent { reason, .. } => assert!(reason.contains("max_tokens")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// A null reply without a finish reason identifies null content and its billed usage.
    #[test]
    fn null_content_without_finish_reason_is_distinct() {
        match null_content_error(None) {
            LlmError::EmptyContent { billed, reason } => {
                assert!(reason.contains("was null"));
                assert!(reason.contains("finish_reason absent"));
                assert!(billed.cost_micro_usd > 0);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// An empty choice list retains billed usage as an empty completion.
    #[test]
    fn empty_choices_with_usage_are_billed_empty_content() {
        let payload = OpenAIResponseBody {
            model: "gpt-4.1-mini".into(),
            choices: vec![],
            usage: OpenAIUsage {
                prompt_tokens: 1000,
                completion_tokens: 0,
                prompt_tokens_details: OpenAIPromptTokensDetails::default(),
            },
        };
        match decode_openai_response(payload, &LlmRequest::new("gpt-4.1-mini", "x")).unwrap_err() {
            LlmError::EmptyContent { billed, .. } => assert!(billed.cost_micro_usd > 0),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn decode_response_errors_when_content_empty_string() {
        let payload = OpenAIResponseBody {
            model: "m".into(),
            choices: vec![OpenAIChoice {
                finish_reason: None,
                message: OpenAIChoiceMessage {
                    content: Some(String::new()),
                },
            }],
            usage: OpenAIUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                prompt_tokens_details: OpenAIPromptTokensDetails::default(),
            },
        };
        let err = decode_openai_response(payload, &LlmRequest::new("m", "x")).unwrap_err();
        assert!(matches!(
            err,
            LlmError::EmptyContent { ref reason, .. } if reason.contains("empty")
        ));
    }

    /// A billed empty completion retains its usage and cost for accounting.
    #[test]
    fn empty_completion_carries_billed_cost() {
        let payload = OpenAIResponseBody {
            model: "gpt-4.1-mini".into(),
            choices: vec![OpenAIChoice {
                finish_reason: None,
                message: OpenAIChoiceMessage {
                    content: Some(String::new()),
                },
            }],
            usage: OpenAIUsage {
                prompt_tokens: 1000,
                completion_tokens: 0,
                prompt_tokens_details: OpenAIPromptTokensDetails::default(),
            },
        };
        let err =
            decode_openai_response(payload, &LlmRequest::new("gpt-4.1-mini", "x")).unwrap_err();
        match err {
            LlmError::EmptyContent { billed, .. } => {
                assert_eq!(billed.tokens_in, 1000);
                // The provider billed the call, so the error carries that cost.
                assert_eq!(billed.cost_micro_usd, 400); // 1000 * 400 nano / 1000 = 400 micro-USD.
                assert!(billed.cost_micro_usd > 0);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// Cached OpenAI prompt tokens are excluded from live input and billed at the cache rate.
    #[test]
    fn cached_prompt_tokens_are_recorded_and_priced() {
        let payload = OpenAIResponseBody {
            model: "gpt-4.1-mini".into(),
            choices: vec![OpenAIChoice {
                finish_reason: Some("stop".into()),
                message: OpenAIChoiceMessage {
                    content: Some("ok".into()),
                },
            }],
            usage: OpenAIUsage {
                prompt_tokens: 1000,
                completion_tokens: 0,
                prompt_tokens_details: OpenAIPromptTokensDetails { cached_tokens: 800 },
            },
        };
        let resp = decode_openai_response(payload, &LlmRequest::new("gpt-4.1-mini", "x")).unwrap();
        assert_eq!(resp.tokens_in, 200);
        assert_eq!(resp.cache_read_input_tokens, 800);
        assert_eq!(resp.cost_micro_usd, 160); // (200*400 + 800*100) nano / 1000 = 160 micro-USD.
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

    // ── truncation detection ────────────────────────────────────────────

    fn body_with_finish(reason: Option<&str>) -> OpenAIResponseBody {
        OpenAIResponseBody {
            model: "gpt-4o-mini".into(),
            choices: vec![OpenAIChoice {
                message: OpenAIChoiceMessage {
                    content: Some("{\"a\":1}".into()),
                },
                finish_reason: reason.map(str::to_string),
            }],
            usage: OpenAIUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                prompt_tokens_details: OpenAIPromptTokensDetails::default(),
            },
        }
    }

    #[test]
    fn finish_reason_length_marks_the_response_truncated() {
        // The signal that tells a budget problem from a schema problem.
        // Without it, output cut mid-JSON is indistinguishable from a
        // model that cannot follow the schema, and the caller burns a
        // second identical call to learn nothing.
        let r = decode_openai_response(
            body_with_finish(Some("length")),
            &LlmRequest::new("gpt-4o-mini", "x"),
        )
        .unwrap();
        assert!(r.truncated);
    }

    /// A reasoning model with too small a budget answers 200 with empty
    /// content and `finish_reason = length`. Reported as a bare "content was
    /// empty" it reads as a transient hiccup, so the caller retries it — and
    /// retries it, billed every time, because nothing about the request has
    /// changed. The error has to name the budget for anyone to fix it.
    #[test]
    fn an_empty_truncated_reply_blames_the_budget_not_the_model() {
        let mut body = body_with_finish(Some("length"));
        body.choices[0].message.content = Some(String::new());

        let err = decode_openai_response(body, &LlmRequest::new("gpt-4o-mini", "x"))
            .expect_err("empty content is an error");
        let msg = err.to_string();
        assert!(msg.contains("finish_reason=length"), "{msg}");
        assert!(msg.contains("max_tokens"), "{msg}");
        if let LlmError::EmptyContent { billed, reason } = err {
            // A billed empty completion still records what it cost.
            assert!(billed.cost_micro_usd > 0);
            assert!(billed.truncated);
            assert!(reason.contains("finish_reason=length"));
        } else {
            panic!("expected billed empty content");
        }
    }

    /// An empty reply that was NOT truncated is a different fault and must
    /// not be mislabelled as a budget problem.
    #[test]
    fn an_empty_reply_that_finished_cleanly_is_not_blamed_on_the_budget() {
        let mut body = body_with_finish(Some("stop"));
        body.choices[0].message.content = Some(String::new());

        let msg = decode_openai_response(body, &LlmRequest::new("gpt-4o-mini", "x"))
            .expect_err("empty content is an error")
            .to_string();
        assert!(msg.contains("content was empty"), "{msg}");
        assert!(!msg.contains("max_tokens"), "{msg}");
    }

    #[test]
    fn a_clean_stop_is_not_truncated() {
        let r = decode_openai_response(
            body_with_finish(Some("stop")),
            &LlmRequest::new("gpt-4o-mini", "x"),
        )
        .unwrap();
        assert!(!r.truncated);
    }

    #[test]
    fn an_absent_or_unknown_finish_reason_is_not_truncated() {
        // Fail toward "not truncated": a false positive would swallow a
        // real schema failure behind a misleading budget message, and some
        // proxies omit the field entirely.
        for reason in [None, Some("content_filter"), Some("tool_calls")] {
            let r = decode_openai_response(
                body_with_finish(reason),
                &LlmRequest::new("gpt-4o-mini", "x"),
            )
            .unwrap();
            assert!(!r.truncated, "{reason:?} must not read as truncated");
        }
    }

    /// Drive a future to completion with no reactor of any kind.
    ///
    /// As far as Tokio is concerned this is the shape of a glommio shard
    /// executor: a thread polling futures with nothing registered to service
    /// I/O. Hand-rolled rather than pulled from a crate so the test cannot
    /// accidentally acquire the very runtime it is meant to do without.
    fn block_on_without_reactor<F: std::future::Future>(fut: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};

        struct Unparker(std::thread::Thread);
        impl Wake for Unparker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }

        let waker = Waker::from(Arc::new(Unparker(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(fut);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => std::thread::park(),
            }
        }
    }

    #[test]
    fn completing_without_a_tokio_reactor_does_not_panic() {
        // Every other provider test is `#[tokio::test]`, which hands the
        // client a reactor that production does NOT have:
        // `complete` is awaited from the extractor tier and the HyPE generator
        // on a glommio shard executor.
        //
        // The suite stayed green through that gap because no write could reach
        // this call: resolution returned `Missing` for every namespace, so the
        // write degraded before a client was built. The first namespace with a
        // registered credential aborted the shard (SIGABRT) on its first write.
        //
        // Port 1 refuses immediately, so this exercises the connect path — the
        // one that panicked — without needing a server.
        let client = OpenAIClient::with_endpoint("gpt-4o-mini", "sk-test", "http://127.0.0.1:1");
        let out = block_on_without_reactor(client.complete(LlmRequest::new("gpt-4o-mini", "hi")));
        assert!(
            out.is_err(),
            "an unreachable provider is an error, not a panic"
        );
    }

    /// The shape the provider actually sent when a model refused an explicit
    /// temperature, trimmed to the fields matched on.
    const TEMPERATURE_REFUSAL: &str = r#"{
      "error": {
        "message": "Unsupported value: 'temperature' does not support 0.3 with this model. Only the default (1) value is supported.",
        "type": "invalid_request_error",
        "param": "temperature",
        "code": "unsupported_value"
      }
    }"#;

    #[test]
    fn a_temperature_refusal_is_recognised() {
        assert!(rejects_temperature(TEMPERATURE_REFUSAL));
    }

    /// The retry must not swallow unrelated 400s. A model that does not exist,
    /// or a malformed request, has to keep its own error — otherwise the
    /// second attempt fails the same way and the caller sees a confusing
    /// duplicate instead of the real cause.
    #[test]
    fn other_four_hundreds_are_not_temperature_refusals() {
        for body in [
            r#"{"error":{"message":"The model `nope` does not exist","type":"invalid_request_error","param":null,"code":"model_not_found"}}"#,
            // Mentions temperature in prose, but is not a refusal OF it.
            r#"{"error":{"message":"temperature must be a number","type":"invalid_request_error","param":"max_tokens","code":"unsupported_value"}}"#,
            // Right param, different code.
            r#"{"error":{"message":"x","type":"invalid_request_error","param":"temperature","code":"invalid_type"}}"#,
            "not json at all",
            "",
        ] {
            assert!(
                !rejects_temperature(body),
                "must not treat this as a temperature refusal: {body}"
            );
        }
    }
}
