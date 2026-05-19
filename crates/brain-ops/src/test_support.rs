//! Test helpers shared between brain-ops's unit tests and its
//! integration tests under `tests/`. Linux-only — the runtime
//! production targets (Glommio) is Linux-only, and so are the
//! tests that exercise it.
//!
//! Keep this surface minimal. Test-specific helpers that belong to
//! one file should stay in that file; only put things here when they
//! are needed in two places.

#[cfg(target_os = "linux")]
pub use linux::run_in_glommio;

#[cfg(target_os = "linux")]
mod linux {
    /// Run an async test body inside a fresh Glommio executor on a
    /// dedicated OS thread. Mirrors the `glommio_run` helper used
    /// across `brain-workers/tests/*`.
    ///
    /// Tests that drive any code which production runs on a shard
    /// (e.g. anything reached through `brain_ops::dispatch`, or any
    /// worker spawned via `glommio::spawn_local`) must use this so
    /// the runtime under test matches the runtime in production.
    pub fn run_in_glommio<F, Fut, T>(f: F) -> T
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + 'static,
        T: Send + 'static,
    {
        glommio::LocalExecutorBuilder::default()
            .name("brain-ops-test")
            .spawn(move || async move { f().await })
            .expect("spawn glommio test executor")
            .join()
            .expect("test executor join")
    }
}
