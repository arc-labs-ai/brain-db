//! FORGET handler — tombstones the memory in HNSW and stamps
//! `tombstoned_at_unix_nanos` on the metadata row inside the same
//! idempotency txn. Emits the change-feed event only on the
//! `Tombstoned` transition (spec §10/4).

use brain_core::MemoryKind;
use brain_metadata::tables::fingerprint::{fingerprint_key, FINGERPRINTS_TABLE};
use brain_metadata::tables::idempotency::{IdempotencyEntry, IDEMPOTENCY_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_planner::{ForgetAck, ForgetOp, ForgetOutcome, WriterError};
use brain_protocol::request::ForgetMode as ForgetModeWire;
use brain_protocol::response::EventType;
use brain_storage::wal::payload::{
    ForgetMode as WalForgetMode, ForgetPayload as WalForgetPayload,
    ForgetReason as WalForgetReason, WalPayload,
};
use brain_storage::wal::record::{Lsn, WalRecord};
use redb::ReadableTable;

use crate::idempotency::{
    decode_forget_payload, encode_forget_payload, hash_forget_request, RESPONSE_KIND_FORGET,
};
use crate::subscribe::EventEnvelope;

use super::{hex_short, now_unix_nanos, RealWriterHandle};

pub(super) async fn do_forget(
    writer: &RealWriterHandle,
    op: ForgetOp,
) -> Result<ForgetAck, WriterError> {
    let request_hash = hash_forget_request(&op);
    let request_id_bytes: [u8; 16] = op.request_id.into();

    // ── Idempotency lookup. ───────────────────────────────────────
    {
        let db = writer.metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| WriterError::Internal(format!("idempotency read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(IDEMPOTENCY_TABLE)
            .map_err(|e| WriterError::Internal(format!("open IDEMPOTENCY_TABLE: {e:?}")))?;
        if let Some(access) = table
            .get(request_id_bytes)
            .map_err(|e| WriterError::Internal(format!("idempotency get: {e:?}")))?
        {
            let prior = access.value();
            if prior.request_hash != request_hash {
                return Err(WriterError::Conflict(format!(
                    "forget request_id={} hash mismatch",
                    hex_short(&request_id_bytes)
                )));
            }
            if prior.response_kind != RESPONSE_KIND_FORGET {
                return Err(WriterError::Conflict(format!(
                    "forget request_id={} kind mismatch",
                    hex_short(&request_id_bytes)
                )));
            }
            let (memory_id, outcome) = decode_forget_payload(&prior.response_payload)
                .map_err(|e| WriterError::Internal(format!("decode forget payload: {e}")))?;
            return Ok(ForgetAck {
                memory_id,
                outcome,
                replayed: true,
            });
        }
    }

    // ── Already tombstoned (this process-lifetime)? ──────────────
    let already = writer.tombstoned.lock().contains(&op.memory_id);
    if already {
        return record_and_return_forget(
            writer,
            &op,
            request_hash,
            ForgetOutcome::AlreadyTombstoned,
            None,
        )
        .await;
    }

    // ── Look up the memory row (existence + context/kind/salience
    //    for the change-feed event). ─────────────────────────────
    let meta_snapshot: Option<MemoryMetadata> = {
        let db = writer.metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| WriterError::Internal(format!("memory read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| WriterError::Internal(format!("memory open_table: {e:?}")))?;
        table
            .get(op.memory_id.to_be_bytes())
            .ok()
            .flatten()
            .map(|access| access.value())
    };
    let Some(meta) = meta_snapshot else {
        return record_and_return_forget(
            writer,
            &op,
            request_hash,
            ForgetOutcome::MemoryNotFound,
            None,
        )
        .await;
    };

    // ── Tombstone in HNSW (durable in the sense it survives this
    //    process's lifetime; Phase 8/9 wires the WAL-recoverable
    //    flag). ─────────────────────────────────────────────────
    writer
        .hnsw_writer
        .lock()
        .mark_tombstoned(op.memory_id)
        .map_err(|e| WriterError::Internal(format!("mark_tombstoned: {e:?}")))?;
    writer.tombstoned.lock().insert(op.memory_id);

    record_and_return_forget(
        writer,
        &op,
        request_hash,
        ForgetOutcome::Tombstoned,
        Some(meta),
    )
    .await
}

async fn record_and_return_forget(
    writer: &RealWriterHandle,
    op: &ForgetOp,
    request_hash: [u8; 32],
    outcome: ForgetOutcome,
    meta: Option<MemoryMetadata>,
) -> Result<ForgetAck, WriterError> {
    let request_id_bytes: [u8; 16] = op.request_id.into();
    let created_at = now_unix_nanos();
    let payload = encode_forget_payload(op.memory_id, outcome);

    // ── WAL append for the substrate-affecting transition only.
    // `MemoryNotFound` and `AlreadyTombstoned` are pure no-ops on
    // arena + index; their idempotency stamp lives in redb and
    // doesn't need a separate WAL record. The Tombstoned transition
    // DOES need durable WAL ordering so subscribe-replay can emit
    // the matching `Forgotten` event and recovery can re-tombstone
    // the slot on a fresh boot. ────────────────────────────────────
    let wal_lsn: Option<Lsn> = if matches!(outcome, ForgetOutcome::Tombstoned) {
        if let Some(sink) = &writer.wal_sink {
            let wal_mode = match op.mode {
                ForgetModeWire::Soft => WalForgetMode::Soft,
                ForgetModeWire::Hard => WalForgetMode::Hard,
            };
            let record_payload = WalPayload::Forget(WalForgetPayload {
                memory_id: op.memory_id,
                request_id: op.request_id,
                agent_id: op.agent_id,
                mode: wal_mode,
                reason: WalForgetReason::ClientRequest,
            });
            let agent_bytes: [u8; 16] = op.agent_id.into();
            let agent_id_lo64 = u64::from_be_bytes(agent_bytes[8..16].try_into().unwrap());
            let record = WalRecord::from_typed(
                Lsn(0),
                /* flags */ 0,
                created_at,
                agent_id_lo64,
                &record_payload,
            );
            let lsn = sink
                .append(record)
                .await
                .map_err(|e| WriterError::Internal(format!("wal append: {e}")))?;
            Some(lsn)
        } else {
            None
        }
    } else {
        None
    };

    {
        let mut db = writer.metadata.lock();
        let wtxn = db
            .write_txn()
            .map_err(|e| WriterError::Internal(format!("forget write_txn: {e:?}")))?;
        {
            let mut idem_t = wtxn
                .open_table(IDEMPOTENCY_TABLE)
                .map_err(|e| WriterError::Internal(format!("forget open idempotency: {e:?}")))?;
            let entry = IdempotencyEntry::new(
                RESPONSE_KIND_FORGET,
                Some(op.memory_id.to_be_bytes()),
                payload,
                request_hash,
                created_at,
                wal_lsn.map(|l| l.raw()).unwrap_or(0),
            );
            idem_t
                .insert(request_id_bytes, entry)
                .map_err(|e| WriterError::Internal(format!("forget idempotency insert: {e:?}")))?;
        }
        // Sub-task 8.7: stamp tombstoned_at on the MEMORIES row so the
        // slot-reclamation worker (and any tombstone-aware filter)
        // can discover age. Set-once — replays don't bump the stamp.
        // Phase 8.dedup: at the same time, if the row carried a
        // dedup back-reference (`content_hash`), evict the matching
        // `FINGERPRINTS` entry so future dedup-true ENCODEs don't
        // hit a tombstoned memory (spec §07/07 §6.3 option b).
        if matches!(outcome, ForgetOutcome::Tombstoned) {
            let mut memories_t = wtxn
                .open_table(MEMORIES_TABLE)
                .map_err(|e| WriterError::Internal(format!("forget open MEMORIES: {e:?}")))?;
            let prior = memories_t
                .get(op.memory_id.to_be_bytes())
                .map_err(|e| WriterError::Internal(format!("forget memories get: {e:?}")))?
                .map(|access| access.value());
            if let Some(mut row) = prior {
                let dedup_back_ref = row.content_hash;
                let row_agent = row.agent_id();
                let row_context = row.context();
                // Snapshot the original-encode timestamp before we
                // consume `row` in the metadata stamp below. The
                // timeline-index delete needs it.
                let row_created_at = row.created_at_unix_nanos;
                if row.tombstoned_at_unix_nanos.is_none() {
                    row.tombstoned_at_unix_nanos = Some(created_at);
                    memories_t
                        .insert(op.memory_id.to_be_bytes(), row)
                        .map_err(|e| {
                            WriterError::Internal(format!("forget memories stamp: {e:?}"))
                        })?;
                }
                if let Some(hash) = dedup_back_ref {
                    let key = fingerprint_key(row_agent, row_context, &hash);
                    let mut fp_t = wtxn.open_table(FINGERPRINTS_TABLE).map_err(|e| {
                        WriterError::Internal(format!("forget open FINGERPRINTS: {e:?}"))
                    })?;
                    fp_t.remove(key).map_err(|e| {
                        WriterError::Internal(format!("forget fingerprints remove: {e:?}"))
                    })?;
                }
                // Drop the agent-timeline index row so the
                // TemporalEdgeWorker never proposes a tombstoned
                // memory as a temporal predecessor. Same write txn
                // as the metadata stamp so the two stay consistent.
                {
                    use brain_metadata::tables::memory::{
                        agent_timeline_key, MEMORIES_BY_AGENT_TIMELINE_TABLE,
                    };
                    let key = agent_timeline_key(
                        row_agent.0.into_bytes(),
                        row_created_at,
                        row_context.0,
                        op.memory_id.to_be_bytes(),
                    );
                    let mut tl_t =
                        wtxn.open_table(MEMORIES_BY_AGENT_TIMELINE_TABLE)
                            .map_err(|e| {
                                WriterError::Internal(format!(
                                    "forget open MEMORIES_BY_AGENT_TIMELINE: {e:?}"
                                ))
                            })?;
                    tl_t.remove(key.as_slice()).map_err(|e| {
                        WriterError::Internal(format!("forget timeline remove: {e:?}"))
                    })?;
                }
            }
        }
        wtxn.commit()
            .map_err(|e| WriterError::Internal(format!("forget commit: {e:?}")))?;
    }

    // ── Change-feed (sub-task 7.10): only Tombstoned transitions
    //    emit. MemoryNotFound / AlreadyTombstoned aren't state
    //    changes, so per spec §10/4 we don't publish them. ──────
    if matches!(outcome, ForgetOutcome::Tombstoned) {
        if let Some(m) = meta {
            // `kind()` returns `Err(BadMemoryKind)` only if the stored
            // byte is corrupt (out-of-band write). Fall back to
            // Episodic in that pathological case — the change-feed
            // event is best-effort, not load-bearing.
            let kind = m.kind().unwrap_or(MemoryKind::Episodic);
            writer.publish_with_lsn(
                EventEnvelope {
                    lsn: 0,
                    event_type: EventType::Forgotten,
                    memory_id: op.memory_id,
                    context_id: m.context(),
                    kind,
                    salience: m.salience,
                    timestamp_unix_nanos: created_at,
                    text: None,
                    knowledge_payload: None,
                    edge_payload: None,
                    agent_id: op.agent_id,
                },
                wal_lsn.map(|l| l.raw()),
            );
        }
    }

    Ok(ForgetAck {
        memory_id: op.memory_id,
        outcome,
        replayed: false,
    })
}
