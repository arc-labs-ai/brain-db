//! ENCODE handler — splits memory-row write + edge insertion +
//! idempotency stamp into a single redb txn; HNSW insert + change-feed
//! event follow the durability barrier.

use std::sync::atomic::Ordering;

use brain_core::MemoryId;
use brain_metadata::tables::edge::{
    self, derived_by, origin, EdgeData, EDGES_IN_TABLE, EDGES_OUT_TABLE,
};
use brain_metadata::tables::idempotency::{IdempotencyEntry, IDEMPOTENCY_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_planner::{EdgeOutcome, EncodeAck, EncodeOp, WriterError};
use brain_protocol::response::EventType;
use redb::ReadableTable;

use crate::idempotency::{
    decode_encode_payload, encode_encode_payload, hash_encode_request, RESPONSE_KIND_ENCODE,
};
use crate::subscribe::EventEnvelope;

use super::{hex_short, now_unix_nanos, RealWriterHandle};

pub(super) fn do_encode(writer: &RealWriterHandle, op: EncodeOp) -> Result<EncodeAck, WriterError> {
    let request_hash = hash_encode_request(&op);
    let request_id_bytes: [u8; 16] = op.request_id.into();

    // ── Idempotency lookup (read txn). ────────────────────────────
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
                    "encode request_id={} hash mismatch",
                    hex_short(&request_id_bytes)
                )));
            }
            if prior.response_kind != RESPONSE_KIND_ENCODE {
                return Err(WriterError::Conflict(format!(
                    "encode request_id={} kind mismatch",
                    hex_short(&request_id_bytes)
                )));
            }
            let (memory_id, edge_outcomes) = decode_encode_payload(&prior.response_payload)
                .map_err(|e| WriterError::Internal(format!("decode encode payload: {e}")))?;
            return Ok(EncodeAck {
                memory_id,
                edge_results: edge_outcomes,
                replayed: true,
            });
        }
    }

    // ── Mint slot + MemoryId. ─────────────────────────────────────
    let slot = writer.next_slot.fetch_add(1, Ordering::Relaxed);
    let memory_id = MemoryId::pack(/* shard */ 0, slot, /* version */ 1);
    let created_at = now_unix_nanos();

    // ── Compute edge outcomes against existing memories. ──────────
    // (Read txn before the write txn; minimises lock duration.)
    let edge_outcomes: Vec<EdgeOutcome> = {
        let db = writer.metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| WriterError::Internal(format!("edges read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| WriterError::Internal(format!("edges open_table: {e:?}")))?;
        op.edges
            .iter()
            .map(|edge| {
                let exists = table
                    .get(edge.target.to_be_bytes())
                    .ok()
                    .flatten()
                    .is_some();
                if exists {
                    EdgeOutcome::Inserted
                } else {
                    EdgeOutcome::TargetMissing
                }
            })
            .collect()
    };

    // ── Apply: metadata row + idempotency entry + edges in ONE write txn. ─
    let response_payload = encode_encode_payload(memory_id, &edge_outcomes);
    {
        let mut db = writer.metadata.lock();
        let wtxn = db
            .write_txn()
            .map_err(|e| WriterError::Internal(format!("write_txn: {e:?}")))?;

        // First open all the tables we'll touch.
        let mut new_memory_outgoing: u32 = 0;
        let target_count_bumps: Vec<MemoryId> = op
            .edges
            .iter()
            .zip(edge_outcomes.iter())
            .filter(|(_, o)| matches!(o, EdgeOutcome::Inserted))
            .map(|(e, _)| e.target)
            .collect();

        {
            let mut edges_out_t = wtxn
                .open_table(EDGES_OUT_TABLE)
                .map_err(|e| WriterError::Internal(format!("open EDGES_OUT: {e:?}")))?;
            let mut edges_in_t = wtxn
                .open_table(EDGES_IN_TABLE)
                .map_err(|e| WriterError::Internal(format!("open EDGES_IN: {e:?}")))?;

            // Insert edges whose target exists (Inserted outcomes).
            for (edge, outcome) in op.edges.iter().zip(edge_outcomes.iter()) {
                if !matches!(outcome, EdgeOutcome::Inserted) {
                    continue;
                }
                let data = EdgeData::new(
                    edge.weight,
                    origin::EXPLICIT,
                    derived_by::CLIENT,
                    created_at,
                );
                edge::link(
                    &mut edges_out_t,
                    &mut edges_in_t,
                    memory_id,
                    edge.kind,
                    edge.target,
                    &data,
                )
                .map_err(|e| WriterError::Internal(format!("edge::link: {e:?}")))?;
                new_memory_outgoing += 1;
            }
        }

        // Bump edges_in_count on the targets.
        {
            let mut memories_t = wtxn
                .open_table(MEMORIES_TABLE)
                .map_err(|e| WriterError::Internal(format!("open MEMORIES_TABLE: {e:?}")))?;
            for target_id in &target_count_bumps {
                let key = target_id.to_be_bytes();
                let prior = memories_t
                    .get(key)
                    .map_err(|e| WriterError::Internal(format!("memories get: {e:?}")))?
                    .map(|access| access.value());
                if let Some(mut meta) = prior {
                    meta.edges_in_count = meta.edges_in_count.saturating_add(1);
                    memories_t
                        .insert(key, meta)
                        .map_err(|e| WriterError::Internal(format!("memories update: {e:?}")))?;
                }
            }

            // Insert the new memory row with the right outgoing count.
            let mut meta = MemoryMetadata::new_active(
                memory_id,
                writer.agent_id,
                op.context_id,
                slot,
                /* slot_version */ 1,
                op.kind,
                op.fingerprint,
                op.salience_initial,
                /* text_size */ op.text.len() as u32,
                created_at,
            );
            meta.edges_out_count = new_memory_outgoing;
            memories_t
                .insert(memory_id.to_be_bytes(), meta)
                .map_err(|e| WriterError::Internal(format!("memories insert: {e:?}")))?;
        }
        {
            let mut idem_t = wtxn
                .open_table(IDEMPOTENCY_TABLE)
                .map_err(|e| WriterError::Internal(format!("open IDEMPOTENCY_TABLE: {e:?}")))?;
            let entry = IdempotencyEntry::new(
                RESPONSE_KIND_ENCODE,
                Some(memory_id.to_be_bytes()),
                response_payload,
                request_hash,
                created_at,
            );
            idem_t
                .insert(request_id_bytes, entry)
                .map_err(|e| WriterError::Internal(format!("idempotency insert: {e:?}")))?;
        }
        wtxn.commit()
            .map_err(|e| WriterError::Internal(format!("commit: {e:?}")))?;
    }

    // ── HNSW insert (after the durability barrier). ───────────────
    writer
        .hnsw_writer
        .lock()
        .insert(memory_id, &op.vector)
        .map_err(|e| WriterError::Internal(format!("hnsw insert: {e:?}")))?;

    // ── Change-feed (sub-task 7.10). ─────────────────────────────
    writer.publish(EventEnvelope {
        lsn: 0, // stamped by bus
        event_type: EventType::Encoded,
        memory_id,
        context_id: op.context_id,
        kind: op.kind,
        salience: op.salience_initial,
        timestamp_unix_nanos: created_at,
        text: Some(op.text.clone()),
    });

    Ok(EncodeAck {
        memory_id,
        edge_results: edge_outcomes,
        replayed: false,
    })
}
