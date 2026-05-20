//! RECALL handler.
//!
//! RECALL is one verb with one server-side routing rule:
//!
//! - `txn_id` present → substrate path. Transactional
//!   read-your-writes requires the per-txn buffer overlay, and the
//!   lexical + graph retrievers only see committed state, so they
//!   can't honour a pending write.
//! - otherwise → hybrid (semantic + lexical + graph fused via RRF).
//!
//! Hybrid is the default for every deployment. A schema upload does
//! not gate retrieval — it only narrows what STATEMENT_CREATE /
//! RELATION_CREATE / predicate filters accept. The substrate code
//! path stays internal so transactional recalls keep working, but it
//! is not selectable from the wire.

use std::collections::HashSet;

use brain_core::{ContextId, MemoryId};
use brain_index::RankedItemId;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_planner::knowledge::executor::{
    execute as hybrid_execute, ExecutionError, HybridExecutorContext, QueryResult,
};
use brain_planner::knowledge::planner::{plan as hybrid_plan, PlanError};
use brain_planner::knowledge::router::{
    QueryRequest as PlannerQueryRequest, Retriever, RetrieverSelection,
};
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
    if req.txn_id.is_some() {
        return substrate_recall(req, ctx).await;
    }
    // Cold-start posture: a context with zero retrievers wired is
    // either a unit-test fixture that bypasses shard spawn, or a
    // shard whose tantivy + HNSW slots haven't been populated yet.
    // The substrate path is the only thing it can serve. Production
    // shards wire all three at spawn; reaching the hybrid path with
    // any individual slot missing is still a real internal error
    // (see `map_execution_error`).
    if ctx.semantic_retriever.is_none()
        && ctx.lexical_retriever.is_none()
        && ctx.graph_retriever.is_none()
    {
        return substrate_recall(req, ctx).await;
    }
    let HybridRecallOutcome::Frame(frame) = hybrid_recall(&req, ctx).await?;
    Ok(frame)
}

// ---------------------------------------------------------------------------
// Substrate path. Reachable only from inside this module (transactional
// recalls). Never selectable from the wire.
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

    // Every memory returned by RECALL is a candidate for the next
    // access-boost cycle.
    for h in &hits {
        ctx.access_buffer.record(h.memory_id);
    }

    // When include_edges is set, fetch outgoing builtin edges for each
    // surviving hit. Mentions / typed-relation edges (knowledge layer)
    // are filtered out — they're answered by separate ops (entity_get,
    // relation_list), not by the RECALL response. One redb read txn
    // serves every hit.
    let edges_per_hit = if req.include_edges {
        Some(fetch_outgoing_edges_for(&hits, ctx)?)
    } else {
        None
    };
    // When include_graph is set, fetch knowledge-layer enrichment
    // (mentioned entities / sourced statements / incident typed
    // relations) per hit. Independent of include_edges — both can be
    // set; both share the metadata lock but each opens its own read
    // txn for now (cheap; future optimisation can fold them).
    let graph_per_hit = if req.include_graph {
        let ids: Vec<MemoryId> = hits.iter().map(|h| h.memory_id).collect();
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard.read_txn().map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
        Some(fetch_enrichment_for(&ids, &rtxn)?)
    } else {
        None
    };

    let results: Vec<MemoryResult> = hits
        .into_iter()
        .enumerate()
        .map(|(i, h)| {
            let edges = edges_per_hit.as_ref().and_then(|v| v.get(i).cloned());
            // graph_per_hit is Vec<Option<GraphEnrichment>>; index → Option<&Option<…>>;
            // flatten the outer Option (i is in range by construction).
            let graph = graph_per_hit
                .as_ref()
                .and_then(|v| v.get(i).cloned())
                .flatten();
            hit_to_wire(h, edges, graph)
        })
        .collect();
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

/// For each `RecallHit`, walk the unified edge table prefix-scanned at
/// (NodeRef::Memory(hit.memory_id), *, *) and project to the wire
/// `EdgeView` shape. One redb read transaction serves every hit so the
/// cost is amortised. Only `EdgeKindRef::Builtin` edges are included —
/// memory→entity (`Mentions`) and typed relations belong to the
/// knowledge-layer ops, not to RECALL.
fn fetch_outgoing_edges_for(
    hits: &[brain_planner::RecallHit],
    ctx: &OpsContext,
) -> Result<Vec<Vec<brain_protocol::response::EdgeView>>, OpError> {
    use brain_core::NodeRef;
    use brain_metadata::tables::edge::walk_outgoing;

    let db_guard = ctx.executor.metadata.lock();
    let rtxn = db_guard.read_txn().map_err(|e| {
        OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
    })?;
    let mut out: Vec<Vec<brain_protocol::response::EdgeView>> = Vec::with_capacity(hits.len());
    for hit in hits {
        let rows = walk_outgoing(&rtxn, NodeRef::Memory(hit.memory_id), None).map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
        let edges: Vec<brain_protocol::response::EdgeView> = rows
            .into_iter()
            .filter_map(|(kind, to, _disamb, data)| {
                // Only builtin substrate edges (Caused / FollowedBy /
                // DerivedFrom / SimilarTo / Contradicts / Supports /
                // References / PartOf) surface in RECALL. Mentions and
                // typed relations are knowledge-layer surface.
                let builtin = match kind {
                    brain_core::EdgeKindRef::Builtin(k) => k,
                    _ => return None,
                };
                let target = match to {
                    NodeRef::Memory(mid) => mid,
                    _ => return None,
                };
                Some(brain_protocol::response::EdgeView {
                    target: target.into(),
                    kind: builtin.into(),
                    weight: data.weight,
                })
            })
            .collect();
        out.push(edges);
    }
    Ok(out)
}

/// Per-hit knowledge-layer enrichment populated when the request
/// carries `include_graph = true`. One redb read txn serves all
/// hits; per hit we issue a small handful of point/range reads.
/// Schema-gating is by table presence + edge presence: if
/// `STATEMENTS_BY_EVIDENCE_TABLE` doesn't exist AND the hit has no
/// `Mentions` edges, the result is `None` (memory wasn't through
/// extractors). Otherwise the lists may be empty — "extracted, found
/// nothing" is a distinct state from "not extracted."
///
/// Caps:
///   * entities  — first 16 mentioned (mention order)
///   * statements — top 5 by `confidence` desc, tombstoned skipped
///   * relations  — top 5 by `created_at_unix_nanos` desc, both
///     incoming and outgoing typed edges incident to mentioned
///     entities
fn fetch_enrichment_for(
    memory_ids: &[MemoryId],
    rtxn: &redb::ReadTransaction,
) -> Result<Vec<Option<brain_protocol::response::GraphEnrichment>>, OpError> {
    use brain_core::knowledge::{EntityId, StatementId, SubjectRef};
    use brain_core::{EdgeKindRef, NodeRef};
    use brain_metadata::entity_ops::entity_get;
    use brain_metadata::predicate_ops::predicate_get;
    use brain_metadata::relation_type_ops::relation_type_get;
    use brain_metadata::statement_ops::statement_get;
    use brain_metadata::tables::edge::{walk_incoming, walk_outgoing};
    use brain_metadata::tables::knowledge::entity_type::ENTITY_TYPES_TABLE;
    use brain_metadata::tables::knowledge::statement::STATEMENTS_BY_EVIDENCE_TABLE;
    use brain_protocol::response::{
        EnrichedEntity, EnrichedRelation, EnrichedStatement, GraphEnrichment,
    };

    const ENTITY_CAP: usize = 16;
    const STATEMENT_CAP: usize = 5;
    const RELATION_CAP: usize = 5;
    let entity_types = rtxn.open_table(ENTITY_TYPES_TABLE).ok();
    let evidence_table = match rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE) {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => {
            return Err(OpError::Internal(format!(
                "include_graph: open STATEMENTS_BY_EVIDENCE_TABLE: {e}"
            )));
        }
    };

    let mut out: Vec<Option<GraphEnrichment>> = Vec::with_capacity(memory_ids.len());
    for &memory_id in memory_ids {
        // 1. Mentioned entities (walk Mentions edges from memory).
        let mention_rows = walk_outgoing(
            rtxn,
            NodeRef::Memory(memory_id),
            Some(EdgeKindRef::Mentions),
        )
        .map_err(|e| OpError::Internal(format!("include_graph: walk_outgoing(Mentions): {e}")))?;
        let entity_ids: Vec<EntityId> = mention_rows
            .iter()
            .filter_map(|(_, to, _, _)| match to {
                NodeRef::Entity(eid) => Some(*eid),
                _ => None,
            })
            .collect();

        // Hard schema gate: no mentions AND no statements infra → the
        // memory never went through extractors. Distinguish from
        // "extracted but no entities" (Some(empty)) below.
        if entity_ids.is_empty() && evidence_table.is_none() {
            out.push(None);
            continue;
        }

        let mut enriched_entities: Vec<EnrichedEntity> =
            Vec::with_capacity(entity_ids.len().min(ENTITY_CAP));
        for eid in entity_ids.iter().take(ENTITY_CAP) {
            let Some(ent) = entity_get(rtxn, *eid)
                .map_err(|e| OpError::Internal(format!("include_graph: entity_get: {e}")))?
            else {
                continue;
            };
            let type_name = entity_types
                .as_ref()
                .and_then(|t| t.get(&ent.entity_type.raw()).ok().flatten())
                .map(|g| g.value().name)
                .unwrap_or_default();
            enriched_entities.push(EnrichedEntity {
                id: eid.to_bytes(),
                name: ent.canonical_name,
                type_qname: type_name,
            });
        }

        // 2. Statements sourced by this memory. STATEMENTS_BY_EVIDENCE
        // keys are `(MemoryId.to_be_bytes(), StatementId.to_bytes())`.
        let mut enriched_statements: Vec<EnrichedStatement> = Vec::new();
        if let Some(et) = &evidence_table {
            let mid = memory_id.to_be_bytes();
            let lo = (mid, [0u8; 16]);
            let hi = (mid, [0xFFu8; 16]);
            let mut stmts: Vec<brain_core::knowledge::Statement> = Vec::new();
            for entry in et
                .range(lo..=hi)
                .map_err(|e| OpError::Internal(format!("include_graph: evidence range: {e}")))?
            {
                let (k, _v) = entry
                    .map_err(|e| OpError::Internal(format!("include_graph: evidence row: {e}")))?;
                let (_mem_bytes, sid_bytes) = k.value();
                let sid = StatementId::from_bytes(sid_bytes);
                if let Some(stmt) = statement_get(rtxn, sid)
                    .map_err(|e| OpError::Internal(format!("include_graph: statement_get: {e}")))?
                {
                    if !stmt.tombstoned {
                        stmts.push(stmt);
                    }
                }
            }
            stmts.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for stmt in stmts.into_iter().take(STATEMENT_CAP) {
                let subject_name = match stmt.subject {
                    SubjectRef::Entity(eid) => entity_get(rtxn, eid)
                        .ok()
                        .flatten()
                        .map(|e| e.canonical_name)
                        .unwrap_or_default(),
                    _ => "(ambiguous)".to_string(),
                };
                let predicate = predicate_get(rtxn, stmt.predicate)
                    .ok()
                    .flatten()
                    .map(|p| p.canonical())
                    .unwrap_or_default();
                let object_label = match &stmt.object {
                    brain_core::knowledge::StatementObject::Entity(eid) => entity_get(rtxn, *eid)
                        .ok()
                        .flatten()
                        .map(|e| e.canonical_name)
                        .unwrap_or_default(),
                    brain_core::knowledge::StatementObject::Value(v) => format!("{v:?}"),
                    brain_core::knowledge::StatementObject::Memory(mid) => {
                        format!("memory:{:x?}", mid.to_be_bytes())
                    }
                    brain_core::knowledge::StatementObject::Statement(sid) => {
                        format!("statement:{:x?}", sid.to_bytes())
                    }
                };
                enriched_statements.push(EnrichedStatement {
                    id: stmt.id.to_bytes(),
                    subject_name,
                    predicate,
                    object_label,
                    confidence: stmt.confidence,
                });
            }
        }

        // 3. Typed relations incident to any mentioned entity. Both
        // directions; top RELATION_CAP by created_at desc across the
        // pool.
        let mut all_rels: Vec<(u64, EnrichedRelation)> = Vec::new();
        for eid in &entity_ids {
            for outgoing in [true, false] {
                let rows = if outgoing {
                    walk_outgoing(rtxn, NodeRef::Entity(*eid), None)
                } else {
                    walk_incoming(rtxn, NodeRef::Entity(*eid), None)
                }
                .map_err(|e| OpError::Internal(format!("include_graph: walk relation: {e}")))?;
                for (kind, other, _disamb, data) in rows {
                    let typed_id = match kind {
                        EdgeKindRef::Typed(rt_id) => rt_id,
                        _ => continue,
                    };
                    let other_entity = match other {
                        NodeRef::Entity(oid) => oid,
                        _ => continue,
                    };
                    let Some(rt) = relation_type_get(rtxn, typed_id).map_err(|e| {
                        OpError::Internal(format!("include_graph: relation_type_get: {e}"))
                    })?
                    else {
                        continue;
                    };
                    let (from_id, to_id) = if outgoing {
                        (*eid, other_entity)
                    } else {
                        (other_entity, *eid)
                    };
                    let from_name = entity_get(rtxn, from_id)
                        .ok()
                        .flatten()
                        .map(|e| e.canonical_name)
                        .unwrap_or_default();
                    let to_name = entity_get(rtxn, to_id)
                        .ok()
                        .flatten()
                        .map(|e| e.canonical_name)
                        .unwrap_or_default();
                    all_rels.push((
                        data.created_at_unix_nanos,
                        EnrichedRelation {
                            from_name,
                            predicate: rt.canonical(),
                            to_name,
                        },
                    ));
                }
            }
        }
        all_rels.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        let enriched_relations: Vec<EnrichedRelation> = all_rels
            .into_iter()
            .take(RELATION_CAP)
            .map(|(_, r)| r)
            .collect();

        out.push(Some(GraphEnrichment {
            entities: enriched_entities,
            statements: enriched_statements,
            relations: enriched_relations,
        }));
    }
    Ok(out)
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

fn hit_to_wire(
    hit: RecallHit,
    edges: Option<Vec<brain_protocol::response::EdgeView>>,
    graph: Option<brain_protocol::response::GraphEnrichment>,
) -> MemoryResult {
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
        edges,
        graph,
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

/// Outcome of the hybrid path. Single-variant today; kept as an enum
/// because every other production result-shape on this hot path is an
/// enum, and a future "deferred to background" outcome (the planned
/// async-fusion mode) lands here without churning the call site.
enum HybridRecallOutcome {
    Frame(RecallResponseFrame),
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
    let result = hybrid_execute(&plan, &planner_req, &exec_ctx).map_err(map_execution_error)?;

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

    // Pre-fetch knowledge-layer enrichment in one pass if requested.
    // The hybrid path holds metadata_guard for the whole loop, so the
    // helper takes the existing rtxn (no double-lock).
    let graph_per_memory: Option<
        std::collections::HashMap<MemoryId, brain_protocol::response::GraphEnrichment>,
    > = if req.include_graph {
        let ids: Vec<MemoryId> = result
            .items
            .iter()
            .filter_map(|fused| match fused.id {
                RankedItemId::Memory(mid) => Some(mid),
                _ => None,
            })
            .collect();
        let enriched = fetch_enrichment_for(&ids, &rtxn)?;
        Some(
            ids.into_iter()
                .zip(enriched)
                .filter_map(|(id, e)| e.map(|g| (id, g)))
                .collect(),
        )
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

        // similarity_score on the hybrid path is the semantic
        // retriever's raw cosine — the same quantity the substrate
        // path returns in this field. This keeps the field's meaning
        // stable across paths so the client-side cluster-warning
        // heuristic and any user-facing threshold reasoning don't
        // need to know which path produced the row. If the semantic
        // retriever didn't contribute (lexical-only or graph-only
        // hit), report 0.0 — the contributing_retrievers list tells
        // the renderer which retrievers actually ran.
        let semantic_score = fused
            .contributing
            .iter()
            .find(|c| matches!(c.retriever, Retriever::Semantic))
            .map(|c| c.raw_score)
            .unwrap_or(0.0);
        // Per-hit outgoing-edge projection — only builtin substrate
        // edges. Knowledge-layer edges (Mentions / Typed) belong to
        // entity/relation ops, not RECALL. The rtxn opened above
        // serves every hit; one prefix scan per memory.
        let edges = if req.include_edges {
            use brain_core::NodeRef;
            let rows = brain_metadata::tables::edge::walk_outgoing(
                &rtxn,
                NodeRef::Memory(memory_id),
                None,
            )
            .map_err(|e| OpError::Internal(format!("hybrid recall walk_outgoing: {e}")))?;
            Some(
                rows.into_iter()
                    .filter_map(|(kind, to, _disamb, data)| {
                        let builtin = match kind {
                            brain_core::EdgeKindRef::Builtin(k) => k,
                            _ => return None,
                        };
                        let target = match to {
                            NodeRef::Memory(mid) => mid,
                            _ => return None,
                        };
                        Some(brain_protocol::response::EdgeView {
                            target: target.into(),
                            kind: builtin.into(),
                            weight: data.weight,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        out.push(MemoryResult {
            memory_id: memory_id.raw(),
            text,
            similarity_score: semantic_score,
            // `confidence` carries the score the --confidence threshold
            // is compared against. On hybrid that's the RRF-fused
            // score (matching the server-side filter in
            // brain-planner::recall). On substrate it equals
            // similarity_score (set in hit_to_wire).
            confidence: fused.fused_score as f32,
            salience: row.salience,
            kind: wire_kind,
            context_id: ContextId(row.context_id).into(),
            created_at_unix_nanos: row.created_at_unix_nanos,
            last_accessed_at_unix_nanos: row.last_accessed_at_unix_nanos,
            edges,
            graph: graph_per_memory
                .as_ref()
                .and_then(|m| m.get(&memory_id).cloned()),
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

/// A retriever slot being empty on a shard that accepted a RECALL is
/// a real internal error: shard spawn is responsible for wiring every
/// required retriever, and a recall reaching the handler means spawn
/// succeeded. If we see `MissingRetriever` here, somebody downgraded
/// a sink to `None` after spawn — flag it loud rather than silently
/// degrading.
fn map_execution_error(e: ExecutionError) -> OpError {
    match e {
        ExecutionError::MissingRetriever(r) => OpError::Internal(format!(
            "hybrid retriever slot empty for {r:?} after shard spawn",
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
