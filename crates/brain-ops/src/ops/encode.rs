//! ENCODE handler (sub-task 7.3 + 7.9 transactional path).
//!
//! Without `txn_id`: plan + execute + wire the response immediately.
//! With `txn_id`: validate the txn is Active, embed + reserve a
//! `MemoryId`, push to the buffer, return a preview response. The
//! actual redb + HNSW writes happen at TXN_COMMIT time.

use brain_core::{ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_metadata::tables::memory::MemoryMetadata;
use brain_planner::{execute_encode, plan_encode_inner, EdgeOutcome};
use brain_protocol::request::EncodeRequest;
use brain_protocol::response::EncodeResponse;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::idempotency::hash_encode_request;
use crate::txn::{BufferedEdgeSpec, BufferedEncode, BufferedReplay};

pub async fn handle_encode(
    req: EncodeRequest,
    ctx: &OpsContext,
) -> Result<EncodeResponse, OpError> {
    if let Some(txn_id) = req.txn_id {
        return handle_encode_in_txn(req, txn_id, ctx).await;
    }

    // Non-txn path: plan → execute → wire.
    let plan = plan_encode_inner(&req, &ctx.planner_ctx)?;
    let salience = plan.wal_append.salience_initial;
    let result = execute_encode(plan, &ctx.executor).await?;
    let auto_edges_added = result
        .edge_results
        .iter()
        .filter(|o| matches!(o, EdgeOutcome::Inserted))
        .count() as u32;

    Ok(EncodeResponse {
        memory_id: result.memory_id.into(),
        was_deduplicated: result.replayed,
        salience,
        auto_edges_added,
    })
}

async fn handle_encode_in_txn(
    req: EncodeRequest,
    txn_id: [u8; 16],
    ctx: &OpsContext,
) -> Result<EncodeResponse, OpError> {
    // 1. Validate via the planner first — same input checks the non-
    //    txn path runs (text length, salience range, edge cap, kind).
    let plan = plan_encode_inner(&req, &ctx.planner_ctx)?;
    let salience = plan.wal_append.salience_initial;
    let _ = plan;

    // 2. Validate the txn is Active.
    let _ = ctx.txn_store.validate_active(txn_id)?;

    // 3. Build an EncodeOp shape for hashing (matches the non-txn
    //    idempotency hash so a cross-txn replay surfaces conflicts).
    let request_hash = {
        let op = brain_planner::EncodeOp {
            request_id: brain_core::RequestId::from(req.request_id),
            context_id: ContextId::from(req.context_id),
            kind: MemoryKind::from(req.kind),
            text: req.text.clone(),
            vector: [0.0; brain_embed::VECTOR_DIM],
            salience_initial: req.salience_hint,
            fingerprint: ctx.executor.embedder.fingerprint(),
            edges: req
                .edges
                .iter()
                .map(|e| brain_planner::EncodeOpEdge {
                    target: MemoryId::from(e.target),
                    kind: EdgeKind::from(e.kind),
                    weight: e.weight,
                })
                .collect(),
        };
        hash_encode_request(&op)
    };

    // 4. Intra-txn replay check.
    let replay = ctx.txn_store.with_buffer(txn_id, |buf| {
        if let Some(prior_hash) = buf.request_hashes.get(&req.request_id) {
            if prior_hash != &request_hash {
                return Err(OpError::Conflict(format!(
                    "encode in-txn request_id replay with different params: txn={}",
                    hex_short(&txn_id)
                )));
            }
            // Same request → return cached preview.
            if let Some(BufferedReplay::Encode {
                memory_id,
                edge_outcomes,
            }) = buf.request_id_cache.get(&req.request_id)
            {
                let auto = edge_outcomes
                    .iter()
                    .filter(|o| matches!(o, EdgeOutcome::Inserted))
                    .count() as u32;
                return Ok(Some((*memory_id, auto)));
            }
        }
        Ok(None)
    })?;
    if let Some((memory_id, auto_edges_added)) = replay {
        return Ok(EncodeResponse {
            memory_id: memory_id.into(),
            was_deduplicated: true,
            salience,
            auto_edges_added,
        });
    }

    // 5. Embed.
    let vector = ctx
        .executor
        .embedder
        .embed(&req.text)
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::EmbedFailed(e)))?;

    // 6. Reserve a MemoryId from the writer.
    let memory_id = ctx
        .executor
        .writer
        .reserve_memory_id()
        .await
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::WriterFailed(e)))?;
    let created_at = crate::txn::now_unix_nanos_pub();

    // 7. Compute edge outcomes against committed + in-buffer memories.
    //    The metadata read uses a fresh redb read txn; pending
    //    memories are checked against the buffer.
    let edge_outcomes: Vec<EdgeOutcome> = {
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard.read_txn().map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
        let mems_table = rtxn
            .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
            .map_err(|e| {
                OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
            })?;

        let pending_ids: std::collections::HashSet<MemoryId> =
            ctx.txn_store.with_buffer(txn_id, |buf| {
                Ok(buf.encodes.iter().map(|e| e.memory_id).collect())
            })?;

        req.edges
            .iter()
            .map(|edge| {
                let target = MemoryId::from(edge.target);
                let committed = mems_table
                    .get(target.to_be_bytes())
                    .map(|opt| opt.is_some())
                    .unwrap_or(false);
                if committed || pending_ids.contains(&target) {
                    EdgeOutcome::Inserted
                } else {
                    EdgeOutcome::TargetMissing
                }
            })
            .collect()
    };

    let auto_edges_added = edge_outcomes
        .iter()
        .filter(|o| matches!(o, EdgeOutcome::Inserted))
        .count() as u32;

    // 8. Build the BufferedEncode and push.
    let metadata = MemoryMetadata::new_active(
        memory_id,
        brain_core::AgentId(uuid::Uuid::nil()),
        ContextId::from(req.context_id),
        memory_id.slot(),
        1,
        MemoryKind::from(req.kind),
        ctx.executor.embedder.fingerprint(),
        salience,
        req.text.len() as u32,
        created_at,
    );

    let buffered = BufferedEncode {
        memory_id,
        metadata,
        text: req.text.clone(),
        vector,
        edges: req
            .edges
            .iter()
            .zip(edge_outcomes.iter())
            .filter_map(|(e, o)| {
                if matches!(o, EdgeOutcome::Inserted) {
                    Some(BufferedEdgeSpec {
                        target: MemoryId::from(e.target),
                        kind: EdgeKind::from(e.kind),
                        weight: e.weight,
                    })
                } else {
                    None
                }
            })
            .collect(),
        kind: MemoryKind::from(req.kind),
        context_id: ContextId::from(req.context_id),
        salience_initial: salience,
        fingerprint: ctx.executor.embedder.fingerprint(),
        request_id: req.request_id,
        request_hash,
        created_at_unix_nanos: created_at,
    };

    ctx.txn_store.with_buffer(txn_id, |buf| {
        buf.encodes.push(buffered);
        buf.request_hashes.insert(req.request_id, request_hash);
        buf.request_id_cache.insert(
            req.request_id,
            BufferedReplay::Encode {
                memory_id,
                edge_outcomes: edge_outcomes.clone(),
            },
        );
        Ok(())
    })?;

    Ok(EncodeResponse {
        memory_id: memory_id.into(),
        was_deduplicated: false,
        salience,
        auto_edges_added,
    })
}

fn hex_short(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(8);
    for b in &bytes[..4] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
