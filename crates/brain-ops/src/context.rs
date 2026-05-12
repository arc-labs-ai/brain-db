//! `OpsContext` — the handle bag handlers consume.
//!
//! Thin wrapper over `brain_planner::ExecutorContext` for v1. Each
//! later sub-task that needs new shared state (txn store in 7.9,
//! subscribe broadcast in 7.10) adds a field non-breakingly.

use std::sync::Arc;
use std::time::Duration;

use brain_planner::{ExecutorContext, PlannerContext};

use crate::access_buffer::AccessBuffer;
use crate::subscribe::{EventBus, SubscriptionRegistry};
use crate::txn::TxnStore;

/// Default bounded poll window for the one-shot SUBSCRIBE dispatcher
/// path. Phase 9's long-lived stream bypasses this entirely.
pub const DEFAULT_SUBSCRIBE_POLL_WINDOW: Duration = Duration::from_secs(5);

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
    /// In-process change-feed bus (sub-task 7.10). The real writer
    /// publishes here after every successful commit; subscribers
    /// receive matching events.
    pub events: Arc<EventBus>,
    /// Registry of active subscriptions (sub-task 7.10). Phase 9's
    /// connection task will call `events.receiver()` /
    /// `subscriptions.register(...)` to drive the long-lived stream
    /// directly; the dispatcher path uses the same surface.
    pub subscriptions: Arc<SubscriptionRegistry>,
    /// One-shot dispatcher poll window for `handle_subscribe`. Tests
    /// override this to keep the timeout-path test fast.
    pub subscribe_poll_window: Duration,
    /// Recently-accessed memory ids (sub-task 8.3). RECALL pushes
    /// every returned hit's id here; the AccessBoostWorker drains
    /// the buffer on its 10 s cycle and applies a salience bump.
    pub access_buffer: Arc<AccessBuffer>,
}

impl OpsContext {
    #[must_use]
    pub fn new(executor: ExecutorContext) -> Self {
        let events = Arc::new(EventBus::default());
        let subscriptions = Arc::new(SubscriptionRegistry::new(events.clone()));
        Self {
            executor,
            planner_ctx: PlannerContext::default(),
            txn_store: Arc::new(TxnStore::new()),
            events,
            subscriptions,
            subscribe_poll_window: DEFAULT_SUBSCRIBE_POLL_WINDOW,
            access_buffer: Arc::new(AccessBuffer::default()),
        }
    }

    /// Override the bounded poll window for the one-shot subscribe
    /// dispatcher path. Mostly useful for tests; production servers
    /// drive streaming via [`SubscriptionRegistry::register`] directly
    /// (Phase 9).
    #[must_use]
    pub fn with_subscribe_poll_window(mut self, window: Duration) -> Self {
        self.subscribe_poll_window = window;
        self
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

    /// Replace the event bus + subscription registry pair. The
    /// registry is rebuilt against the new bus so it never points at
    /// the old bus.
    #[must_use]
    pub fn with_event_bus(mut self, events: Arc<EventBus>) -> Self {
        self.subscriptions = Arc::new(SubscriptionRegistry::new(events.clone()));
        self.events = events;
        self
    }

    /// Replace the access buffer. Tests use this to inject a
    /// small-capacity buffer for overflow exercises.
    #[must_use]
    pub fn with_access_buffer(mut self, buffer: Arc<AccessBuffer>) -> Self {
        self.access_buffer = buffer;
        self
    }
}

// Compile-time guard: the context must be Send + Sync so handlers
// can run on any executor task.
const _: fn() = || {
    fn require<T: Send + Sync>() {}
    require::<OpsContext>();
};
