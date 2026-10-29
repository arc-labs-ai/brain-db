//! The runtime provider HTTP runs on.
//!
//! `reqwest` is hyper-based, and `tokio::time::timeout` needs a timer. Both
//! panic with "there is no reactor running, must be called from the context of
//! a Tokio 1.x runtime" unless polled inside a Tokio runtime. A provider's
//! `complete()` is awaited from the LLM extractor tier and the HyPE generator,
//! which run on a **glommio** shard executor, so there is no such runtime at
//! the call site.
//!
//! This is the same defect as issue #34 and the same fix as
//! [`crate::broker`]'s runtime, one layer further on. #34 was found and fixed
//! on the *resolve* call, which is as far as a write could get while no
//! namespace had a credential: resolution returned `Missing` and the write
//! degraded before any provider client was built. Registering the first real
//! credential is what let a write reach the completion call, and it aborted
//! the shard with SIGABRT on the first one.
//!
//! Kept separate from the broker's runtime rather than shared, because the two
//! are sized for different jobs: the broker carries one small JSON request per
//! namespace per TTL and is a control path, while this carries every
//! extraction and HyPE generation on the shard and is the traffic path.
//!
//! Multi-threaded rather than current-thread on purpose: a current-thread
//! runtime only advances inside `block_on`, and nothing here calls that, so
//! work spawned onto it from a glommio thread would never run.
//!
//! Built once and leaked for the process lifetime. Dropping a Tokio runtime
//! blocks on its workers and panics if done from async context; a client
//! dropped on a shard thread would hit exactly that.

use std::sync::OnceLock;

/// Worker threads for provider HTTP. Completions are long and I/O-bound, so
/// this is about overlapping in-flight requests, not CPU.
const WORKER_THREADS: usize = 2;

/// The process-wide runtime for provider HTTP, or `None` when it could not be
/// built.
pub(crate) fn provider_runtime() -> Option<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(WORKER_THREADS)
                .thread_name("brain-llm-provider")
                .enable_all()
                .build()
            {
                Ok(rt) => Some(rt),
                Err(e) => {
                    tracing::error!(
                        target: "brain_llm::providers",
                        error = %e,
                        "could not build the provider runtime; LLM completions are disabled",
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Run `fut` on [`provider_runtime`] and await it from the caller's executor.
///
/// The whole request — send, status checks, body decode — must happen inside
/// the spawned task. A `reqwest::Response` holds a connection owned by the
/// runtime that created it, so only the finished, owned value may cross back.
///
/// `on_unavailable` supplies the error when there is no runtime to spawn onto,
/// and `on_join_failure` when the task panicked or was cancelled; a provider
/// fault must surface as an error rather than take the shard down.
pub(crate) async fn run_on_provider_runtime<T, E, F>(
    fut: F,
    on_unavailable: impl FnOnce() -> E,
    on_join_failure: impl FnOnce(tokio::task::JoinError) -> E,
) -> Result<T, E>
where
    F: std::future::Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    let Some(runtime) = provider_runtime() else {
        return Err(on_unavailable());
    };
    match runtime.spawn(fut).await {
        Ok(result) => result,
        Err(e) => Err(on_join_failure(e)),
    }
}
