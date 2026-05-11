//! `OpsContext` — the handle bag handlers consume.
//!
//! Thin wrapper over `brain_planner::ExecutorContext` for v1. Each
//! later sub-task that needs new shared state (txn store in 7.9,
//! subscribe broadcast in 7.10) adds a field non-breakingly.

use brain_planner::ExecutorContext;

#[derive(Clone)]
pub struct OpsContext {
    /// Inner executor context — embedder, index, metadata, writer.
    /// Handlers borrow this to call brain-planner's `execute_*`.
    pub executor: ExecutorContext,
    // 7.9 will add: pub txn_store: Arc<Mutex<TxnStore>>,
    // 7.10 will add: pub subscribe_tx: broadcast::Sender<SubscribeEvent>,
}

impl OpsContext {
    #[must_use]
    pub fn new(executor: ExecutorContext) -> Self {
        Self { executor }
    }
}

// Compile-time guard: the context must be Send + Sync so handlers
// can run on any executor task.
const _: fn() = || {
    fn require<T: Send + Sync>() {}
    require::<OpsContext>();
};
