//! The runtime provider HTTP runs on.
//!
//! `reqwest` is hyper-based, and `tokio::time::timeout` needs a timer. Both
//! panic with "there is no reactor running, must be called from the context of
//! a Tokio 1.x runtime" unless polled inside a Tokio runtime. A provider's
//! `complete()` is awaited from the LLM extractor tier and the HyPE generator,
//! which run on a **glommio** shard executor, so there is no such runtime at
//! the call site.
//!
//! A namespace with no stored credential never reaches this far: resolution
//! answers `Missing` and the write degrades before any provider client is
//! built. The first namespace to register a key is therefore the first to
//! exercise this path, which is why it carries every extraction and HyPE
//! generation on the shard rather than a trickle.
//!
//! Multi-threaded rather than current-thread on purpose: a current-thread
//! runtime only advances inside `block_on`, and nothing here calls that, so
//! work spawned onto it from a glommio thread would never run.
//!
//! Built once and leaked for the process lifetime. Dropping a Tokio runtime
//! blocks on its workers and panics if done from async context; a client
//! dropped on a shard thread would hit exactly that.
//!
//! How wide it is belongs to the machine, not to this file: see
//! [`crate::providers::sizing`], which counts these threads against the shard
//! threads so the process does not run more than its cores.

use std::sync::OnceLock;

use crate::providers::sizing;

/// The process-wide runtime for provider HTTP, or `None` when it could not be
/// built. The outcome is cached for the process lifetime, failure included:
/// once this answers `None`, it answers `None` for every later call in the
/// process, which is why callers treat that as permanent rather than
/// retryable.
fn provider_runtime() -> Option<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            // Reading the size here is what fixes it: the first provider call
            // in the process settles it, and every later caller gets this same
            // runtime.
            let worker_threads = sizing::worker_threads();
            match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads)
                .thread_name("brain-llm-provider")
                .enable_all()
                .build()
            {
                Ok(rt) => {
                    tracing::info!(
                        target: "brain_llm::providers",
                        worker_threads,
                        "provider HTTP runtime started",
                    );
                    Some(rt)
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_is_built_at_the_size_the_machine_was_given() {
        // The decision and the thing built must be the same number: this is
        // what makes the sizing observable on a real machine, including inside
        // a cgroup with fewer cpus than the host.
        let Some(runtime) = provider_runtime() else {
            // Preserved behaviour: a runtime that will not build disables
            // completions rather than failing the process.
            return;
        };
        assert_eq!(runtime.metrics().num_workers(), sizing::worker_threads());
    }
}
