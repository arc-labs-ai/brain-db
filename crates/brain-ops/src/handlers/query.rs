//! Retrieval query introspection handlers.
//!
//! Wire entry points for the two retrieval-query introspection opcodes:
//!
//! - `QueryExplain`  (0x0161 / 0x01E1) — plan only; return rendered plan text.
//! - `QueryTrace`    (0x0162 / 0x01E2) — plan + execute; return rendered
//!   plan-with-execution text.
//!
//! Each handler does three things:
//!
//! 1. Translate the embedded wire query into the planner's
//!    `brain_planner::retrieval::router::QueryRequest`.
//! 2. Call `plan(&req)` and (for TRACE) `execute(...)`.
//! 3. Render the plan / trace text back onto a wire response.
//!
//! The handler reuses the per-shard retriever slots already
//! installed on [`OpsContext`] (semantic / lexical / graph) and
//! the shared `MetadataDb`.

use brain_core::StatementKind;
use brain_core::{EntityId, PredicateId};
use brain_metadata::schema::predicate::predicate_lookup_by_qname;
use brain_metadata::schema::store::schema_active;
use brain_planner::retrieval::executor::{execute, ExecutionError, RetrievalExecutorContext};
use brain_planner::retrieval::explain::{render_plan, render_trace};
use brain_planner::retrieval::planner::{plan, PlanError};
use brain_planner::retrieval::router::{
    FusionConfig, PerRetrieverWeights, QueryRequest as PlannerQueryRequest, Retriever,
    RetrieverSelection, TimeRange,
};
use brain_protocol::{
    FusionConfigWire, QueryExplainRequest, QueryExplainResponse, QueryRequest as WireQueryRequest,
    QueryTraceRequest, QueryTraceResponse, RetrieverSelectionWire, RetrieverWire, TimeRangeWire,
};

use crate::context::OpsContext;
use crate::error::OpError;

// ---------------------------------------------------------------------------
// Limits.
// ---------------------------------------------------------------------------

/// Max bytes of `text` accepted at handler entry. Mirrors RECALL's
/// existing cue-text bound; keeps a single rkyv decode from
/// amplifying into a huge backing string.
pub const MAX_QUERY_TEXT_BYTES: usize = 16 * 1024;

/// Max entries in `RetrieverSelectionWire::Explicit(_)`. Matches
/// the router's `MAX_RETRIEVERS = 3`.
pub const MAX_EXPLICIT_RETRIEVERS: usize = 3;

/// Max entries in a `predicate_filter`. Each entry costs a registry
/// lookup, so cap the count to bound per-request I/O against a crafted
/// payload.
pub const MAX_PREDICATE_FILTER: usize = 256;

/// Max entries in a `kind_filter`. Statement kinds are a small fixed
/// enum, so any honest filter is tiny; the cap rejects a crafted
/// oversized list with a clear error rather than mapping every byte.
pub const MAX_KIND_FILTER: usize = 256;

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

/// Outcome of resolving a `Vec<String>` predicate filter against the
/// registry: either we got the requested PredicateIds (possibly an
/// empty vector when the schemaless caller named no predicates we
/// know yet) or we short-circuited with an empty response because at
/// least one qname is unknown in schemaless mode.
enum PredicateResolution {
    Ok(Vec<PredicateId>),
    EmptyResultSet,
}

/// EXPLAIN — plan only, return rendered plan text.
pub async fn handle_query_explain(
    req: QueryExplainRequest,
    ctx: &OpsContext,
) -> Result<QueryExplainResponse, OpError> {
    validate_text_length(&req.query.text)?;
    let predicate_ids = match resolve_predicate_filter(&req.query.predicate_filter, ctx)? {
        PredicateResolution::Ok(v) => v,
        // For EXPLAIN we still want to produce a plan even when the
        // filter would zero out — explain the plan the planner would
        // build with no predicate constraint applied.
        PredicateResolution::EmptyResultSet => Vec::new(),
    };
    let planner_req = wire_to_planner_request(req.query, predicate_ids, ctx)?;
    let qp = plan(&planner_req).map_err(map_plan_error)?;
    Ok(QueryExplainResponse {
        plan_text: render_plan(&qp),
        estimated_cost_ms: qp.estimated_cost_ms,
    })
}

/// TRACE — plan + execute, return rendered plan-with-execution text.
pub async fn handle_query_trace(
    req: QueryTraceRequest,
    ctx: &OpsContext,
) -> Result<QueryTraceResponse, OpError> {
    validate_text_length(&req.query.text)?;
    let predicate_ids = match resolve_predicate_filter(&req.query.predicate_filter, ctx)? {
        PredicateResolution::Ok(v) => v,
        PredicateResolution::EmptyResultSet => {
            return Ok(QueryTraceResponse {
                trace_text: "PLAN: skipped (predicate filter contains an unknown qname; \
                             schemaless mode short-circuits to empty result set)"
                    .into(),
                total_latency_ms: 0.0,
            });
        }
    };
    let planner_req = wire_to_planner_request(req.query, predicate_ids, ctx)?;
    let qp = plan(&planner_req).map_err(map_plan_error)?;
    let exec_ctx = build_executor_context(ctx)?;
    // TRACE executes the full retrieval, statement corpus included.
    // `trace_detail = false`: QUERY_TRACE renders text from the always-on
    // count/latency fields already on `QueryMetadata`; it doesn't build a
    // wire `RecallTrace`, so it has no use for the opt-in per-item detail.
    let result = execute(&qp, &planner_req, true, false, &exec_ctx)
        .await
        .map_err(map_executor_error)?;
    Ok(QueryTraceResponse {
        trace_text: render_trace(&qp, &result.metadata),
        total_latency_ms: result.metadata.total_latency_ms,
    })
}

/// Resolve a wire `Vec<String>` predicate filter (canonical qnames)
/// to the planner's `Vec<PredicateId>`. Behavior:
///
/// - Empty input → empty output. No DB hit.
/// - Each qname is validated for `"namespace:name"` shape.
/// - Unknown qname in schemaless mode → return
///   [`PredicateResolution::EmptyResultSet`] so the handler can
///   short-circuit (no matching rows are possible).
/// - Unknown qname in schema-strict mode → `PredicateNotInSchema`.
fn resolve_predicate_filter(
    qnames: &[String],
    ctx: &OpsContext,
) -> Result<PredicateResolution, OpError> {
    if qnames.is_empty() {
        return Ok(PredicateResolution::Ok(Vec::new()));
    }
    if qnames.len() > MAX_PREDICATE_FILTER {
        return Err(OpError::InvalidRequest(format!(
            "predicate_filter has {} entries; max is {MAX_PREDICATE_FILTER}",
            qnames.len()
        )));
    }
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
    let mut out = Vec::with_capacity(qnames.len());
    for q in qnames {
        if q.is_empty() || !q.contains(':') {
            return Err(OpError::InvalidRequest(format!(
                "predicate filter qname {q:?} must be \"namespace:name\""
            )));
        }
        let (ns, name) = q
            .split_once(':')
            .ok_or_else(|| OpError::InvalidRequest("predicate qname missing ':'".into()))?;
        let active_version = schema_active(&rtxn, ns)
            .map_err(|e| OpError::Internal(format!("schema_active: {e}")))?;
        match predicate_lookup_by_qname(&rtxn, ns, name)
            .map_err(|e| OpError::InvalidRequest(format!("predicate lookup ({q:?}): {e}")))?
        {
            Some(p) => out.push(p.id),
            None => {
                if let Some(version) = active_version {
                    return Err(OpError::PredicateNotInSchema {
                        predicate: q.clone(),
                        namespace: ns.to_string(),
                        version,
                    });
                }
                return Ok(PredicateResolution::EmptyResultSet);
            }
        }
    }
    Ok(PredicateResolution::Ok(out))
}

// ---------------------------------------------------------------------------
// Validation / translation helpers.
// ---------------------------------------------------------------------------

fn validate_text_length(text: &str) -> Result<(), OpError> {
    if text.len() > MAX_QUERY_TEXT_BYTES {
        return Err(OpError::InvalidRequest(format!(
            "query text exceeds {MAX_QUERY_TEXT_BYTES} bytes",
        )));
    }
    Ok(())
}

fn wire_to_planner_request(
    req: WireQueryRequest,
    predicate_filter: Vec<PredicateId>,
    ctx: &OpsContext,
) -> Result<PlannerQueryRequest, OpError> {
    let text = if req.text.is_empty() {
        None
    } else {
        Some(req.text)
    };
    let entity_anchor = req.entity_anchor.map(EntityId::from_bytes);
    if req.kind_filter.len() > MAX_KIND_FILTER {
        return Err(OpError::InvalidRequest(format!(
            "kind_filter has {} entries; max is {MAX_KIND_FILTER}",
            req.kind_filter.len()
        )));
    }
    let kind_filter = req
        .kind_filter
        .iter()
        .copied()
        .map(statement_kind_from_byte)
        .collect::<Result<Vec<_>, OpError>>()?;
    let time_filter = req.time_filter.map(time_range_from_wire);
    let retrievers = retriever_selection_from_wire(req.retrievers)?;
    let fusion_config = req.fusion_config.map(fusion_config_from_wire);
    Ok(PlannerQueryRequest {
        text,
        entity_anchor,
        kind_filter,
        predicate_filter,
        time_filter,
        // Wire-level QUERY does not yet expose a context filter — the
        // funnel will pick it up once the wire shape gains the field.
        context_filter: Vec::new(),
        // Strict per-space isolation: every row belongs to exactly one space,
        // so QUERY is scoped to the caller's own space (from the key), never
        // widened by a client field.
        space_filter: vec![ctx.executor.caller_space],
        confidence_min: req.confidence_min,
        include_tombstoned: req.include_tombstoned,
        include_superseded: req.include_superseded,
        as_of_record_time_unix_nanos: req.as_of_record_time_unix_nanos,
        limit: req.limit,
        retrievers,
        fusion_config,
    })
}

fn statement_kind_from_byte(b: u8) -> Result<StatementKind, OpError> {
    // 0-based kind byte (matches `StatementKind::as_u8`). Every byte is a
    // valid kind now (builtin 0..=5, else Custom).
    Ok(StatementKind::from_u8(b))
}

fn time_range_from_wire(w: TimeRangeWire) -> TimeRange {
    TimeRange {
        from_unix_ms: w.from_unix_ms,
        to_unix_ms: w.to_unix_ms,
    }
}

fn retriever_selection_from_wire(w: RetrieverSelectionWire) -> Result<RetrieverSelection, OpError> {
    match w {
        RetrieverSelectionWire::Auto => Ok(RetrieverSelection::Auto),
        RetrieverSelectionWire::Explicit(list) => {
            if list.len() > MAX_EXPLICIT_RETRIEVERS {
                return Err(OpError::InvalidRequest(format!(
                    "explicit retriever list exceeds {MAX_EXPLICIT_RETRIEVERS} entries",
                )));
            }
            let list = list.into_iter().map(retriever_from_wire).collect();
            Ok(RetrieverSelection::Explicit(list))
        }
    }
}

fn retriever_from_wire(w: RetrieverWire) -> Retriever {
    match w {
        RetrieverWire::Semantic => Retriever::Semantic,
        RetrieverWire::Lexical => Retriever::Lexical,
        RetrieverWire::Graph => Retriever::Graph,
    }
}

fn fusion_config_from_wire(w: FusionConfigWire) -> FusionConfig {
    FusionConfig {
        k: w.k,
        weights: PerRetrieverWeights {
            semantic: w.semantic_weight,
            lexical: w.lexical_weight,
            graph: w.graph_weight,
            temporal: 0.5,
        },
    }
}

// ---------------------------------------------------------------------------
// Executor context assembly.
// ---------------------------------------------------------------------------

fn build_executor_context(ctx: &OpsContext) -> Result<RetrievalExecutorContext, OpError> {
    Ok(RetrievalExecutorContext {
        semantic: ctx.semantic_retriever.clone(),
        lexical: ctx.lexical_retriever.clone(),
        graph: ctx.graph_retriever.clone(),
        metadata: ctx.executor.metadata.clone(),
        caller_namespace: ctx.executor.caller_namespace.raw(),
        caller_space: ctx.executor.caller_space,
        // Rerank is always-on for QUERY just as for RECALL: the
        // executor reranks whenever the cross-encoder is loaded. When
        // the operator disabled the load this is `None` and the query
        // returns RRF-only.
        cross_encoder: ctx.cross_encoder.as_arc().cloned(),
    })
}

// (The above mirrors the analogous block in `handle_recall`; the
// retrievers are now mandatory Arcs so each `clone()` is just an
// Arc bump.)

// ---------------------------------------------------------------------------
// Error mapping.
// ---------------------------------------------------------------------------

fn map_plan_error(e: PlanError) -> OpError {
    match e {
        PlanError::NoSignal => {
            OpError::InvalidRequest("query has neither text nor entity anchor".into())
        }
    }
}

fn map_executor_error(e: ExecutionError) -> OpError {
    match e {
        ExecutionError::Filter(inner) => OpError::Internal(format!("filter chain: {inner}")),
        ExecutionError::Recency(inner) => OpError::Internal(format!("recency ranking: {inner}")),
    }
}
