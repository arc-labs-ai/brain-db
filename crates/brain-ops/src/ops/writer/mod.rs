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

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_index::Writer as HnswWriter;
use brain_metadata::tables::edge::{
    self, derived_by, origin, EdgeData, EDGES_IN_TABLE, EDGES_OUT_TABLE,
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
    /// Change-feed publisher (sub-task 7.10). Single-op encode/forget
    /// commits and TXN_COMMIT batches publish here *after* the redb
    /// commit() succeeds. Optional so existing callers don't break
    /// (defaults to no publication — events are dropped on the floor).
    events: Option<Arc<EventBus>>,
}

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
                let _ = wtxn.open_table(EDGES_OUT_TABLE);
                let _ = wtxn.open_table(EDGES_IN_TABLE);
                let _ = wtxn.commit();
            }
        }
        Self {
            metadata,
            hnsw_writer: Mutex::new(hnsw_writer),
            next_slot: AtomicU64::new(1),
            tombstoned: Mutex::new(HashSet::new()),
            agent_id: AgentId(Uuid::nil()),
            events: None,
        }
    }

    #[must_use]
    pub fn with_agent_id(mut self, agent_id: AgentId) -> Self {
        self.agent_id = agent_id;
        self
    }

    /// Wire the change-feed bus. After this call every successful
    /// commit publishes an [`EventEnvelope`] onto the bus.
    #[must_use]
    pub fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.events = Some(bus);
        self
    }

    fn publish(&self, env: EventEnvelope) {
        if let Some(bus) = &self.events {
            bus.publish(env);
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
        Box::pin(async move { do_encode(self, op) })
    }

    fn submit_forget<'a>(
        &'a self,
        op: ForgetOp,
    ) -> Pin<Box<dyn Future<Output = Result<ForgetAck, WriterError>> + 'a>> {
        Box::pin(async move { do_forget(self, op) })
    }

    fn submit_link<'a>(
        &'a self,
        op: LinkOp,
    ) -> Pin<Box<dyn Future<Output = Result<LinkAck, WriterError>> + 'a>> {
        Box::pin(async move { do_link(self, op) })
    }

    fn submit_unlink<'a>(
        &'a self,
        op: UnlinkOp,
    ) -> Pin<Box<dyn Future<Output = Result<UnlinkAck, WriterError>> + 'a>> {
        Box::pin(async move { do_unlink(self, op) })
    }

    fn reserve_memory_id<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<MemoryId, WriterError>> + 'a>> {
        Box::pin(async move {
            let slot = self.next_slot.fetch_add(1, Ordering::Relaxed);
            Ok(MemoryId::pack(0, slot, 1))
        })
    }

    fn submit_batch<'a>(
        &'a self,
        batch: brain_planner::TxnBatch,
    ) -> Pin<Box<dyn Future<Output = Result<brain_planner::TxnBatchAck, WriterError>> + 'a>> {
        Box::pin(async move { do_submit_batch(self, batch) })
    }
}

// ---------------------------------------------------------------------------
// Handler modules — one per cognitive op handler.
// ---------------------------------------------------------------------------

mod encode;
mod forget;
mod link;
mod unlink;

use encode::do_encode;
use forget::do_forget;
use link::do_link;
use unlink::do_unlink;

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

fn do_submit_batch(writer: &RealWriterHandle, batch: TxnBatch) -> Result<TxnBatchAck, WriterError> {
    use crate::idempotency::{
        encode_encode_payload, encode_forget_payload, encode_link_payload, encode_unlink_payload,
    };

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
            let mut edges_out_t = wtxn
                .open_table(EDGES_OUT_TABLE)
                .map_err(|e| WriterError::Internal(format!("batch open EDGES_OUT: {e:?}")))?;
            let mut edges_in_t = wtxn
                .open_table(EDGES_IN_TABLE)
                .map_err(|e| WriterError::Internal(format!("batch open EDGES_IN: {e:?}")))?;
            let mut idem_t = wtxn
                .open_table(IDEMPOTENCY_TABLE)
                .map_err(|e| WriterError::Internal(format!("batch open IDEMPOTENCY: {e:?}")))?;

            // 1. Memories + their inline edges.
            for enc in &batch.memories {
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
                        &mut edges_out_t,
                        &mut edges_in_t,
                        enc.memory_id,
                        edge.kind,
                        edge.target,
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
                    writer.agent_id,
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
                memories_t
                    .insert(enc.memory_id.to_be_bytes(), meta)
                    .map_err(|e| WriterError::Internal(format!("batch memories insert: {e:?}")))?;

                // Idempotency entry.
                let payload = encode_encode_payload(enc.memory_id, &edge_outcomes);
                let entry = IdempotencyEntry::new(
                    crate::idempotency::RESPONSE_KIND_ENCODE,
                    Some(enc.memory_id.to_be_bytes()),
                    payload,
                    enc.request_hash,
                    enc.created_at_unix_nanos,
                );
                idem_t
                    .insert(<[u8; 16]>::from(enc.request_id), entry)
                    .map_err(|e| {
                        WriterError::Internal(format!("batch idempotency insert (encode): {e:?}"))
                    })?;

                batch_memory_ids.insert(enc.memory_id);
                encode_acks.push(EncodeAck {
                    memory_id: enc.memory_id,
                    edge_results: edge_outcomes,
                    replayed: false,
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
            for link in &batch.links {
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
                let key = (
                    link.source.to_be_bytes(),
                    link.kind as u8,
                    link.target.to_be_bytes(),
                );
                let already_existed = edges_out_t
                    .get(&key)
                    .map_err(|e| WriterError::Internal(format!("batch get edge: {e:?}")))?
                    .is_some();
                let data = EdgeData::new(
                    link.weight,
                    origin::EXPLICIT,
                    derived_by::CLIENT,
                    link.created_at_unix_nanos,
                );
                edge::link(
                    &mut edges_out_t,
                    &mut edges_in_t,
                    link.source,
                    link.kind,
                    link.target,
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
            for unlink in &batch.unlinks {
                let removed = edge::unlink(
                    &mut edges_out_t,
                    &mut edges_in_t,
                    unlink.source,
                    unlink.kind,
                    unlink.target,
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
            for forget in &batch.forgets {
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
                    let event = if let Some(m) = meta_row {
                        Some(PendingEvent {
                            event_type: EventType::Forgotten,
                            memory_id: forget.memory_id,
                            context_id: m.context(),
                            kind: m.kind().unwrap_or(MemoryKind::Episodic),
                            salience: m.salience,
                            timestamp_unix_nanos: forget.created_at_unix_nanos,
                            text: None,
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
    {
        let mut tombstoned = writer.tombstoned.lock();
        for id in &hnsw_tombstones {
            tombstoned.insert(*id);
        }
    }

    // ── Change-feed (sub-task 7.10). Publish in buffer order. ────
    for ev in pending_events {
        writer.publish(EventEnvelope {
            lsn: 0,
            event_type: ev.event_type,
            memory_id: ev.memory_id,
            context_id: ev.context_id,
            kind: ev.kind,
            salience: ev.salience,
            timestamp_unix_nanos: ev.timestamp_unix_nanos,
            text: ev.text,
        });
    }

    Ok(TxnBatchAck {
        encodes: encode_acks,
        links: link_acks,
        unlinks: unlink_acks,
        forgets: forget_acks,
    })
}

// Silence unused-import warnings when EdgeKind is referenced only
// inside conditional code paths.
#[allow(dead_code)]
fn _kind_use(_: EdgeKind) {}
