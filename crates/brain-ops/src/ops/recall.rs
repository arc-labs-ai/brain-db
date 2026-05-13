//! RECALL handler (sub-task 7.4 + 7.9 transactional read-lens).
//!
//! Without `txn_id`: plan + execute against committed state.
//! With `txn_id`: execute against committed state, then layer the
//! txn buffer on top (read-your-writes). Tombstones from in-txn
//! FORGET drop matching hits; pending in-txn ENCODEs are scored
//! linearly against the cue vector and merged into the result.

use brain_core::MemoryId;
use brain_planner::{execute_recall, plan_recall_inner, RecallHit};
use brain_protocol::request::RecallRequest;
use brain_protocol::response::{MemoryResult, RecallResponseFrame};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::txn::BufferedEncode;

pub async fn handle_recall(
    req: RecallRequest,
    ctx: &OpsContext,
) -> Result<RecallResponseFrame, OpError> {
    // 1. Plan.
    let plan = plan_recall_inner(&req, &ctx.planner_ctx)?;

    // 2. If a txn is set, embed the cue once and snapshot the buffer
    //    so we can layer pending memories on top of HNSW hits + drop
    //    tombstoned ids.
    let txn_snapshot = if let Some(txn_id) = req.txn_id {
        let _ = ctx.txn_store.validate_active(txn_id)?;
        let snap = ctx.txn_store.with_buffer(txn_id, |buf| {
            Ok(TxnReadSnapshot {
                pending: buf.encodes.clone(),
                tombstoned: buf.tombstoned.clone(),
            })
        })?;
        Some(snap)
    } else {
        None
    };

    // 3. Execute committed RECALL.
    let result = execute_recall(plan, &ctx.executor).await?;

    // 4. Merge in pending-memory hits and drop tombstoned ids.
    let merged = if let Some(snap) = txn_snapshot {
        merge_with_txn(&req, result.hits, &snap, ctx)?
    } else {
        result.hits
    };

    // 5. Sort by score desc and trim to top_k.
    let mut hits = merged;
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if req.confidence_threshold > 0.0 {
        hits.retain(|h| h.score >= req.confidence_threshold);
    }
    hits.truncate(req.top_k as usize);

    // Sub-task 8.3 — record returned hits in the access-boost buffer.
    // Spec §11/02 §7, §16: every memory returned by RECALL is a
    // candidate for the next boost cycle.
    for h in &hits {
        ctx.access_buffer.record(h.memory_id);
    }

    let results: Vec<MemoryResult> = hits.into_iter().map(hit_to_wire).collect();
    let cumulative_count = u32::try_from(results.len()).unwrap_or(u32::MAX);

    Ok(RecallResponseFrame {
        results,
        is_final: true,
        cumulative_count,
        estimated_remaining: None,
    })
}

struct TxnReadSnapshot {
    pending: Vec<BufferedEncode>,
    tombstoned: std::collections::HashSet<MemoryId>,
}

fn merge_with_txn(
    req: &RecallRequest,
    committed: Vec<RecallHit>,
    snap: &TxnReadSnapshot,
    ctx: &OpsContext,
) -> Result<Vec<RecallHit>, OpError> {
    // Drop tombstoned ids from the committed side.
    let mut hits: Vec<RecallHit> = committed
        .into_iter()
        .filter(|h| !snap.tombstoned.contains(&h.memory_id))
        .collect();

    // Embed the cue once for the linear pending scan. Reuse the
    // dispatcher embed call; with a CachingDispatcher this is free
    // after the first call.
    let cue_vec = ctx
        .executor
        .embedder
        .embed(&req.cue_text)
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::EmbedFailed(e)))?;

    // Filters reused: kind, context, salience floor.
    let kind_filter = req
        .kind_filter
        .as_ref()
        .map(|v| v.iter().copied().collect::<std::collections::HashSet<_>>());
    let context_filter = req
        .context_filter
        .as_ref()
        .map(|v| v.iter().copied().collect::<std::collections::HashSet<_>>());

    for pending in &snap.pending {
        if snap.tombstoned.contains(&pending.memory_id) {
            continue;
        }
        if let Some(kinds) = &kind_filter {
            let wire_kind = brain_protocol::request::MemoryKindWire::from(pending.kind);
            if !kinds.contains(&wire_kind) {
                continue;
            }
        }
        if let Some(contexts) = &context_filter {
            if !contexts.contains(&pending.context_id.raw()) {
                continue;
            }
        }
        if pending.salience_initial < req.salience_floor {
            continue;
        }
        let score = cosine(&cue_vec, &pending.vector);
        hits.push(RecallHit {
            memory_id: pending.memory_id,
            score,
            kind: pending.kind,
            context_id: pending.context_id,
            salience: pending.salience_initial,
            created_at_unix_nanos: pending.created_at_unix_nanos,
            text: None,
        });
    }

    Ok(hits)
}

fn cosine(a: &[f32; brain_embed::VECTOR_DIM], b: &[f32; brain_embed::VECTOR_DIM]) -> f32 {
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for i in 0..brain_embed::VECTOR_DIM {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom <= 0.0 {
        0.0
    } else {
        dot / denom
    }
}

fn hit_to_wire(hit: RecallHit) -> MemoryResult {
    MemoryResult {
        memory_id: hit.memory_id.into(),
        text: hit.text.unwrap_or_default(),
        similarity_score: hit.score,
        confidence: hit.score,
        salience: hit.salience,
        kind: hit.kind.into(),
        context_id: hit.context_id.into(),
        created_at_unix_nanos: hit.created_at_unix_nanos,
        last_accessed_at_unix_nanos: hit.created_at_unix_nanos,
        vector_offset: 0,
        vector_dim: 0,
        edges: None,
    }
}
