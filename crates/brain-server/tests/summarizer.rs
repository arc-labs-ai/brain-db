//! Integration tests for the OpenAI / Ollama
//! Summarizer adapters.
//!
//! Each test stands up a hand-rolled mock HTTP server (Tokio
//! `TcpListener` on `127.0.0.1:0`, returns a canned JSON response)
//! and points the configured Summarizer at it. We don't talk to
//! real OpenAI / Ollama in CI — that needs API keys + a model
//! install. The mock proves wire correctness + error mapping.

#![cfg(target_os = "linux")]

#[cfg(not(any(feature = "summarizer-openai", feature = "summarizer-ollama")))]
use std::sync::Arc;

#[cfg(not(any(feature = "summarizer-openai", feature = "summarizer-ollama")))]
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
    let summarizer = llm::factory::build_summarizer(&cfg).expect("build");
    // Disabled summarizer always returns `Disabled` from a non-empty
    // memory list. (Empty list is the consolidation worker's startup
    // probe; we send non-empty.)
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = rt.block_on(async { summarizer.summarize(&["hello"]).await });
    assert!(matches!(result, Err(SummarizerError::Disabled)));
}

// ---------------------------------------------------------------------------
// OpenAI tests (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "summarizer-openai")]
mod openai_tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawn a one-shot mock server that returns the given (status,
    /// body) JSON response to the first POST it sees. Returns the
    /// bound address.
    async fn one_shot_mock(status: u16, body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept mock");
            // Read request bytes until we see the end of headers
            // (CRLFCRLF). For body-bearing POSTs Prometheus / curl
            // would send a Content-Length; we don't need to read
            // the body for the test.
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf).await;
            let reason = match status {
                200 => "OK",
                401 => "Unauthorized",
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

    fn openai_cfg(api_base: String) -> Config {
        // The summarizer shares the single `[llm] api_key`; the mock
        // server accepts any non-empty key.
        let mut cfg = super::cfg_with_summarizer(SummarizerConfig {
            backend: SummarizerBackend::Openai,
            request_timeout_sec: 5,
            max_summary_chars: 256,
            openai_api_base: api_base,
            openai_model: "gpt-test".to_owned(),
            openai_temperature: 0.0,
            ..SummarizerConfig::default()
        });
        cfg.llm.api_key = Some("test-key".to_owned());
        cfg
    }

    /// Spin up the mock first (its own runtime), then the
    /// summarizer (its own bridge runtime), then call summarize on a
    /// third throwaway runtime. The two background runtimes are
    /// independent; dropping each happens outside any other
    /// runtime's async context.
    fn run_openai_case(status: u16, body: &'static str) -> Result<String, SummarizerError> {
        // 1. Mock server on its own runtime.
        let mock_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mock_addr = mock_rt.block_on(one_shot_mock(status, body));
        // Spawn a thread to drive the mock's accept loop.
        std::thread::spawn(move || {
            mock_rt.block_on(async {
                // Hold mock_rt alive while the canned response is
                // delivered. 1s window is comfortable.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            });
        });
        // 2. Summarizer constructed synchronously on the test thread.
        let cfg = openai_cfg(format!("http://{}", mock_addr));
        let summarizer = llm::factory::build_summarizer(&cfg).expect("build");
        // 3. Call summarize on a throwaway current-thread runtime.
        let driver = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = driver.block_on(async { summarizer.summarize(&["m1", "m2"]).await });
        // Drop driver explicitly (sync context); then summarizer
        // drops at end of scope (sync); no nested-runtime panic.
        drop(driver);
        drop(summarizer);
        result
    }

    #[test]
    fn openai_round_trips_a_summary() {
        let result = run_openai_case(
            200,
            r#"{"choices":[{"message":{"content":"the summary"}}]}"#,
        );
        match result {
            Ok(s) => assert_eq!(s, "the summary"),
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn openai_4xx_surfaces_as_failed() {
        let result = run_openai_case(401, r#"{"error":"unauthorized"}"#);
        match result {
            Err(SummarizerError::Failed(msg)) => {
                assert!(
                    msg.contains("401"),
                    "expected 401 in error message, got: {msg}"
                );
            }
            Ok(s) => panic!("expected Failed(401), got Ok({s:?})"),
            Err(other) => panic!("expected Failed(401), got {other:?}"),
        }
    }

    #[test]
    fn openai_missing_key_fails_construction() {
        // No shared `[llm] api_key` → construction fails. The summarizer
        // reads only that one field, so there is no ambient env var to
        // guard against.
        let mut cfg = super::cfg_with_summarizer(SummarizerConfig {
            backend: SummarizerBackend::Openai,
            ..SummarizerConfig::default()
        });
        cfg.llm.api_key = None;
        // `Result<Arc<dyn Summarizer>, _>` isn't `Debug`, so we can't
        // use `expect_err`. Pattern-match directly.
        let failed = matches!(
            llm::factory::build_summarizer(&cfg),
            Err(llm::factory::BuildSummarizerError::OpenAiKeyMissing)
        );
        assert!(failed, "expected OpenAiKeyMissing with no shared LLM key");
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
            backend: SummarizerBackend::Ollama,
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
        let summarizer = llm::factory::build_summarizer(&cfg).expect("build");
        let driver = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = driver.block_on(async { summarizer.summarize(&["m1", "m2"]).await });
        drop(driver);
        drop(summarizer);
        match result {
            Ok(s) => assert_eq!(s, "ollama summary"),
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}

// Keep this `use` from being flagged as unused in feature-off builds.
#[cfg(not(any(feature = "summarizer-openai", feature = "summarizer-ollama")))]
#[allow(dead_code)]
fn _unused_keep_arc() {
    let _: Arc<dyn Summarizer> = Arc::new(brain_workers::DisabledSummarizer);
}
