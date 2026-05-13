//! Ollama `/api/generate` Summarizer adapter (sub-task 9.15).
//!
//! Ollama runs locally (typical `http://localhost:11434`) and
//! doesn't require auth. We use the non-streaming `/api/generate`
//! shape: send a single combined prompt, wait for the full response
//! body, return `response.response` as the summary.

#![cfg(feature = "summarizer-ollama")]

use std::future::Future;
use std::pin::Pin;

use brain_workers::{Summarizer, SummarizerError};
use serde::Serialize;
use tracing::warn;

use crate::llm::bridge::{BridgePayload, SummarizerBridge};
use crate::llm::prompt::combined_prompt;

pub(crate) struct OllamaSummarizer {
    base_url: String,
    model: String,
    temperature: f32,
    bridge: SummarizerBridge,
}

impl OllamaSummarizer {
    pub(crate) fn new(
        base_url: String,
        model: String,
        temperature: f32,
        bridge: SummarizerBridge,
    ) -> Self {
        Self {
            base_url,
            model,
            temperature,
            bridge,
        }
    }
}

impl Summarizer for OllamaSummarizer {
    fn summarize<'a>(
        &'a self,
        memories: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = Result<String, SummarizerError>> + 'a>> {
        let bridge = self.bridge.clone();
        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));
        let model = self.model.clone();
        let temperature = self.temperature;
        Box::pin(async move {
            if memories.is_empty() {
                // Worker startup probe — see OpenAI adapter for the
                // same rationale.
                return Ok(String::new());
            }
            let prompt = combined_prompt(memories);
            let req = OllamaRequest {
                url,
                body: OllamaBody {
                    model,
                    prompt,
                    stream: false,
                    options: OllamaOptions { temperature },
                },
            };
            bridge.request(BridgePayload::Ollama(req)).await
        })
    }
}

pub(crate) struct OllamaRequest {
    pub url: String,
    pub body: OllamaBody,
}

#[derive(Serialize)]
pub(crate) struct OllamaBody {
    pub model: String,
    pub prompt: String,
    pub stream: bool,
    pub options: OllamaOptions,
}

#[derive(Serialize)]
pub(crate) struct OllamaOptions {
    pub temperature: f32,
}

pub(crate) async fn execute(
    client: &reqwest::Client,
    req: OllamaRequest,
) -> Result<String, SummarizerError> {
    let OllamaRequest { url, body } = req;
    let resp = match client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(SummarizerError::Failed(format!("ollama POST {url}: {e}")));
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let truncated: String = body.chars().take(256).collect();
        warn!(url, status = status.as_u16(), "ollama non-success response",);
        return Err(SummarizerError::Failed(format!(
            "ollama {status}: {truncated}"
        )));
    }
    let parsed: OllamaResponseBody = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return Err(SummarizerError::Failed(format!(
                "ollama response JSON parse: {e}"
            )));
        }
    };
    Ok(parsed.response)
}

#[derive(serde::Deserialize)]
struct OllamaResponseBody {
    response: String,
}
