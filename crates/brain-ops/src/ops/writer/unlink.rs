//! UNLINK handler — removes the canonical edge; non-existent edge is a
//! no-op (`removed=false`), not an error. Successful unlink decrements
//! both endpoints' edge counts.

use brain_metadata::tables::edge::{self, EDGES_IN_TABLE, EDGES_OUT_TABLE};
use brain_metadata::tables::idempotency::{IdempotencyEntry, IDEMPOTENCY_TABLE};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_planner::{UnlinkAck, UnlinkOp, WriterError};

use crate::idempotency::{
    decode_unlink_payload, encode_unlink_payload, hash_unlink_request, RESPONSE_KIND_UNLINK,
};

use super::{bump_edge_count, hex_short, now_unix_nanos, RealWriterHandle};

pub(super) fn do_unlink(writer: &RealWriterHandle, op: UnlinkOp) -> Result<UnlinkAck, WriterError> {
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

    // ── Apply: edge remove + count decrement + idempotency. ───────
    let removed = {
        let mut db = writer.metadata.lock();
        let wtxn = db
            .write_txn()
            .map_err(|e| WriterError::Internal(format!("unlink write_txn: {e:?}")))?;
        let removed = {
            let mut edges_out_t = wtxn
                .open_table(EDGES_OUT_TABLE)
                .map_err(|e| WriterError::Internal(format!("unlink open EDGES_OUT: {e:?}")))?;
            let mut edges_in_t = wtxn
                .open_table(EDGES_IN_TABLE)
                .map_err(|e| WriterError::Internal(format!("unlink open EDGES_IN: {e:?}")))?;
            edge::unlink(
                &mut edges_out_t,
                &mut edges_in_t,
                op.source,
                op.kind,
                op.target,
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
