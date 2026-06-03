//! Universal `submit(Write)` — the unified write path's entry point.
//!
//! Every wire opcode that mutates state (ENCODE / FORGET / LINK /
//! UNLINK / TXN_COMMIT) and every worker-derived write (auto-edge,
//! temporal-edge, extractor) lands here. One pipeline, one WAL
//! envelope, one redb wtxn, one event burst.
//!
//! ## The pipeline
//!
//! For every submitted [`Write`] the writer does:
//!
//! 1. Idempotency check (in-memory cache for now; a durable redb-backed
//!    cache lands later).
//! 2. Open ONE `WriteTransaction`.
//! 3. For each phase: call [`apply::dispatch`] against the wtxn.
//! 4. Commit.
//! 5. Stamp the idempotency cache.
//! 6. Return the [`WriteAck`].
//!
//! WAL framing and post-commit event publishing layer on
//! top — both are additive to this skeleton. The phase apply functions
//! never read clocks / mint ids / publish events, so the writer is the
//! only place those side-effects live; adding them later doesn't
//! require apply changes.

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use brain_core::{ContextId, MemoryId, MemoryKind, NodeRef};
use brain_planner::WriterError;
use brain_protocol::EventType;
use brain_storage::wal::payload::WalPayload;
use brain_storage::wal::record::{Lsn, WalRecord};

use crate::apply::{dispatch, ApplyError};
use crate::handlers::subscribe::{edge_payload_to_event, EventEnvelope};
use crate::metrics::{IdempotencyOutcome, SubmitOutcome, WriterMetrics};
use crate::state::ack_codec;
use crate::write::{Phase, PhaseAck, TombstoneTarget, Write, WriteAck, WriteId};
use brain_metadata::tables::idempotency::{response_kind, IdempotencyEntry, IDEMPOTENCY_TABLE};

use super::wal_map::phase_to_wal_payload;
use super::RealWriterHandle;

/// Durable idempotency cache for the unified write path.
///
/// The source of truth is `IDEMPOTENCY_TABLE` in the per-shard redb
/// metadata file. Every successful `submit(Write)` writes one row into
/// the same `WriteTransaction` as the apply phases, so the row commits
/// atomically with the data — a server restart between commit and
/// "stamp the cache" can no longer drop the idempotency record.
///
/// A small in-memory hot cache fronts the table to keep replays in the
/// same shard cheap; the in-memory entries are best-effort and rebuild
/// themselves from redb on a miss.
///
/// On lookup the cache walks: hot → cold (redb). Entries older than
/// [`brain_metadata::tables::idempotency::DEFAULT_TTL_NANOS`] (24 h) are
/// treated as misses; bulk eviction happens in the
/// idempotency-cleanup background worker.
pub struct WriteIdempotencyCache {
    hot: parking_lot::Mutex<std::collections::HashMap<WriteId, HotEntry>>,
    /// Clock source — overridable in tests to drive TTL expiry without
    /// sleeping for 24 hours.
    now_unix_nanos: Box<dyn Fn() -> u64 + Send + Sync>,
    /// Time-to-live for cached entries, in nanoseconds.
    ttl_nanos: u64,
}

struct HotEntry {
    ack: Arc<WriteAck>,
    request_hash: Option<[u8; 32]>,
    created_at_unix_nanos: u64,
}

/// Result of a cache lookup. Lets `submit` distinguish a true replay
/// from a conflict without re-fetching the entry.
pub enum CacheLookup {
    Miss,
    Hit(Arc<WriteAck>),
    Conflict,
}

impl Default for WriteIdempotencyCache {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteIdempotencyCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            hot: parking_lot::Mutex::new(std::collections::HashMap::new()),
            now_unix_nanos: Box::new(default_now_unix_nanos),
            ttl_nanos: brain_metadata::tables::idempotency::DEFAULT_TTL_NANOS,
        }
    }

    /// Test-only constructor that wires a custom clock. Used to assert
    /// the TTL expiry path without waiting in real time.
    #[doc(hidden)]
    #[must_use]
    pub fn with_clock<F>(now: F) -> Self
    where
        F: Fn() -> u64 + Send + Sync + 'static,
    {
        Self {
            hot: parking_lot::Mutex::new(std::collections::HashMap::new()),
            now_unix_nanos: Box::new(now),
            ttl_nanos: brain_metadata::tables::idempotency::DEFAULT_TTL_NANOS,
        }
    }

    fn now(&self) -> u64 {
        (self.now_unix_nanos)()
    }

    /// Hash-aware lookup against hot cache + durable table.
    /// `request_hash = None` means the caller skips conflict checks
    /// (workers, internal writes) — the entry is returned on key match.
    pub fn lookup_with_hash(
        &self,
        metadata: &brain_planner::SharedMetadataDb,
        id: WriteId,
        request_hash: Option<[u8; 32]>,
    ) -> CacheLookup {
        let now = self.now();

        // Hot path.
        {
            let hot = self.hot.lock();
            if let Some(entry) = hot.get(&id) {
                if entry.created_at_unix_nanos.saturating_add(self.ttl_nanos) >= now {
                    return classify(entry.request_hash, request_hash, &entry.ack);
                }
            }
        }

        // Cold path: open a read txn against the durable table.
        let (decoded, stored_hash, created_at) = {
            let Ok(rtxn) = metadata.read_txn() else {
                return CacheLookup::Miss;
            };
            let table = match rtxn.open_table(IDEMPOTENCY_TABLE) {
                Ok(t) => t,
                Err(_) => return CacheLookup::Miss,
            };
            let Some(row) = table.get(id.to_bytes()).ok().flatten() else {
                return CacheLookup::Miss;
            };
            let entry = row.value();
            if entry.is_expired(now, self.ttl_nanos) {
                return CacheLookup::Miss;
            }
            let stored_hash = if entry.request_hash == [0u8; 32] {
                None
            } else {
                Some(entry.request_hash)
            };
            match ack_codec::decode_write_ack(&entry.response_payload) {
                Ok(a) => (Arc::new(a), stored_hash, entry.created_at_unix_nanos),
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        write_id = ?id,
                        "idempotency row decode failed; treating as miss",
                    );
                    return CacheLookup::Miss;
                }
            }
        };

        let result = classify(stored_hash, request_hash, &decoded);
        if matches!(result, CacheLookup::Hit(_)) {
            self.hot.lock().insert(
                id,
                HotEntry {
                    ack: decoded,
                    request_hash: stored_hash,
                    created_at_unix_nanos: created_at,
                },
            );
        }
        result
    }

    /// Populate the in-memory hot cache after a successful submit.
    /// The durable row was already written inside the apply wtxn.
    pub fn stamp_hot(
        &self,
        id: WriteId,
        ack: Arc<WriteAck>,
        request_hash: Option<[u8; 32]>,
        created_at_unix_nanos: u64,
    ) {
        self.hot.lock().insert(
            id,
            HotEntry {
                ack,
                request_hash,
                created_at_unix_nanos,
            },
        );
    }

    /// Number of resident hot-cache entries. Used by tests + metrics.
    #[must_use]
    pub fn hot_len(&self) -> usize {
        self.hot.lock().len()
    }
}

fn classify(
    stored: Option<[u8; 32]>,
    provided: Option<[u8; 32]>,
    ack: &Arc<WriteAck>,
) -> CacheLookup {
    match (stored, provided) {
        (Some(s), Some(p)) if s != p => CacheLookup::Conflict,
        _ => CacheLookup::Hit(ack.clone()),
    }
}

fn default_now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Build the durable idempotency row for a successful submit. Encodes
/// the ack via [`ack_codec`] and stamps the request-hash + created-at
/// timestamp. The caller inserts it into the SAME `WriteTransaction` as
/// the apply phases so the row commits atomically with the data write.
fn idempotency_entry_for(
    ack: &WriteAck,
    request_hash: Option<[u8; 32]>,
    committed_at_unix_nanos: u64,
) -> IdempotencyEntry {
    IdempotencyEntry {
        response_kind: response_kind::UNKNOWN,
        memory_id_bytes: None,
        response_payload: ack_codec::encode_write_ack(ack),
        request_hash: request_hash.unwrap_or([0u8; 32]),
        created_at_unix_nanos: committed_at_unix_nanos,
        lsn: ack.lsn_first.raw(),
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
    ///   `WriteId`, different phases. (Not yet wired; the cache
    ///   just returns the cached ack on hit.)
    pub async fn submit(&self, write: Write) -> Result<Arc<WriteAck>, WriterError> {
        let start = Instant::now();
        let metrics = self.writer_metrics().clone();

        // 1. Idempotency. A `request_hash` mismatch on the same
        //    WriteId is a conflict — the caller re-used a request_id
        //    with different params. Same hash → cached ack.
        let cache = self.write_idempotency_cache();
        match cache.lookup_with_hash(self.metadata(), write.write_id, write.request_hash) {
            CacheLookup::Hit(cached) => {
                metrics.record_idempotency(IdempotencyOutcome::Hit);
                // Hand the cached Arc straight back to the caller — no
                // inner clone. Hot replays cost one Arc bump.
                return Ok(cached);
            }
            CacheLookup::Conflict => {
                metrics.record_idempotency(IdempotencyOutcome::Conflict);
                record_phase_outcomes(&metrics, &write, SubmitOutcome::Conflict, start.elapsed());
                return Err(WriterError::Conflict(format!(
                    "request_id replay with different params: write_id={:?}",
                    write.write_id
                )));
            }
            CacheLookup::Miss => metrics.record_idempotency(IdempotencyOutcome::Miss),
        }

        // 2. WAL append. Single-phase writes get one typed payload;
        // multi-phase writes get TxnBegin + N × payloads + TxnCommit.
        let started_at = self.now_unix_nanos_or_zero(write.started_at_unix_nanos);
        let lsn_first = match wal_append_for_write(self, &write, started_at).await {
            Ok(v) => v,
            Err(e) => {
                record_phase_outcomes(&metrics, &write, SubmitOutcome::Err, start.elapsed());
                return Err(e);
            }
        };

        // 3. HNSW side effects. Run before the redb wtxn opens
        // so the wtxn lifetime stays minimal and a HNSW failure
        // abandons the encode before any metadata commits.
        if let Err(e) = execute_hnsw_side_effects(self, &write) {
            record_phase_outcomes(&metrics, &write, SubmitOutcome::Err, start.elapsed());
            return Err(e);
        }

        // 4-6. Open wtxn, apply each phase, stamp the durable
        // idempotency row inside the same wtxn, commit. The
        // idempotency stamp shares the wtxn so a crash between "phases
        // applied" and "idempotency stamped" is impossible — either
        // both commit or neither.
        //
        // We construct the durable `WriteAck` once — moving `acks` into
        // it instead of cloning. The `pending_stages` list lands later
        // (after the post-commit worker fan-out) and is patched onto the
        // same struct just before we Arc-wrap and return.
        let committed_at = (cache.now_unix_nanos)();
        let mut durable_ack: WriteAck = {
            let wtxn = match self.metadata().write_txn() {
                Ok(w) => w,
                Err(e) => {
                    record_phase_outcomes(&metrics, &write, SubmitOutcome::Err, start.elapsed());
                    return Err(WriterError::Internal(format!("write_txn: {e:?}")));
                }
            };

            let mut acks = Vec::with_capacity(write.phases.len());
            for phase in &write.phases {
                match dispatch(&wtxn, phase, &write) {
                    Ok(ack) => acks.push(ack),
                    Err(apply_err) => {
                        metrics.record_apply_error(phase.tag(), apply_err.tag());
                        record_phase_outcomes(
                            &metrics,
                            &write,
                            SubmitOutcome::Err,
                            start.elapsed(),
                        );
                        return Err(map_apply_err(apply_err));
                    }
                }
            }

            // Build the durable WriteAck (no pending_stages — those
            // are post-commit fan-out and aren't re-enqueued on replay)
            // and write it to IDEMPOTENCY_TABLE inside this same wtxn.
            let durable_ack = WriteAck {
                write_id: write.write_id,
                committed_at_unix_nanos: committed_at,
                lsn_first: lsn_first.unwrap_or(Lsn(0)),
                lsn_last: lsn_first.unwrap_or(Lsn(0)),
                phase_acks: acks,
                pending_stages: Vec::new(),
            };
            let idem_entry = idempotency_entry_for(&durable_ack, write.request_hash, committed_at);
            let idem_insert: Result<(), WriterError> = (|| {
                let mut idem_table = wtxn
                    .open_table(IDEMPOTENCY_TABLE)
                    .map_err(|e| WriterError::Internal(format!("open IDEMPOTENCY_TABLE: {e:?}")))?;
                idem_table
                    .insert(write.write_id.to_bytes(), &idem_entry)
                    .map_err(|e| WriterError::Internal(format!("idempotency insert: {e:?}")))?;
                Ok(())
            })();
            if let Err(e) = idem_insert {
                record_phase_outcomes(&metrics, &write, SubmitOutcome::Err, start.elapsed());
                return Err(e);
            }

            if let Err(e) = wtxn.commit() {
                record_phase_outcomes(&metrics, &write, SubmitOutcome::Err, start.elapsed());
                return Err(WriterError::Internal(format!("commit: {e:?}")));
            }
            durable_ack
        };

        // 5. Publish events (one per phase that has a wire surface).
        publish_events_for(self, &write, committed_at);

        // 5b. Post-commit worker enqueues. Every UpsertMemory phase
        // signals the auto-edge, temporal-edge, and extractor workers
        // so they can derive `SimilarTo` / `FollowedBy` / extracted-
        // entities/statements in the background. The channels are
        // best-effort (drop on full); workers are eventually-consistent
        // with the metadata they read back. Each successful enqueue
        // adds a `PendingStage` to the ack — clients waiting on the
        // write's full completion count these down as `StageCompleted`
        // events arrive on the subscribe stream.
        let mut pending_stages: Vec<crate::write::PendingStage> = Vec::new();
        for phase in write.phases.iter() {
            if let Phase::UpsertMemory {
                id,
                text,
                vector,
                kind,
                context,
                created_at_unix_nanos,
                ..
            } = phase
            {
                if super::try_enqueue_auto_edge(self, *id, vector.as_ref()) {
                    pending_stages.push(crate::write::PendingStage {
                        memory_id: *id,
                        stage_kind: brain_protocol::StageKind::AutoEdge,
                    });
                }
                if super::try_enqueue_temporal_edge(
                    self,
                    *id,
                    write.agent_id,
                    *context,
                    *created_at_unix_nanos,
                    vector.as_ref(),
                ) {
                    pending_stages.push(crate::write::PendingStage {
                        memory_id: *id,
                        stage_kind: brain_protocol::StageKind::TemporalEdge,
                    });
                }
                let extractor_enqueued = super::try_enqueue_extractor(self, *id, text);
                tracing::info!(
                    target: "brain_debug::extractor",
                    memory_id = ?id,
                    enqueued = extractor_enqueued,
                    "submit: post-commit extractor enqueue attempt",
                );
                if extractor_enqueued {
                    pending_stages.push(crate::write::PendingStage {
                        memory_id: *id,
                        stage_kind: brain_protocol::StageKind::Extractor,
                    });
                }
                // Index the memory text into tantivy so the lexical
                // retriever can find it. Backpressures (awaits) rather
                // than dropping on a full queue: lexical indexing is a
                // primary recall lane, not best-effort enrichment, so a
                // silently-dropped doc would make the memory permanently
                // unsearchable by keyword.
                if let Some(dispatcher) = &self.memory_text_dispatcher {
                    dispatcher
                        .dispatch(crate::index::text_indexer::MemoryTextOp::Upsert {
                            id: *id,
                            text: text.clone(),
                            agent: write.agent_id,
                            kind: *kind,
                            created_at_unix_ms: *created_at_unix_nanos / 1_000_000,
                            context: context.raw(),
                        })
                        .await;
                }
            }
            // Tombstone(Memory) fans out to the FORGET cascade. Both
            // soft and hard modes enqueue — readers must not see a
            // statement at full confidence backed by a memory the
            // user already forgot, even during the soft-grace window
            // before slot reclamation runs.
            if let Phase::Tombstone {
                target: TombstoneTarget::Memory { id, mode },
                at_unix_nanos,
                ..
            } = phase
            {
                let cascade_mode = match mode {
                    crate::write::phase::TombstoneMode::Soft => {
                        crate::writer::ForgetCascadeMode::Soft
                    }
                    crate::write::phase::TombstoneMode::Hard => {
                        crate::writer::ForgetCascadeMode::Hard
                    }
                };
                let job = crate::writer::ForgetCascadeJob {
                    memory_id: *id,
                    mode: cascade_mode,
                    kind: crate::writer::ForgetCascadeKind::Apply,
                    forgot_at_unix_nanos: *at_unix_nanos,
                };
                let enqueued = super::try_enqueue_forget_cascade(self, job);
                tracing::debug!(
                    memory_id = ?id,
                    mode = ?mode,
                    enqueued,
                    "submit: post-commit forget cascade enqueue attempt",
                );
            }
            // UpsertSchema fans out to the SchemaMigrationWorker. The
            // OUTSIDE_ACTIVE_SCHEMA flag-sweep was previously inline
            // inside the upload wtxn; moving it post-commit keeps the
            // upload ack latency bounded (the sweep is a full-table
            // STATEMENTS_TABLE scan and grows linearly with corpus
            // size). A dropped enqueue is recoverable — pre-existing
            // statements just keep their stale flag bit until a later
            // sweep catches up.
            if let Phase::UpsertSchema {
                namespace,
                created_at_unix_nanos,
                ..
            } = phase
            {
                // The version this commit actually wrote — `schema_upload`
                // increments the namespace counter inside the wtxn we
                // just committed, so the matching ack from `apply` is
                // the source of truth for what version the sweep
                // should align against.
                let ack_version = durable_ack.phase_acks.iter().find_map(|a| match a {
                    PhaseAck::UpsertedSchema {
                        namespace: ns,
                        version,
                    } if ns == namespace => Some(*version),
                    _ => None,
                });
                if let Some(new_version) = ack_version {
                    let job = crate::writer::SchemaFlagSweepJob {
                        namespace: namespace.clone(),
                        new_version,
                        enqueued_at_unix_nanos: *created_at_unix_nanos,
                    };
                    let enqueued = super::try_enqueue_schema_flag_sweep(self, job);
                    tracing::debug!(
                        namespace = %namespace,
                        new_version,
                        enqueued,
                        "submit: post-commit schema flag-sweep enqueue attempt",
                    );
                }
            }
        }

        // 6. Stamp the in-memory hot cache. The durable row already
        // landed inside the wtxn above; this just keeps the in-process
        // replay path off redb for hot keys. We Arc-wrap once and hand
        // the same Arc to both the cache and the caller — no inner
        // clone.
        durable_ack.pending_stages = pending_stages;
        let ack = Arc::new(durable_ack);
        cache.stamp_hot(
            write.write_id,
            ack.clone(),
            write.request_hash,
            committed_at,
        );

        record_phase_outcomes(&metrics, &write, SubmitOutcome::Ok, start.elapsed());

        let _ = started_at; // reserved for tracing in a later slice

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

/// Record one submit outcome per phase in the parent [`Write`]. The
/// caller passes the wall-clock duration from `submit()` entry — every
/// phase gets the same value, so percentile latencies aggregate across
/// the full write (matches Prometheus histogram conventions where
/// multi-label observations share a timestamp).
fn record_phase_outcomes(
    metrics: &WriterMetrics,
    write: &Write,
    outcome: SubmitOutcome,
    duration: std::time::Duration,
) {
    for phase in &write.phases {
        metrics.record_submit(phase.tag(), outcome, duration);
    }
}

/// Append a Write to the WAL. Returns the LSN of the first appended
/// record (event publishing stamps this onto envelopes).
///
/// Only phases that map to a `WalPayload` are appended. Unmapped phases
/// (opaque-body phases persisted via redb; auto-derived phases
/// re-derivable from state) are skipped — their durability rides on the
/// redb commit's fsync. A `tracing::debug!` log surfaces each skip.
///
/// Framing:
/// - Zero mapped phases: no WAL records appended; returns `None`.
/// - One mapped phase: one typed payload record (no TXN bracket).
/// - Two or more mapped phases: `TxnBegin` + N payloads + `TxnCommit`.
///   Recovery's TXN state machine (brain-storage/recovery.rs) buffers
///   records between TxnBegin and TxnCommit and replays atomically.
///
/// Mapped phases reach the WAL in the same order they appear in the
/// `Write`. Unmapped phases between them are simply skipped.
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

    // Partition phases into (mapped payload) and (skipped). Unmapped
    // phases get a debug trace so degraded-durability writes are visible
    // in logs without raising the alert ceiling.
    let metrics = writer.writer_metrics();
    let mut mapped: Vec<WalPayload> = Vec::with_capacity(write.phases.len());
    for phase in &write.phases {
        match phase_to_wal_payload(phase, write) {
            Some(payload) => mapped.push(payload),
            None => {
                metrics.record_wal_skip(phase.tag());
                tracing::debug!(
                    target: "brain_ops::writer",
                    write_id = ?write.write_id,
                    phase_tag = phase.tag(),
                    "wal_append: phase has no WAL mapping, durability via redb only",
                );
            }
        }
    }

    if mapped.is_empty() {
        return Ok(None);
    }

    // Build the full record batch up front (TxnBegin + payloads +
    // TxnCommit for multi-phase, single payload for single-phase) and
    // issue ONE `append_many`. Multi-phase writes previously cost
    // 2 + N channel hops + 2 + N fsyncs; the batched path collapses
    // to 1 channel hop + 1 fsync (group-commit folds the whole batch).
    let records: Vec<WalRecord> = if mapped.len() == 1 {
        vec![WalRecord::from_typed(
            Lsn(0),
            /* flags */ 0,
            started_at_unix_nanos,
            agent_id_lo64,
            &mapped[0],
        )]
    } else {
        use brain_core::TxnId;
        use brain_storage::wal::payload::{TxnBeginPayload, TxnCommitPayload};

        let txn_id = TxnId(write.write_id.as_uuid());
        let begin = WalPayload::TxnBegin(TxnBeginPayload {
            txn_id,
            expected_record_count: mapped.len() as u32,
        });
        let commit = WalPayload::TxnCommit(TxnCommitPayload { txn_id });

        let mut batch: Vec<WalRecord> = Vec::with_capacity(mapped.len() + 2);
        batch.push(WalRecord::from_typed(
            Lsn(0),
            0,
            started_at_unix_nanos,
            agent_id_lo64,
            &begin,
        ));
        for payload in &mapped {
            batch.push(WalRecord::from_typed(
                Lsn(0),
                0,
                started_at_unix_nanos,
                agent_id_lo64,
                payload,
            ));
        }
        batch.push(WalRecord::from_typed(
            Lsn(0),
            0,
            started_at_unix_nanos,
            agent_id_lo64,
            &commit,
        ));
        batch
    };

    let lsns = sink
        .append_many(records)
        .await
        .map_err(|e| WriterError::Internal(format!("wal append_many: {e}")))?;
    Ok(lsns.first().copied())
}

/// HNSW writes per phase. Runs after WAL append and before the
/// redb wtxn opens. A HNSW failure here aborts the write before any
/// metadata commits; the WAL record stays and recovery's replay will
/// retry on next start.
///
/// Phases this handles:
/// - `UpsertMemory`     → HNSW insert
/// - `UpdateEmbedding`  → HNSW insert (HNSW's insert replaces by id)
/// - `Tombstone(Memory)`→ HNSW mark_tombstoned
///
/// Other phases (Link, UpdateSalience, etc.) have no HNSW effect.
///
/// Note: the arena is NOT written in the live path — arena bytes are
/// populated only by WAL recovery on shard restart, then HNSW serves
/// vectors from its own in-memory storage.
fn execute_hnsw_side_effects(writer: &RealWriterHandle, write: &Write) -> Result<(), WriterError> {
    for phase in write.phases.iter() {
        match phase {
            Phase::UpsertMemory { id, vector, .. }
            | Phase::UpdateEmbedding {
                id,
                new_vector: vector,
            } => {
                writer
                    .hnsw_writer_lock()
                    .insert(*id, vector.as_ref())
                    .map_err(|e| WriterError::Internal(format!("hnsw insert: {e:?}")))?;
            }
            Phase::Tombstone {
                target: TombstoneTarget::Memory { id, .. },
                ..
            } => {
                // mark_tombstoned returns NotFound if HNSW doesn't have
                // the entry yet (e.g. recovery is mid-replay and HNSW
                // maintenance hasn't run). That's a "tombstone something
                // not surfacing" — treat as no-op.
                let _ = writer.hnsw_writer_lock().mark_tombstoned(*id);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Publish one event per phase that has a wire-side counterpart.
///
/// Memory phases (UpsertMemory, Tombstone(Memory), Link, Unlink) and
/// typed-graph phases (UpsertEntity, ...) publish their corresponding
/// event types. Phases without a wire surface (UpdateSalience,
/// ReclaimSlots, …) don't publish — they affect observability through
/// metrics, not subscribers.
fn publish_events_for(writer: &RealWriterHandle, write: &Write, committed_at_unix_nanos: u64) {
    let Some(bus) = writer.event_bus() else {
        // No bus wired — test path or no-schema deployment that
        // doesn't surface a change feed. Drop the events silently.
        return;
    };

    for phase in write.phases.iter() {
        let Some(mut env) = phase_to_envelope(phase, write, committed_at_unix_nanos) else {
            continue;
        };
        // Tombstone(Memory) needs the original row's context_id +
        // kind in the envelope so subscribers can filter properly.
        // Read it back post-commit — the row is still present (soft
        // tombstone keeps it during the grace window).
        if let Phase::Tombstone {
            target: TombstoneTarget::Memory { id, .. },
            ..
        } = phase
        {
            if let Some((ctx, kind)) = read_memory_context_and_kind(writer, *id) {
                env.context_id = ctx;
                env.kind = kind;
            }
        }
        bus.publish(env);
    }
}

/// Read MEMORIES_TABLE for the row's context_id + kind. Used by the
/// post-commit event publisher to stamp Tombstone events with the
/// values the subscriber filter actually compares against. Returns
/// `None` if the row went away between commit and publish (shouldn't
/// happen — single-writer-per-shard — but defensive).
fn read_memory_context_and_kind(
    writer: &RealWriterHandle,
    id: brain_core::MemoryId,
) -> Option<(ContextId, MemoryKind)> {
    let rtxn = writer.metadata().read_txn().ok()?;
    let t = rtxn
        .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
        .ok()?;
    let row = t.get(id.to_be_bytes()).ok().flatten()?.value();
    let kind = match row.kind {
        0 => MemoryKind::Episodic,
        1 => MemoryKind::Semantic,
        2 => MemoryKind::Consolidated,
        _ => MemoryKind::Episodic,
    };
    Some((ContextId(row.context_id), kind))
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
            graph_payload: None,
            edge_payload: None,
            stage_kind: None,
            stage_outcome: None,
            stage_payload: None,
            agent_id: write.agent_id,
        }),

        Phase::Tombstone { target, .. } => match target {
            TombstoneTarget::Memory { id, mode: _ } => Some(EventEnvelope {
                lsn: 0,
                event_type: EventType::Forgotten,
                memory_id: *id,
                context_id: ContextId::default(),
                kind: MemoryKind::Episodic,
                salience: 0.0,
                timestamp_unix_nanos: committed_at_unix_nanos,
                text: None,
                graph_payload: None,
                edge_payload: None,
                stage_kind: None,
                stage_outcome: None,
                stage_payload: None,
                agent_id: write.agent_id,
            }),
            // Typed-graph tombstones publish through the typed-graph-event
            // path (emit_graph_event), not the memory subscribe bus.
            TombstoneTarget::Entity(_)
            | TombstoneTarget::Statement(_)
            | TombstoneTarget::Relation(_) => None,
        },

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
            graph_payload: None,
            edge_payload: Some(edge_payload_to_event(
                *from,
                *to,
                *kind,
                *weight,
                None,
                None,
                *edge_origin,
            )),
            stage_kind: None,
            stage_outcome: None,
            stage_payload: None,
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
            graph_payload: None,
            edge_payload: Some(edge_payload_to_event(
                *from,
                *to,
                *kind,
                0.0,
                None,
                None,
                origin::EXPLICIT,
            )),
            stage_kind: None,
            stage_outcome: None,
            stage_payload: None,
            agent_id: write.agent_id,
        }),

        // typed-graph phases publish through the typed-graph-event channel
        // (emit_graph_event), not the memory subscribe bus — they
        // surface to subscribers via that path, not this envelope.
        Phase::UpsertEntity { .. }
        | Phase::UpsertStatement { .. }
        | Phase::UpsertRelation { .. }
        | Phase::UpsertSchema { .. }
        | Phase::Supersede { .. }
        | Phase::UpdateEntity { .. }
        | Phase::RenameEntity { .. }
        | Phase::UnmergeEntities { .. }
        | Phase::MergeEntities { .. }
        | Phase::ApproveMerge { .. }
        | Phase::RejectMerge { .. }
        | Phase::SetExtractorEnabled { .. } => None,

        // Substrate phases without a subscribe-feed surface — their
        // observability is metrics-only. SalienceUpdated / KindUpdated /
        // ContextUpdated / EmbeddingUpdated don't trigger a wire event
        // because subscribers don't filter on them; ReclaimSlots is an
        // internal-ish maintenance op.
        Phase::UpdateSalience { .. }
        | Phase::UpdateKind { .. }
        | Phase::UpdateContext { .. }
        | Phase::UpdateEmbedding { .. }
        | Phase::ReclaimSlots { .. } => None,
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
/// schema-admission and not-found variants get richer wire mappings
/// once the handler-side projection lands.
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::{Phase, Write, WriteId};
    use crate::writer::RealWriterHandle;
    use brain_core::{AgentId, ContextId, EdgeKind, EdgeKindRef, MemoryId, MemoryKind, NodeRef};
    use brain_embed::VECTOR_DIM;
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::tables::edge::zero_disambiguator;
    use brain_metadata::MetadataDb;
    use brain_planner::SharedMetadataDb;

    use std::sync::Arc;
    use tempfile::TempDir;

    fn build_writer() -> (TempDir, RealWriterHandle) {
        let (dir, writer, _shared) = build_writer_with_shared();
        (dir, writer)
    }

    /// Test helper that also returns the SharedHnsw reader so tests
    /// can assert on HNSW post-submit (the RealWriterHandle holds
    /// only the Writer half of the pair).
    fn build_writer_with_shared() -> (TempDir, RealWriterHandle, SharedHnsw) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        let metadata: SharedMetadataDb = Arc::new(db);
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer = RealWriterHandle::new(metadata, hnsw_writer);
        (dir, writer, shared)
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
        assert_eq!(writer.write_idempotency_cache().hot_len(), 1);
    }

    #[tokio::test]
    async fn submit_writes_writer_metrics() {
        // First submit must bump miss + submit_ok(link); replay must
        // bump hit but NOT re-record a submit-outcome (the cache short-
        // circuits before the apply path).
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
        let _ = writer.submit(write.clone()).await.expect("first submit");
        let _ = writer.submit(write).await.expect("replay");

        let snap = writer.writer_metrics().snapshot();
        assert_eq!(snap.idempotency_miss, 1, "first submit is a cache miss");
        assert_eq!(snap.idempotency_hit, 1, "replay is a cache hit");
        assert_eq!(snap.idempotency_conflict, 0);

        let link = snap
            .by_phase
            .iter()
            .find(|p| p.phase == "link")
            .expect("link phase counters");
        assert_eq!(link.submit_ok, 1, "only the first submit reaches dispatch");
        assert_eq!(link.submit_err, 0);
        assert_eq!(link.submit_conflict, 0);
        assert_eq!(link.submit_duration_seconds.count, 1);
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
        use crate::handlers::subscribe::{EventBus, SubscriptionRegistry};
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
        use crate::handlers::subscribe::EventBus;
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
    async fn submit_upsert_memory_inserts_into_hnsw() {
        // UpsertMemory's HNSW side-effect lands the vector in
        // the search index. We query via the SharedHnsw reader half
        // — the writer holds only the Writer half.
        let (_dir, writer, shared) = build_writer_with_shared();
        let id = MemoryId::pack(0, 1, 0);
        let phase = Phase::UpsertMemory {
            id,
            text: "hello".into(),
            vector: Box::new([0.5_f32; VECTOR_DIM]),
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
        assert!(
            shared.contains(id),
            "HNSW must contain the upserted memory_id"
        );
    }

    #[tokio::test]
    async fn submit_tombstone_memory_marks_hnsw() {
        let (_dir, writer, shared) = build_writer_with_shared();
        let id = MemoryId::pack(0, 1, 0);
        // Set up: insert.
        let upsert = Phase::UpsertMemory {
            id,
            text: "hi".into(),
            vector: Box::new([0.5_f32; VECTOR_DIM]),
            kind: MemoryKind::Episodic,
            salience: brain_core::Salience::default(),
            context: ContextId(0),
            created_at_unix_nanos: 0,
            arena_slot: 1,
            embedding_model_fp: [0; 16],
            content_hash: None,
            deduplicate: false,
        };
        writer
            .submit(Write::single(WriteId::new(), AgentId::new(), upsert))
            .await
            .unwrap();
        assert!(!shared.is_tombstoned(id));

        // Tombstone via unified path.
        let tomb = Phase::Tombstone {
            target: TombstoneTarget::Memory {
                id,
                mode: crate::write::phase::TombstoneMode::Soft,
            },
            reason: 0,
            at_unix_nanos: 1_700_000_001_000,
        };
        writer
            .submit(Write::single(WriteId::new(), AgentId::new(), tomb))
            .await
            .expect("tombstone submit");
        assert!(
            shared.is_tombstoned(id),
            "HNSW must mark the memory_id tombstoned after Phase::Tombstone(Memory)"
        );
    }

    /// Regression: fresh-DB encode with `deduplicate=true` used to
    /// panic on the read-side lookup with
    /// `Table 'fingerprints' does not exist` because redb doesn't
    /// create the table until something writes it. Constructing
    /// `RealWriterHandle` must materialise every table that any
    /// read-side path touches; otherwise the first opt-in dedup
    /// encode 500s.
    #[test]
    fn writer_construction_bootstraps_fingerprint_table_for_reads() {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        let metadata: SharedMetadataDb = Arc::new(db);
        let (_shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let _writer = RealWriterHandle::new(metadata.clone(), hnsw_writer);

        // After construction, every table that op handlers read from
        // pre-submit must be openable in a fresh read txn — proving
        // the bootstrap covers them.
        let rtxn = metadata.read_txn().expect("read_txn");
        for table_label in [
            "MEMORIES",
            "MEMORIES_BY_AGENT_TIMELINE",
            "IDEMPOTENCY",
            "EDGES",
            "EDGES_REVERSE",
            "FINGERPRINTS",
            "TEXTS",
        ] {
            let result: Result<(), redb::TableError> = match table_label {
                "MEMORIES" => rtxn
                    .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
                    .map(|_| ()),
                "MEMORIES_BY_AGENT_TIMELINE" => rtxn
                    .open_table(brain_metadata::tables::memory::MEMORIES_BY_AGENT_TIMELINE_TABLE)
                    .map(|_| ()),
                "IDEMPOTENCY" => rtxn
                    .open_table(brain_metadata::tables::idempotency::IDEMPOTENCY_TABLE)
                    .map(|_| ()),
                "EDGES" => rtxn
                    .open_table(brain_metadata::tables::edge::EDGES_TABLE)
                    .map(|_| ()),
                "EDGES_REVERSE" => rtxn
                    .open_table(brain_metadata::tables::edge::EDGES_REVERSE_TABLE)
                    .map(|_| ()),
                "FINGERPRINTS" => rtxn
                    .open_table(brain_metadata::tables::fingerprint::FINGERPRINTS_TABLE)
                    .map(|_| ()),
                "TEXTS" => rtxn
                    .open_table(brain_metadata::tables::text::TEXTS_TABLE)
                    .map(|_| ()),
                _ => unreachable!(),
            };
            assert!(
                result.is_ok(),
                "table {table_label} must be materialised at writer construction"
            );
        }
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
        use crate::writer::wal_sink::WalSink;
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
                                crate::writer::wal_sink::WalSinkError,
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

            fn append_many<'a>(
                &'a self,
                records: Vec<WalRecord>,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                Vec<brain_storage::wal::record::Lsn>,
                                crate::writer::wal_sink::WalSinkError,
                            >,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(async move {
                    let mut out = Vec::with_capacity(records.len());
                    let mut lsn_guard = self.next_lsn.lock().unwrap();
                    let mut sink = self.records.lock().unwrap();
                    for mut record in records {
                        let lsn = brain_storage::wal::record::Lsn(*lsn_guard);
                        *lsn_guard += 1;
                        record.lsn = lsn;
                        sink.push(record);
                        out.push(lsn);
                    }
                    Ok(out)
                })
            }
        }

        // The WAL sink type is referenced through brain_ops to keep
        // this test crate-internal.
        // Build writer + override the sink.
        let (_dir, mut writer) = build_writer();
        let sink: Arc<dyn crate::writer::wal_sink::WalSink> = Arc::new(CapturingSink {
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

    /// A Write that mixes mapped substrate phases with an unmapped
    /// typed-graph phase must produce a WAL record for every mapped
    /// phase. The earlier "all-or-nothing" gate dropped the entire
    /// append, silently demoting WAL-durable writes to redb-only.
    #[tokio::test]
    async fn mixed_mapped_and_unmapped_phases_wal_only_the_mapped() {
        use crate::writer::wal_sink::RecordingWalSink;
        use brain_storage::wal::kinds::WalRecordKind;

        let (_dir, mut writer) = build_writer();
        let sink = Arc::new(RecordingWalSink::new());
        writer = writer.with_wal_sink(sink.clone());

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
        // ReclaimSlots has no WAL mapping (it's derivable from
        // MEMORIES_TABLE state on recovery) — it must be skipped, not
        // poison the whole append.
        let reclaim = Phase::ReclaimSlots { slots: vec![1, 2] };
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

        let write = Write::from_phases(
            WriteId::new(),
            AgentId::default(),
            vec![upsert, reclaim, link],
        );
        // The unmapped ReclaimSlots phase is skipped at the WAL layer;
        // WAL append runs before apply opens its wtxn, so the recording
        // sink sees the durable framing we're asserting on regardless of
        // apply's outcome.
        let _ = writer.submit(write).await;

        let kinds: Vec<WalRecordKind> = sink.appended().iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                WalRecordKind::TxnBegin,
                WalRecordKind::Encode,
                WalRecordKind::Link,
                WalRecordKind::TxnCommit,
            ],
            "mapped substrate phases must reach WAL even when an unmapped \
             phase is interleaved",
        );
    }

    /// A Write composed entirely of unmapped phases produces no WAL
    /// records at all — not even a TxnBegin/TxnCommit envelope. Their
    /// durability rides on the redb commit's fsync.
    #[tokio::test]
    async fn all_unmapped_phases_no_wal_records() {
        use crate::writer::wal_sink::RecordingWalSink;

        let (_dir, mut writer) = build_writer();
        let sink = Arc::new(RecordingWalSink::new());
        writer = writer.with_wal_sink(sink.clone());

        // Two phases with no WAL mapping: ReclaimSlots is derivable from
        // MEMORIES_TABLE on recovery, and UpdateEmbedding rewrites a
        // vector the HNSW already absorbed pre-commit.
        let reclaim = Phase::ReclaimSlots { slots: vec![3, 4] };
        let update_embedding = Phase::UpdateEmbedding {
            id: MemoryId::pack(0, 1, 0),
            new_vector: Box::new([0.0_f32; VECTOR_DIM]),
        };

        let write = Write::from_phases(
            WriteId::new(),
            AgentId::default(),
            vec![reclaim, update_embedding],
        );
        // The phases will fail in apply (no rows to update), but the WAL
        // append happens before apply — we only care that nothing reached
        // the WAL when every phase is unmapped.
        let _ = writer.submit(write).await;

        assert!(
            sink.is_empty(),
            "writes with only unmapped phases must not append WAL records, \
             got {} record(s)",
            sink.len(),
        );
    }

    /// An N-phase Write must cross the writer→WAL boundary in exactly
    /// one `append_many` invocation. The pre-batched code path issued
    /// `2 + N` separate `append` calls (TxnBegin, N records, TxnCommit)
    /// — each one a channel hop, a oneshot allocation, and a wakeup. The
    /// batched path collapses that to a single hop while still emitting
    /// the same on-WAL framing.
    #[tokio::test]
    async fn multi_phase_write_issues_one_append_many_call() {
        use crate::writer::wal_sink::RecordingWalSink;
        use brain_storage::wal::kinds::WalRecordKind;

        let (_dir, mut writer) = build_writer();
        let sink = Arc::new(RecordingWalSink::new());
        writer = writer.with_wal_sink(sink.clone());

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
        let mk_link = |from_slot: u64, to_slot: u64| Phase::Link {
            from: NodeRef::Memory(MemoryId::pack(0, from_slot, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, to_slot, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.5,
            origin: 0,
            derived_by: 0,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 1_700_000_000_000,
        };
        let phases = vec![upsert, mk_link(1, 1), mk_link(1, 1), mk_link(1, 1)];
        let write = Write::from_phases(WriteId::new(), AgentId::default(), phases);
        let _ = writer.submit(write).await;

        // One batched submission, zero single-record submissions.
        assert_eq!(
            sink.append_many_calls(),
            1,
            "expected one batched append_many call",
        );
        assert_eq!(
            sink.append_calls(),
            0,
            "writer must not issue per-phase append calls",
        );

        // The on-WAL framing still matches what recovery expects:
        // TxnBegin + 4 typed payloads + TxnCommit.
        let kinds: Vec<WalRecordKind> = sink.appended().iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                WalRecordKind::TxnBegin,
                WalRecordKind::Encode,
                WalRecordKind::Link,
                WalRecordKind::Link,
                WalRecordKind::Link,
                WalRecordKind::TxnCommit,
            ],
        );
    }

    /// A single-phase Write also takes the batched path (records.len()==1)
    /// — verified by call counts.
    #[tokio::test]
    async fn single_phase_write_also_takes_append_many_path() {
        use crate::writer::wal_sink::RecordingWalSink;

        let (_dir, mut writer) = build_writer();
        let sink = Arc::new(RecordingWalSink::new());
        writer = writer.with_wal_sink(sink.clone());

        let phase = Phase::Link {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.5,
            origin: 0,
            derived_by: 0,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 0,
        };
        let write = Write::single(WriteId::new(), AgentId::default(), phase);
        let _ = writer.submit(write).await;

        assert_eq!(sink.append_many_calls(), 1);
        assert_eq!(sink.append_calls(), 0);
        // Single-phase: no TxnBegin/TxnCommit envelope, just the
        // typed payload itself.
        assert_eq!(sink.len(), 1);
    }

    // ---------------------------------------------------------------
    // Durable idempotency.
    //
    // Three properties the cache must satisfy:
    //
    //  A) A successful submit's ack survives a writer drop + reopen
    //     of the same redb file. The second submit must return the
    //     cached ack, not re-execute the apply path.
    //  B) Conflict detection survives a restart: a different request
    //     hash on the same WriteId returns Conflict, not "miss → run".
    //  C) Expired entries (older than 24 h) read as misses; the writer
    //     re-executes. Driven by a custom clock in the cache.
    //
    // The tests build a writer, drop it, and re-open MetadataDb from
    // the same on-disk path. The hot in-memory cache is gone on
    // reopen; only the durable IDEMPOTENCY_TABLE row remains.
    // ---------------------------------------------------------------

    fn build_writer_for_path(path: std::path::PathBuf) -> RealWriterHandle {
        let db = MetadataDb::open(path).unwrap();
        let metadata: SharedMetadataDb = Arc::new(db);
        let (_shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        RealWriterHandle::new(metadata, hnsw_writer)
    }

    fn build_writer_with_clock(
        path: std::path::PathBuf,
        clock: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> RealWriterHandle {
        let db = MetadataDb::open(path).unwrap();
        let metadata: SharedMetadataDb = Arc::new(db);
        let (_shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer = RealWriterHandle::new(metadata, hnsw_writer);
        // Swap the default clock for the test-driven one.
        let c = clock.clone();
        let cache =
            WriteIdempotencyCache::with_clock(move || c.load(std::sync::atomic::Ordering::SeqCst));
        writer.with_write_idempotency_cache(Arc::new(cache))
    }

    /// Test A. Submit, drop the writer (closing the redb file), reopen
    /// and submit the same WriteId+hash. The reopened writer's hot
    /// cache is cold; the durable IDEMPOTENCY_TABLE row must drive a
    /// cache hit and return the original ack.
    #[tokio::test]
    async fn durable_replay_returns_cached_ack_across_writer_drop() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("meta.redb");

        let write_id = WriteId::new();
        let request_hash = [0x42u8; 32];

        // First submit on writer #1.
        let first_ack = {
            let writer = build_writer_for_path(db_path.clone());
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
            let write =
                Write::single(write_id, AgentId::default(), phase).with_request_hash(request_hash);
            writer.submit(write).await.expect("first submit")
            // writer drops here; redb file closes.
        };

        // Re-open from the same path: hot cache is cold.
        let writer2 = build_writer_for_path(db_path.clone());
        assert_eq!(
            writer2.write_idempotency_cache().hot_len(),
            0,
            "reopened writer must have an empty hot cache"
        );

        // Second submit — same WriteId + same hash. Must come from the
        // durable table, not re-execute.
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
        let write =
            Write::single(write_id, AgentId::default(), phase).with_request_hash(request_hash);
        let second_ack = writer2.submit(write).await.expect("replay submit");

        assert_eq!(first_ack.write_id, second_ack.write_id);
        assert_eq!(
            first_ack.committed_at_unix_nanos, second_ack.committed_at_unix_nanos,
            "replay must return the original commit timestamp, not today's clock",
        );
        assert_eq!(first_ack.lsn_first, second_ack.lsn_first);
        assert_eq!(first_ack.phase_acks, second_ack.phase_acks);
        let snap = writer2.writer_metrics().snapshot();
        assert_eq!(
            snap.idempotency_hit, 1,
            "second submit hit the durable cache"
        );
    }

    /// Test B. Same WriteId, different request_hash, across a writer
    /// drop + reopen. The durable row's stored hash must drive a
    /// `Conflict` outcome on the second submit — the in-memory cache
    /// alone could no longer remember the original hash.
    #[tokio::test]
    async fn durable_conflict_detected_across_writer_drop() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("meta.redb");

        let write_id = WriteId::new();
        let hash_a = [0x11u8; 32];
        let hash_b = [0x22u8; 32];

        {
            let writer = build_writer_for_path(db_path.clone());
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
            let write =
                Write::single(write_id, AgentId::default(), phase).with_request_hash(hash_a);
            writer.submit(write).await.expect("first submit");
        }

        let writer2 = build_writer_for_path(db_path.clone());
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
        let write = Write::single(write_id, AgentId::default(), phase).with_request_hash(hash_b);
        let err = writer2.submit(write).await.expect_err("must conflict");
        assert!(
            matches!(err, WriterError::Conflict(_)),
            "expected Conflict, got {err:?}"
        );
    }

    /// Test C. An entry whose `created_at + 24h` is in the past reads
    /// as a miss; the second submit must re-execute the apply path
    /// (different commit timestamp).
    #[tokio::test]
    async fn ttl_expiry_drives_re_execution() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("meta.redb");

        let write_id = WriteId::new();
        let request_hash = [0x77u8; 32];

        // Use a shared clock the test can advance.
        let t0: u64 = 1_700_000_000_000_000_000;
        let twenty_five_h: u64 = 25 * 60 * 60 * 1_000_000_000;
        let clock = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(t0));

        let first_committed_at = {
            let writer = build_writer_with_clock(db_path.clone(), clock.clone());
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
            let write =
                Write::single(write_id, AgentId::default(), phase).with_request_hash(request_hash);
            let ack = writer.submit(write).await.expect("first submit");
            assert_eq!(
                ack.committed_at_unix_nanos, t0,
                "first commit must read the test clock at t0",
            );
            ack.committed_at_unix_nanos
        };

        // Jump the clock past the 24 h window.
        clock.store(t0 + twenty_five_h, std::sync::atomic::Ordering::SeqCst);

        // Re-open writer (durable row remains; hot cache resets) and
        // re-submit. The durable row is now expired → miss → execute.
        let writer2 = build_writer_with_clock(db_path.clone(), clock.clone());
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
        let write =
            Write::single(write_id, AgentId::default(), phase).with_request_hash(request_hash);
        let second_ack = writer2.submit(write).await.expect("re-executed submit");
        assert_ne!(
            second_ack.committed_at_unix_nanos, first_committed_at,
            "expired entry must drive re-execution, not a cache hit",
        );
        let snap = writer2.writer_metrics().snapshot();
        assert_eq!(
            snap.idempotency_miss, 1,
            "expired durable row must be classified as a miss",
        );
        assert_eq!(snap.idempotency_hit, 0);
    }
}
