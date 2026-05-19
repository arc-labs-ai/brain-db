//! Real per-shard write surface.
//!
//! Implements `brain_planner::WriterHandle` against real
//! `MetadataDb` + `HnswWriter`. Idempotency lives here because spec
//! §08/04 §4 + §07/06 §3 mandate the lookup-then-act protocol with
//! the response payload written in the **same redb txn** as the
//! memory row.
//!
//! **No WAL** — spec §08/08 §10's group-commit channel-fed writer
//! lands in Phase 8 / 9. The trait surface doesn't change; production
//! swaps the implementation.
//!
//! Concurrency: every interior mutable piece is `Mutex`-wrapped.
//! Concurrent submits serialise on the metadata mutex; throughput is
//! bounded by redb's single-writer-per-database lock, which matches
//! the spec §07/08 §3 single-writer-per-shard discipline.
//!
//! ## Edge maintenance
//!
//! - **Encode-inline edges** (spec §09/02 §1.5): each `EncodeOpEdge`
//!   targeting a live memory is inserted into `edges_out` + `edges_in`
//!   via [`brain_metadata::tables::edge::link`], and the source /
//!   target memory rows' `edges_out_count` / `edges_in_count` denorms
//!   are bumped — all inside the same write txn as the memory row.
//! - **LINK** (spec §09/07 §1-§3): same pattern. `do_link` returns
//!   `already_existed=true` when the canonical `(source, kind, target)`
//!   was present (overwrite-weight semantics, no count bump).
//! - **UNLINK** (spec §09/07 §4-§5): non-existent edge is a no-op
//!   (`removed=false`), not an error. Successful unlink decrements
//!   both counts.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind, ShardId};
use brain_index::Writer as HnswWriter;
use brain_metadata::tables::edge::{
    self, derived_by, origin, zero_disambiguator, EdgeData, EdgeKey, EDGES_REVERSE_TABLE,
    EDGES_TABLE,
};
use brain_metadata::tables::idempotency::{IdempotencyEntry, IDEMPOTENCY_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_planner::{
    EdgeOutcome, EncodeAck, EncodeOp, ForgetAck, ForgetOp, ForgetOutcome, LinkAck, LinkOp,
    SharedMetadataDb, TxnBatch, TxnBatchAck, UnlinkAck, UnlinkOp, WriterError, WriterHandle,
};
use brain_protocol::response::EventType;
use parking_lot::Mutex;
use redb::ReadableTable;
use uuid::Uuid;

use crate::subscribe::{EventBus, EventEnvelope};

/// Real per-shard writer backed by `MetadataDb` + `HnswWriter`. No
/// WAL — Phase 8 / 9 swap this for a WAL-backed implementation
/// without changing `WriterHandle`'s public surface.
pub struct RealWriterHandle {
    metadata: SharedMetadataDb,
    hnsw_writer: Mutex<HnswWriter<384>>,
    /// In-process slot counter. Phase 8 / 9 will replace with the
    /// arena allocator. Starts at 1.
    next_slot: AtomicU64,
    /// Memories we've tombstoned this process-lifetime. Used to
    /// surface `AlreadyTombstoned` per spec §08/06 §10 when a
    /// **different** RequestId targets a previously-tombstoned id.
    /// (Same-RequestId replay is caught by the idempotency table.)
    tombstoned: Mutex<HashSet<MemoryId>>,
    /// Agent id stamped on every memory metadata row. Phase 9 will
    /// derive this from the authenticated connection; for now it's
    /// nil. Carried as a field so tests + the future server can pin
    /// it without re-creating the writer.
    agent_id: AgentId,
    /// Shard id stamped into every `MemoryId` this writer issues.
    /// Routing back to the owning shard (LINK / UNLINK / FORGET in
    /// `brain-server::network::dispatch::shard_for_memory`) reads
    /// the shard prefix from the `MemoryId`, so stamping the wrong
    /// shard here silently steers follow-up ops to the wrong shard
    /// and surfaces as `NotFound`. Defaults to `0`; production
    /// callers must override via [`Self::with_shard_id`].
    shard_id: ShardId,
    /// Change-feed publisher (sub-task 7.10). Single-op encode/forget
    /// commits and TXN_COMMIT batches publish here *after* the redb
    /// commit() succeeds. Optional so existing callers don't break
    /// (defaults to no publication — events are dropped on the floor).
    events: Option<Arc<EventBus>>,
    /// WAL append sink (Phase 9 wiring). When `Some`, every write
    /// op appends a typed [`brain_storage::wal::payload::WalPayload`]
    /// record to the WAL **before** mutating redb — establishing the
    /// spec §05/07 durability barrier. The returned LSN is stamped
    /// onto the published event so subscribe-replay finds the right
    /// position. When `None`, the writer falls back to the legacy
    /// "redb-first, EventBus mints LSN" path used by unit tests that
    /// don't spin up a shard.
    wal_sink: Option<Arc<dyn wal_sink::WalSink>>,
    /// Optional non-blocking sender feeding the per-shard
    /// AutoEdgeWorker. Each successful ENCODE enqueues
    /// `(memory_id, vector)` post-fsync + post-commit + post-HNSW; the
    /// worker drains the channel and writes SimilarTo edges back into
    /// the unified edge tables. `None` means the worker isn't wired
    /// (substrate-only test fixtures); enqueue becomes a no-op.
    auto_edge_tx: Option<flume::Sender<AutoEdgeEnqueue>>,
    /// Optional non-blocking sender feeding the per-shard
    /// ExtractorWorker. Each successful ENCODE enqueues
    /// `(memory_id, text)` post-WAL-fsync + post-commit + post-HNSW;
    /// the worker drains the channel and runs the three-tier
    /// extractor pipeline against the text. `None` means the worker
    /// isn't wired (substrate-only test fixtures); enqueue becomes
    /// a no-op. The `Arc<str>` keeps the payload cheap to push and
    /// avoids the worker re-reading text from the metadata DB on a
    /// hot path.
    extractor_tx: Option<flume::Sender<ExtractorEnqueue>>,
    /// Shared metric handle for the AutoEdgeWorker family. When
    /// wired (production), `try_enqueue_auto_edge` bumps `drops_total`
    /// on `Full`. The worker holds the same `Arc` and publishes
    /// edges-written / cycle-duration / neighbours-found into it.
    /// `None` in test fixtures that don't care about observability.
    auto_edge_metrics: Option<Arc<crate::worker_metrics::AutoEdgeMetrics>>,
    /// Companion to [`Self::auto_edge_metrics`] for the extractor
    /// pipeline. Writer bumps `drops_total`; worker publishes the
    /// rest.
    extractor_metrics: Option<Arc<crate::worker_metrics::ExtractorMetrics>>,
}

/// What the writer pushes into the AutoEdgeWorker's channel after a
/// successful ENCODE. The vector is carried inline so the worker can
/// run `HNSW.search_active` without re-reading from arena or HNSW (the
/// HNSW reader has no public vector accessor, and arena reads would
/// require crossing the storage boundary into the worker).
pub type AutoEdgeEnqueue = (brain_core::MemoryId, [f32; brain_embed::VECTOR_DIM]);

/// What the writer pushes into the ExtractorWorker's channel after a
/// successful ENCODE. Text travels inline as an `Arc<str>` so the
/// worker can clone cheaply across cycles without re-reading the
/// row from redb (which would re-cross the metadata lock on the
/// extractor's hot path).
pub type ExtractorEnqueue = (brain_core::MemoryId, std::sync::Arc<str>);

impl RealWriterHandle {
    #[must_use]
    pub fn new(metadata: SharedMetadataDb, hnsw_writer: HnswWriter<384>) -> Self {
        // Materialise the tables we read from. redb creates tables
        // on first write_txn().open_table(), but read_txn() on a
        // never-opened table returns `TableDoesNotExist`. We do a
        // one-time empty write txn at construction so subsequent
        // idempotency + metadata reads succeed even before the
        // first submit.
        {
            let mut db = metadata.lock();
            if let Ok(wtxn) = db.write_txn() {
                let _ = wtxn.open_table(MEMORIES_TABLE);
                let _ = wtxn.open_table(IDEMPOTENCY_TABLE);
                let _ = wtxn.open_table(EDGES_TABLE);
                let _ = wtxn.open_table(EDGES_REVERSE_TABLE);
                let _ = wtxn.commit();
            }
        }
        Self {
            metadata,
            hnsw_writer: Mutex::new(hnsw_writer),
            next_slot: AtomicU64::new(1),
            tombstoned: Mutex::new(HashSet::new()),
            agent_id: AgentId(Uuid::nil()),
            shard_id: 0,
            events: None,
            wal_sink: None,
            auto_edge_tx: None,
            extractor_tx: None,
            auto_edge_metrics: None,
            extractor_metrics: None,
        }
    }

    #[must_use]
    pub fn with_agent_id(mut self, agent_id: AgentId) -> Self {
        self.agent_id = agent_id;
        self
    }

    /// Stamp the shard prefix on every `MemoryId` this writer issues.
    /// Production must call this with the owning shard's id;
    /// otherwise LINK / UNLINK / FORGET route to the wrong shard and
    /// return `NotFound`. Tests on a single shard can keep the
    /// default (`0`).
    #[must_use]
    pub fn with_shard_id(mut self, shard_id: ShardId) -> Self {
        self.shard_id = shard_id;
        self
    }

    /// Wire the change-feed bus. After this call every successful
    /// commit publishes an [`EventEnvelope`] onto the bus.
    #[must_use]
    pub fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.events = Some(bus);
        self
    }

    /// Wire the WAL sink. After this call every write op runs the
    /// spec §05/07 ordering — WAL append → fsync → redb → indexes →
    /// publish — so a restart can replay the durable log onto a fresh
    /// shard and a subscribe-replay can synthesise the change feed
    /// from segment data.
    #[must_use]
    pub fn with_wal_sink(mut self, sink: Arc<dyn wal_sink::WalSink>) -> Self {
        self.wal_sink = Some(sink);
        self
    }

    /// Wire the AutoEdgeWorker's feed channel. After this call every
    /// successful ENCODE enqueues `(memory_id, vector)` into the
    /// channel post-fsync + post-commit + post-HNSW. Without this call
    /// the enqueue path is a no-op and no auto-derived edges are
    /// produced — operators using a substrate-only deployment skip
    /// this step.
    ///
    /// The channel must be bounded; on `Full` the writer logs a warn
    /// and drops the enqueue (encode still succeeds — auto-edges are
    /// best-effort). On `Disconnected` the writer logs at debug and
    /// continues.
    pub fn set_auto_edge_sender(&mut self, sender: flume::Sender<AutoEdgeEnqueue>) {
        self.auto_edge_tx = Some(sender);
    }

    /// Accessor for the auto-edge sender so the encode + batch paths
    /// can call `try_send` without re-borrowing the whole writer.
    pub(super) fn auto_edge_sender(&self) -> Option<&flume::Sender<AutoEdgeEnqueue>> {
        self.auto_edge_tx.as_ref()
    }

    /// Wire the ExtractorWorker's feed channel. After this call every
    /// successful ENCODE enqueues `(memory_id, text)` post-fsync +
    /// post-commit + post-HNSW. Without this call the enqueue path is
    /// a no-op and no auto-extraction happens — operators running a
    /// substrate-only deployment skip this step.
    ///
    /// Channel must be bounded; on `Full` the writer logs a warn and
    /// drops the enqueue (encode still succeeds — extraction is best-
    /// effort). On `Disconnected` the writer logs at debug.
    pub fn set_extractor_sender(&mut self, sender: flume::Sender<ExtractorEnqueue>) {
        self.extractor_tx = Some(sender);
    }

    /// Accessor for the extractor sender so the encode + batch paths
    /// can call `try_send` without re-borrowing the whole writer.
    pub(super) fn extractor_sender(&self) -> Option<&flume::Sender<ExtractorEnqueue>> {
        self.extractor_tx.as_ref()
    }

    /// Install the shared `AutoEdgeMetrics` handle. The same `Arc`
    /// must be threaded to the matching `AutoEdgeWorker` so both
    /// sides see the same counters. Drop counters become visible to
    /// `/metrics` as soon as this is wired.
    pub fn set_auto_edge_metrics(&mut self, metrics: Arc<crate::worker_metrics::AutoEdgeMetrics>) {
        self.auto_edge_metrics = Some(metrics);
    }

    pub(super) fn auto_edge_metrics(&self) -> Option<&Arc<crate::worker_metrics::AutoEdgeMetrics>> {
        self.auto_edge_metrics.as_ref()
    }

    /// Install the shared `ExtractorMetrics` handle. Same semantics
    /// as [`Self::set_auto_edge_metrics`].
    pub fn set_extractor_metrics(&mut self, metrics: Arc<crate::worker_metrics::ExtractorMetrics>) {
        self.extractor_metrics = Some(metrics);
    }

    pub(super) fn extractor_metrics(
        &self,
    ) -> Option<&Arc<crate::worker_metrics::ExtractorMetrics>> {
        self.extractor_metrics.as_ref()
    }

    /// Publish an event. When a WAL sink is wired, callers pass
    /// `Some(lsn)` so the envelope carries the WAL-assigned LSN
    /// instead of the bus's internal allocator stamp.
    fn publish_with_lsn(&self, mut env: EventEnvelope, lsn: Option<u64>) {
        if let Some(bus) = &self.events {
            if let Some(lsn) = lsn {
                // Pre-stamp so the bus's LsnAllocator becomes a noop
                // for events that already carry a durable LSN. The
                // bus's send() is the only side-effect we want.
                env.lsn = lsn;
                bus.publish_prestamped(env);
            } else {
                bus.publish(env);
            }
        }
    }
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

impl WriterHandle for RealWriterHandle {
    fn submit_encode<'a>(
        &'a self,
        op: EncodeOp,
    ) -> Pin<Box<dyn Future<Output = Result<EncodeAck, WriterError>> + 'a>> {
        Box::pin(async move { do_encode(self, op).await })
    }

    fn submit_forget<'a>(
        &'a self,
        op: ForgetOp,
    ) -> Pin<Box<dyn Future<Output = Result<ForgetAck, WriterError>> + 'a>> {
        Box::pin(async move { do_forget(self, op).await })
    }

    fn submit_link<'a>(
        &'a self,
        op: LinkOp,
    ) -> Pin<Box<dyn Future<Output = Result<LinkAck, WriterError>> + 'a>> {
        Box::pin(async move { do_link(self, op).await })
    }

    fn submit_unlink<'a>(
        &'a self,
        op: UnlinkOp,
    ) -> Pin<Box<dyn Future<Output = Result<UnlinkAck, WriterError>> + 'a>> {
        Box::pin(async move { do_unlink(self, op).await })
    }

    fn reserve_memory_id<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<MemoryId, WriterError>> + 'a>> {
        Box::pin(async move {
            let slot = self.next_slot.fetch_add(1, Ordering::Relaxed);
            Ok(MemoryId::pack(self.shard_id, slot, 1))
        })
    }

    fn submit_batch<'a>(
        &'a self,
        batch: brain_planner::TxnBatch,
    ) -> Pin<Box<dyn Future<Output = Result<brain_planner::TxnBatchAck, WriterError>> + 'a>> {
        Box::pin(async move { do_submit_batch(self, batch).await })
    }

    fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    fn enqueue_for_extraction(&self, memory_id: MemoryId, text: &str) -> bool {
        let Some(sender) = self.extractor_sender() else {
            return false;
        };
        let payload: ExtractorEnqueue = (memory_id, std::sync::Arc::from(text));
        match sender.try_send(payload) {
            Ok(()) => true,
            Err(flume::TrySendError::Full(_)) => {
                tracing::warn!(
                    memory_id = ?memory_id,
                    "extract_backfill: extractor channel full; dropping enqueue"
                );
                false
            }
            Err(flume::TrySendError::Disconnected(_)) => {
                tracing::debug!(
                    memory_id = ?memory_id,
                    "extract_backfill: extractor worker disconnected"
                );
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Handler modules — one per cognitive op handler.
// ---------------------------------------------------------------------------

mod encode;
mod forget;
mod link;
mod unlink;
pub mod wal_sink;

use encode::do_encode;
use forget::do_forget;
use link::do_link;
use unlink::do_unlink;

pub use wal_sink::{
    channel_wal_sink, channel_wal_sink_with_capacity, ChannelWalSink, FailingWalSink, NoopWalSink,
    RecordingWalSink, WalAppendMessage, WalSink, WalSinkError, DEFAULT_WAL_DRAIN_CAPACITY,
};

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Bump `edges_out_count` (or `edges_in_count`) on `memory_id` by
/// `delta`. No-op if the memory row doesn't exist (the LINK / UNLINK
/// paths validate existence separately; this is defensive).
fn bump_edge_count(
    memories_t: &mut redb::Table<'_, [u8; 16], MemoryMetadata>,
    memory_id: MemoryId,
    out: bool,
    delta: i32,
) -> Result<(), WriterError> {
    let key = memory_id.to_be_bytes();
    let prior = memories_t
        .get(key)
        .map_err(|e| WriterError::Internal(format!("bump_edge_count get: {e:?}")))?
        .map(|access| access.value());
    let Some(mut meta) = prior else {
        return Ok(());
    };
    let cur = if out {
        meta.edges_out_count
    } else {
        meta.edges_in_count
    };
    let new = if delta >= 0 {
        cur.saturating_add(delta as u32)
    } else {
        cur.saturating_sub((-delta) as u32)
    };
    if out {
        meta.edges_out_count = new;
    } else {
        meta.edges_in_count = new;
    }
    memories_t
        .insert(key, meta)
        .map_err(|e| WriterError::Internal(format!("bump_edge_count insert: {e:?}")))?;
    Ok(())
}

fn hex_short(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(8);
    for b in &bytes[..4] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// submit_batch — atomic apply of a TXN_COMMIT buffer.
// ---------------------------------------------------------------------------

async fn do_submit_batch(
    writer: &RealWriterHandle,
    batch: TxnBatch,
) -> Result<TxnBatchAck, WriterError> {
    use brain_core::TxnId;
    use brain_storage::wal::payload::{
        EdgePayload as WalEdgePayload, EncodePayload as WalEncodePayload,
        ForgetMode as WalForgetMode, ForgetPayload as WalForgetPayload,
        ForgetReason as WalForgetReason, LinkPayload as WalLinkPayload,
        TxnBeginPayload as WalTxnBeginPayload, TxnCommitPayload as WalTxnCommitPayload,
        UnlinkPayload as WalUnlinkPayload, WalPayload,
    };
    use brain_storage::wal::record::{Lsn, WalRecord};

    use crate::idempotency::{
        encode_encode_payload, encode_forget_payload, encode_link_payload, encode_unlink_payload,
    };

    // ── WAL prologue (spec §05/07 + §05/08 §6). Bracket the batch
    // with TxnBegin / TxnCommit so recovery's buffered-apply replays
    // it atomically. The intra-batch ops go in between; recovery
    // doesn't apply any of them until it sees TxnCommit. ─────────
    let mut wal_lsn_for_encode: Vec<Option<u64>> = Vec::with_capacity(batch.memories.len());
    let mut wal_lsn_for_link: Vec<Option<u64>> = Vec::with_capacity(batch.links.len());
    let mut wal_lsn_for_unlink: Vec<Option<u64>> = Vec::with_capacity(batch.unlinks.len());
    let mut wal_lsn_for_forget: Vec<Option<u64>> = Vec::with_capacity(batch.forgets.len());
    let txn_id = TxnId::from(*uuid::Uuid::now_v7().as_bytes());
    if let Some(sink) = &writer.wal_sink {
        let agent_bytes: [u8; 16] = writer.agent_id.into();
        let agent_id_lo64 = u64::from_be_bytes(agent_bytes[8..16].try_into().unwrap());
        let stage_count =
            batch.memories.len() + batch.links.len() + batch.unlinks.len() + batch.forgets.len();
        let begin_ts = now_unix_nanos();
        let begin = WalPayload::TxnBegin(WalTxnBeginPayload {
            txn_id,
            expected_record_count: u32::try_from(stage_count).unwrap_or(u32::MAX),
        });
        sink.append(WalRecord::from_typed(
            Lsn(0),
            0,
            begin_ts,
            agent_id_lo64,
            &begin,
        ))
        .await
        .map_err(|e| WriterError::Internal(format!("txn wal begin: {e}")))?;

        for enc in &batch.memories {
            // Compute response_payload identically to the redb path so
            // recovery can replay the cached EncodeAck byte-for-byte.
            // edge_outcomes aren't known yet (the in-batch resolution
            // happens below) — recovery doesn't need them for arena/
            // hnsw correctness, only for the idempotency cache. Use
            // an empty edge_outcomes here; the redb write below will
            // produce the authoritative response. This is a known
            // simplification documented in `wal-tail-subscribe.md` —
            // an idempotency retry after a crash mid-batch would
            // return an empty edge_results vector instead of the
            // original. Worst case the caller re-issues edges.
            let response_payload = encode_encode_payload(enc.memory_id, &[]);
            let wal_edges: Vec<WalEdgePayload> = enc
                .edges
                .iter()
                .map(|e| WalEdgePayload {
                    source: brain_core::NodeRef::Memory(enc.memory_id),
                    target: brain_core::NodeRef::Memory(e.target),
                    kind: brain_core::EdgeKindRef::Builtin(e.kind),
                    weight: e.weight,
                    origin: brain_core::EdgeOrigin::Explicit,
                })
                .collect();
            let payload = WalPayload::Encode(WalEncodePayload {
                memory_id: enc.memory_id,
                request_id: enc.request_id,
                agent_id: enc.agent_id,
                context_id: enc.context_id,
                kind: enc.kind,
                salience_initial: enc.salience_initial,
                embedding_model_fp: enc.fingerprint,
                text: enc.text.clone(),
                vector: enc.vector.to_vec(),
                edges: wal_edges,
                request_hash: enc.request_hash,
                response_payload,
                deduplicate: false,
            });
            let lsn = sink
                .append(WalRecord::from_typed(
                    Lsn(0),
                    0,
                    enc.created_at_unix_nanos,
                    agent_id_lo64,
                    &payload,
                ))
                .await
                .map_err(|e| WriterError::Internal(format!("txn wal encode: {e}")))?;
            wal_lsn_for_encode.push(Some(lsn.raw()));
        }
        for link in &batch.links {
            let payload = WalPayload::Link(WalLinkPayload {
                source: brain_core::NodeRef::Memory(link.source),
                target: brain_core::NodeRef::Memory(link.target),
                edge_kind: brain_core::EdgeKindRef::Builtin(link.kind),
                weight: link.weight,
                origin: brain_core::EdgeOrigin::Explicit,
            });
            let lsn = sink
                .append(WalRecord::from_typed(
                    Lsn(0),
                    0,
                    link.created_at_unix_nanos,
                    agent_id_lo64,
                    &payload,
                ))
                .await
                .map_err(|e| WriterError::Internal(format!("txn wal link: {e}")))?;
            wal_lsn_for_link.push(Some(lsn.raw()));
        }
        for unlink in &batch.unlinks {
            let payload = WalPayload::Unlink(WalUnlinkPayload {
                source: brain_core::NodeRef::Memory(unlink.source),
                target: brain_core::NodeRef::Memory(unlink.target),
                edge_kind: brain_core::EdgeKindRef::Builtin(unlink.kind),
                edge_seq: 0,
            });
            let lsn = sink
                .append(WalRecord::from_typed(
                    Lsn(0),
                    0,
                    unlink.created_at_unix_nanos,
                    agent_id_lo64,
                    &payload,
                ))
                .await
                .map_err(|e| WriterError::Internal(format!("txn wal unlink: {e}")))?;
            wal_lsn_for_unlink.push(Some(lsn.raw()));
        }
        for forget in &batch.forgets {
            let wal_mode = match forget.mode {
                brain_protocol::request::ForgetMode::Soft => WalForgetMode::Soft,
                brain_protocol::request::ForgetMode::Hard => WalForgetMode::Hard,
            };
            let payload = WalPayload::Forget(WalForgetPayload {
                memory_id: forget.memory_id,
                request_id: forget.request_id,
                agent_id: forget.agent_id,
                mode: wal_mode,
                reason: WalForgetReason::ClientRequest,
            });
            let lsn = sink
                .append(WalRecord::from_typed(
                    Lsn(0),
                    0,
                    forget.created_at_unix_nanos,
                    agent_id_lo64,
                    &payload,
                ))
                .await
                .map_err(|e| WriterError::Internal(format!("txn wal forget: {e}")))?;
            wal_lsn_for_forget.push(Some(lsn.raw()));
        }
        let commit_ts = now_unix_nanos();
        let commit = WalPayload::TxnCommit(WalTxnCommitPayload { txn_id });
        sink.append(WalRecord::from_typed(
            Lsn(0),
            0,
            commit_ts,
            agent_id_lo64,
            &commit,
        ))
        .await
        .map_err(|e| WriterError::Internal(format!("txn wal commit: {e}")))?;
    } else {
        // No WAL sink: leave the per-op LSN slots empty; the publish
        // path falls back to the bus allocator.
        for _ in &batch.memories {
            wal_lsn_for_encode.push(None);
        }
        for _ in &batch.links {
            wal_lsn_for_link.push(None);
        }
        for _ in &batch.unlinks {
            wal_lsn_for_unlink.push(None);
        }
        for _ in &batch.forgets {
            wal_lsn_for_forget.push(None);
        }
    }

    let mut encode_acks: Vec<EncodeAck> = Vec::with_capacity(batch.memories.len());
    let mut link_acks: Vec<LinkAck> = Vec::with_capacity(batch.links.len());
    let mut unlink_acks: Vec<UnlinkAck> = Vec::with_capacity(batch.unlinks.len());
    let mut forget_acks: Vec<ForgetAck> = Vec::with_capacity(batch.forgets.len());

    // ── HNSW inserts queued post-wtxn so we can report failures
    //    cleanly without leaving the index orphaned. ───────────────
    let mut hnsw_inserts: Vec<(MemoryId, [f32; brain_embed::VECTOR_DIM])> = Vec::new();
    let mut hnsw_tombstones: Vec<MemoryId> = Vec::new();

    // ── Change-feed envelopes for sub-task 7.10. Built during the
    //    write txn (so we capture pre-tombstone metadata snapshots);
    //    published after commit() succeeds — never on rollback. The
    //    order matches the batch's natural order: encodes first,
    //    then forgets (links/unlinks don't emit events in v1). ────
    struct PendingEvent {
        event_type: EventType,
        memory_id: MemoryId,
        context_id: ContextId,
        kind: MemoryKind,
        salience: f32,
        timestamp_unix_nanos: u64,
        text: Option<String>,
        /// WAL-assigned LSN for this event. `Some` when the batch was
        /// WAL-recorded; `None` for the test path where the bus mints
        /// the LSN.
        wal_lsn: Option<u64>,
        /// Per-op agent (not the per-shard writer.agent_id) — used
        /// so the subscribe `agents` filter can isolate per-tenant
        /// even inside a TXN batch.
        agent_id: brain_core::AgentId,
    }
    let mut pending_events: Vec<PendingEvent> = Vec::new();

    // Track in-batch creations so subsequent operations within the
    // same batch can see them (e.g., LINK targeting a memory ENCODEd
    // earlier in the same txn).
    let mut batch_memory_ids: HashSet<MemoryId> = HashSet::new();

    {
        let mut db = writer.metadata.lock();
        let wtxn = db
            .write_txn()
            .map_err(|e| WriterError::Internal(format!("batch write_txn: {e:?}")))?;
        {
            let mut memories_t = wtxn
                .open_table(MEMORIES_TABLE)
                .map_err(|e| WriterError::Internal(format!("batch open MEMORIES: {e:?}")))?;
            let mut edges_t = wtxn
                .open_table(EDGES_TABLE)
                .map_err(|e| WriterError::Internal(format!("batch open EDGES: {e:?}")))?;
            let mut edges_rev_t = wtxn
                .open_table(EDGES_REVERSE_TABLE)
                .map_err(|e| WriterError::Internal(format!("batch open EDGES_REVERSE: {e:?}")))?;
            let mut idem_t = wtxn
                .open_table(IDEMPOTENCY_TABLE)
                .map_err(|e| WriterError::Internal(format!("batch open IDEMPOTENCY: {e:?}")))?;

            // 1. Memories + their inline edges.
            for (enc_idx, enc) in batch.memories.iter().enumerate() {
                // Compute edge outcomes against committed + in-batch ids.
                let mut edge_outcomes: Vec<EdgeOutcome> = Vec::with_capacity(enc.edges.len());
                for edge in &enc.edges {
                    let exists = batch_memory_ids.contains(&edge.target)
                        || memories_t
                            .get(edge.target.to_be_bytes())
                            .map_err(|e| {
                                WriterError::Internal(format!("batch memories get: {e:?}"))
                            })?
                            .is_some();
                    edge_outcomes.push(if exists {
                        EdgeOutcome::Inserted
                    } else {
                        EdgeOutcome::TargetMissing
                    });
                }
                let inserted_count = edge_outcomes
                    .iter()
                    .filter(|o| matches!(o, EdgeOutcome::Inserted))
                    .count();

                // Insert edges + bump target in-counts.
                for (edge, outcome) in enc.edges.iter().zip(edge_outcomes.iter()) {
                    if !matches!(outcome, EdgeOutcome::Inserted) {
                        continue;
                    }
                    let data = EdgeData::new(
                        edge.weight,
                        origin::EXPLICIT,
                        derived_by::CLIENT,
                        enc.created_at_unix_nanos,
                    );
                    edge::link(
                        &mut edges_t,
                        &mut edges_rev_t,
                        brain_core::NodeRef::Memory(enc.memory_id),
                        brain_core::EdgeKindRef::Builtin(edge.kind),
                        brain_core::NodeRef::Memory(edge.target),
                        zero_disambiguator(),
                        &data,
                    )
                    .map_err(|e| WriterError::Internal(format!("batch edge::link: {e:?}")))?;
                    // Bump target's edges_in_count, but only if target
                    // is a committed memory; in-batch targets handle
                    // their own count below.
                    if !batch_memory_ids.contains(&edge.target) {
                        bump_edge_count(&mut memories_t, edge.target, false, 1)?;
                    }
                }

                // Build the metadata row.
                let mut meta = MemoryMetadata::new_active(
                    enc.memory_id,
                    enc.agent_id,
                    enc.context_id,
                    enc.memory_id.slot(),
                    /* slot_version */ 1,
                    enc.kind,
                    enc.fingerprint,
                    enc.salience_initial,
                    enc.text.len() as u32,
                    enc.created_at_unix_nanos,
                );
                meta.edges_out_count = u32::try_from(inserted_count).unwrap_or(u32::MAX);
                // edges_in_count starts at 0 — any in-batch edge to this
                // memory bumps it as part of *that* edge's loop below.
                // Stamp the per-encode WAL LSN; matches what
                // `wal_lsn_for_encode[enc_idx]` recorded earlier in
                // this same batch's prologue.
                if let Some(lsn) = wal_lsn_for_encode.get(enc_idx).copied().flatten() {
                    meta.encoded_at_lsn = lsn;
                }
                memories_t
                    .insert(enc.memory_id.to_be_bytes(), meta)
                    .map_err(|e| WriterError::Internal(format!("batch memories insert: {e:?}")))?;

                // Idempotency entry. Stamp the per-encode WAL LSN
                // (or 0 when no WAL sink is wired) so a retry replays
                // the original durable position to clients chaining
                // subscribe.
                let payload = encode_encode_payload(enc.memory_id, &edge_outcomes);
                let entry = IdempotencyEntry::new(
                    crate::idempotency::RESPONSE_KIND_ENCODE,
                    Some(enc.memory_id.to_be_bytes()),
                    payload,
                    enc.request_hash,
                    enc.created_at_unix_nanos,
                    wal_lsn_for_encode
                        .get(enc_idx)
                        .copied()
                        .flatten()
                        .unwrap_or(0),
                );
                idem_t
                    .insert(<[u8; 16]>::from(enc.request_id), entry)
                    .map_err(|e| {
                        WriterError::Internal(format!("batch idempotency insert (encode): {e:?}"))
                    })?;

                batch_memory_ids.insert(enc.memory_id);
                let edges_out_count_enc = u32::try_from(inserted_count).unwrap_or(u32::MAX);
                encode_acks.push(EncodeAck {
                    memory_id: enc.memory_id,
                    edge_results: edge_outcomes,
                    replayed: false,
                    // TXN batch path does not wire fingerprint dedup
                    // in this phase — dedup is a per-encode
                    // single-write feature and the batch commits in
                    // one redb txn; allowing dedup inside a batch
                    // would require cross-encode coordination. Phase
                    // 8.dedup+1 if needed.
                    was_deduplicated: false,
                    lsn: wal_lsn_for_encode.get(enc_idx).copied().flatten(),
                    edges_out_count: edges_out_count_enc,
                    created_at_unix_nanos: enc.created_at_unix_nanos,
                });
                hnsw_inserts.push((enc.memory_id, enc.vector));
                pending_events.push(PendingEvent {
                    event_type: EventType::Encoded,
                    memory_id: enc.memory_id,
                    context_id: enc.context_id,
                    kind: enc.kind,
                    salience: enc.salience_initial,
                    timestamp_unix_nanos: enc.created_at_unix_nanos,
                    text: Some(enc.text.clone()),
                    wal_lsn: wal_lsn_for_encode.get(enc_idx).copied().flatten(),
                    agent_id: enc.agent_id,
                });

                // Bump in-counts for any in-batch edges that target a
                // memory already inserted in this batch.
                // (Handled by reading the inserted row + writing back.)
                for edge in &enc.edges {
                    if !batch_memory_ids.contains(&edge.target) || edge.target == enc.memory_id {
                        continue;
                    }
                    // The target was inserted earlier in this batch.
                    bump_edge_count(&mut memories_t, edge.target, false, 1)?;
                }
            }

            // 2. Top-level LINKs.
            for (link_idx, link) in batch.links.iter().enumerate() {
                // Source/target must exist (committed or in-batch).
                let src_exists = batch_memory_ids.contains(&link.source)
                    || memories_t
                        .get(link.source.to_be_bytes())
                        .map_err(|e| WriterError::Internal(format!("batch get src: {e:?}")))?
                        .is_some();
                let tgt_exists = batch_memory_ids.contains(&link.target)
                    || memories_t
                        .get(link.target.to_be_bytes())
                        .map_err(|e| WriterError::Internal(format!("batch get tgt: {e:?}")))?
                        .is_some();
                if !src_exists {
                    return Err(WriterError::Internal(format!(
                        "LINK source memory {} not found in batch",
                        link.source.raw()
                    )));
                }
                if !tgt_exists {
                    return Err(WriterError::Internal(format!(
                        "LINK target memory {} not found in batch",
                        link.target.raw()
                    )));
                }
                let key = EdgeKey {
                    from: brain_core::NodeRef::Memory(link.source),
                    kind: brain_core::EdgeKindRef::Builtin(link.kind),
                    to: brain_core::NodeRef::Memory(link.target),
                    disambiguator: zero_disambiguator(),
                }
                .encode();
                let already_existed = edges_t
                    .get(key.as_slice())
                    .map_err(|e| WriterError::Internal(format!("batch get edge: {e:?}")))?
                    .is_some();
                let data = EdgeData::new(
                    link.weight,
                    origin::EXPLICIT,
                    derived_by::CLIENT,
                    link.created_at_unix_nanos,
                );
                edge::link(
                    &mut edges_t,
                    &mut edges_rev_t,
                    brain_core::NodeRef::Memory(link.source),
                    brain_core::EdgeKindRef::Builtin(link.kind),
                    brain_core::NodeRef::Memory(link.target),
                    zero_disambiguator(),
                    &data,
                )
                .map_err(|e| WriterError::Internal(format!("batch link insert: {e:?}")))?;
                if !already_existed {
                    bump_edge_count(&mut memories_t, link.source, true, 1)?;
                    bump_edge_count(&mut memories_t, link.target, false, 1)?;
                }
                let payload =
                    encode_link_payload(link.weight, link.created_at_unix_nanos, already_existed);
                let entry = IdempotencyEntry::new(
                    crate::idempotency::RESPONSE_KIND_LINK,
                    None,
                    payload,
                    link.request_hash,
                    link.created_at_unix_nanos,
                    wal_lsn_for_link
                        .get(link_idx)
                        .copied()
                        .flatten()
                        .unwrap_or(0),
                );
                idem_t
                    .insert(<[u8; 16]>::from(link.request_id), entry)
                    .map_err(|e| {
                        WriterError::Internal(format!("batch idem insert (link): {e:?}"))
                    })?;
                link_acks.push(LinkAck {
                    source: link.source,
                    target: link.target,
                    kind: link.kind,
                    weight: link.weight,
                    created_at_unix_nanos: link.created_at_unix_nanos,
                    already_existed,
                    replayed: false,
                });
            }

            // 3. UNLINKs.
            for (unlink_idx, unlink) in batch.unlinks.iter().enumerate() {
                let removed = edge::unlink(
                    &mut edges_t,
                    &mut edges_rev_t,
                    brain_core::NodeRef::Memory(unlink.source),
                    brain_core::EdgeKindRef::Builtin(unlink.kind),
                    brain_core::NodeRef::Memory(unlink.target),
                    zero_disambiguator(),
                )
                .map_err(|e| WriterError::Internal(format!("batch unlink: {e:?}")))?;
                if removed {
                    bump_edge_count(&mut memories_t, unlink.source, true, -1)?;
                    bump_edge_count(&mut memories_t, unlink.target, false, -1)?;
                }
                let payload = encode_unlink_payload(removed);
                let entry = IdempotencyEntry::new(
                    crate::idempotency::RESPONSE_KIND_UNLINK,
                    None,
                    payload,
                    unlink.request_hash,
                    unlink.created_at_unix_nanos,
                    wal_lsn_for_unlink
                        .get(unlink_idx)
                        .copied()
                        .flatten()
                        .unwrap_or(0),
                );
                idem_t
                    .insert(<[u8; 16]>::from(unlink.request_id), entry)
                    .map_err(|e| {
                        WriterError::Internal(format!("batch idem insert (unlink): {e:?}"))
                    })?;
                unlink_acks.push(UnlinkAck {
                    source: unlink.source,
                    target: unlink.target,
                    kind: unlink.kind,
                    removed,
                    replayed: false,
                });
            }

            // 4. FORGETs.
            for (forget_idx, forget) in batch.forgets.iter().enumerate() {
                let meta_row: Option<MemoryMetadata> = memories_t
                    .get(forget.memory_id.to_be_bytes())
                    .map_err(|e| WriterError::Internal(format!("batch get forget: {e:?}")))?
                    .map(|access| access.value());
                let exists = batch_memory_ids.contains(&forget.memory_id) || meta_row.is_some();
                let outcome = if !exists {
                    ForgetOutcome::MemoryNotFound
                } else if writer.tombstoned.lock().contains(&forget.memory_id) {
                    ForgetOutcome::AlreadyTombstoned
                } else {
                    ForgetOutcome::Tombstoned
                };
                if matches!(outcome, ForgetOutcome::Tombstoned) {
                    hnsw_tombstones.push(forget.memory_id);
                    // Build the pending event from the metadata row.
                    // In-batch encodes don't have a row yet (they're
                    // inserted above), but a same-batch encode→forget
                    // ordering is unusual; if no row, fall back to
                    // searching the batch's pending events.
                    let wal_lsn_here = wal_lsn_for_forget.get(forget_idx).copied().flatten();
                    let event = if let Some(m) = meta_row {
                        Some(PendingEvent {
                            event_type: EventType::Forgotten,
                            memory_id: forget.memory_id,
                            context_id: m.context(),
                            kind: m.kind().unwrap_or(MemoryKind::Episodic),
                            salience: m.salience,
                            timestamp_unix_nanos: forget.created_at_unix_nanos,
                            text: None,
                            wal_lsn: wal_lsn_here,
                            agent_id: forget.agent_id,
                        })
                    } else {
                        pending_events
                            .iter()
                            .rev()
                            .find(|p| {
                                p.memory_id == forget.memory_id
                                    && matches!(p.event_type, EventType::Encoded)
                            })
                            .map(|p| PendingEvent {
                                event_type: EventType::Forgotten,
                                memory_id: forget.memory_id,
                                context_id: p.context_id,
                                kind: p.kind,
                                salience: p.salience,
                                timestamp_unix_nanos: forget.created_at_unix_nanos,
                                text: None,
                                wal_lsn: wal_lsn_here,
                                agent_id: forget.agent_id,
                            })
                    };
                    if let Some(e) = event {
                        pending_events.push(e);
                    }
                }
                let payload = encode_forget_payload(forget.memory_id, outcome);
                let entry = IdempotencyEntry::new(
                    crate::idempotency::RESPONSE_KIND_FORGET,
                    Some(forget.memory_id.to_be_bytes()),
                    payload,
                    forget.request_hash,
                    forget.created_at_unix_nanos,
                    wal_lsn_for_forget
                        .get(forget_idx)
                        .copied()
                        .flatten()
                        .unwrap_or(0),
                );
                idem_t
                    .insert(<[u8; 16]>::from(forget.request_id), entry)
                    .map_err(|e| {
                        WriterError::Internal(format!("batch idem insert (forget): {e:?}"))
                    })?;
                forget_acks.push(ForgetAck {
                    memory_id: forget.memory_id,
                    outcome,
                    replayed: false,
                });
            }
        }
        wtxn.commit()
            .map_err(|e| WriterError::Internal(format!("batch commit: {e:?}")))?;
    }

    // Post-wtxn: HNSW. Failures here are logged; the redb state is
    // already durable. Same hazard as the non-txn ENCODE path.
    {
        let mut hnsw = writer.hnsw_writer.lock();
        for (id, vector) in &hnsw_inserts {
            hnsw.insert(*id, vector)
                .map_err(|e| WriterError::Internal(format!("batch hnsw insert: {e:?}")))?;
        }
        for id in &hnsw_tombstones {
            hnsw.mark_tombstoned(*id)
                .map_err(|e| WriterError::Internal(format!("batch hnsw tombstone: {e:?}")))?;
        }
    }
    // Auto-edge enqueue once HNSW catches up to redb — the worker
    // searches the same shared index after this point and would see
    // the new memory by id.
    for (id, vector) in &hnsw_inserts {
        try_enqueue_auto_edge(writer, *id, vector);
    }
    // Extractor enqueue mirrors the auto-edge enqueue: post-durability,
    // post-HNSW, and one push per memory in the batch. The text comes
    // straight from the batch entry — no metadata re-read.
    for enc in &batch.memories {
        try_enqueue_extractor(writer, enc.memory_id, &enc.text);
    }
    {
        let mut tombstoned = writer.tombstoned.lock();
        for id in &hnsw_tombstones {
            tombstoned.insert(*id);
        }
    }

    // ── Change-feed (sub-task 7.10). Publish in buffer order,
    //    stamping the WAL-assigned LSN onto each event so subscribe
    //    replay and the live tail share a coordinate system. ─────
    for ev in pending_events {
        writer.publish_with_lsn(
            EventEnvelope {
                lsn: 0,
                event_type: ev.event_type,
                memory_id: ev.memory_id,
                context_id: ev.context_id,
                kind: ev.kind,
                salience: ev.salience,
                timestamp_unix_nanos: ev.timestamp_unix_nanos,
                text: ev.text,
                knowledge_payload: None,
                edge_payload: None,
                agent_id: ev.agent_id,
            },
            ev.wal_lsn,
        );
    }

    Ok(TxnBatchAck {
        encodes: encode_acks,
        links: link_acks,
        unlinks: unlink_acks,
        forgets: forget_acks,
    })
}

/// Enqueue `(memory_id, vector)` onto the AutoEdgeWorker channel if
/// one is wired. Non-blocking; full channel logs a warn and drops
/// (encode succeeds without auto-edges). Disconnected channel logs at
/// debug. This is the single enqueue point both the single-encode and
/// TXN batch paths route through.
pub(super) fn try_enqueue_auto_edge(
    writer: &RealWriterHandle,
    memory_id: MemoryId,
    vector: &[f32; brain_embed::VECTOR_DIM],
) {
    let Some(sender) = writer.auto_edge_sender() else {
        return;
    };
    match sender.try_send((memory_id, *vector)) {
        Ok(()) => {
            tracing::trace!(
                memory_id = ?memory_id,
                "auto_edge enqueue"
            );
        }
        Err(flume::TrySendError::Full(_)) => {
            if let Some(m) = writer.auto_edge_metrics() {
                m.inc_drop();
            }
            tracing::warn!(
                memory_id = ?memory_id,
                "auto_edge channel full; dropping enqueue"
            );
        }
        Err(flume::TrySendError::Disconnected(_)) => {
            tracing::debug!(
                memory_id = ?memory_id,
                "auto_edge worker disconnected; encode continues"
            );
        }
    }
}

// Silence unused-import warnings when EdgeKind is referenced only
// inside conditional code paths.
#[allow(dead_code)]
fn _kind_use(_: EdgeKind) {}

/// Enqueue `(memory_id, text)` onto the ExtractorWorker channel if
/// one is wired. Non-blocking; full channel logs a warn and drops
/// (encode succeeds without extraction). Disconnected channel logs
/// at debug. This is the single enqueue point both the single-encode
/// and TXN batch paths route through.
pub(super) fn try_enqueue_extractor(writer: &RealWriterHandle, memory_id: MemoryId, text: &str) {
    let Some(sender) = writer.extractor_sender() else {
        return;
    };
    let payload: ExtractorEnqueue = (memory_id, std::sync::Arc::from(text));
    match sender.try_send(payload) {
        Ok(()) => {
            tracing::trace!(
                memory_id = ?memory_id,
                "extractor enqueue"
            );
        }
        Err(flume::TrySendError::Full(_)) => {
            if let Some(m) = writer.extractor_metrics() {
                m.inc_drop();
            }
            tracing::warn!(
                memory_id = ?memory_id,
                "extractor channel full; dropping enqueue"
            );
        }
        Err(flume::TrySendError::Disconnected(_)) => {
            tracing::debug!(
                memory_id = ?memory_id,
                "extractor worker disconnected; encode continues"
            );
        }
    }
}
