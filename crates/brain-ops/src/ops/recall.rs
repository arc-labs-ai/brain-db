//! RECALL handler.
//!
//! Routing (spec §28/08 §5):
//!
//! - When the per-shard [`SchemaGate`] is `true` and no txn is
//!   attached, route through the hybrid query engine
//!   (phase 23.6–23.7). Memory hits are projected back onto the
//!   substrate's `MemoryResult` with `contributing_retrievers` and
//!   `fused_score` populated.
//! - Otherwise (no schema, or a transaction is in progress), run the
//!   substrate vector recall and leave the new fields empty / zero.
//!
//! Transactional read-your-writes only applies on the substrate path
//! (spec §09/08 §5). The hybrid path falls back to substrate when a
//! txn is set; full hybrid-in-txn semantics are deferred past v1.
//!
//! Substrate path notes (originally sub-task 7.4 + 7.9): plan +
//! execute against committed state, layer the txn buffer on top for
//! read-your-writes, sort, filter, truncate.

use std::collections::HashSet;

use brain_core::{ContextId, MemoryId};
use brain_index::RankedItemId;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_planner::knowledge::executor::{
    execute as hybrid_execute, ExecutionError, HybridExecutorContext, QueryResult,
};
use brain_planner::knowledge::planner::{plan as hybrid_plan, PlanError};
use brain_planner::knowledge::router::{QueryRequest as PlannerQueryRequest, RetrieverSelection};
use brain_planner::{execute_recall, plan_recall_inner, RecallHit};
use brain_protocol::request::{MemoryKindWire, RecallRequest};
use brain_protocol::response::{MemoryResult, RecallResponseFrame};
use brain_protocol::responses::types::RetrieverNameWire;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::txn::BufferedEncode;

pub async fn handle_recall(
    req: RecallRequest,
    ctx: &OpsContext,
) -> Result<RecallResponseFrame, OpError> {
    if ctx.schema_gate.is_declared() && req.txn_id.is_none() {
        match hybrid_recall(&req, ctx).await {
            Ok(HybridRecallOutcome::Frame(frame)) => return Ok(frame),
            Ok(HybridRecallOutcome::FallbackToSubstrate { retriever }) => {
                tracing::warn!(
                    target: "brain_ops::recall",
                    ?retriever,
                    "hybrid recall fell back to substrate (retriever slot empty)",
                );
                // Fall through to substrate path.
            }
            Err(e) => return Err(e),
        }
    }
    substrate_recall(req, ctx).await
}

// ---------------------------------------------------------------------------
// Substrate path (the pre-23.11 logic, refactored out unchanged).
// ---------------------------------------------------------------------------

async fn substrate_recall(
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
            // Pending (buffered) memories — no committed metadata
            // yet, so decay/access/flags are all defaults.
            salience_initial: pending.salience_initial,
            access_count: 0,
            flags: 0,
            consolidated_at_unix_nanos: None,
            edges_out_count: 0,
            edges_in_count: 0,
            last_accessed_at_unix_nanos: pending.created_at_unix_nanos,
            // Buffered ops haven't been WAL'd yet — they get an LSN
            // at TXN_COMMIT. Recall inside a txn sees them with
            // encoded_at_lsn=0 (unknown until commit).
            encoded_at_lsn: 0,
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
        last_accessed_at_unix_nanos: hit.last_accessed_at_unix_nanos,
        vector_offset: 0,
        vector_dim: 0,
        edges: None,
        // Substrate path — no hybrid metadata.
        contributing_retrievers: Vec::new(),
        fused_score: 0.0,
        salience_initial: hit.salience_initial,
        access_count: hit.access_count,
        // WAL position the row was originally encoded at. Stamped
        // by the live writer (and recovery, on replay) onto
        // `MemoryMetadata.encoded_at_lsn`. Clients chain
        // `recall → subscribe --start-lsn lsn+1` off this.
        lsn: hit.encoded_at_lsn,
        flags: hit.flags,
        consolidated_at_unix_nanos: hit.consolidated_at_unix_nanos,
        edges_out_count: hit.edges_out_count,
        edges_in_count: hit.edges_in_count,
    }
}

// ---------------------------------------------------------------------------
// Hybrid path.
// ---------------------------------------------------------------------------

/// Outcome of the hybrid path that the caller pattern-matches on.
///
/// `FallbackToSubstrate` is a typed signal that `handle_recall`
/// should re-run the substrate path. Previously this was an
/// `OpError::Internal` with a string marker; matching on a
/// stringly-typed error is fragile, so we surface the typed
/// outcome instead. Spec §28/08 §5 + §27/00 §"Idempotency
/// reminders".
enum HybridRecallOutcome {
    Frame(RecallResponseFrame),
    FallbackToSubstrate {
        retriever: brain_planner::knowledge::router::Retriever,
    },
}

async fn hybrid_recall(
    req: &RecallRequest,
    ctx: &OpsContext,
) -> Result<HybridRecallOutcome, OpError> {
    let planner_req = build_planner_request(req);

    let plan = hybrid_plan(&planner_req).map_err(map_plan_error)?;
    let exec_ctx = HybridExecutorContext {
        semantic: ctx.semantic_retriever.clone(),
        lexical: ctx.lexical_retriever.clone(),
        graph: ctx.graph_retriever.clone(),
        metadata: ctx.executor.metadata.clone(),
    };
    let result = match hybrid_execute(&plan, &planner_req, &exec_ctx) {
        Ok(r) => r,
        Err(ExecutionError::MissingRetriever(retriever)) => {
            // Typed signal — let the caller fall back to substrate.
            return Ok(HybridRecallOutcome::FallbackToSubstrate { retriever });
        }
        Err(e) => return Err(map_execution_error(e)),
    };

    let memory_results = project_memory_results(&result, req, ctx)?;
    let cumulative_count = u32::try_from(memory_results.len()).unwrap_or(u32::MAX);

    for r in &memory_results {
        ctx.access_buffer.record(MemoryId::from_raw(r.memory_id));
    }

    Ok(HybridRecallOutcome::Frame(RecallResponseFrame {
        results: memory_results,
        is_final: true,
        cumulative_count,
        estimated_remaining: None,
    }))
}

fn build_planner_request(req: &RecallRequest) -> PlannerQueryRequest {
    PlannerQueryRequest {
        text: Some(req.cue_text.clone()),
        entity_anchor: None,
        // RECALL doesn't filter by statement kind; the hybrid
        // planner uses an empty filter to mean "any kind". Substrate
        // post-filters (kind / context / salience) re-apply below.
        kind_filter: Vec::new(),
        predicate_filter: Vec::new(),
        time_filter: None,
        confidence_min: if req.confidence_threshold > 0.0 {
            Some(req.confidence_threshold)
        } else {
            None
        },
        include_tombstoned: false,
        include_superseded: false,
        limit: req.top_k,
        retrievers: RetrieverSelection::Auto,
        fusion_config: None,
    }
}

fn project_memory_results(
    result: &QueryResult,
    req: &RecallRequest,
    ctx: &OpsContext,
) -> Result<Vec<MemoryResult>, OpError> {
    // Pre-extract substrate post-filters from the request — the
    // fused list is small (≤ planner top_n), so we iterate once
    // collecting only Memory hits.
    let kind_filter: Option<HashSet<MemoryKindWire>> = req
        .kind_filter
        .as_ref()
        .map(|v| v.iter().copied().collect());
    let context_filter: Option<HashSet<u64>> = req
        .context_filter
        .as_ref()
        .map(|v| v.iter().copied().collect());

    let metadata_guard = ctx.executor.metadata.lock();
    let rtxn = metadata_guard
        .read_txn()
        .map_err(|e| OpError::Internal(format!("hybrid recall read_txn: {e}")))?;
    let table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| OpError::Internal(format!("hybrid recall open MEMORIES_TABLE: {e}")))?;
    // Opening the texts table costs a redb seek; only do it when the
    // caller asked for text, so the common ids-only path stays cheap.
    // A shard that hasn't received an encode yet won't have a texts
    // table — treat that as "no texts available" rather than 500.
    let texts_table = if req.include_text {
        match rtxn.open_table(TEXTS_TABLE) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => {
                return Err(OpError::Internal(format!(
                    "hybrid recall open TEXTS_TABLE: {e}"
                )));
            }
        }
    } else {
        None
    };

    let mut out: Vec<MemoryResult> = Vec::with_capacity(result.items.len());
    for fused in &result.items {
        let RankedItemId::Memory(memory_id) = fused.id else {
            continue;
        };

        let row = match table.get(&memory_id.to_be_bytes()) {
            Ok(Some(guard)) => guard.value(),
            Ok(None) => continue, // Tombstoned between fusion and projection — drop.
            Err(e) => {
                return Err(OpError::Internal(format!(
                    "hybrid recall MEMORIES_TABLE get: {e}",
                )));
            }
        };

        if row.is_tombstoned() {
            continue;
        }

        let kind = match row.kind() {
            Ok(k) => k,
            Err(_) => continue,
        };
        let wire_kind: MemoryKindWire = kind.into();
        if let Some(allowed) = &kind_filter {
            if !allowed.contains(&wire_kind) {
                continue;
            }
        }
        if let Some(allowed) = &context_filter {
            if !allowed.contains(&row.context().raw()) {
                continue;
            }
        }
        if row.salience < req.salience_floor {
            continue;
        }
        if let Some(bound) = req.age_bound_unix_nanos {
            if row.created_at_unix_nanos < bound {
                continue;
            }
        }

        let text = if let Some(texts) = texts_table.as_ref() {
            match texts.get(&memory_id.to_be_bytes()) {
                Ok(Some(guard)) => std::str::from_utf8(guard.value())
                    .map(str::to_owned)
                    .map_err(|e| {
                        OpError::Internal(format!(
                            "hybrid recall TEXTS_TABLE non-UTF-8 for {memory_id:?}: {e}",
                        ))
                    })?,
                Ok(None) => String::new(),
                Err(e) => {
                    return Err(OpError::Internal(format!(
                        "hybrid recall TEXTS_TABLE get: {e}",
                    )));
                }
            }
        } else {
            String::new()
        };

        out.push(MemoryResult {
            memory_id: memory_id.raw(),
            text,
            similarity_score: fused.fused_score as f32,
            confidence: fused.fused_score as f32,
            salience: row.salience,
            kind: wire_kind,
            context_id: ContextId(row.context_id).into(),
            created_at_unix_nanos: row.created_at_unix_nanos,
            last_accessed_at_unix_nanos: row.last_accessed_at_unix_nanos,
            vector_offset: 0,
            vector_dim: 0,
            edges: None,
            contributing_retrievers: fused
                .contributing
                .iter()
                .map(|c| retriever_to_wire_name(c.retriever))
                .collect(),
            fused_score: fused.fused_score as f32,
            salience_initial: row.salience_initial,
            access_count: row.access_count,
            // WAL position the row was originally encoded at.
            lsn: row.encoded_at_lsn,
            flags: row.flags,
            consolidated_at_unix_nanos: row.consolidated_at_unix_nanos,
            edges_out_count: row.edges_out_count,
            edges_in_count: row.edges_in_count,
        });

        if out.len() == req.top_k as usize {
            break;
        }
    }

    Ok(out)
}

fn map_plan_error(e: PlanError) -> OpError {
    match e {
        PlanError::NoSignal => {
            // RECALL always provides cue_text, so this branch is
            // unreachable in practice. Still: surface a clear error
            // rather than panicking.
            OpError::InvalidRequest("recall: cue produced no retrievable signal".into())
        }
    }
}

/// Maps every `ExecutionError` variant **except** `MissingRetriever`,
/// which `hybrid_recall` intercepts and turns into a typed fallback
/// signal (see `HybridRecallOutcome::FallbackToSubstrate`). If we
/// ever see `MissingRetriever` here it means a refactor missed the
/// short-circuit upstream; we still map it to an `Internal` so the
/// caller doesn't get a panic, but the pattern-matched fallback in
/// `hybrid_recall` should always catch it first.
fn map_execution_error(e: ExecutionError) -> OpError {
    match e {
        ExecutionError::MissingRetriever(r) => OpError::Internal(format!(
            "hybrid retriever slot empty for {r:?} — should have been intercepted by hybrid_recall",
        )),
        ExecutionError::Filter(inner) => OpError::Internal(format!("hybrid filter: {inner}")),
    }
}

/// Translate the planner's `Retriever` directly to the substrate
/// `RetrieverNameWire`. Avoids round-tripping through the knowledge
/// namespace's wire enum (which would require chained `From`s on
/// foreign types, an orphan-rule violation).
fn retriever_to_wire_name(r: brain_planner::knowledge::router::Retriever) -> RetrieverNameWire {
    use brain_planner::knowledge::router::Retriever as R;
    match r {
        R::Semantic => RetrieverNameWire::Semantic,
        R::Lexical => RetrieverNameWire::Lexical,
        R::Graph => RetrieverNameWire::Graph,
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_planner::knowledge::router::Retriever;

    #[test]
    fn retriever_to_wire_name_matches_each_variant() {
        assert_eq!(
            retriever_to_wire_name(Retriever::Semantic),
            RetrieverNameWire::Semantic
        );
        assert_eq!(
            retriever_to_wire_name(Retriever::Lexical),
            RetrieverNameWire::Lexical
        );
        assert_eq!(
            retriever_to_wire_name(Retriever::Graph),
            RetrieverNameWire::Graph
        );
    }
}
