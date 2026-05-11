//! LINK / UNLINK handlers (sub-task 7.8 + 7.9 transactional path).

use brain_core::{EdgeKind, MemoryId, RequestId};
use brain_planner::{LinkOp, UnlinkOp, WriterError};
use brain_protocol::request::{EdgeKindWire, LinkRequest, UnlinkRequest};
use brain_protocol::response::{LinkResponse, UnlinkResponse};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::idempotency::{hash_link_request, hash_unlink_request};
use crate::txn::{BufferedLink, BufferedReplay, BufferedUnlink};

pub async fn handle_link(req: LinkRequest, ctx: &OpsContext) -> Result<LinkResponse, OpError> {
    validate_weight(req.weight, EdgeKind::from(req.kind))?;

    if let Some(txn_id) = req.txn_id {
        return handle_link_in_txn(req, txn_id, ctx).await;
    }

    let op = LinkOp {
        request_id: RequestId::from(req.request_id),
        source: MemoryId::from(req.source),
        target: MemoryId::from(req.target),
        kind: req.kind.into(),
        weight: req.weight,
    };
    let ack = ctx
        .executor
        .writer
        .submit_link(op)
        .await
        .map_err(map_writer_err_for_link)?;
    Ok(LinkResponse {
        source: ack.source.into(),
        target: ack.target.into(),
        kind: EdgeKindWire::from(ack.kind),
        weight: ack.weight,
        created_at_unix_nanos: ack.created_at_unix_nanos,
        already_existed: ack.already_existed,
    })
}

pub async fn handle_unlink(
    req: UnlinkRequest,
    ctx: &OpsContext,
) -> Result<UnlinkResponse, OpError> {
    if let Some(txn_id) = req.txn_id {
        return handle_unlink_in_txn(req, txn_id, ctx).await;
    }
    let op = UnlinkOp {
        request_id: RequestId::from(req.request_id),
        source: MemoryId::from(req.source),
        target: MemoryId::from(req.target),
        kind: req.kind.into(),
    };
    let ack = ctx
        .executor
        .writer
        .submit_unlink(op)
        .await
        .map_err(map_writer_err_for_unlink)?;
    Ok(UnlinkResponse {
        source: ack.source.into(),
        target: ack.target.into(),
        kind: EdgeKindWire::from(ack.kind),
        removed: ack.removed,
    })
}

async fn handle_link_in_txn(
    req: LinkRequest,
    txn_id: [u8; 16],
    ctx: &OpsContext,
) -> Result<LinkResponse, OpError> {
    let _ = ctx.txn_store.validate_active(txn_id)?;

    let source = MemoryId::from(req.source);
    let target = MemoryId::from(req.target);
    let kind = EdgeKind::from(req.kind);
    let op = LinkOp {
        request_id: RequestId::from(req.request_id),
        source,
        target,
        kind,
        weight: req.weight,
    };
    let request_hash = hash_link_request(&op);

    // Replay check.
    let cached = ctx.txn_store.with_buffer(txn_id, |buf| {
        if let Some(prior) = buf.request_hashes.get(&req.request_id) {
            if prior != &request_hash {
                return Err(OpError::Conflict(
                    "link in-txn request_id replay with different params".into(),
                ));
            }
            if let Some(BufferedReplay::Link {
                source,
                target,
                kind,
                weight,
                created_at_unix_nanos,
                already_existed,
            }) = buf.request_id_cache.get(&req.request_id)
            {
                return Ok(Some(LinkResponse {
                    source: (*source).into(),
                    target: (*target).into(),
                    kind: EdgeKindWire::from(*kind),
                    weight: *weight,
                    created_at_unix_nanos: *created_at_unix_nanos,
                    already_existed: *already_existed,
                }));
            }
        }
        Ok(None)
    })?;
    if let Some(resp) = cached {
        return Ok(resp);
    }

    // Validate both endpoints (committed or pending in-buffer).
    let (src_committed, tgt_committed) = {
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard.read_txn().map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
        let table = rtxn
            .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
            .map_err(|e| {
                OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
            })?;
        let s = table.get(source.to_be_bytes()).ok().flatten().is_some();
        let t = table.get(target.to_be_bytes()).ok().flatten().is_some();
        (s, t)
    };
    let (src_pending, tgt_pending) = ctx.txn_store.with_buffer(txn_id, |buf| {
        let ids: std::collections::HashSet<MemoryId> =
            buf.encodes.iter().map(|e| e.memory_id).collect();
        Ok((ids.contains(&source), ids.contains(&target)))
    })?;
    if !(src_committed || src_pending) {
        return Err(OpError::NotFound {
            what: "memory",
            detail: format!("LINK source memory {} not found", source.raw()),
        });
    }
    if !(tgt_committed || tgt_pending) {
        return Err(OpError::NotFound {
            what: "memory",
            detail: format!("LINK target memory {} not found", target.raw()),
        });
    }

    // Detect already_existed against committed edges + earlier-in-txn links.
    let already_existed = {
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard.read_txn().map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
        let table = rtxn
            .open_table(brain_metadata::tables::edge::EDGES_OUT_TABLE)
            .map_err(|e| {
                OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
            })?;
        let key = (source.to_be_bytes(), kind as u8, target.to_be_bytes());
        let committed_has = table.get(&key).ok().flatten().is_some();
        let key_triple = (source, kind, target);
        let pending_has = ctx
            .txn_store
            .with_buffer(txn_id, |buf| {
                Ok(buf
                    .links
                    .iter()
                    .any(|l| (l.source, l.kind, l.target) == key_triple)
                    || buf.encodes.iter().any(|e| {
                        e.memory_id == source
                            && e.edges
                                .iter()
                                .any(|edge| edge.target == target && edge.kind == kind)
                    }))
            })
            .unwrap_or(false);
        // Also consider pending unlinks — if the canonical triple is
        // queued for removal, the next LINK is treated as a fresh
        // create (not an overwrite) unless the unlink is removed.
        let pending_unlinked = ctx
            .txn_store
            .with_buffer(txn_id, |buf| Ok(buf.unlinked_edges.contains(&key_triple)))
            .unwrap_or(false);
        (committed_has || pending_has) && !pending_unlinked
    };

    let created_at = crate::txn::now_unix_nanos_pub();
    let buffered = BufferedLink {
        source,
        target,
        kind,
        weight: req.weight,
        request_id: req.request_id,
        request_hash,
        created_at_unix_nanos: created_at,
    };
    ctx.txn_store.with_buffer(txn_id, |buf| {
        buf.links.push(buffered);
        // If this LINK undoes a pending UNLINK, drop the unlink mark.
        buf.unlinked_edges.remove(&(source, kind, target));
        buf.request_hashes.insert(req.request_id, request_hash);
        buf.request_id_cache.insert(
            req.request_id,
            BufferedReplay::Link {
                source,
                target,
                kind,
                weight: req.weight,
                created_at_unix_nanos: created_at,
                already_existed,
            },
        );
        Ok(())
    })?;

    Ok(LinkResponse {
        source: source.into(),
        target: target.into(),
        kind: EdgeKindWire::from(kind),
        weight: req.weight,
        created_at_unix_nanos: created_at,
        already_existed,
    })
}

async fn handle_unlink_in_txn(
    req: UnlinkRequest,
    txn_id: [u8; 16],
    ctx: &OpsContext,
) -> Result<UnlinkResponse, OpError> {
    let _ = ctx.txn_store.validate_active(txn_id)?;

    let source = MemoryId::from(req.source);
    let target = MemoryId::from(req.target);
    let kind = EdgeKind::from(req.kind);
    let op = UnlinkOp {
        request_id: RequestId::from(req.request_id),
        source,
        target,
        kind,
    };
    let request_hash = hash_unlink_request(&op);

    let cached = ctx.txn_store.with_buffer(txn_id, |buf| {
        if let Some(prior) = buf.request_hashes.get(&req.request_id) {
            if prior != &request_hash {
                return Err(OpError::Conflict(
                    "unlink in-txn request_id replay with different params".into(),
                ));
            }
            if let Some(BufferedReplay::Unlink {
                source,
                target,
                kind,
                removed,
            }) = buf.request_id_cache.get(&req.request_id)
            {
                return Ok(Some(UnlinkResponse {
                    source: (*source).into(),
                    target: (*target).into(),
                    kind: EdgeKindWire::from(*kind),
                    removed: *removed,
                }));
            }
        }
        Ok(None)
    })?;
    if let Some(resp) = cached {
        return Ok(resp);
    }

    // Decide `removed` at preview time. An edge "exists" if it's in
    // the committed `edges_out` table OR appears in any pending
    // LINK in the buffer (including inline encode-edges), and isn't
    // already queued for unlink.
    let key_triple = (source, kind, target);
    let committed_has = {
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard.read_txn().map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
        let table = rtxn
            .open_table(brain_metadata::tables::edge::EDGES_OUT_TABLE)
            .map_err(|e| {
                OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
            })?;
        let key = (source.to_be_bytes(), kind as u8, target.to_be_bytes());
        table.get(&key).ok().flatten().is_some()
    };
    let pending_has = ctx.txn_store.with_buffer(txn_id, |buf| {
        Ok(buf
            .links
            .iter()
            .any(|l| (l.source, l.kind, l.target) == key_triple)
            || buf.encodes.iter().any(|e| {
                e.memory_id == source
                    && e.edges
                        .iter()
                        .any(|edge| edge.target == target && edge.kind == kind)
            }))
    })?;
    let already_unlinked = ctx
        .txn_store
        .with_buffer(txn_id, |buf| Ok(buf.unlinked_edges.contains(&key_triple)))?;

    let removed = (committed_has || pending_has) && !already_unlinked;

    let created_at = crate::txn::now_unix_nanos_pub();
    let buffered = BufferedUnlink {
        source,
        target,
        kind,
        request_id: req.request_id,
        request_hash,
        created_at_unix_nanos: created_at,
    };
    ctx.txn_store.with_buffer(txn_id, |buf| {
        buf.unlinks.push(buffered);
        if removed {
            buf.unlinked_edges.insert(key_triple);
        }
        buf.request_hashes.insert(req.request_id, request_hash);
        buf.request_id_cache.insert(
            req.request_id,
            BufferedReplay::Unlink {
                source,
                target,
                kind,
                removed,
            },
        );
        Ok(())
    })?;

    Ok(UnlinkResponse {
        source: source.into(),
        target: target.into(),
        kind: EdgeKindWire::from(kind),
        removed,
    })
}

fn validate_weight(weight: f32, kind: EdgeKind) -> Result<(), OpError> {
    let (lo, hi) = if matches!(kind, EdgeKind::Contradicts) {
        (-1.0_f32, 1.0_f32)
    } else {
        (0.0_f32, 1.0_f32)
    };
    if !(lo..=hi).contains(&weight) || weight.is_nan() {
        return Err(OpError::InvalidRequest(format!(
            "LINK weight {weight} out of range [{lo}, {hi}] for kind {kind:?}"
        )));
    }
    Ok(())
}

fn map_writer_err_for_link(err: WriterError) -> OpError {
    match err {
        WriterError::Internal(msg) if msg.contains("not found") => OpError::NotFound {
            what: "memory",
            detail: msg,
        },
        other => OpError::ExecError(brain_planner::ExecError::WriterFailed(other)),
    }
}

fn map_writer_err_for_unlink(err: WriterError) -> OpError {
    OpError::ExecError(brain_planner::ExecError::WriterFailed(err))
}
