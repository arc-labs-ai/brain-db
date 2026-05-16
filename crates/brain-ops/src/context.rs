//! `OpsContext` — the per-shard handle bag handlers consume.
//!
//! Thin wrapper over `brain_planner::ExecutorContext` for v1. Each
//! later sub-task that needs new shared state (txn store in 7.9,
//! subscribe broadcast in 7.10) adds a field non-breakingly.
//!
//! After sub-task 9.7 (audit §4) `OpsContext` is transitively `!Send`
//! because `ExecutorContext` holds `Arc<dyn WriterHandle>` and
//! `WriterHandle` is no longer `Send + Sync`. The interior `Arc<...>`
//! fields are kept (vs the audit's suggested `Rc<...>` swap) to avoid
//! gratuitous test churn — single-threaded usage is enforced by the
//! per-shard Glommio executor, not by the field types.

use std::sync::Arc;
use std::time::Duration;

use brain_extractors::{ClassifierConfig, ExtractorRegistry};
use brain_planner::{ExecutorContext, PlannerContext};
use parking_lot::RwLock;

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
    /// Per-shard transaction registry.
    pub txn_store: Arc<TxnStore>,
    /// Per-shard change-feed bus. Cross-shard fan-out is the
    /// connection layer's job (9.11).
    pub events: Arc<EventBus>,
    /// Per-shard subscription registry.
    pub subscriptions: Arc<SubscriptionRegistry>,
    /// One-shot dispatcher poll window for `handle_subscribe`. Tests
    /// override this to keep the timeout-path test fast.
    pub subscribe_poll_window: Duration,
    /// Recently-accessed memory ids (sub-task 8.3).
    pub access_buffer: Arc<AccessBuffer>,
    /// Live extractor registry. Populated at server startup by
    /// phase 20.7's system-schema bootstrap; defaults to empty.
    /// Wrapped in `RwLock` because `EXTRACTOR_DISABLE` / `_ENABLE`
    /// wire ops (phase 20.8) mutate it.
    pub extractor_registry: Arc<RwLock<ExtractorRegistry>>,
    /// Per-deployment classifier config (operator-provided NER
    /// model path). Defaults to `unloaded`; operators wire
    /// `BRAIN_NER_MODEL_PATH` via `with_classifier_config`.
    pub classifier_config: Arc<ClassifierConfig>,
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
            extractor_registry: Arc::new(RwLock::new(ExtractorRegistry::new())),
            classifier_config: Arc::new(ClassifierConfig::unloaded()),
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

    /// Replace the extractor registry. Servers call this once at
    /// startup with the registry materialised from the persisted
    /// `EXTRACTORS_TABLE` rows; tests use it to inject mock
    /// extractors.
    #[must_use]
    pub fn with_extractor_registry(mut self, reg: ExtractorRegistry) -> Self {
        self.extractor_registry = Arc::new(RwLock::new(reg));
        self
    }

    /// Replace the classifier config. Operators wire
    /// `BRAIN_NER_MODEL_PATH` here at server startup.
    #[must_use]
    pub fn with_classifier_config(mut self, cfg: ClassifierConfig) -> Self {
        self.classifier_config = Arc::new(cfg);
        self
    }
}
