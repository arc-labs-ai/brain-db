//! OpenAI Chat Completions Summarizer adapter (sub-task 9.15).
//!
//! Posts to `<api_base>/chat/completions` with the spec §11/03 §7
//! prompt. API key read once from the env var named in
//! `cfg.summarizer.openai_api_key_env`; the key never lives in TOML
//! and is never logged.
//!
//! Errors:
//! - HTTP 4xx → `SummarizerError::Failed(format!("openai {status}: …"))`.
//! - HTTP 5xx / timeout / connection refused → `SummarizerError::Failed`.
//! - JSON shape mismatch → `SummarizerError::Failed`.
//!
//! The consolidation worker logs + skips the cycle either way. v2
//! adds a circuit breaker (spec §11/09 §6).

#![cfg(feature = "summarizer-openai")]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use brain_workers::{Summarizer, SummarizerError};
use serde::Serialize;
use tracing::warn;

use crate::llm::bridge::{BridgePayload, SummarizerBridge};
use crate::llm::prompt::build_consolidation_prompt;

pub(crate) struct OpenAiSummarizer {
    api_base: String,
    api_key: Arc<str>,
    model: String,
    temperature: f32,
    max_tokens: u32,
    bridge: SummarizerBridge,
}

impl OpenAiSummarizer {
    /// Build the adapter. Caller already loaded the API key from env
    /// (see `factory::build_summarizer`).
    pub(crate) fn new(
        api_base: String,
        api_key: String,
        model: String,
        temperature: f32,
        max_summary_chars: u32,
        bridge: SummarizerBridge,
    ) -> Self {
        // ~4 chars per token is the rough OpenAI ratio. Round up so
        // a short completion can fit comfortably.
        let max_tokens = (max_summary_chars / 4).max(64);
        Self {
            api_base,
            api_key: Arc::from(api_key),
            model,
            temperature,
            max_tokens,
            bridge,
        }
    }
}

impl Summarizer for OpenAiSummarizer {
    fn summarize<'a>(
        &'a self,
        memories: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = Result<String, SummarizerError>> + 'a>> {
        let bridge = self.bridge.clone();
        let url = format!("{}/chat/completions", self.api_base.trim_end_matches('/'));
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let temperature = self.temperature;
        let max_tokens = self.max_tokens;
        Box::pin(async move {
            if memories.is_empty() {
                // Disabled-style probe (`handle_consolidate_cycle`
                // calls `summarize(&[])` once at startup to detect
                // whether the backend is wired). A configured backend
                // is by definition not Disabled, so we surface a
                // benign "ok" — the cycle then proceeds with real
                // input.
                return Ok(String::new());
            }
            let (system, user) = build_consolidation_prompt(memories);
            let req = OpenAiRequest {
                url,
                api_key,
                body: OpenAiChatBody {
                    model,
                    temperature,
                    max_tokens,
                    messages: vec![
                        OpenAiMessage {
                            role: "system",
                            content: system.to_owned(),
                        },
                        OpenAiMessage {
                            role: "user",
                            content: user,
                        },
                    ],
                },
            };
            bridge.request(BridgePayload::OpenAi(req)).await
        })
    }
}

pub(crate) struct OpenAiRequest {
    pub url: String,
    pub api_key: Arc<str>,
    pub body: OpenAiChatBody,
}

#[derive(Serialize)]
pub(crate) struct OpenAiChatBody {
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub messages: Vec<OpenAiMessage>,
}

#[derive(Serialize)]
pub(crate) struct OpenAiMessage {
    pub role: &'static str,
    pub content: String,
}

/// Execute the request on the bridge runtime. Called from
/// `bridge::worker_loop`.
pub(crate) async fn execute(
    client: &reqwest::Client,
    req: OpenAiRequest,
) -> Result<String, SummarizerError> {
    let OpenAiRequest { url, api_key, body } = req;
    let resp = match client
        .post(&url)
        .bearer_auth(&*api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Err(SummarizerError::Failed(format!("openai POST {url}: {e}")));
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        // Truncate at 256 chars so a verbose 4xx body doesn't flood
        // the log.
        let truncated: String = body.chars().take(256).collect();
        warn!(url, status = status.as_u16(), "openai non-success response",);
        return Err(SummarizerError::Failed(format!(
            "openai {status}: {truncated}"
        )));
    }
    let parsed: OpenAiResponseBody = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return Err(SummarizerError::Failed(format!(
                "openai response JSON parse: {e}"
            )));
        }
    };
    parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .ok_or_else(|| SummarizerError::Failed("openai response missing choices".into()))
}

#[derive(serde::Deserialize)]
struct OpenAiResponseBody {
    choices: Vec<OpenAiChoice>,
}

#[derive(serde::Deserialize)]
struct OpenAiChoice {
    message: OpenAiResponseMessage,
}

#[derive(serde::Deserialize)]
struct OpenAiResponseMessage {
    content: String,
}
