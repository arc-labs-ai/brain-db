//! UNLINK handler — removes the canonical edge; non-existent edge is a
//! no-op (`removed=false`), not an error. Successful unlink decrements
//! both endpoints' edge counts.

use brain_metadata::tables::edge::{self, zero_disambiguator, EDGES_REVERSE_TABLE, EDGES_TABLE};
use brain_metadata::tables::idempotency::{IdempotencyEntry, IDEMPOTENCY_TABLE};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_planner::{UnlinkAck, UnlinkOp, WriterError};
use brain_storage::wal::payload::{UnlinkPayload as WalUnlinkPayload, WalPayload};
use brain_storage::wal::record::{Lsn, WalRecord};

use crate::idempotency::{
    decode_unlink_payload, encode_unlink_payload, hash_unlink_request, RESPONSE_KIND_UNLINK,
};

use super::{bump_edge_count, hex_short, now_unix_nanos, RealWriterHandle};

pub(super) async fn do_unlink(
    writer: &RealWriterHandle,
    op: UnlinkOp,
) -> Result<UnlinkAck, WriterError> {
    let request_hash = hash_unlink_request(&op);
    let request_id_bytes: [u8; 16] = op.request_id.into();

    // ── Idempotency lookup. ───────────────────────────────────────
    {
        let db = writer.metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| WriterError::Internal(format!("unlink idempotency read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(IDEMPOTENCY_TABLE)
            .map_err(|e| WriterError::Internal(format!("unlink open IDEMPOTENCY: {e:?}")))?;
        if let Some(access) = table
            .get(request_id_bytes)
            .map_err(|e| WriterError::Internal(format!("unlink idempotency get: {e:?}")))?
        {
            let prior = access.value();
            if prior.request_hash != request_hash {
                return Err(WriterError::Conflict(format!(
                    "unlink request_id={} hash mismatch",
                    hex_short(&request_id_bytes)
                )));
            }
            if prior.response_kind != RESPONSE_KIND_UNLINK {
                return Err(WriterError::Conflict(format!(
                    "unlink request_id={} kind mismatch",
                    hex_short(&request_id_bytes)
                )));
            }
            let removed = decode_unlink_payload(&prior.response_payload)
                .map_err(|e| WriterError::Internal(format!("decode unlink payload: {e}")))?;
            return Ok(UnlinkAck {
                source: op.source,
                target: op.target,
                kind: op.kind,
                removed,
                replayed: true,
            });
        }
    }

    let created_at = now_unix_nanos();

    // ── WAL append BEFORE the redb txn. The recovery path's
    // apply_unlink is idempotent (missing edge = no-op) so we WAL
    // unconditionally; whether the edge actually got removed lives
    // in the response payload, not the WAL record.
    let wal_lsn: Option<Lsn> = if let Some(sink) = &writer.wal_sink {
        let record_payload = WalPayload::Unlink(WalUnlinkPayload {
            source: brain_core::NodeRef::Memory(op.source),
            target: brain_core::NodeRef::Memory(op.target),
            edge_kind: brain_core::EdgeKindRef::Builtin(op.kind),
            edge_seq: 0,
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
    };
    let _ = wal_lsn; // UNLINK has no change-feed event in v1.

    // ── Apply: edge remove + count decrement + idempotency. ───────
    let removed = {
        let mut db = writer.metadata.lock();
        let wtxn = db
            .write_txn()
            .map_err(|e| WriterError::Internal(format!("unlink write_txn: {e:?}")))?;
        let removed = {
            let mut edges_t = wtxn
                .open_table(EDGES_TABLE)
                .map_err(|e| WriterError::Internal(format!("unlink open EDGES: {e:?}")))?;
            let mut edges_rev_t = wtxn
                .open_table(EDGES_REVERSE_TABLE)
                .map_err(|e| WriterError::Internal(format!("unlink open EDGES_REVERSE: {e:?}")))?;
            edge::unlink(
                &mut edges_t,
                &mut edges_rev_t,
                brain_core::NodeRef::Memory(op.source),
                brain_core::EdgeKindRef::Builtin(op.kind),
                brain_core::NodeRef::Memory(op.target),
                zero_disambiguator(),
            )
            .map_err(|e| WriterError::Internal(format!("edge::unlink: {e:?}")))?
        };
        if removed {
            let mut memories_t = wtxn
                .open_table(MEMORIES_TABLE)
                .map_err(|e| WriterError::Internal(format!("unlink open MEMORIES: {e:?}")))?;
            bump_edge_count(&mut memories_t, op.source, /* out */ true, -1)?;
            bump_edge_count(&mut memories_t, op.target, /* out */ false, -1)?;
        }
        {
            let mut idem_t = wtxn
                .open_table(IDEMPOTENCY_TABLE)
                .map_err(|e| WriterError::Internal(format!("unlink open IDEMPOTENCY: {e:?}")))?;
            let payload = encode_unlink_payload(removed);
            let entry = IdempotencyEntry::new(
                RESPONSE_KIND_UNLINK,
                None,
                payload,
                request_hash,
                created_at,
                wal_lsn.map(|l| l.raw()).unwrap_or(0),
            );
            idem_t
                .insert(request_id_bytes, entry)
                .map_err(|e| WriterError::Internal(format!("unlink idempotency insert: {e:?}")))?;
        }
        wtxn.commit()
            .map_err(|e| WriterError::Internal(format!("unlink commit: {e:?}")))?;
        removed
    };

    Ok(UnlinkAck {
        source: op.source,
        target: op.target,
        kind: op.kind,
        removed,
        replayed: false,
    })
}
