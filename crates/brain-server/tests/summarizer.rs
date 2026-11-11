//! Integration tests for the OpenAI / Ollama
//! Summarizer adapters.
//!
//! Each test stands up a hand-rolled mock HTTP server (Tokio
//! `TcpListener` on `127.0.0.1:0`, returns a canned JSON response)
//! and points the configured Summarizer at it. We don't talk to
//! real OpenAI / Ollama in CI — that needs API keys + a model
//! install. The mock proves wire correctness + error mapping.

#![cfg(target_os = "linux")]

#[cfg(not(any(feature = "summarizer-llm", feature = "summarizer-ollama")))]
use std::sync::Arc;

#[cfg(not(any(feature = "summarizer-llm", feature = "summarizer-ollama")))]
use brain_workers::Summarizer;
use brain_workers::SummarizerError;

// Pull the source modules in so `crate::llm::…` resolves the same
// as in main.rs. Each test binary owns its own compilation; we
// only need the pieces the summarizer adapters touch.
#[allow(dead_code)]
#[path = "../src/config/mod.rs"]
mod config;
#[path = "../src/llm/mod.rs"]
mod llm;

use config::{Config, SummarizerConfig};

fn cfg_with_summarizer(s: SummarizerConfig) -> Config {
    // Borrow the rest of Config's defaults from the dev TOML by
    // reading + replacing only the summarizer section.
    let raw = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/dev.toml"),
    )
    .expect("read dev.toml");
    let mut cfg: Config = toml::from_str(&raw).expect("parse dev.toml");
    cfg.summarizer = s;
    cfg
}

// ---------------------------------------------------------------------------
// Always-on tests
// ---------------------------------------------------------------------------

#[test]
fn build_summarizer_disabled_returns_disabled_implementation() {
    let cfg = cfg_with_summarizer(SummarizerConfig::default());
    let summarizer = llm::factory::build_summarizer(&cfg, None).expect("build");
    assert!(
        !summarizer.enabled(),
        "the default backend reports itself off, so the worker skips the cycle"
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = rt.block_on(async { summarizer.summarize("acme", &["hello"]).await });
    assert!(matches!(result, Err(SummarizerError::Disabled)));
}

// ---------------------------------------------------------------------------
// Resolved-credential tests (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "summarizer-llm")]
mod resolved_tests {
    use super::*;
    use std::sync::Arc;

    use brain_llm::client::LlmFuture;
    use brain_llm::{LlmClient, LlmError, LlmRequest, LlmResponse};

    /// Records the model it was asked for and answers with a canned summary.
    struct StubClient {
        model: String,
        reply: Result<String, LlmError>,
    }

    impl LlmClient for StubClient {
        fn complete<'a>(&'a self, _request: LlmRequest) -> LlmFuture<'a> {
            let reply = match &self.reply {
                Ok(text) => Ok(LlmResponse {
                    content: text.clone(),
                    tokens_in: 0,
                    tokens_out: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    cost_micro_usd: 0,
                    model_version: "stub-model".to_string(),
                    truncated: false,
                }),
                Err(_) => Err(LlmError::ProviderError {
                    status: 500,
                    message: "stub failure".into(),
                }),
            };
            Box::pin(async move { reply })
        }

        fn model(&self) -> &str {
            &self.model
        }

        fn model_id_hash(&self) -> u64 {
            0
        }
    }

    fn summarizer_over(reply: Result<String, LlmError>) -> Arc<dyn brain_workers::Summarizer> {
        let client: Arc<dyn LlmClient> = Arc::new(StubClient {
            model: "stub-model".to_string(),
            reply,
        });
        let resolver: Arc<dyn brain_llm::LlmCredentialResolver> =
            Arc::new(brain_llm::FixedResolver::new(client));
        let cfg = cfg_with_summarizer(SummarizerConfig {
            backend: config::SummarizerBackend::Llm,
            request_timeout_sec: 5,
            max_summary_chars: 256,
            ..SummarizerConfig::default()
        });
        llm::factory::build_summarizer(&cfg, Some(resolver)).expect("build")
    }

    fn drive<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn a_resolved_credential_produces_the_summary() {
        let s = summarizer_over(Ok("consolidated".to_string()));
        assert!(s.enabled());
        let out = drive(async { s.summarize("acme", &["m1", "m2"]).await });
        assert_eq!(out.unwrap(), "consolidated");
    }

    #[test]
    fn a_provider_failure_is_reported_not_swallowed() {
        let s = summarizer_over(Err(LlmError::ProviderError {
            status: 500,
            message: "boom".into(),
        }));
        let out = drive(async { s.summarize("acme", &["m1"]).await });
        assert!(matches!(out, Err(SummarizerError::Failed(_))), "{out:?}");
    }

    #[test]
    fn the_llm_backend_refuses_to_build_without_a_credential_store() {
        // A deployment with no store cannot resolve any namespace's key, so
        // selecting this backend is a configuration error rather than a
        // silent no-op.
        let cfg = cfg_with_summarizer(SummarizerConfig {
            backend: config::SummarizerBackend::Llm,
            ..SummarizerConfig::default()
        });
        assert!(llm::factory::build_summarizer(&cfg, None).is_err());
    }
}

// ---------------------------------------------------------------------------
// Ollama tests (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "summarizer-ollama")]
mod ollama_tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn one_shot_mock(status: u16, body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept mock");
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf).await;
            let reason = match status {
                200 => "OK",
                500 => "Internal Server Error",
                _ => "Other",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n{body}",
                len = body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        });
        addr
    }

    fn ollama_cfg(base: String) -> Config {
        super::cfg_with_summarizer(SummarizerConfig {
            backend: config::SummarizerBackend::Ollama,
            request_timeout_sec: 5,
            ollama_base: base,
            ollama_model: "llama-test".to_owned(),
            ..SummarizerConfig::default()
        })
    }

    #[test]
    fn ollama_round_trips_a_summary() {
        // Mock on its own runtime + thread; same shape as the
        // openai tests' `run_openai_case` to avoid a nested-runtime
        // drop panic.
        let mock_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mock_addr = mock_rt.block_on(one_shot_mock(
            200,
            r#"{"response":"ollama summary","done":true}"#,
        ));
        std::thread::spawn(move || {
            mock_rt.block_on(async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            });
        });
        let cfg = ollama_cfg(format!("http://{}", mock_addr));
        let summarizer = llm::factory::build_summarizer(&cfg, None).expect("build");
        let driver = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = driver.block_on(async { summarizer.summarize("acme", &["m1", "m2"]).await });
        drop(driver);
        drop(summarizer);
        match result {
            Ok(s) => assert_eq!(s, "ollama summary"),
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}

// Keep this `use` from being flagged as unused in feature-off builds.
#[cfg(not(any(feature = "summarizer-llm", feature = "summarizer-ollama")))]
#[allow(dead_code)]
fn _unused_keep_arc() {
    let _: Arc<dyn Summarizer> = Arc::new(brain_workers::DisabledSummarizer);
}
