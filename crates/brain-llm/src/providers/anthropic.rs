//! Anthropic Messages API client.
//!
//! POST `https://api.anthropic.com/v1/messages`. Constructed with the
//! single resolved credential (`[llm] api_key` / `BRAIN__LLM__API_KEY`);
//! deployments without the key produce a `None` client that the registry
//! materializer routes through degraded extractors.
//!
//! ## Wire shape
//!
//! Request body (with prompt caching):
//! ```json
//! {
//!   "model": "claude-haiku-4-5",
//!   "max_tokens": 1024,
//!   "system": [
//!     {"type": "text", "text": "...role block...",   "cache_control": {"type": "ephemeral"}},
//!     {"type": "text", "text": "...schema block...", "cache_control": {"type": "ephemeral"}}
//!   ],
//!   "messages": [{"role": "user", "content": "...per-call body..."}]
//! }
//! ```
//!
//! When the request has a single uncached system block we fall back
//! to the string-style `"system": "..."` shape. That keeps simple
//! callers off the array path while extractor / judge calls (which
//! want cache breakpoints) use the structured form.
//!
//! Response body:
//! ```json
//! {
//!   "id": "msg_...",
//!   "model": "claude-haiku-4-5-20240307",
//!   "content": [{"type": "text", "text": "..."}],
//!   "usage": {
//!     "input_tokens": 100,
//!     "cache_creation_input_tokens": 1500,
//!     "cache_read_input_tokens": 0,
//!     "output_tokens": 50
//!   },
//!   "stop_reason": "end_turn"
//! }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client::{model_id_hash, LlmClient, LlmFuture};
use crate::error::LlmError;
use crate::types::{LlmRequest, LlmResponse, LlmRole, SystemBlock};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";

/// Pricing (dollar micro-units per token) for known Anthropic
/// models. Conservative defaults for unknown models.
const PRICE_INPUT_PER_TOKEN_DEFAULT: u64 = 1;
const PRICE_OUTPUT_PER_TOKEN_DEFAULT: u64 = 5;
/// Cache writes are billed at roughly 125% of the live input rate
/// on Anthropic. We approximate at 1 µ$ / token here — the precise
/// per-model multiplier lives in the operator's pricing config.
/// Cache reads are nominally ~10% of the live rate but the default
/// table charges 0 so the cost figure stays a conservative
/// lower-bound; downstream metrics expose `cache_read_input_tokens`
/// directly for accurate accounting.
const PRICE_CACHE_WRITE_PER_TOKEN_DEFAULT: u64 = 1;

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
    /// Construct with an explicit API key against the default endpoint.
    /// The key comes from the single resolved credential (`[llm] api_key`
    /// / `BRAIN__LLM__API_KEY`). Returns `None` for an empty key so
    /// callers can fall back uniformly.
    pub fn with_key(model: impl Into<String>, api_key: impl Into<String>) -> Option<Self> {
        let key = api_key.into();
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

impl LlmClient for AnthropicClient {
    fn complete<'a>(&'a self, request: LlmRequest) -> LlmFuture<'a> {
        Box::pin(async move {
            if self.api_key.is_empty() {
                return Err(LlmError::Auth {
                    provider: "anthropic",
                });
            }

            let body = build_request_body(&request);
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
                resp.json()
                    .await
                    .map_err(|e| LlmError::OutputDecodeFailed {
                        reason: format!("anthropic response JSON decode: {e}"),
                    })?;

            let response = decode_anthropic_response(payload, &request)?;
            log_call(&self.model, &response);
            Ok(response)
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
// Request serialisation.
// ---------------------------------------------------------------------------

/// Build the wire-shape body struct for an `LlmRequest`. We
/// serialise through a `#[derive(Serialize)]` struct so f32 values
/// (like `temperature`) round-trip via Serde's ryu-based number
/// formatter ("0.7", not "0.699999988079071"). Reqwest's
/// `.json(&body)` serialises directly to bytes via the same ryu
/// path, so the struct goes on the wire without intermediate f64
/// quantisation through `serde_json::Value`.
fn build_request_body(req: &LlmRequest) -> AnthropicRequestBody {
    let messages: Vec<AnthropicMessageWire> = req
        .messages
        .iter()
        .map(|m| AnthropicMessageWire {
            role: match m.role {
                LlmRole::User => "user",
                LlmRole::Assistant => "assistant",
            },
            content: m.content.clone(),
        })
        .collect();

    AnthropicRequestBody {
        model: req.model.clone(),
        max_tokens: req.max_tokens,
        system: build_system_field(&req.system_blocks),
        messages,
        temperature: req.temperature,
    }
}

#[derive(Serialize, Debug)]
struct AnthropicRequestBody {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Value>,
    messages: Vec<AnthropicMessageWire>,
    #[serde(skip_serializing_if = "is_zero_f32")]
    temperature: f32,
}

#[derive(Serialize, Debug)]
struct AnthropicMessageWire {
    role: &'static str,
    content: String,
}

fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}

/// Build the `system` field. Returns `None` when there are no
/// system blocks (omit the field entirely). Returns a JSON string
/// when there is exactly one block and it isn't marked for caching
/// — that's the simple shape and keeps non-extractor callers off
/// the array path. Otherwise returns a JSON array of typed text
/// blocks with `cache_control: ephemeral` set on the cached ones.
fn build_system_field(blocks: &[SystemBlock]) -> Option<Value> {
    match blocks {
        [] => None,
        [single] if !single.cache => Some(Value::String(single.text.clone())),
        _ => Some(build_system_array(blocks)),
    }
}

/// Serialise `blocks` to the Anthropic system-array shape. Each
/// block becomes `{"type": "text", "text": ...}`; cached blocks
/// additionally carry `"cache_control": {"type": "ephemeral"}`.
/// Anthropic permits up to 4 cache breakpoints per request — the
/// caller is responsible for staying under that limit (debug
/// assertion in dev builds).
fn build_system_array(blocks: &[SystemBlock]) -> Value {
    debug_assert!(
        blocks.iter().filter(|b| b.cache).count() <= 4,
        "anthropic prompt caching allows at most 4 cache_control breakpoints per request",
    );
    let arr: Vec<Value> = blocks
        .iter()
        .map(|b| {
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), Value::String("text".into()));
            obj.insert("text".into(), Value::String(b.text.clone()));
            if b.cache {
                obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
            }
            Value::Object(obj)
        })
        .collect();
    Value::Array(arr)
}

// ---------------------------------------------------------------------------
// Response deserialisation.
// ---------------------------------------------------------------------------

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

#[derive(Deserialize, Debug, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    /// Tokens billed at the cache-write rate. Present when a
    /// cache_control breakpoint was newly populated on this call.
    #[serde(default)]
    cache_creation_input_tokens: u64,
    /// Tokens served from a previously-populated cache breakpoint.
    /// Present on subsequent calls that hit the cache within its
    /// 5-minute TTL.
    #[serde(default)]
    cache_read_input_tokens: u64,
}

fn decode_anthropic_response(
    payload: AnthropicResponseBody,
    _request: &LlmRequest,
) -> Result<LlmResponse, LlmError> {
    // Concatenate all text-kind content blocks. Tool-use / thinking
    // blocks are ignored.
    let mut content = String::new();
    for block in &payload.content {
        if block.kind == "text" {
            if !content.is_empty() {
                content.push('\n');
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
        + payload.usage.output_tokens * PRICE_OUTPUT_PER_TOKEN_DEFAULT
        + payload.usage.cache_creation_input_tokens * PRICE_CACHE_WRITE_PER_TOKEN_DEFAULT;

    Ok(LlmResponse {
        content,
        tokens_in: payload.usage.input_tokens,
        tokens_out: payload.usage.output_tokens,
        cache_creation_input_tokens: payload.usage.cache_creation_input_tokens,
        cache_read_input_tokens: payload.usage.cache_read_input_tokens,
        cost_micro_usd,
        model_version: payload.model,
    })
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> u64 {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|secs| secs * 1000)
        .unwrap_or(0)
}

/// Cache hit ratio of an Anthropic response: cached reads divided by
/// total input tokens routed through the call (live + write + read).
/// Returns 0.0 when no input was billed at all (degenerate empty
/// call). Production target is ≥ 0.7 steady-state for extractor +
/// judge prompts that share role + schema blocks across calls.
#[must_use]
pub fn cache_hit_ratio(resp: &LlmResponse) -> f64 {
    let total = resp.tokens_in + resp.cache_creation_input_tokens + resp.cache_read_input_tokens;
    if total == 0 {
        return 0.0;
    }
    resp.cache_read_input_tokens as f64 / total as f64
}

fn log_call(model: &str, resp: &LlmResponse) {
    tracing::info!(
        target: "brain_llm::anthropic",
        model = %model,
        input_tokens = resp.tokens_in,
        output_tokens = resp.tokens_out,
        cache_creation = resp.cache_creation_input_tokens,
        cache_read = resp.cache_read_input_tokens,
        cache_hit_ratio = cache_hit_ratio(resp),
        "anthropic call",
    );
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LlmMessage, LlmRole};

    #[test]
    fn request_body_minimal_shape_no_system() {
        let req = LlmRequest::new("claude-haiku-4-5", "hello");
        let body = build_request_body(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"model\":\"claude-haiku-4-5\""));
        assert!(json.contains("\"max_tokens\":1024"));
        assert!(json.contains("\"role\":\"user\""));
        // No system blocks => field omitted entirely.
        assert!(!json.contains("\"system\""));
        // temperature 0.0 is skipped.
        assert!(!json.contains("\"temperature\""));
    }

    #[test]
    fn request_body_includes_temperature_when_nonzero() {
        let mut req = LlmRequest::new("m", "u");
        req.temperature = 0.7;
        let body = build_request_body(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"temperature\":0.7"));
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
        let body = build_request_body(&req);
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.matches("\"role\":\"user\"").count() == 2);
        assert!(json.matches("\"role\":\"assistant\"").count() == 1);
    }

    #[test]
    fn build_system_array_marks_cached_blocks() {
        let blocks = vec![
            SystemBlock::cached("ROLE BLOCK"),
            SystemBlock::cached("SCHEMA BLOCK"),
            SystemBlock::live("PER-CALL BLOCK"),
        ];
        let arr = build_system_array(&blocks);
        let arr = arr.as_array().expect("array shape");
        assert_eq!(arr.len(), 3);
        // First two cached.
        for (i, expected_text) in [(0, "ROLE BLOCK"), (1, "SCHEMA BLOCK")] {
            assert_eq!(arr[i]["type"], "text");
            assert_eq!(arr[i]["text"], expected_text);
            assert_eq!(
                arr[i]["cache_control"]["type"], "ephemeral",
                "block {i} must declare ephemeral cache_control",
            );
        }
        // Third is live — no cache_control key at all.
        assert_eq!(arr[2]["type"], "text");
        assert_eq!(arr[2]["text"], "PER-CALL BLOCK");
        assert!(
            arr[2].get("cache_control").is_none(),
            "live block must not carry cache_control",
        );
    }

    #[test]
    fn build_system_array_single_block_uncached_falls_back_to_string() {
        let blocks = vec![SystemBlock::live("just a string")];
        let v = build_system_field(&blocks).expect("Some");
        // Single uncached block uses the clean string shape.
        assert_eq!(v, Value::String("just a string".into()));
    }

    #[test]
    fn build_system_field_returns_none_for_empty_blocks() {
        assert!(build_system_field(&[]).is_none());
    }

    #[test]
    fn build_system_field_single_cached_block_uses_array_form() {
        // A single block that wants caching MUST go through the array
        // form, otherwise the cache_control directive is unreachable.
        let v = build_system_field(&[SystemBlock::cached("role")]).expect("Some");
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn request_body_emits_system_array_for_cached_blocks() {
        let req = LlmRequest {
            model: "claude-haiku-4-5".into(),
            system_blocks: vec![SystemBlock::cached("role"), SystemBlock::cached("schema")],
            messages: vec![LlmMessage {
                role: LlmRole::User,
                content: "u".into(),
            }],
            response_schema: None,
            temperature: 0.0,
            max_tokens: 256,
            timeout: std::time::Duration::from_secs(5),
        };
        let body = build_request_body(&req);
        // Serialise + reparse so we can inspect the system field
        // structurally (round-tripping via serde_json::Value here is
        // fine — there are no floats in this scope to lose precision
        // on).
        let v: Value = serde_json::from_str(&serde_json::to_string(&body).unwrap()).unwrap();
        let system = &v["system"];
        assert!(system.is_array(), "cached blocks => system must be array");
        assert_eq!(system.as_array().unwrap().len(), 2);
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
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        };
        let req = LlmRequest::new("claude-haiku-4-5", "x");
        let resp = decode_anthropic_response(payload, &req).unwrap();
        assert_eq!(resp.content, "first line\nsecond line");
        assert_eq!(resp.tokens_in, 100);
        assert_eq!(resp.tokens_out, 50);
        assert_eq!(resp.cache_creation_input_tokens, 0);
        assert_eq!(resp.cache_read_input_tokens, 0);
        // 100 * 1 + 50 * 5 = 350 (default pricing, no cache contribution).
        assert_eq!(resp.cost_micro_usd, 350);
        assert_eq!(resp.model_version, "claude-haiku-4-5-20240307");
    }

    #[test]
    fn parse_response_extracts_cache_token_counts() {
        // First-call shape: cache_creation populated, cache_read zero.
        let raw = r#"{
            "model": "claude-haiku-4-5-20240307",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_creation_input_tokens": 1500,
                "cache_read_input_tokens": 0
            }
        }"#;
        let payload: AnthropicResponseBody = serde_json::from_str(raw).unwrap();
        let req = LlmRequest::new("claude-haiku-4-5", "x");
        let resp = decode_anthropic_response(payload, &req).unwrap();
        assert_eq!(resp.cache_creation_input_tokens, 1500);
        assert_eq!(resp.cache_read_input_tokens, 0);

        // Subsequent-call shape: cache_read populated, no creation.
        let raw2 = r#"{
            "model": "claude-haiku-4-5-20240307",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 1500
            }
        }"#;
        let payload2: AnthropicResponseBody = serde_json::from_str(raw2).unwrap();
        let resp2 = decode_anthropic_response(payload2, &req).unwrap();
        assert_eq!(resp2.cache_creation_input_tokens, 0);
        assert_eq!(resp2.cache_read_input_tokens, 1500);
    }

    #[test]
    fn parse_response_defaults_cache_counts_when_field_missing() {
        // Older models / non-cached calls omit the cache fields.
        let raw = r#"{
            "model": "m",
            "content": [{"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }"#;
        let payload: AnthropicResponseBody = serde_json::from_str(raw).unwrap();
        let req = LlmRequest::new("m", "x");
        let resp = decode_anthropic_response(payload, &req).unwrap();
        assert_eq!(resp.cache_creation_input_tokens, 0);
        assert_eq!(resp.cache_read_input_tokens, 0);
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
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
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
            usage: AnthropicUsage::default(),
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
    fn cache_hit_ratio_handles_zero_total() {
        let mut r = LlmResponse {
            content: "x".into(),
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_micro_usd: 0,
            model_version: "m".into(),
        };
        assert_eq!(cache_hit_ratio(&r), 0.0);

        r.cache_read_input_tokens = 700;
        r.cache_creation_input_tokens = 200;
        r.tokens_in = 100;
        // 700 / (100 + 200 + 700) = 0.7
        assert!((cache_hit_ratio(&r) - 0.7).abs() < 1e-9);
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
        headers.insert(
            "retry-after",
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&headers), 0);
    }
}
