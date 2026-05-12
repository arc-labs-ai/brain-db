//! Worker runtime context. Spec §11/01 §2 + §11/00 §3 — the handle
//! bag every worker reads from while a cycle is in flight.

use std::sync::Arc;

use brain_ops::OpsContext;
use tokio::sync::watch;

/// Per-cycle handle bag. Cloned cheaply; cloning bumps Arc refcounts
/// and clones the watch receiver.
#[derive(Clone)]
pub struct WorkerContext {
    /// Substrate handles — embedder, index, metadata, writer, txn
    /// store, subscribe bus. Workers borrow into this for everything.
    pub ops: Arc<OpsContext>,
    /// Shutdown signal. The scheduler flips the channel to `true` on
    /// `shutdown()`; workers consult it between units of work and the
    /// scheduler loop consults it between cycles.
    pub shutdown: watch::Receiver<bool>,
}

impl WorkerContext {
    /// `true` iff shutdown has been requested.
    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        *self.shutdown.borrow()
    }
}
