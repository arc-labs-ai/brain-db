//! Cross-runtime bridge between the per-shard Glommio executor and a
//! Tokio runtime where `reqwest` lives.
//!
//! ## Why this exists
//!
//! Every per-shard task runs on a Glommio
//! `LocalExecutor`. Glommio has its own I/O reactor (io_uring) and
//! doesn't share Tokio's. A `reqwest::Response::text().await` polled
//! inside a Glommio task hangs (its waker is never armed by Glommio's
//! event loop). `reqwest::blocking` is the other obvious choice but
//! blocks the entire shard's executor for the duration of the HTTP
//! round-trip — a tail-latency catastrophe at scale.
//!
//! The bridge owns a small multi-thread Tokio runtime (1 worker
//! thread, named `brain-llm`). A `SummarizerBridge::request()` call
//! posts a `BridgeRequest` over a `flume` channel; a worker task on
//! the Tokio side executes the reqwest call and sends the result
//! back over the request's per-call reply channel. flume's
//! `send_async` / `recv_async` are runtime-agnostic, so both ends
//! `.await` cleanly under whichever runtime owns them.

#![cfg(any(feature = "summarizer-openai", feature = "summarizer-ollama"))]

use std::io;
use std::sync::Arc;
use std::time::Duration;

use brain_workers::SummarizerError;
use tracing::{debug, warn};

#[cfg(feature = "summarizer-ollama")]
use crate::llm::ollama::OllamaRequest;
#[cfg(feature = "summarizer-openai")]
use crate::llm::openai::OpenAiRequest;

/// The cross-runtime bridge handle. Cloneable (the inner state is
/// `Arc`-wrapped). One bridge per server — the consolidation worker
/// runs at a slow cadence (10 min default), so a single concurrent
/// in-flight LLM call is plenty.
#[derive(Clone)]
pub(crate) struct SummarizerBridge {
    inner: Arc<Inner>,
}

struct Inner {
    tx: flume::Sender<BridgeRequest>,
    /// Hold the runtime so the worker thread stays alive for the
    /// life of the bridge. When the bridge drops, `tx` drops →
    /// worker exits → runtime drops → thread joins.
    _runtime: tokio::runtime::Runtime,
}

pub(crate) enum BridgePayload {
    #[cfg(feature = "summarizer-openai")]
    OpenAi(OpenAiRequest),
    #[cfg(feature = "summarizer-ollama")]
    Ollama(OllamaRequest),
}

pub(crate) struct BridgeRequest {
    pub payload: BridgePayload,
    pub reply: flume::Sender<Result<String, SummarizerError>>,
}

impl SummarizerBridge {
    pub(crate) fn new(timeout: Duration) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("brain-llm")
            .enable_all()
            .build()?;
        let (tx, rx) = flume::bounded::<BridgeRequest>(64);
        runtime.spawn(worker_loop(rx, timeout));
        Ok(Self {
            inner: Arc::new(Inner {
                tx,
                _runtime: runtime,
            }),
        })
    }

    /// Post a request through the bridge. Returns the response body
    /// (the parsed completion text) or a `SummarizerError`.
    pub(crate) async fn request(&self, payload: BridgePayload) -> Result<String, SummarizerError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send_async(BridgeRequest {
                payload,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SummarizerError::Failed("summarizer bridge channel closed".into()))?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| SummarizerError::Failed("summarizer bridge reply channel closed".into()))?
    }
}

async fn worker_loop(rx: flume::Receiver<BridgeRequest>, timeout: Duration) {
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "summarizer bridge reqwest client init failed");
            return;
        }
    };
    while let Ok(req) = rx.recv_async().await {
        let result = match req.payload {
            #[cfg(feature = "summarizer-openai")]
            BridgePayload::OpenAi(call) => crate::llm::openai::execute(&client, call).await,
            #[cfg(feature = "summarizer-ollama")]
            BridgePayload::Ollama(call) => crate::llm::ollama::execute(&client, call).await,
        };
        if req.reply.send_async(result).await.is_err() {
            debug!("summarizer bridge reply receiver dropped before result");
        }
    }
    debug!("summarizer bridge worker exiting");
}
