//! LINK handler — validates both endpoints exist, inserts the canonical
//! `(source, kind, target)` edge, bumps both endpoints' counts (unless
//! the edge already existed), and stamps the idempotency row in the
//! same redb txn.

use brain_metadata::tables::edge::{
    self, derived_by, origin, zero_disambiguator, EdgeData, EdgeKey, EDGES_REVERSE_TABLE,
    EDGES_TABLE,
};
use brain_metadata::tables::idempotency::{IdempotencyEntry, IDEMPOTENCY_TABLE};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_planner::{LinkAck, LinkOp, WriterError};
use brain_storage::wal::payload::{LinkPayload as WalLinkPayload, WalPayload};
use brain_storage::wal::record::{Lsn, WalRecord};

use crate::idempotency::{
    decode_link_payload, encode_link_payload, hash_link_request, RESPONSE_KIND_LINK,
};

use super::{bump_edge_count, hex_short, now_unix_nanos, RealWriterHandle};

pub(super) async fn do_link(writer: &RealWriterHandle, op: LinkOp) -> Result<LinkAck, WriterError> {
    let request_hash = hash_link_request(&op);
    let request_id_bytes: [u8; 16] = op.request_id.into();

    // ── Idempotency lookup. ───────────────────────────────────────
    {
        let db = writer.metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| WriterError::Internal(format!("link idempotency read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(IDEMPOTENCY_TABLE)
            .map_err(|e| WriterError::Internal(format!("link open IDEMPOTENCY: {e:?}")))?;
        if let Some(access) = table
            .get(request_id_bytes)
            .map_err(|e| WriterError::Internal(format!("link idempotency get: {e:?}")))?
        {
            let prior = access.value();
            if prior.request_hash != request_hash {
                return Err(WriterError::Conflict(format!(
                    "link request_id={} hash mismatch",
                    hex_short(&request_id_bytes)
                )));
            }
            if prior.response_kind != RESPONSE_KIND_LINK {
                return Err(WriterError::Conflict(format!(
                    "link request_id={} kind mismatch",
                    hex_short(&request_id_bytes)
                )));
            }
            let (weight, created_at, already_existed) =
                decode_link_payload(&prior.response_payload)
                    .map_err(|e| WriterError::Internal(format!("decode link payload: {e}")))?;
            return Ok(LinkAck {
                source: op.source,
                target: op.target,
                kind: op.kind,
                weight,
                created_at_unix_nanos: created_at,
                already_existed,
                replayed: true,
            });
        }
    }

    // ── Validate both endpoints exist. ────────────────────────────
    let (source_exists, target_exists) = {
        let db = writer.metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| WriterError::Internal(format!("link read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| WriterError::Internal(format!("link open MEMORIES: {e:?}")))?;
        let s = table
            .get(op.source.to_be_bytes())
            .map_err(|e| WriterError::Internal(format!("link memory get: {e:?}")))?
            .is_some();
        let t = table
            .get(op.target.to_be_bytes())
            .map_err(|e| WriterError::Internal(format!("link memory get: {e:?}")))?
            .is_some();
        (s, t)
    };
    if !source_exists {
        return Err(WriterError::Internal(format!(
            "LINK source memory {} not found",
            op.source.raw()
        )));
    }
    if !target_exists {
        return Err(WriterError::Internal(format!(
            "LINK target memory {} not found",
            op.target.raw()
        )));
    }

    let created_at = now_unix_nanos();

    // ── Check whether the canonical (source, kind, target) already exists. ─
    let already_existed = {
        let db = writer.metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| WriterError::Internal(format!("link edges read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(EDGES_TABLE)
            .map_err(|e| WriterError::Internal(format!("link open EDGES: {e:?}")))?;
        let key = EdgeKey {
            from: brain_core::NodeRef::Memory(op.source),
            kind: brain_core::EdgeKindRef::Builtin(op.kind),
            to: brain_core::NodeRef::Memory(op.target),
            disambiguator: zero_disambiguator(),
        }
        .encode();
        table
            .get(key.as_slice())
            .map_err(|e| WriterError::Internal(format!("link edges get: {e:?}")))?
            .is_some()
    };

    // ── WAL append (spec §05/07 durability barrier). ─────────────
    // LINK always WALs: even an `already_existed` op writes a new
    // weight + timestamp into redb (overwrite semantics), so the
    // log must reflect it for replay determinism.
    let wal_lsn: Option<Lsn> = if let Some(sink) = &writer.wal_sink {
        let record_payload = WalPayload::Link(WalLinkPayload {
            source: brain_core::NodeRef::Memory(op.source),
            target: brain_core::NodeRef::Memory(op.target),
            edge_kind: brain_core::EdgeKindRef::Builtin(op.kind),
            weight: op.weight,
            origin: brain_core::EdgeOrigin::Explicit,
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
    let _ = wal_lsn; // LINK has no change-feed event in v1.

    // ── Apply: edge insert + count bumps + idempotency in one txn. ─
    {
        let mut db = writer.metadata.lock();
        let wtxn = db
            .write_txn()
            .map_err(|e| WriterError::Internal(format!("link write_txn: {e:?}")))?;
        {
            let mut edges_t = wtxn
                .open_table(EDGES_TABLE)
                .map_err(|e| WriterError::Internal(format!("link open EDGES: {e:?}")))?;
            let mut edges_rev_t = wtxn
                .open_table(EDGES_REVERSE_TABLE)
                .map_err(|e| WriterError::Internal(format!("link open EDGES_REVERSE: {e:?}")))?;
            let data = EdgeData::new(op.weight, origin::EXPLICIT, derived_by::CLIENT, created_at);
            edge::link(
                &mut edges_t,
                &mut edges_rev_t,
                brain_core::NodeRef::Memory(op.source),
                brain_core::EdgeKindRef::Builtin(op.kind),
                brain_core::NodeRef::Memory(op.target),
                zero_disambiguator(),
                &data,
            )
            .map_err(|e| WriterError::Internal(format!("edge::link: {e:?}")))?;
        }
        if !already_existed {
            // Bump counts on both endpoints.
            let mut memories_t = wtxn
                .open_table(MEMORIES_TABLE)
                .map_err(|e| WriterError::Internal(format!("link open MEMORIES: {e:?}")))?;
            bump_edge_count(&mut memories_t, op.source, /* out */ true, 1)?;
            bump_edge_count(&mut memories_t, op.target, /* out */ false, 1)?;
        }
        {
            let mut idem_t = wtxn
                .open_table(IDEMPOTENCY_TABLE)
                .map_err(|e| WriterError::Internal(format!("link open IDEMPOTENCY: {e:?}")))?;
            let payload = encode_link_payload(op.weight, created_at, already_existed);
            let entry = IdempotencyEntry::new(
                RESPONSE_KIND_LINK,
                None,
                payload,
                request_hash,
                created_at,
                wal_lsn.map(|l| l.raw()).unwrap_or(0),
            );
            idem_t
                .insert(request_id_bytes, entry)
                .map_err(|e| WriterError::Internal(format!("link idempotency insert: {e:?}")))?;
        }
        wtxn.commit()
            .map_err(|e| WriterError::Internal(format!("link commit: {e:?}")))?;
    }

    Ok(LinkAck {
        source: op.source,
        target: op.target,
        kind: op.kind,
        weight: op.weight,
        created_at_unix_nanos: created_at,
        already_existed,
        replayed: false,
    })
}
