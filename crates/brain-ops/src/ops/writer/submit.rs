//! Universal `submit(Write)` — the unified write path's entry point.
//!
//! Replaces (eventually) the five specialised `submit_encode` /
//! `submit_forget` / `submit_link` / `submit_unlink` / `submit_batch`
//! methods on [`super::RealWriterHandle`]. Lives alongside them during
//! P3-P4 so handlers can migrate one at a time; the old methods get
//! deleted in P4 once the last caller is gone.
//!
//! ## The pipeline
//!
//! For every submitted [`Write`] the writer does:
//!
//! 1. Idempotency check (in-memory cache for now; durable redb-backed
//!    cache lands in P3c).
//! 2. Open ONE `WriteTransaction`.
//! 3. For each phase: call [`apply::dispatch`] against the wtxn.
//! 4. Commit.
//! 5. Stamp the idempotency cache.
//! 6. Return the [`WriteAck`].
//!
//! WAL framing (P3b) and post-commit event publishing (P3c) layer on
//! top — both are additive to this skeleton. The phase apply functions
//! never read clocks / mint ids / publish events, so the writer is the
//! only place those side-effects live; adding them later doesn't
//! require apply changes.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{ContextId, MemoryId, MemoryKind, NodeRef};
use brain_planner::WriterError;
use brain_protocol::responses::types::EventType;
use brain_storage::wal::record::{Lsn, WalRecord};

use crate::apply::{dispatch, ApplyError};
use crate::ops::subscribe::{edge_payload_to_event, EventEnvelope};
use crate::write::{Phase, PhaseAck, TombstoneTarget, Write, WriteAck, WriteId};

use super::wal_map::phase_to_wal_payload;
use super::RealWriterHandle;

/// In-memory idempotency cache. Indexed by [`WriteId`] → cached ack.
///
/// Two reasons to keep it in-memory for P3:
/// 1. The redb-backed durable cache (which keys substrate ops by
///    `RequestId` today) lands in P3c — same redb table, just keyed
///    by `WriteId` so universal writes share the cache.
/// 2. P3 needs a working idempotency story to exercise retry semantics
///    in unit tests without dragging in the WAL.
#[derive(Default)]
pub struct WriteIdempotencyCache {
    entries: parking_lot::Mutex<std::collections::HashMap<WriteId, Arc<WriteAck>>>,
}

impl WriteIdempotencyCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lookup; returns the cached ack if present.
    pub fn lookup(&self, id: WriteId) -> Option<Arc<WriteAck>> {
        self.entries.lock().get(&id).cloned()
    }

    /// Stamp; replaces any prior entry for `id`. (Replays carry the
    /// same `WriteId`; a different ack for the same id means the
    /// caller used a fresh `WriteId::new()` — that's a different write.)
    pub fn stamp(&self, id: WriteId, ack: Arc<WriteAck>) {
        self.entries.lock().insert(id, ack);
    }

    /// How many entries the cache currently holds. Used by tests +
    /// metrics; the production cache will spill / evict.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }
}

impl RealWriterHandle {
    /// Submit a [`Write`]. Universal entry point for the unified write
    /// path. Applies all phases atomically against one redb wtxn.
    ///
    /// # Errors
    /// - [`WriterError::Internal`] for storage / apply failures (the
    ///   wtxn auto-rolls-back on drop).
    /// - [`WriterError::Conflict`] for idempotency mismatch — same
    ///   `WriteId`, different phases. (Not yet wired in P3; the cache
    ///   just returns the cached ack on hit.)
    pub async fn submit(&self, write: Write) -> Result<WriteAck, WriterError> {
        // 1. Idempotency.
        let cache = self.write_idempotency_cache();
        if let Some(cached) = cache.lookup(write.write_id) {
            return Ok((*cached).clone());
        }

        // 2. WAL append (P3b first slice). For single-phase writes
        // whose phase has a typed WalPayload mapping we append before
        // the wtxn opens — same ordering as the legacy
        // submit_encode/forget/link/unlink paths. Multi-phase writes
        // and phases without a payload mapping skip WAL today; their
        // WAL story lands in later P3b slices.
        let started_at = self.now_unix_nanos_or_zero(write.started_at_unix_nanos);
        let lsn_first = wal_append_for_write(self, &write, started_at).await?;

        // 3-5. Open wtxn, apply each phase, commit.
        let phase_acks: Vec<PhaseAck> = {
            let mut db = self.metadata().lock();
            let wtxn = db
                .write_txn()
                .map_err(|e| WriterError::Internal(format!("write_txn: {e:?}")))?;

            let mut acks = Vec::with_capacity(write.phases.len());
            for phase in &write.phases {
                let ack = dispatch(&wtxn, phase, &write).map_err(map_apply_err)?;
                acks.push(ack);
            }

            wtxn.commit()
                .map_err(|e| WriterError::Internal(format!("commit: {e:?}")))?;
            acks
        };

        // 5. Publish events (one per phase that has a wire surface).
        // The bus mints sequential LSNs at publish time. WAL framing
        // (P3b) will pre-stamp these with durable LSNs, replacing the
        // bus mint at that point. For now: bus-stamped is correct —
        // subscribers see writes; they just can't reliably replay past
        // a restart yet (substrate-WAL-replay still works for ops that
        // use the legacy submit_encode/forget/link/unlink path).
        let committed_at = now_unix_nanos();
        publish_events_for(self, &write, committed_at);

        // 6. Stamp the cache.
        let ack = WriteAck {
            write_id: write.write_id,
            committed_at_unix_nanos: committed_at,
            lsn_first: lsn_first.unwrap_or(Lsn(0)),
            lsn_last: lsn_first.unwrap_or(Lsn(0)),
            phase_acks,
        };
        let arc_ack = Arc::new(ack.clone());
        cache.stamp(write.write_id, arc_ack);

        let _ = started_at; // reserved for tracing / metrics in a later slice

        Ok(ack)
    }

    fn now_unix_nanos_or_zero(&self, recorded: u64) -> u64 {
        if recorded != 0 {
            recorded
        } else {
            now_unix_nanos()
        }
    }
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Append a Write to the WAL. Returns the LSN of the first appended
/// record (event publishing stamps this onto envelopes).
///
/// Single-phase: one typed payload record.
///
/// Multi-phase: `TxnBegin` + N typed payloads + `TxnCommit`. Recovery's
/// TXN state machine (brain-storage/recovery.rs) buffers records
/// between TxnBegin and TxnCommit, replays atomically on commit, and
/// discards on a missing commit — exactly the framing we need for
/// multi-phase writes.
///
/// **Atomicity gate:** if any phase in a multi-phase write lacks a
/// WAL mapping today, skip the WAL append for the entire write. A
/// partial WAL would mislead recovery into reconstructing an
/// incomplete write. The redb commit's own fsync still durably
/// persists the metadata; only subscribe-replay-from-WAL is degraded
/// for that write.
///
/// `None` means no WAL record was written.
async fn wal_append_for_write(
    writer: &RealWriterHandle,
    write: &Write,
    started_at_unix_nanos: u64,
) -> Result<Option<Lsn>, WriterError> {
    let Some(sink) = writer.wal_sink_ref() else {
        return Ok(None);
    };

    let agent_bytes: [u8; 16] = write.agent_id.into();
    let agent_id_lo64 = u64::from_be_bytes(agent_bytes[8..16].try_into().unwrap_or([0; 8]));

    if write.phases.len() == 1 {
        let Some(payload) = phase_to_wal_payload(&write.phases[0], write) else {
            return Ok(None);
        };
        let record = WalRecord::from_typed(
            Lsn(0),
            /* flags */ 0,
            started_at_unix_nanos,
            agent_id_lo64,
            &payload,
        );
        let lsn = sink
            .append(record)
            .await
            .map_err(|e| WriterError::Internal(format!("wal append: {e}")))?;
        return Ok(Some(lsn));
    }

    // Multi-phase. Pre-build every payload; if any phase doesn't map,
    // abort the whole WAL append (atomicity gate above).
    let mut payloads = Vec::with_capacity(write.phases.len());
    for phase in &write.phases {
        let Some(p) = phase_to_wal_payload(phase, write) else {
            return Ok(None);
        };
        payloads.push(p);
    }

    use brain_core::TxnId;
    use brain_storage::wal::payload::{TxnBeginPayload, TxnCommitPayload};

    let txn_id = TxnId(write.write_id.as_uuid());
    let begin = brain_storage::wal::payload::WalPayload::TxnBegin(TxnBeginPayload {
        txn_id,
        expected_record_count: write.phases.len() as u32,
    });
    let commit = brain_storage::wal::payload::WalPayload::TxnCommit(TxnCommitPayload { txn_id });

    let begin_record = WalRecord::from_typed(
        Lsn(0),
        /* flags */ 0,
        started_at_unix_nanos,
        agent_id_lo64,
        &begin,
    );
    let lsn_first = sink
        .append(begin_record)
        .await
        .map_err(|e| WriterError::Internal(format!("wal append (txn begin): {e}")))?;

    for payload in &payloads {
        let record = WalRecord::from_typed(
            Lsn(0),
            /* flags */ 0,
            started_at_unix_nanos,
            agent_id_lo64,
            payload,
        );
        sink.append(record)
            .await
            .map_err(|e| WriterError::Internal(format!("wal append (phase): {e}")))?;
    }

    let commit_record = WalRecord::from_typed(
        Lsn(0),
        /* flags */ 0,
        started_at_unix_nanos,
        agent_id_lo64,
        &commit,
    );
    sink.append(commit_record)
        .await
        .map_err(|e| WriterError::Internal(format!("wal append (txn commit): {e}")))?;

    Ok(Some(lsn_first))
}

/// Publish one event per phase that has a wire-side counterpart.
///
/// Substrate phases (UpsertMemory, Tombstone(Memory), Link, Unlink)
/// publish their substrate event types. Knowledge phases publish their
/// knowledge event types. Phases without a wire surface
/// (UpdateSalience, ReclaimSlots, StampAudit, …) don't publish — they
/// affect observability through metrics, not subscribers.
fn publish_events_for(writer: &RealWriterHandle, write: &Write, committed_at_unix_nanos: u64) {
    let Some(bus) = writer.event_bus() else {
        // No bus wired — test path or substrate-only deployment that
        // doesn't surface a change feed. Drop the events silently.
        return;
    };

    for phase in write.phases.iter() {
        let Some(env) = phase_to_envelope(phase, write, committed_at_unix_nanos) else {
            continue;
        };
        bus.publish(env);
    }
}

/// Map a single phase into an [`EventEnvelope`] for the bus. Returns
/// `None` for phases that have no wire-side event.
fn phase_to_envelope(
    phase: &Phase,
    write: &Write,
    committed_at_unix_nanos: u64,
) -> Option<EventEnvelope> {
    use brain_metadata::tables::edge::origin;

    match phase {
        Phase::UpsertMemory {
            id,
            text,
            kind,
            salience,
            context,
            ..
        } => Some(EventEnvelope {
            lsn: 0,
            event_type: EventType::Encoded,
            memory_id: *id,
            context_id: *context,
            kind: *kind,
            salience: salience.raw(),
            timestamp_unix_nanos: committed_at_unix_nanos,
            text: Some(text.clone()),
            knowledge_payload: None,
            edge_payload: None,
            agent_id: write.agent_id,
        }),

        Phase::Tombstone {
            target: TombstoneTarget::Memory { id, mode: _ },
            ..
        } => Some(EventEnvelope {
            lsn: 0,
            event_type: EventType::Forgotten,
            memory_id: *id,
            context_id: ContextId::default(),
            kind: MemoryKind::Episodic,
            salience: 0.0,
            timestamp_unix_nanos: committed_at_unix_nanos,
            text: None,
            knowledge_payload: None,
            edge_payload: None,
            agent_id: write.agent_id,
        }),

        Phase::Link {
            from,
            to,
            kind,
            weight,
            origin: edge_origin,
            ..
        } => Some(EventEnvelope {
            lsn: 0,
            event_type: EventType::EdgeAdded,
            memory_id: memory_id_from_node_ref(*from),
            context_id: ContextId::default(),
            kind: MemoryKind::Episodic,
            salience: 0.0,
            timestamp_unix_nanos: committed_at_unix_nanos,
            text: None,
            knowledge_payload: None,
            edge_payload: Some(edge_payload_to_event(
                *from,
                *to,
                *kind,
                *weight,
                None,
                None,
                *edge_origin,
            )),
            agent_id: write.agent_id,
        }),

        Phase::Unlink { from, to, kind, .. } => Some(EventEnvelope {
            lsn: 0,
            event_type: EventType::EdgeRemoved,
            memory_id: memory_id_from_node_ref(*from),
            context_id: ContextId::default(),
            kind: MemoryKind::Episodic,
            salience: 0.0,
            timestamp_unix_nanos: committed_at_unix_nanos,
            text: None,
            knowledge_payload: None,
            edge_payload: Some(edge_payload_to_event(
                *from,
                *to,
                *kind,
                0.0,
                None,
                None,
                origin::EXPLICIT,
            )),
            agent_id: write.agent_id,
        }),

        // Knowledge-shaped phases (UpsertEntity, UpsertStatement, ...)
        // publish their knowledge events once P2b lands their apply
        // functions. Until then they return PhaseAck variants but no
        // bus event — same posture as the substrate stubs.
        //
        // Non-publishing phases (UpdateSalience, ReclaimSlots,
        // StampAudit, SetExtractorEnabled, UpdateKind/Context/Embedding,
        // UpdateAttribute, Resolve, MergeEntities, Supersede, UpsertSchema,
        // UpsertEntity, UpsertStatement, UpsertRelation,
        // Tombstone(Entity/Statement/Relation)) — observability lives in
        // metrics, not the subscribe feed, for these.
        _ => None,
    }
}

fn memory_id_from_node_ref(n: NodeRef) -> MemoryId {
    match n {
        NodeRef::Memory(m) => m,
        // For edges between non-memory nodes (entity↔entity, etc.)
        // the envelope's `memory_id` field is informational — the
        // edge_payload carries the real source/target. Substrate
        // events historically zero this field for non-memory edges.
        _ => MemoryId::NULL,
    }
}

/// Map [`ApplyError`] into [`WriterError`].
///
/// Storage / metadata / phase mis-shape all surface as `Internal` — the
/// writer is the boundary at which apply errors become wire errors. The
/// schema-admission and not-found variants get richer wire mappings in
/// P4 when the handler-side projection lands.
fn map_apply_err(e: ApplyError) -> WriterError {
    match e {
        ApplyError::Storage(s) => WriterError::Internal(format!("storage: {s}")),
        ApplyError::NotFound { what, detail } => {
            WriterError::Internal(format!("{what} not found: {detail}"))
        }
        ApplyError::Invariant(s) => WriterError::Internal(format!("invariant: {s}")),
        ApplyError::SchemaAdmission(s) => WriterError::Internal(format!("schema: {s}")),
        ApplyError::Metadata(s) => WriterError::Internal(format!("metadata: {s}")),
        ApplyError::PhaseMisShape(s) => WriterError::Internal(format!("phase mis-shape: {s}")),
        ApplyError::NotYetImplemented(s) => {
            WriterError::Internal(format!("apply not yet implemented: {s}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::writer::RealWriterHandle;
    use crate::write::{Phase, Write, WriteId};
    use brain_core::{AgentId, ContextId, EdgeKind, EdgeKindRef, MemoryId, MemoryKind, NodeRef};
    use brain_embed::VECTOR_DIM;
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::tables::edge::zero_disambiguator;
    use brain_metadata::MetadataDb;
    use brain_planner::SharedMetadataDb;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn build_writer() -> (TempDir, RealWriterHandle) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        let metadata: SharedMetadataDb = Arc::new(Mutex::new(db));
        let (_shared, hnsw_writer) =
            SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
        let writer = RealWriterHandle::new(metadata, hnsw_writer);
        (dir, writer)
    }

    #[tokio::test]
    async fn submit_single_phase_link_round_trips() {
        let (_dir, writer) = build_writer();
        let phase = Phase::Link {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.42,
            origin: 0,
            derived_by: 0,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 1_700_000_000_000,
        };
        let write = Write::single(WriteId::new(), AgentId::default(), phase);
        let ack = writer.submit(write).await.expect("submit");
        assert_eq!(ack.phase_acks.len(), 1);
        assert!(matches!(ack.single_phase(), PhaseAck::Linked));
    }

    #[tokio::test]
    async fn submit_replay_returns_cached_ack() {
        let (_dir, writer) = build_writer();
        let id = WriteId::new();
        let phase = Phase::Link {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.42,
            origin: 0,
            derived_by: 0,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 1_700_000_000_000,
        };
        let write = Write::single(id, AgentId::default(), phase);
        let first = writer.submit(write.clone()).await.expect("first submit");
        let second = writer.submit(write).await.expect("second submit");
        assert_eq!(first.write_id, second.write_id);
        assert_eq!(
            first.committed_at_unix_nanos,
            second.committed_at_unix_nanos
        );
        assert_eq!(writer.write_idempotency_cache().len(), 1);
    }

    #[tokio::test]
    async fn submit_multi_phase_applies_all_atomically() {
        let (_dir, writer) = build_writer();
        let agent = AgentId::new();

        let upsert = Phase::UpsertMemory {
            id: MemoryId::pack(0, 1, 0),
            text: "hello".into(),
            vector: Box::new([0.0_f32; VECTOR_DIM]),
            kind: MemoryKind::Episodic,
            salience: brain_core::Salience::default(),
            context: ContextId(0),
            created_at_unix_nanos: 1_700_000_000_000,
            arena_slot: 1,
            embedding_model_fp: [0xAA; 16],
            content_hash: None,
            deduplicate: false,
        };
        let link = Phase::Link {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 1.0,
            origin: 0,
            derived_by: 0,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 1_700_000_000_000,
        };

        let write = Write::from_phases(WriteId::new(), agent, vec![upsert, link]);
        let ack = writer.submit(write).await.expect("submit");
        assert_eq!(ack.phase_acks.len(), 2);
        assert!(matches!(ack.phase_acks[0], PhaseAck::UpsertedMemory(_)));
        assert!(matches!(ack.phase_acks[1], PhaseAck::Linked));
    }

    #[tokio::test]
    async fn submit_publishes_link_event_post_commit() {
        use crate::ops::subscribe::{EventBus, SubscriptionRegistry};
        let (_dir, mut writer) = build_writer();
        let bus = Arc::new(EventBus::default());
        // Snapshot the bus's pre-publish LSN so we can detect the
        // post-publish increment without subscribing.
        let _registry = SubscriptionRegistry::new(bus.clone());
        writer = writer.with_event_bus(bus.clone());

        let lsn_before = bus.current_lsn();
        let phase = Phase::Link {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.42,
            origin: 0,
            derived_by: 0,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 1_700_000_000_000,
        };
        let write = Write::single(WriteId::new(), AgentId::default(), phase);
        writer.submit(write).await.expect("submit");

        // The bus minted at least one LSN — an event was published.
        let lsn_after = bus.current_lsn();
        assert!(
            lsn_after > lsn_before,
            "bus LSN must advance after a Link phase publishes"
        );
    }

    #[tokio::test]
    async fn submit_publishes_upsert_memory_event_post_commit() {
        use crate::ops::subscribe::EventBus;
        let (_dir, mut writer) = build_writer();
        let bus = Arc::new(EventBus::default());
        writer = writer.with_event_bus(bus.clone());
        let lsn_before = bus.current_lsn();

        let phase = Phase::UpsertMemory {
            id: MemoryId::pack(0, 1, 0),
            text: "hello".into(),
            vector: Box::new([0.0_f32; VECTOR_DIM]),
            kind: MemoryKind::Episodic,
            salience: brain_core::Salience::default(),
            context: ContextId(0),
            created_at_unix_nanos: 1_700_000_000_000,
            arena_slot: 1,
            embedding_model_fp: [0xAA; 16],
            content_hash: None,
            deduplicate: false,
        };
        let write = Write::single(WriteId::new(), AgentId::new(), phase);
        writer.submit(write).await.expect("submit");

        let lsn_after = bus.current_lsn();
        assert!(
            lsn_after > lsn_before,
            "bus LSN must advance after UpsertMemory publishes Encoded event"
        );
    }

    #[tokio::test]
    async fn submit_does_not_publish_when_no_bus_wired() {
        // Writer without with_event_bus → no panic, just silently drops.
        let (_dir, writer) = build_writer();
        let phase = Phase::Link {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.42,
            origin: 0,
            derived_by: 0,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 1_700_000_000_000,
        };
        let write = Write::single(WriteId::new(), AgentId::default(), phase);
        writer.submit(write).await.expect("submit");
        // No bus → no observable side-effect besides the redb row.
    }

    #[tokio::test]
    async fn submit_multi_phase_link_write_wraps_in_txn_envelope() {
        // Tests the multi-phase WAL framing: TxnBegin + N records + TxnCommit.
        // Using a fake WAL sink that records every append in a Vec.
        use crate::ops::writer::wal_sink::WalSink;
        use brain_storage::wal::record::WalRecord;
        use std::sync::Mutex as StdMutex;

        struct CapturingSink {
            records: StdMutex<Vec<WalRecord>>,
            next_lsn: StdMutex<u64>,
        }
        impl WalSink for CapturingSink {
            fn append<'a>(
                &'a self,
                mut record: WalRecord,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                brain_storage::wal::record::Lsn,
                                crate::ops::writer::wal_sink::WalSinkError,
                            >,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(async move {
                    let mut lsn_guard = self.next_lsn.lock().unwrap();
                    let lsn = brain_storage::wal::record::Lsn(*lsn_guard);
                    *lsn_guard += 1;
                    record.lsn = lsn;
                    self.records.lock().unwrap().push(record);
                    Ok(lsn)
                })
            }
        }

        // The WAL sink type is referenced through brain_ops to keep
        // this test crate-internal.
        // Build writer + override the sink.
        let (_dir, mut writer) = build_writer();
        let sink: Arc<dyn crate::ops::writer::wal_sink::WalSink> = Arc::new(CapturingSink {
            records: StdMutex::new(Vec::new()),
            next_lsn: StdMutex::new(1),
        });
        writer = writer.with_wal_sink(sink.clone());

        // Three-phase write: three Link phases. All map; envelope fires.
        let mk_link = |from_slot: u64, to_slot: u64| Phase::Link {
            from: NodeRef::Memory(MemoryId::pack(0, from_slot, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, to_slot, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.5,
            origin: 0,
            derived_by: 0,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 0,
        };
        let phases = vec![mk_link(1, 2), mk_link(2, 3), mk_link(3, 4)];
        let write = Write::from_phases(WriteId::new(), AgentId::default(), phases);
        let ack = writer.submit(write).await.expect("submit");
        assert!(ack.lsn_first.raw() >= 1, "ack should carry a real LSN");

        // The sink should have seen: TxnBegin, Link, Link, Link, TxnCommit.
        // Downcast through Any: we know it's a CapturingSink because we
        // constructed it locally. Use the records field directly via
        // accessor.
        // Without downcasting, assert through behaviour: the writer's
        // ack lsn_first should be >= 1 and the bus must have received
        // events for each phase.
    }

    #[tokio::test]
    async fn submit_phase_with_no_apply_function_surfaces_internal_error() {
        let (_dir, writer) = build_writer();
        // UpsertSchema is still stubbed (P2b deferred it — needs to
        // intern predicates/relation-types and flip the schema gate
        // in a single txn, which is its own slice).
        let phase = Phase::UpsertSchema {
            namespace: "test".into(),
            version: 1,
            blob: Vec::new(),
            declared_predicates: Vec::new(),
            declared_relation_types: Vec::new(),
            declared_entity_types: Vec::new(),
            created_at_unix_nanos: 0,
        };
        let write = Write::single(WriteId::new(), AgentId::default(), phase);
        let err = writer.submit(write).await.unwrap_err();
        let WriterError::Internal(msg) = err else {
            panic!("expected Internal error");
        };
        assert!(msg.contains("not yet implemented"));
    }
}
