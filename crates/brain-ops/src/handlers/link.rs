//! LINK / UNLINK handlers — submit Link / Unlink phases through the
//! unified writer; in-TXN ops buffer for later commit.

use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{EdgeKind, EdgeKindRef, MemoryId, NodeRef, RequestId};
use brain_planner::{LinkOp, UnlinkOp, WriterError};
use brain_protocol::envelope::request::{EdgeKindWire, LinkRequest, UnlinkRequest};
use brain_protocol::envelope::response::{LinkResponse, UnlinkResponse};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::state::idempotency::{hash_link_request, hash_unlink_request};
use crate::txn::{BufferedLink, BufferedReplay, BufferedUnlink};
use crate::write::{Phase, PhaseAck, Write, WriteId};
use crate::writer::RealWriterHandle;

pub async fn handle_link(req: LinkRequest, ctx: &OpsContext) -> Result<LinkResponse, OpError> {
    validate_weight(req.weight, EdgeKind::from(req.kind))?;

    if let Some(txn_id) = req.txn_id {
        return handle_link_in_txn(req, txn_id, ctx).await;
    }

    let source = MemoryId::from(req.source);
    let target = MemoryId::from(req.target);
    let kind = EdgeKind::from(req.kind);

    let real_writer = downcast_writer(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_link_request(&LinkOp {
        request_id: RequestId::from(req.request_id),
        source,
        target,
        kind,
        weight: req.weight,
        agent_id: ctx.executor.caller_agent,
    });

    // Idempotency replay short-circuit + conflict detection.
    match real_writer.idempotency_lookup(write_id, Some(request_hash)) {
        crate::writer::submit::CacheLookup::Hit(_cached) => {
            // Idempotency replay: mirror the original
            // outcome transparently. The original was a successful
            // insert, so `already_existed=false` — same shape as the
            // first response. No fresh write — counts don't bump again
            // because submit returns early before apply_link runs.
            return Ok(LinkResponse {
                source: source.into(),
                target: target.into(),
                kind: EdgeKindWire::from(kind),
                weight: req.weight,
                created_at_unix_nanos: 0,
                already_existed: false,
            });
        }
        crate::writer::submit::CacheLookup::Conflict => {
            return Err(map_writer_err_for_link(WriterError::Conflict(
                "link request_id replay with different params".into(),
            )));
        }
        crate::writer::submit::CacheLookup::Miss => {}
    }

    // Validate endpoints + compute already_existed in one rtxn. The
    // wire response needs `already_existed` to distinguish a fresh
    // edge from an overwrite, and the submit ack doesn't surface it.
    let (src_exists, tgt_exists, already_existed) = peek_link_state(ctx, source, target, kind)?;
    if !src_exists {
        return Err(OpError::NotFound {
            what: "memory",
            detail: format!("LINK source memory {} not found", source.raw()),
        });
    }
    if !tgt_exists {
        return Err(OpError::NotFound {
            what: "memory",
            detail: format!("LINK target memory {} not found", target.raw()),
        });
    }

    let created_at = now_unix_nanos();
    let phase = Phase::Link {
        from: NodeRef::Memory(source),
        to: NodeRef::Memory(target),
        kind: EdgeKindRef::Builtin(kind),
        weight: req.weight,
        origin: brain_metadata::tables::edge::origin::EXPLICIT,
        derived_by: brain_metadata::tables::edge::derived_by::CLIENT,
        disambiguator: brain_metadata::tables::edge::zero_disambiguator(),
        created_at_unix_nanos: created_at,
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
    let ack = real_writer
        .submit(write)
        .await
        .map_err(map_writer_err_for_link)?;

    // Project the single PhaseAck back into the wire shape.
    debug_assert!(matches!(ack.single_phase(), PhaseAck::Linked));
    Ok(LinkResponse {
        source: source.into(),
        target: target.into(),
        kind: EdgeKindWire::from(kind),
        weight: req.weight,
        created_at_unix_nanos: created_at,
        already_existed,
    })
}

/// Read MEMORIES_TABLE for source + target existence and EDGES_TABLE
/// for the (source, kind, target) tuple. Returns
/// `(src_exists, tgt_exists, already_existed)` in one rtxn. Failure
/// of any of these reads becomes `OpError::ExecError(MetadataReadFailed)`.
fn peek_link_state(
    ctx: &OpsContext,
    source: MemoryId,
    target: MemoryId,
    kind: EdgeKind,
) -> Result<(bool, bool, bool), OpError> {
    let db_guard = ctx.executor.metadata.lock();
    let rtxn = db_guard.read_txn().map_err(|e| {
        OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
    })?;
    let mem_t = rtxn
        .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
        .map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
    let src_exists = mem_t.get(source.to_be_bytes()).ok().flatten().is_some();
    let tgt_exists = mem_t.get(target.to_be_bytes()).ok().flatten().is_some();
    drop(mem_t);

    let edges_t = rtxn
        .open_table(brain_metadata::tables::edge::EDGES_TABLE)
        .map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
    let key = brain_metadata::tables::edge::EdgeKey {
        from: NodeRef::Memory(source),
        kind: EdgeKindRef::Builtin(kind),
        to: NodeRef::Memory(target),
        disambiguator: brain_metadata::tables::edge::zero_disambiguator(),
    }
    .encode();
    let already_existed = edges_t.get(key.as_slice()).ok().flatten().is_some();

    Ok((src_exists, tgt_exists, already_existed))
}

/// Downcast `ctx.executor.writer` to the concrete [`RealWriterHandle`]
/// so we can call its `submit(Write)` method. Wire handlers that
/// migrate to the unified path go through this helper.
fn downcast_writer(ctx: &OpsContext) -> Result<&RealWriterHandle, OpError> {
    downcast_writer_pub(ctx)
}

/// pub(crate) version of [`downcast_writer`] for other handler
/// modules to use as they migrate to submit(Write).
pub(crate) fn downcast_writer_pub(ctx: &OpsContext) -> Result<&RealWriterHandle, OpError> {
    ctx.executor
        .writer
        .as_any()
        .downcast_ref::<RealWriterHandle>()
        .ok_or_else(|| OpError::Internal("unified path requires RealWriterHandle".into()))
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub async fn handle_unlink(
    req: UnlinkRequest,
    ctx: &OpsContext,
) -> Result<UnlinkResponse, OpError> {
    if let Some(txn_id) = req.txn_id {
        return handle_unlink_in_txn(req, txn_id, ctx).await;
    }
    let source = MemoryId::from(req.source);
    let target = MemoryId::from(req.target);
    let kind = EdgeKind::from(req.kind);

    let real_writer = downcast_writer(ctx)?;
    let write_id = WriteId::from_request(RequestId::from(req.request_id));
    let request_hash = hash_unlink_request(&UnlinkOp {
        request_id: RequestId::from(req.request_id),
        source,
        target,
        kind,
        agent_id: ctx.executor.caller_agent,
    });

    match real_writer.idempotency_lookup(write_id, Some(request_hash)) {
        crate::writer::submit::CacheLookup::Hit(_cached) => {
            // Replay mirrors the original outcome. The first call had
            // an existing edge to remove (otherwise it would have been
            // a no-op and stamped removed=false; we only stamp the
            // cache after the wtxn commits, so the first call's ack
            // existing means it actually removed something).
            return Ok(UnlinkResponse {
                source: source.into(),
                target: target.into(),
                kind: EdgeKindWire::from(kind),
                removed: true,
            });
        }
        crate::writer::submit::CacheLookup::Conflict => {
            return Err(map_writer_err_for_unlink(WriterError::Conflict(
                "unlink request_id replay with different params".into(),
            )));
        }
        crate::writer::submit::CacheLookup::Miss => {}
    }

    // Peek the edge to compute `removed`: the wire response needs to
    // distinguish a real delete from a no-op, and apply_unlink doesn't
    // surface it on its phase ack.
    let removed = peek_edge_exists(ctx, source, target, kind)?;

    let phase = Phase::Unlink {
        from: NodeRef::Memory(source),
        to: NodeRef::Memory(target),
        kind: EdgeKindRef::Builtin(kind),
        disambiguator: brain_metadata::tables::edge::zero_disambiguator(),
    };
    let write =
        Write::single(write_id, ctx.executor.caller_agent, phase).with_request_hash(request_hash);
    let ack = real_writer
        .submit(write)
        .await
        .map_err(map_writer_err_for_unlink)?;
    debug_assert!(matches!(ack.single_phase(), PhaseAck::Unlinked));
    Ok(UnlinkResponse {
        source: source.into(),
        target: target.into(),
        kind: EdgeKindWire::from(kind),
        removed,
    })
}

/// Read EDGES_TABLE for the (source, kind, target) tuple. Returns
/// `true` if the canonical forward row exists. Used by handle_unlink
/// to compute the `removed` wire flag pre-submit.
fn peek_edge_exists(
    ctx: &OpsContext,
    source: MemoryId,
    target: MemoryId,
    kind: EdgeKind,
) -> Result<bool, OpError> {
    let db_guard = ctx.executor.metadata.lock();
    let rtxn = db_guard.read_txn().map_err(|e| {
        OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
    })?;
    let edges_t = rtxn
        .open_table(brain_metadata::tables::edge::EDGES_TABLE)
        .map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
    let key = brain_metadata::tables::edge::EdgeKey {
        from: NodeRef::Memory(source),
        kind: EdgeKindRef::Builtin(kind),
        to: NodeRef::Memory(target),
        disambiguator: brain_metadata::tables::edge::zero_disambiguator(),
    }
    .encode();
    Ok(edges_t.get(key.as_slice()).ok().flatten().is_some())
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
        agent_id: ctx.executor.caller_agent,
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
            .open_table(brain_metadata::tables::edge::EDGES_TABLE)
            .map_err(|e| {
                OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
            })?;
        let key = brain_metadata::tables::edge::EdgeKey {
            from: brain_core::NodeRef::Memory(source),
            kind: brain_core::EdgeKindRef::Builtin(kind),
            to: brain_core::NodeRef::Memory(target),
            disambiguator: brain_metadata::tables::edge::zero_disambiguator(),
        }
        .encode();
        let committed_has = table.get(key.as_slice()).ok().flatten().is_some();
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
        agent_id: ctx.executor.caller_agent,
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
        agent_id: ctx.executor.caller_agent,
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
            .open_table(brain_metadata::tables::edge::EDGES_TABLE)
            .map_err(|e| {
                OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
            })?;
        let key = brain_metadata::tables::edge::EdgeKey {
            from: brain_core::NodeRef::Memory(source),
            kind: brain_core::EdgeKindRef::Builtin(kind),
            to: brain_core::NodeRef::Memory(target),
            disambiguator: brain_metadata::tables::edge::zero_disambiguator(),
        }
        .encode();
        table.get(key.as_slice()).ok().flatten().is_some()
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
        agent_id: ctx.executor.caller_agent,
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
