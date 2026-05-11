//! FORGET handler (sub-task 7.7 + 7.9 transactional path).

use brain_core::MemoryId;
use brain_planner::{execute_forget, plan_forget_inner, ForgetOp, ForgetOutcome};
use brain_protocol::request::ForgetRequest;
use brain_protocol::response::ForgetResponse;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::idempotency::hash_forget_request;
use crate::txn::{BufferedForget, BufferedReplay};

pub async fn handle_forget(
    req: ForgetRequest,
    ctx: &OpsContext,
) -> Result<ForgetResponse, OpError> {
    if let Some(txn_id) = req.txn_id {
        return handle_forget_in_txn(req, txn_id, ctx).await;
    }

    let memory_id_wire = req.memory_id;
    let plan = plan_forget_inner(&req, &ctx.planner_ctx)?;
    let result = execute_forget(plan, &ctx.executor).await?;

    let was_already_forgotten = matches!(
        result.outcome,
        ForgetOutcome::AlreadyTombstoned | ForgetOutcome::MemoryNotFound
    );

    Ok(ForgetResponse {
        memory_id: memory_id_wire,
        was_already_forgotten,
        edges_removed: 0,
    })
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
