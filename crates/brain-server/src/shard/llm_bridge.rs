//! The seam between the shard executors and the LLM providers' HTTP stack.
//!
//! Extraction and HyPE run on per-shard Glommio executors, which have their
//! own io_uring reactor and no Tokio runtime. The provider clients use
//! `reqwest` and `tokio::time`, which panic when polled on a Glommio task.
//!
//! Every provider call therefore runs on one process-wide Tokio runtime kept
//! off the shard cores, and the result crosses back over a runtime-agnostic
//! channel the Glommio side can await. LLM calls are I/O-bound, so a couple
//! of worker threads serve many concurrent requests across all shards.

use std::sync::{Arc, OnceLock};

use brain_llm::client::LlmFuture;
use brain_llm::{LlmClient, LlmError, LlmRequest};

/// The shared runtime that backs every outbound provider call.
pub fn bridge_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("brain-llm-extract")
            .enable_all()
            .build()
            .expect("invariant: LLM bridge Tokio runtime builds with default settings")
    })
}

/// Wraps a provider client so its HTTP runs on [`bridge_runtime`] instead of
/// the caller's executor.
struct BridgedLlmClient {
    inner: Arc<dyn LlmClient>,
}

impl LlmClient for BridgedLlmClient {
    fn complete<'a>(&'a self, request: LlmRequest) -> LlmFuture<'a> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let (tx, rx) = flume::bounded(1);
            // The spawned task owns the `Arc` and the request, so the inner
            // future is `Send + 'static` and runs where reqwest is valid.
            bridge_runtime().spawn(async move {
                let result = inner.complete(request).await;
                let _ = tx.send_async(result).await;
            });
            rx.recv_async().await.unwrap_or_else(|_| {
                Err(LlmError::ProviderError {
                    status: 0,
                    message: "LLM bridge runtime unavailable (reply channel closed)".into(),
                })
            })
        })
    }

    // Routing and cache-key metadata is pure data; no runtime involved.
    fn model(&self) -> &str {
        self.inner.model()
    }

    fn model_id_hash(&self) -> u64 {
        self.inner.model_id_hash()
    }
}

/// Wrap a freshly-built provider client so its HTTP runs on the bridge
/// runtime. Every client reached from a Glommio task goes through this.
#[must_use]
pub fn bridge(client: Arc<dyn LlmClient>) -> Arc<dyn LlmClient> {
    Arc::new(BridgedLlmClient { inner: client })
}
