//! FORGET handler (sub-task 7.7 + 7.9 transactional path).

use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::MemoryId;
use brain_planner::{plan_forget_inner, ForgetOp, ForgetOutcome};
use brain_protocol::request::{ForgetMode, ForgetRequest};
use brain_protocol::response::ForgetResponse;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::idempotency::hash_forget_request;
use crate::ops::link::downcast_writer_pub;
use crate::ops::text_indexer::MemoryTextOp;
use crate::txn::{BufferedForget, BufferedReplay};
use crate::write::phase::TombstoneMode as PhaseTombstoneMode;
use crate::write::{Phase, PhaseAck, TombstoneTarget, Write, WriteId};

pub async fn handle_forget(
    req: ForgetRequest,
    ctx: &OpsContext,
) -> Result<ForgetResponse, OpError> {
    if let Some(txn_id) = req.txn_id {
        return handle_forget_in_txn(req, txn_id, ctx).await;
    }

    let memory_id_wire = req.memory_id;
    // Validate input (mode range etc.) via the existing planner check.
    let _ = plan_forget_inner(&req, &ctx.planner_ctx)?;

    let memory_id = MemoryId::from(memory_id_wire);

    // Peek MEMORIES_TABLE: distinguish MemoryNotFound /
    // AlreadyTombstoned / Tombstoned. apply_tombstone_memory returns
    // NotFound for missing rows; legacy wire semantic treats that as
    // a successful no-op with was_already_forgotten=true. So we
    // branch pre-submit rather than swallowing the apply error.
    let outcome = peek_forget_outcome(ctx, memory_id)?;
    let was_already_forgotten = matches!(
        outcome,
        ForgetOutcome::AlreadyTombstoned | ForgetOutcome::MemoryNotFound
    );

    // Drop the lexical-index row when we'll actually tombstone.
    // Matches the legacy gate.
    if matches!(outcome, ForgetOutcome::Tombstoned) {
        if let Some(dispatcher) = ctx.memory_text_dispatcher.as_ref() {
            dispatcher
                .dispatch(MemoryTextOp::Forget { id: memory_id })
                .await;
        }

        // Only submit a Write when the memory actually exists active.
        // AlreadyTombstoned + MemoryNotFound are wire-level no-ops.
        let real_writer = downcast_writer_pub(ctx)?;
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Memory {
                id: memory_id,
                mode: map_mode(req.mode),
            },
            reason: 1, // ClientRequest
            at_unix_nanos: now_unix_nanos(),
        };
        let write = Write::single(
            WriteId::from_request(brain_core::RequestId::from(req.request_id)),
            ctx.executor.caller_agent,
            phase,
        );
        let ack = real_writer
            .submit(write)
            .await
            .map_err(|e| OpError::ExecError(brain_planner::ExecError::WriterFailed(e)))?;
        debug_assert!(matches!(ack.single_phase(), PhaseAck::Tombstoned(_)));
    }

    Ok(ForgetResponse {
        memory_id: memory_id_wire,
        was_already_forgotten,
        edges_removed: 0,
    })
}

/// Read MEMORIES_TABLE to classify the FORGET outcome before submit.
/// Returns `MemoryNotFound` when no row exists, `AlreadyTombstoned`
/// when the row is present but inactive, `Tombstoned` for the
/// "actually do the work" case.
fn peek_forget_outcome(ctx: &OpsContext, id: MemoryId) -> Result<ForgetOutcome, OpError> {
    let db_guard = ctx.executor.metadata.lock();
    let rtxn = db_guard.read_txn().map_err(|e| {
        OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
    })?;
    let t = rtxn
        .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
        .map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
    let Some(guard) = t.get(id.to_be_bytes()).ok().flatten() else {
        return Ok(ForgetOutcome::MemoryNotFound);
    };
    let row = guard.value();
    if row.flags & brain_metadata::tables::memory::flags::ACTIVE == 0 {
        Ok(ForgetOutcome::AlreadyTombstoned)
    } else {
        Ok(ForgetOutcome::Tombstoned)
    }
}

fn map_mode(mode: ForgetMode) -> PhaseTombstoneMode {
    match mode {
        ForgetMode::Soft => PhaseTombstoneMode::Soft,
        ForgetMode::Hard => PhaseTombstoneMode::Hard,
    }
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

async fn handle_forget_in_txn(
    req: ForgetRequest,
    txn_id: [u8; 16],
    ctx: &OpsContext,
) -> Result<ForgetResponse, OpError> {
    // Validate input via planner (kind range etc.).
    let plan = plan_forget_inner(&req, &ctx.planner_ctx)?;
    let _ = plan;

    let _ = ctx.txn_store.validate_active(txn_id)?;

    let memory_id = MemoryId::from(req.memory_id);
    let request_hash = hash_forget_request(&ForgetOp {
        request_id: brain_core::RequestId::from(req.request_id),
        memory_id,
        mode: req.mode,
        agent_id: ctx.executor.caller_agent,
    });

    // Replay check.
    let cached = ctx.txn_store.with_buffer(txn_id, |buf| {
        if let Some(prior_hash) = buf.request_hashes.get(&req.request_id) {
            if prior_hash != &request_hash {
                return Err(OpError::Conflict(
                    "forget in-txn request_id replay with different params".into(),
                ));
            }
            if let Some(BufferedReplay::Forget {
                memory_id: cached_mid,
                outcome,
            }) = buf.request_id_cache.get(&req.request_id)
            {
                return Ok(Some((*cached_mid, *outcome)));
            }
        }
        Ok(None)
    })?;
    if let Some((cached_mid, outcome)) = cached {
        return Ok(ForgetResponse {
            memory_id: cached_mid.raw(),
            was_already_forgotten: matches!(
                outcome,
                ForgetOutcome::AlreadyTombstoned | ForgetOutcome::MemoryNotFound
            ),
            edges_removed: 0,
        });
    }

    // Decide the outcome at preview time: look up the memory in
    // committed state OR pending in-buffer.
    let outcome = {
        // Committed?
        let committed = {
            let db_guard = ctx.executor.metadata.lock();
            let rtxn = db_guard.read_txn().map_err(|e| {
                OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
            })?;
            let table = rtxn
                .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
                .map_err(|e| {
                    OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
                })?;
            table.get(memory_id.to_be_bytes()).ok().flatten().is_some()
        };

        let (pending, tombstoned) = ctx.txn_store.with_buffer(txn_id, |buf| {
            let pending = buf.encodes.iter().any(|e| e.memory_id == memory_id);
            let tombstoned = buf.tombstoned.contains(&memory_id);
            Ok((pending, tombstoned))
        })?;

        if tombstoned {
            ForgetOutcome::AlreadyTombstoned
        } else if committed || pending {
            ForgetOutcome::Tombstoned
        } else {
            ForgetOutcome::MemoryNotFound
        }
    };

    let buffered = BufferedForget {
        memory_id,
        mode: req.mode,
        request_id: req.request_id,
        request_hash,
        created_at_unix_nanos: crate::txn::now_unix_nanos_pub(),
        agent_id: ctx.executor.caller_agent,
    };
    ctx.txn_store.with_buffer(txn_id, |buf| {
        buf.forgets.push(buffered);
        if matches!(outcome, ForgetOutcome::Tombstoned) {
            buf.tombstoned.insert(memory_id);
        }
        buf.request_hashes.insert(req.request_id, request_hash);
        buf.request_id_cache.insert(
            req.request_id,
            BufferedReplay::Forget { memory_id, outcome },
        );
        Ok(())
    })?;

    Ok(ForgetResponse {
        memory_id: req.memory_id,
        was_already_forgotten: matches!(
            outcome,
            ForgetOutcome::AlreadyTombstoned | ForgetOutcome::MemoryNotFound
        ),
        edges_removed: 0,
    })
}
