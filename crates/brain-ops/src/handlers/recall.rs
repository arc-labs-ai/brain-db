//! RECALL handler.
//!
//! RECALL is one verb, one code path: every request walks the same
//! plan → fan-out → fuse → filter → enrich → project pipeline. Shards
//! always wire all three retrievers (semantic + lexical + graph) at
//! spawn — there is no "substrate-only" fallback. A schema upload does
//! not gate retrieval; it only narrows what STATEMENT_CREATE /
//! RELATION_CREATE / predicate filters accept.
//!
//! In-txn reads: when the caller passes `req.txn_id`, the per-txn
//! buffer is overlaid on the committed result so RECALL inside a
//! transaction sees its own pending ENCODE writes (read-your-writes).
//! Tombstoned ids from the txn buffer are dropped from the committed
//! side before the merge.

use std::collections::HashSet;

use brain_core::{ContextId, MemoryId};
use brain_index::RankedItemId;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_planner::hybrid::executor::{
    execute as hybrid_execute, ExecutionError, HybridExecutorContext, QueryResult,
};
use brain_planner::hybrid::planner::{plan as hybrid_plan, PlanError};
use brain_planner::hybrid::router::{
    QueryRequest as PlannerQueryRequest, Retriever, RetrieverSelection,
};
use brain_protocol::envelope::request::{MemoryKindWire, RecallRequest};
use brain_protocol::envelope::response::{MemoryResult, RecallResponseFrame};
use brain_protocol::RetrieverNameWire;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::txn::BufferedEncode;

pub async fn handle_recall(
    req: RecallRequest,
    ctx: &OpsContext,
) -> Result<RecallResponseFrame, OpError> {
    // Reject obviously-invalid input up front. The hybrid planner
    // silently clamps `limit == 0` to a default, which is the wrong
    // behavior for a caller who literally asked for "zero results."
    if req.top_k == 0 {
        return Err(OpError::InvalidRequest("recall: top_k must be > 0".into()));
    }
    // Hard-fail when the client opted into rerank but the operator
    // turned it off. Returning a clear error beats silently falling
    // back to RRF — the client either drops the flag or picks a
    // shard with rerank enabled.
    if req.rerank && !ctx.cross_encoder.is_enabled() {
        return Err(OpError::CapabilityNotEnabled {
            capability: "rerank",
        });
    }
    let planner_req = build_planner_request(&req);

    let plan = hybrid_plan(&planner_req).map_err(map_plan_error)?;
    let exec_ctx = HybridExecutorContext {
        semantic: ctx.semantic_retriever.clone(),
        lexical: ctx.lexical_retriever.clone(),
        graph: ctx.graph_retriever.clone(),
        metadata: ctx.executor.metadata.clone(),
        cross_encoder: ctx.cross_encoder.as_arc().cloned(),
    };
    let result = hybrid_execute(&plan, &planner_req, &exec_ctx)
        .await
        .map_err(map_execution_error)?;

    let memory_results = project_memory_results(&result, &req, ctx)?;

    // In-txn read-your-writes: overlay the txn's pending ENCODE
    // buffer on top of the committed hybrid result. Without this,
    // an in-txn RECALL would never see writes the same transaction
    // has buffered but not yet committed.
    let memory_results = if let Some(txn_id) = req.txn_id {
        overlay_txn_buffer(memory_results, txn_id, &req, ctx)?
    } else {
        memory_results
    };

    let cumulative_count = u32::try_from(memory_results.len()).unwrap_or(u32::MAX);

    for r in &memory_results {
        ctx.access_buffer.record(MemoryId::from_raw(r.memory_id));
    }

    Ok(RecallResponseFrame {
        results: memory_results,
        is_final: true,
        cumulative_count,
        estimated_remaining: None,
    })
}

/// Merge the txn's pending writes into the committed hybrid result.
/// Drops tombstoned ids on the committed side, scores each pending
/// encode against the cue, applies the post-filters (kind, context,
/// salience, age), then re-sorts by score and trims to `top_k`.
fn overlay_txn_buffer(
    committed: Vec<MemoryResult>,
    txn_id: [u8; 16],
    req: &RecallRequest,
    ctx: &OpsContext,
) -> Result<Vec<MemoryResult>, OpError> {
    let _ = ctx.txn_store.validate_active(txn_id)?;
    let (pending, tombstoned) = ctx.txn_store.with_buffer(txn_id, |buf| {
        Ok::<_, OpError>((buf.encodes.clone(), buf.tombstoned.clone()))
    })?;

    // Drop tombstoned committed hits first — a tombstone in the
    // buffer wins over a committed row for in-txn reads.
    let mut merged: Vec<MemoryResult> = committed
        .into_iter()
        .filter(|m| !tombstoned.contains(&MemoryId::from_raw(m.memory_id)))
        .collect();

    if pending.is_empty() {
        // No buffered writes to overlay — committed result (minus
        // tombstoned) is the final answer.
        // Re-truncate just in case the tombstone filter pushed us
        // over the requested top_k boundary.
        merged.truncate(req.top_k as usize);
        return Ok(merged);
    }

    let cue_vec = ctx
        .executor
        .embedder
        .embed(&req.cue_text)
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::EmbedFailed(e)))?;

    let kind_filter: Option<HashSet<MemoryKindWire>> = req
        .kind_filter
        .as_ref()
        .map(|v| v.iter().copied().collect());
    let context_filter: Option<HashSet<u64>> = req
        .context_filter
        .as_ref()
        .map(|v| v.iter().copied().collect());

    for p in &pending {
        if tombstoned.contains(&p.memory_id) {
            continue;
        }
        let wire_kind = MemoryKindWire::from(p.kind);
        if let Some(ref kinds) = kind_filter {
            if !kinds.contains(&wire_kind) {
                continue;
            }
        }
        if let Some(ref contexts) = context_filter {
            if !contexts.contains(&p.context_id.raw()) {
                continue;
            }
        }
        if p.salience_initial < req.salience_floor {
            continue;
        }
        if let Some(bound) = req.age_bound_unix_nanos {
            if p.created_at_unix_nanos < bound {
                continue;
            }
        }
        let score = cosine(&cue_vec, &p.vector);
        if score < req.confidence_threshold {
            continue;
        }
        merged.push(pending_to_memory_result(p, req, score));
    }

    // Re-sort by similarity_score descending; pending hits are
    // exact-cosine and committed hits carry semantic_score (also
    // exact cosine) so the scale is consistent.
    merged.sort_by(|a, b| {
        b.similarity_score
            .partial_cmp(&a.similarity_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(req.top_k as usize);
    Ok(merged)
}

fn pending_to_memory_result(
    p: &BufferedEncode,
    req: &RecallRequest,
    score: f32,
) -> MemoryResult {
    MemoryResult {
        memory_id: p.memory_id.raw(),
        text: if req.include_text {
            p.text.clone()
        } else {
            String::new()
        },
        similarity_score: score,
        // Within a txn the buffered hit has no fused score (no
        // retrievers contributed); surface the same value as
        // similarity so threshold reasoning on `confidence` works
        // uniformly across both code paths.
        confidence: score,
        salience: p.salience_initial,
        kind: MemoryKindWire::from(p.kind),
        context_id: p.context_id.into(),
        created_at_unix_nanos: p.created_at_unix_nanos,
        last_accessed_at_unix_nanos: p.created_at_unix_nanos,
        // Edges and graph enrichment from buffered writes aren't
        // visible until commit — the typed-graph tables they'd
        // resolve against don't have the buffered rows yet.
        edges: if req.include_edges { Some(Vec::new()) } else { None },
        graph: None,
        contributing_retrievers: Vec::new(),
        fused_score: score,
        salience_initial: p.salience_initial,
        access_count: 0,
        // Buffered writes haven't been WAL'd yet; LSN is assigned
        // at TXN_COMMIT.
        lsn: 0,
        flags: 0,
        consolidated_at_unix_nanos: None,
        edges_out_count: 0,
        edges_in_count: 0,
    }
}

/// Cosine similarity between two equal-length f32 vectors. Both are
/// expected L2-normalised (the embedder normalises by construction);
/// no norm correction needed.
fn cosine(a: &[f32; brain_embed::VECTOR_DIM], b: &[f32; brain_embed::VECTOR_DIM]) -> f32 {
    let mut sum = 0.0_f32;
    for i in 0..brain_embed::VECTOR_DIM {
        sum += a[i] * b[i];
    }
    sum
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
) -> Result<Vec<Option<brain_protocol::envelope::response::GraphEnrichment>>, OpError> {
    use brain_core::{EntityId, StatementId, SubjectRef};
    use brain_core::{EdgeKindRef, NodeRef};
    use brain_metadata::entity::ops::entity_get;
    use brain_metadata::relation::types::relation_type_get;
    use brain_metadata::schema::predicate::predicate_get;
    use brain_metadata::statement::statement_get;
    use brain_metadata::tables::edge::{walk_incoming, walk_outgoing};
    use brain_metadata::tables::entity_type::ENTITY_TYPES_TABLE;
    use brain_metadata::tables::statement::STATEMENTS_BY_EVIDENCE_TABLE;
    use brain_protocol::envelope::response::{
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
            let mut stmts: Vec<brain_core::Statement> = Vec::new();
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
                    brain_core::StatementObject::Entity(eid) => entity_get(rtxn, *eid)
                        .ok()
                        .flatten()
                        .map(|e| e.canonical_name)
                        .unwrap_or_default(),
                    brain_core::StatementObject::Value(v) => format!("{v:?}"),
                    brain_core::StatementObject::Memory(mid) => {
                        format!("memory:{:x?}", mid.to_be_bytes())
                    }
                    brain_core::StatementObject::Statement(sid) => {
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
        // v1.0: bi-temporal as-of is server-internal only — adding it
        // to `RecallRequest` would bump the wire `RecallRequest` archive
        // shape (rkyv field order is structural, not nominal). Hybrid
        // callers route through `PlannerQueryRequest` directly.
        as_of_record_time_unix_nanos: None,
        limit: req.top_k,
        retrievers: RetrieverSelection::Auto,
        fusion_config: None,
        rerank: req.rerank,
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
        std::collections::HashMap<MemoryId, brain_protocol::envelope::response::GraphEnrichment>,
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
                        Some(brain_protocol::envelope::response::EdgeView {
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

fn map_execution_error(e: ExecutionError) -> OpError {
    match e {
        ExecutionError::Filter(inner) => OpError::Internal(format!("hybrid filter: {inner}")),
    }
}

/// Translate the planner's `Retriever` directly to the substrate
/// `RetrieverNameWire`. Avoids round-tripping through the knowledge
/// namespace's wire enum (which would require chained `From`s on
/// foreign types, an orphan-rule violation).
fn retriever_to_wire_name(r: brain_planner::hybrid::router::Retriever) -> RetrieverNameWire {
    use brain_planner::hybrid::router::Retriever as R;
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
    use brain_planner::hybrid::router::Retriever;

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
