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
use brain_index::{GraphRetriever, LexicalRetriever, SemanticRetriever, TantivyShard};
use brain_metadata::LlmCacheDb;
use brain_planner::{ExecutorContext, PlannerContext};
use parking_lot::{Mutex, RwLock};

use crate::access_buffer::AccessBuffer;
use crate::ops::text_indexer::{MemoryTextDispatcher, StatementTextDispatcher};
use crate::ops::writer::WalSink;
use crate::schema_gate::SchemaGate;
use crate::subscribe::{EventBus, EventEnvelope, SubscriptionRegistry};
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
    /// Per-shard LLM extractor response cache (spec §15.4 / §26).
    /// `None` when no API keys are configured, the cache file
    /// failed to open, or the deployment runs substrate-only.
    /// LLM extractors thread this into their cache lookups via
    /// the registry; later ops (RECALL provenance lookups, cache
    /// admin endpoints) can read through this field directly.
    pub llm_cache: Option<Arc<Mutex<LlmCacheDb>>>,
    /// Per-shard tantivy index handle (phase 22.1). `None` until
    /// the server's shard-spawn path wires it via
    /// [`OpsContext::with_tantivy`]. The retriever (22.5) and
    /// indexer workers (22.3 / 22.4) borrow through this field;
    /// substrate-only deployments leave it `None`.
    pub tantivy: Option<Arc<TantivyShard>>,
    /// Memory text indexer dispatcher (phase 22.3). `None` for
    /// substrate-only deployments and tests that don't spawn the
    /// drain task. ENCODE / FORGET handlers check this slot
    /// post-WAL-commit and enqueue an indexer op when present.
    pub memory_text_dispatcher: Option<Arc<MemoryTextDispatcher>>,
    /// Statement text indexer dispatcher (phase 22.4). Wired
    /// alongside `memory_text_dispatcher`; statement_create /
    /// supersede / tombstone / retract handlers enqueue
    /// Upsert / Delete events post-commit.
    pub statement_text_dispatcher: Option<Arc<StatementTextDispatcher>>,
    /// Per-shard lexical retriever (phase 22.5). Reads the
    /// tantivy indexes maintained by the 22.3 + 22.4 workers.
    /// Phase 23's hybrid query consumes this slot; substrate-
    /// only deployments leave it `None`.
    pub lexical_retriever: Option<Arc<dyn LexicalRetriever>>,
    /// Per-shard semantic retriever (phase 23.1). Reads the
    /// substrate memory HNSW + (when wired) the statement
    /// HNSW. Phase 23's hybrid query consumes this slot
    /// alongside [`lexical_retriever`].
    pub semantic_retriever: Option<Arc<dyn SemanticRetriever>>,
    /// Per-shard graph retriever (phase 23.2). Reads the
    /// entity / relation / statement redb tables. Phase 23's
    /// hybrid query consumes this slot alongside the lexical
    /// + semantic retrievers.
    pub graph_retriever: Option<Arc<dyn GraphRetriever>>,
    /// Schema-declared gate (phase 23.11). Lock-free read on
    /// the RECALL hot path; flipped to `true` by
    /// `handle_schema_upload` after a successful commit. Spec
    /// §28/08 §1.
    pub schema_gate: SchemaGate,
    /// WAL append sink for the knowledge-layer subscribe-replay
    /// pipeline. Knowledge handlers ([`crate::ops::knowledge_*`])
    /// call [`OpsContext::publish_knowledge`] after their successful
    /// redb commit; that helper appends a `WalPayload::Knowledge`
    /// record carrying the rkyv-encoded
    /// [`brain_protocol::knowledge::KnowledgeEventPayload`] body, then
    /// publishes the matching [`EventEnvelope`] on the bus with the
    /// WAL-assigned LSN. When `None`, the helper falls back to a
    /// pure bus publish (test wiring / substrate-only deployments).
    ///
    /// Knowledge ops are post-commit WAL'd (not pre-commit like
    /// substrate ENCODE/FORGET): redb is the source of truth for
    /// knowledge state; the WAL record exists purely so subscribe-
    /// replay can reconstruct the event stream. A crash between
    /// commit and WAL append loses the matching subscribe event for
    /// that op, not the underlying knowledge data.
    pub wal_sink: Option<Arc<dyn WalSink>>,
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
            llm_cache: None,
            tantivy: None,
            memory_text_dispatcher: None,
            statement_text_dispatcher: None,
            lexical_retriever: None,
            semantic_retriever: None,
            graph_retriever: None,
            schema_gate: SchemaGate::default(),
            wal_sink: None,
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

    /// Install (or clear) the per-shard LLM cache handle. Phase
    /// 21.5 calls this once at shard startup with an open
    /// `LlmCacheDb`; substrate-only deployments and tests pass
    /// `None`.
    #[must_use]
    pub fn with_llm_cache(mut self, cache: Option<Arc<Mutex<LlmCacheDb>>>) -> Self {
        self.llm_cache = cache;
        self
    }

    /// Install (or clear) the per-shard tantivy handle. Phase
    /// 22.1 calls this once at shard startup with the
    /// `TantivyShard` returned by `TantivyShard::open`. Tests
    /// and substrate-only deployments pass `None`.
    #[must_use]
    pub fn with_tantivy(mut self, tantivy: Option<Arc<TantivyShard>>) -> Self {
        self.tantivy = tantivy;
        self
    }

    /// Install (or clear) the memory text indexer dispatcher
    /// (phase 22.3). The matching drain task is spawned
    /// separately by the caller (server spawn path uses
    /// `glommio::spawn_local`).
    #[must_use]
    pub fn with_memory_text_dispatcher(
        mut self,
        dispatcher: Option<Arc<MemoryTextDispatcher>>,
    ) -> Self {
        self.memory_text_dispatcher = dispatcher;
        self
    }

    /// Install (or clear) the statement text indexer dispatcher
    /// (phase 22.4). Server-spawn pairs this with the drain
    /// task; tests pass `None`.
    #[must_use]
    pub fn with_statement_text_dispatcher(
        mut self,
        dispatcher: Option<Arc<StatementTextDispatcher>>,
    ) -> Self {
        self.statement_text_dispatcher = dispatcher;
        self
    }

    /// Install (or clear) the lexical retriever (phase 22.5).
    /// Phase 23's hybrid query path reads through this slot.
    #[must_use]
    pub fn with_lexical_retriever(mut self, retriever: Option<Arc<dyn LexicalRetriever>>) -> Self {
        self.lexical_retriever = retriever;
        self
    }

    /// Install (or clear) the semantic retriever (phase 23.1).
    /// Phase 23's hybrid query path reads through this slot.
    #[must_use]
    pub fn with_semantic_retriever(
        mut self,
        retriever: Option<Arc<dyn SemanticRetriever>>,
    ) -> Self {
        self.semantic_retriever = retriever;
        self
    }

    /// Install (or clear) the graph retriever (phase 23.2).
    /// Phase 23's hybrid query path reads through this slot.
    #[must_use]
    pub fn with_graph_retriever(mut self, retriever: Option<Arc<dyn GraphRetriever>>) -> Self {
        self.graph_retriever = retriever;
        self
    }

    /// Install the schema-declared gate (phase 23.11). The
    /// server's per-shard spawn path seeds this from the
    /// metadata DB at startup.
    #[must_use]
    pub fn with_schema_gate(mut self, gate: SchemaGate) -> Self {
        self.schema_gate = gate;
        self
    }

    /// Install (or clear) the WAL sink for knowledge-layer event
    /// publishing. The shard's spawn path wires the same sink that
    /// the writer uses, so substrate and knowledge events share one
    /// LSN domain.
    #[must_use]
    pub fn with_wal_sink(mut self, sink: Option<Arc<dyn WalSink>>) -> Self {
        self.wal_sink = sink;
        self
    }

    /// Publish a knowledge-layer event: WAL-append the rkyv-encoded
    /// payload (if a sink is wired), then publish to the bus with
    /// the assigned LSN. The `kind` discriminates the WAL record
    /// type so subscribe-replay can decode it back into the matching
    /// `KnowledgeEventPayload` variant.
    ///
    /// `agent_id` stamps the WAL record so subscribe-replay can
    /// route knowledge events through the per-agent `agents`
    /// filter — without it, a multi-tenant subscriber would
    /// silently drop every replayed knowledge event.
    ///
    /// `make_envelope` builds the bus envelope from the assigned LSN.
    /// Most callers will just stamp `lsn` and clone their payload in.
    pub async fn publish_knowledge<F>(
        &self,
        kind: brain_storage::wal::kinds::WalRecordKind,
        agent_id: brain_core::AgentId,
        payload: brain_protocol::knowledge::KnowledgeEventPayload,
        make_envelope: F,
    ) where
        F: FnOnce(u64, brain_protocol::knowledge::KnowledgeEventPayload) -> EventEnvelope,
    {
        debug_assert!(
            kind.is_knowledge(),
            "publish_knowledge expects a knowledge-layer WalRecordKind, got {kind:?}"
        );
        let lsn = if let Some(sink) = &self.wal_sink {
            // rkyv-encode the typed payload as the WAL record body.
            // Subscribe-replay's `from_wal_record` decodes it back.
            let body = match rkyv::to_bytes::<_, 1024>(&payload) {
                Ok(b) => b.into_vec(),
                Err(e) => {
                    tracing::warn!(error = %e, "rkyv encode of knowledge event failed; publishing bus-only");
                    let _ = self.events.publish(make_envelope(0, payload));
                    return;
                }
            };
            let body_len = body.len();
            let agent_bytes: [u8; 16] = agent_id.into();
            let agent_id_lo64 = u64::from_be_bytes(
                agent_bytes[8..16]
                    .try_into()
                    .expect("invariant: AgentId is 16 bytes"),
            );
            // Build through KnowledgeRecord so the payload bytes carry
            // the agent_id prefix that subscribe-replay's `from_wal_record`
            // decodes back into `EventEnvelope.agent_id`.
            let typed = brain_storage::wal::payload::WalPayload::Knowledge(
                brain_storage::wal::payload::KnowledgeRecord::new(kind, agent_id, body),
            );
            let record = brain_storage::wal::record::WalRecord::from_typed(
                brain_storage::wal::record::Lsn(0),
                0,
                now_unix_nanos_ctx(),
                agent_id_lo64,
                &typed,
            );
            match sink.append(record).await {
                Ok(lsn) => {
                    tracing::trace!(
                        ?kind,
                        body_len,
                        lsn = lsn.raw(),
                        "knowledge event WAL-recorded"
                    );
                    lsn.raw()
                }
                Err(e) => {
                    tracing::warn!(error = %e, "knowledge event WAL append failed; bus-only publish");
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
