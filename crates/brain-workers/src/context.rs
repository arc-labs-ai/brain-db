//! Worker runtime context — the handle
//! bag every worker reads from while a cycle is in flight.
//!
//! Shutdown is `Arc<AtomicBool>` instead of `tokio::sync::watch` — removes
//! the tokio runtime dependency and works under either Tokio (tests) or
//! Glommio (production scheduler). `Arc<OpsContext>` is kept (rather than
//! `Rc<OpsContext>`) because `OpsContext` is already transitively `!Send`
//! via `WriterHandle` losing `Sync`; the Arc wrapper is harmless on a
//! single-threaded executor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use brain_ops::OpsContext;

/// Per-cycle handle bag. Cloned cheaply; cloning bumps Arc refcounts.
#[derive(Clone)]
pub struct WorkerContext {
    /// Substrate handles — embedder, index, metadata, writer, txn
    /// store, subscribe bus. Workers borrow into this for everything.
    pub ops: Arc<OpsContext>,
    /// Shutdown signal. The scheduler sets `shutdown.store(true)` to
    /// signal shutdown; workers consult it between units of work and
    /// the scheduler loop consults it between cycles.
    pub shutdown: Arc<AtomicBool>,
}

impl WorkerContext {
    /// `true` iff shutdown has been requested.
    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
}
