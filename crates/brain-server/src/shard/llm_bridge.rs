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
                // A disconnected reply channel means the request task produced no result, a
                // defect that repeats for the same input and is therefore terminal.
                Err(LlmError::RequestTaskFailed {
                    reason: "LLM bridge reply channel closed before a result arrived: the spawned request task panicked or was cancelled".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for a defect in the provider request path: the future panics
    /// when polled, so the bridge's spawned task dies without ever sending.
    struct PanickingClient;

    impl PanickingClient {
        /// The return type is spelled out so the future's `Output` matches
        /// [`LlmFuture`] without relying on inference through a diverging body.
        async fn panicking_request() -> Result<brain_llm::LlmResponse, LlmError> {
            panic!("test request task panic")
        }
    }

    impl LlmClient for PanickingClient {
        fn complete<'a>(&'a self, _request: LlmRequest) -> LlmFuture<'a> {
            Box::pin(Self::panicking_request())
        }

        fn model(&self) -> &str {
            "test-model"
        }

        fn model_id_hash(&self) -> u64 {
            0
        }
    }

    #[test]
    fn panicking_request_task_is_terminal() {
        // The Tokio runtime catches this task panic; the expected panic output is test noise.
        let client: Arc<dyn LlmClient> = Arc::new(PanickingClient);
        let err = bridge_runtime()
            .block_on(bridge(client).complete(LlmRequest::new("test-model", "x")))
            .unwrap_err();
        assert!(matches!(err, LlmError::RequestTaskFailed { .. }));
        assert_eq!(err.failure_class(), brain_llm::FailureClass::Permanent);
        assert_ne!(err.failure_class(), brain_llm::FailureClass::Transient);
    }
}
