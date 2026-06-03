//! `OpsContext` — the per-shard handle bag handlers consume.
//!
//! Thin wrapper over `brain_planner::ExecutorContext` for v1. Each
//! later addition that needs new shared state (txn store, subscribe
//! broadcast) adds a field non-breakingly.
//!
//! `OpsContext` is transitively `!Send` because `ExecutorContext`
//! holds `Arc<dyn WriterHandle>` and `WriterHandle` is no longer
//! `Send + Sync`. The interior `Arc<...>` fields are kept (rather than
//! swapping to `Rc<...>`) to avoid gratuitous test churn —
//! single-threaded usage is enforced by the per-shard Glommio
//! executor, not by the field types.

use std::sync::Arc;
use std::time::Duration;

use brain_extractors::{ClassifierConfig, ExtractorRegistry};
use brain_index::{GraphRetriever, LexicalRetriever, SemanticRetriever, TantivyShard};
use brain_metadata::LlmCacheDb;
use brain_planner::{ExecutorContext, PlannerContext};
use brain_rerank::RerankService;
use parking_lot::{Mutex, RwLock};

use crate::index::text_indexer::{MemoryTextDispatcher, StatementTextDispatcher};
use crate::state::access_buffer::AccessBuffer;
use crate::subscribe::{EventBus, EventEnvelope, SubscriptionRegistry};
use crate::txn::TxnStore;
use crate::writer::WalSink;

/// Default bounded poll window for the one-shot SUBSCRIBE dispatcher
/// path. The long-lived stream bypasses this entirely.
pub const DEFAULT_SUBSCRIBE_POLL_WINDOW: Duration = Duration::from_secs(5);

/// Per-shard cross-encoder slot. Replaces an earlier
/// `Option<Arc<CrossEncoder>>` whose `None` conflated "model failed to
/// load" with "operator turned this off" — two different failure modes
/// the shard spawn path must keep distinct.
///
/// The shard spawn path resolves which variant applies: an
/// enabled-but-unloadable model is a spawn failure (an operator
/// misconfiguration we refuse to mask), and an explicitly disabled
/// model lands as `Disabled`.
///
/// Rerank is first-class and always-on: when the slot is `Enabled`,
/// every RECALL / QUERY reranks automatically. `Disabled` means the
/// operator opted out of the load — recall succeeds with RRF-only
/// ordering, never an error. There is no per-request toggle to
/// conflict with the slot state.
#[derive(Clone)]
pub enum CrossEncoderSlot {
    /// Operator enabled rerank and the model loaded cleanly. The
    /// inner handle is the off-core rerank service: every concurrent
    /// rerank call on this shard funnels through its worker thread,
    /// so the forward pass never blocks the shard core.
    Enabled(Arc<RerankService>),
    /// Operator set `rerank.enabled = false` in config. Recall runs
    /// RRF-only — no rerank stage, no error.
    Disabled,
}

impl CrossEncoderSlot {
    /// True iff the operator enabled rerank and the model loaded.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled(_))
    }

    /// Borrow the underlying rerank service. `None` when the slot is
    /// `Disabled` — the executor's always-on rerank stage skips and
    /// returns RRF-only ordering.
    #[must_use]
    pub fn as_arc(&self) -> Option<&Arc<RerankService>> {
        match self {
            Self::Enabled(e) => Some(e),
            Self::Disabled => None,
        }
    }
}

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
    /// connection layer's job.
    pub events: Arc<EventBus>,
    /// Per-shard subscription registry.
    pub subscriptions: Arc<SubscriptionRegistry>,
    /// One-shot dispatcher poll window for `handle_subscribe`. Tests
    /// override this to keep the timeout-path test fast.
    pub subscribe_poll_window: Duration,
    /// Recently-accessed memory ids.
    pub access_buffer: Arc<AccessBuffer>,
    /// Live extractor registry. Populated at server startup by the
    /// system-schema bootstrap; defaults to empty.
    /// Wrapped in `RwLock` because `EXTRACTOR_DISABLE` / `_ENABLE`
    /// wire ops mutate it.
    pub extractor_registry: Arc<RwLock<ExtractorRegistry>>,
    /// Per-deployment classifier config (operator-provided NER
    /// model path). Defaults to `unloaded`; operators wire
    /// `BRAIN_NER_MODEL_PATH` via `with_classifier_config`.
    pub classifier_config: Arc<ClassifierConfig>,
    /// Per-shard LLM extractor response cache.
    /// `None` when no API keys are configured, the cache file
    /// failed to open, or no LLM extractors are registered.
    /// LLM extractors thread this into their cache lookups via
    /// the registry; later ops (RECALL provenance lookups, cache
    /// admin endpoints) can read through this field directly.
    pub llm_cache: Option<Arc<Mutex<LlmCacheDb>>>,
    /// Per-shard tantivy index handle. `None` until
    /// the server's shard-spawn path wires it via
    /// [`OpsContext::with_tantivy`]. The retriever and
    /// indexer workers borrow through this field;
    /// no-schema deployments leave it `None`.
    pub tantivy: Option<Arc<TantivyShard>>,
    /// Memory text indexer dispatcher. `None` for
    /// no-schema deployments and tests that don't spawn the
    /// drain task. ENCODE / FORGET handlers check this slot
    /// post-WAL-commit and enqueue an indexer op when present.
    pub memory_text_dispatcher: Option<Arc<MemoryTextDispatcher>>,
    /// Statement text indexer dispatcher. Wired
    /// alongside `memory_text_dispatcher`; statement_create /
    /// supersede / tombstone / retract handlers enqueue
    /// Upsert / Delete events post-commit.
    pub statement_text_dispatcher: Option<Arc<StatementTextDispatcher>>,
    /// Per-shard lexical retriever. Reads the tantivy indexes
    /// maintained by the text-indexer workers. Mandatory: tantivy
    /// is a core shard capability, and a shard that can't serve
    /// lexical queries refuses to spawn (see `ShardError::TantivyInitFailed`).
    pub lexical_retriever: Arc<dyn LexicalRetriever>,
    /// Per-shard semantic retriever. Reads the memory HNSW + the
    /// statement HNSW. Mandatory once the shard is spawned — the
    /// HNSW is constructed at spawn time and can't be missing.
    pub semantic_retriever: Arc<dyn SemanticRetriever>,
    /// Per-shard graph retriever. Reads the entity / relation /
    /// statement redb tables. Mandatory: the metadata DB is open
    /// the moment the shard spawns; the retriever just dispatches
    /// into it.
    pub graph_retriever: Arc<dyn GraphRetriever>,
    /// Per-shard cross-encoder (W2.2 rerank pass). Shared across
    /// shards because the model is read-only and CPU-heavy.
    ///
    /// The enum carries semantic intent the old `Option` lost: a
    /// `Disabled` slot means the operator turned reranking off in
    /// config and an opt-in request must hard-fail (so the client
    /// knows to drop the flag), whereas a previous `None` was
    /// ambiguous between "operator disabled it" and "model failed to
    /// load — silently fall back to RRF". Spawn now refuses to start
    /// when `rerank.enabled = true` but loading fails, so by the
    /// time we get here `Enabled(encoder)` always means a working
    /// encoder.
    pub cross_encoder: CrossEncoderSlot,
    /// WAL append sink for the opaque-body subscribe-replay
    /// pipeline. typed-graph handlers (the `crate::handlers`
    /// modules) call [`OpsContext::publish_graph`] after their successful
    /// redb commit; that helper appends a `WalPayload::PhaseBody`
    /// record carrying the CBOR-encoded
    /// [`brain_protocol::GraphEventPayload`] body, then
    /// publishes the matching [`EventEnvelope`] on the bus with the
    /// WAL-assigned LSN. When `None`, the helper falls back to a
    /// pure bus publish (test wiring / no-schema deployments).
    ///
    /// typed-graph ops are post-commit WAL'd (not pre-commit like
    /// substrate ENCODE/FORGET): redb is the source of truth for
    /// typed-graph state; the WAL record exists purely so subscribe-
    /// replay can reconstruct the event stream. A crash between
    /// commit and WAL append loses the matching subscribe event for
    /// that op, not the underlying typed-graph data.
    pub wal_sink: Option<Arc<dyn WalSink>>,
}

impl OpsContext {
    /// Build an `OpsContext` with all three retrievers wired up. Every
    /// production shard provides real impls (tantivy + brain-index
    /// HNSW + brain-index graph); tests build mock impls via
    /// [`crate::test_support`].
    #[must_use]
    pub fn new(
        executor: ExecutorContext,
        lexical_retriever: Arc<dyn LexicalRetriever>,
        semantic_retriever: Arc<dyn SemanticRetriever>,
        graph_retriever: Arc<dyn GraphRetriever>,
    ) -> Self {
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
            llm_cache: None,
            tantivy: None,
            memory_text_dispatcher: None,
            statement_text_dispatcher: None,
            lexical_retriever,
            semantic_retriever,
            graph_retriever,
            cross_encoder: CrossEncoderSlot::Disabled,
            wal_sink: None,
        }
    }

    /// Override the bounded poll window for the one-shot subscribe
    /// dispatcher path. Mostly useful for tests; production servers
    /// drive streaming via [`SubscriptionRegistry::register`] directly.
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

    /// Install (or clear) the per-shard LLM cache handle. The server
    /// calls this once at shard startup with an open
    /// `LlmCacheDb`; no-schema deployments and tests pass
    /// `None`.
    #[must_use]
    pub fn with_llm_cache(mut self, cache: Option<Arc<Mutex<LlmCacheDb>>>) -> Self {
        self.llm_cache = cache;
        self
    }

    /// Install (or clear) the per-shard tantivy handle. The server
    /// calls this once at shard startup with the
    /// `TantivyShard` returned by `TantivyShard::open`. Tests
    /// and no-schema deployments pass `None`.
    #[must_use]
    pub fn with_tantivy(mut self, tantivy: Option<Arc<TantivyShard>>) -> Self {
        self.tantivy = tantivy;
        self
    }

    /// Install (or clear) the memory text indexer dispatcher.
    /// The matching drain task is spawned separately by the caller
    /// (server spawn path uses `glommio::spawn_local`).
    #[must_use]
    pub fn with_memory_text_dispatcher(
        mut self,
        dispatcher: Option<Arc<MemoryTextDispatcher>>,
    ) -> Self {
        self.memory_text_dispatcher = dispatcher;
        self
    }

    /// Install (or clear) the statement text indexer dispatcher.
    /// Server-spawn pairs this with the drain task; tests pass `None`.
    #[must_use]
    pub fn with_statement_text_dispatcher(
        mut self,
        dispatcher: Option<Arc<StatementTextDispatcher>>,
    ) -> Self {
        self.statement_text_dispatcher = dispatcher;
        self
    }

    /// Replace the lexical retriever. Constructed mandatorily by
    /// the server's shard spawn from the per-shard `TantivyShard`;
    /// tests inject mocks via [`crate::test_support`].
    #[must_use]
    pub fn with_lexical_retriever(mut self, retriever: Arc<dyn LexicalRetriever>) -> Self {
        self.lexical_retriever = retriever;
        self
    }

    /// Replace the semantic retriever.
    #[must_use]
    pub fn with_semantic_retriever(mut self, retriever: Arc<dyn SemanticRetriever>) -> Self {
        self.semantic_retriever = retriever;
        self
    }

    /// Replace the graph retriever.
    #[must_use]
    pub fn with_graph_retriever(mut self, retriever: Arc<dyn GraphRetriever>) -> Self {
        self.graph_retriever = retriever;
        self
    }

    /// Install the cross-encoder slot used by the rerank pass on
    /// the hybrid RECALL path. Pass `CrossEncoderSlot::Enabled(arc)`
    /// when the model is loaded; pass `CrossEncoderSlot::Disabled`
    /// when the operator opted out via `rerank.enabled = false` so
    /// request-time clients learn the slot's intent.
    #[must_use]
    pub fn with_cross_encoder(mut self, slot: CrossEncoderSlot) -> Self {
        self.cross_encoder = slot;
        self
    }

    /// Install (or clear) the WAL sink for opaque-body event
    /// publishing. The shard's spawn path wires the same sink that
    /// the writer uses, so substrate and typed-graph events share one
    /// LSN domain.
    #[must_use]
    pub fn with_wal_sink(mut self, sink: Option<Arc<dyn WalSink>>) -> Self {
        self.wal_sink = sink;
        self
    }

    /// Publish a opaque-body event: WAL-append the CBOR-encoded
    /// payload (if a sink is wired), then publish to the bus with
    /// the assigned LSN. The `kind` discriminates the WAL record
    /// type so subscribe-replay can decode it back into the matching
    /// `GraphEventPayload` variant.
    ///
    /// `make_envelope` builds the bus envelope from the assigned LSN.
    /// Most callers will just stamp `lsn` and clone their payload in.
    pub async fn publish_graph<F>(
        &self,
        kind: brain_storage::wal::kinds::WalRecordKind,
        payload: brain_protocol::GraphEventPayload,
        agent_id: brain_core::AgentId,
        make_envelope: F,
    ) where
        F: FnOnce(u64, brain_protocol::GraphEventPayload) -> EventEnvelope,
    {
        debug_assert!(
            kind.has_opaque_body(),
            "publish_graph expects a opaque-body WalRecordKind, got {kind:?}"
        );
        let lsn = if let Some(sink) = &self.wal_sink {
            // CBOR-encode the typed-graph event, then frame it in the same
            // opaque-body envelope every other typed-graph record uses:
            // `agent_id (16 B) || body`. `WalPayload::decode` strips that
            // 16-byte prefix before handing the body to subscribe-replay's
            // `from_wal_record`, so the prefix is mandatory — without it the
            // CBOR body would be decoded starting 16 bytes in and fail.
            let body = {
                let mut buf = Vec::with_capacity(16);
                buf.extend_from_slice(&<[u8; 16]>::from(agent_id));
                match ciborium::into_writer(&payload, &mut buf) {
                    Ok(()) => buf,
                    Err(e) => {
                        tracing::warn!(error = %e, "CBOR encode of typed-graph event failed; publishing bus-only");
                        let _ = self.events.publish(make_envelope(0, payload));
                        return;
                    }
                }
            };
            let body_len = body.len();
            let record = brain_storage::wal::record::WalRecord {
                lsn: brain_storage::wal::record::Lsn(0),
                kind,
                // Mark as a subscribe-replay change-feed event so recovery
                // skips it — the durable write record carries the state.
                // Without this flag, recovery would try to rkyv-decode this
                // CBOR body as a row and fail (kinds collide across the two
                // record classes).
                flags: brain_storage::wal::record::FLAG_SUBSCRIBE_EVENT,
                timestamp_ns: now_unix_nanos_ctx(),
                agent_id_lo64: 0,
                payload: body,
            };
            match sink.append(record).await {
                Ok(lsn) => {
                    tracing::trace!(
                        ?kind,
                        body_len,
                        lsn = lsn.raw(),
                        "typed-graph event WAL-recorded"
                    );
                    lsn.raw()
                }
                Err(e) => {
                    tracing::warn!(error = %e, "typed-graph event WAL append failed; bus-only publish");
                    self.events.current_lsn().saturating_add(1)
                }
            }
        } else {
            // No sink wired — fall through to bus's allocator.
            0
        };
        let env = make_envelope(lsn, payload);
        if lsn == 0 {
            self.events.publish(env);
        } else {
            self.events.publish_prestamped(env);
        }
    }
}

fn now_unix_nanos_ctx() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
