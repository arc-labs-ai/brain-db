//! `OpsContext` — the handle bag handlers consume.
//!
//! Thin wrapper over `brain_planner::ExecutorContext` for v1. Each
//! later sub-task that needs new shared state (txn store in 7.9,
//! subscribe broadcast in 7.10) adds a field non-breakingly.

use std::sync::Arc;

use brain_planner::{ExecutorContext, PlannerContext};

use crate::txn::TxnStore;

#[derive(Clone)]
pub struct OpsContext {
    /// Inner executor context — embedder, index, metadata, writer.
    /// Handlers borrow this to call brain-planner's `execute_*`.
    pub executor: ExecutorContext,
    /// Planner-side config + budgets. Defaults are fine for v1; the
    /// builder is here so the server can override budgets at startup.
    pub planner_ctx: PlannerContext,
    /// Per-process transaction registry. Active txns hold their
    /// buffered ops here until COMMIT / ABORT (sub-task 7.9).
    pub txn_store: Arc<TxnStore>,
    // 7.10 will add: pub subscribe_tx: broadcast::Sender<SubscribeEvent>,
}

impl OpsContext {
    #[must_use]
    pub fn new(executor: ExecutorContext) -> Self {
        Self {
            executor,
            planner_ctx: PlannerContext::default(),
            txn_store: Arc::new(TxnStore::new()),
        }
    }

    #[must_use]
    pub fn with_planner_context(mut self, planner_ctx: PlannerContext) -> Self {
        self.planner_ctx = planner_ctx;
        self
    }

    #[must_use]
    pub fn with_txn_store(mut self, store: Arc<TxnStore>) -> Self {
        self.txn_store = store;
        self
    }
}

// Compile-time guard: the context must be Send + Sync so handlers
// can run on any executor task.
const _: fn() = || {
    fn require<T: Send + Sync>() {}
    require::<OpsContext>();
};
