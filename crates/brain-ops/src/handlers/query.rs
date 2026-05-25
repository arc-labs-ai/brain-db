//! Hybrid query handlers (phase 23.9).
//!
//! Wire entry points for the four hybrid-query opcodes:
//!
//! - `Query`         (0x0160 / 0x01E0) — plan + execute; return fused items.
//! - `QueryExplain`  (0x0161 / 0x01E1) — plan only; return rendered plan text.
//! - `QueryTrace`    (0x0162 / 0x01E2) — plan + execute; return rendered
//!   plan-with-execution text.
//! - `RecallHybrid`  (0x0163 / 0x01E3) — narrow projection: text → list of
//!   Memory ids + fused scores.
//!
//! Each handler does three things:
//!
//! 1. Translate the wire request into the planner's
//!    `brain_planner::hybrid::router::QueryRequest`.
//! 2. Call `plan(&req)` and (for non-EXPLAIN paths) `execute(...)`.
//! 3. Project the planner's `QueryResult` back onto a wire response.
//!
//! The handler reuses the per-shard retriever slots already
//! installed on [`OpsContext`] (semantic / lexical / graph) and
//! the shared `MetadataDb`.

use brain_core::StatementKind;
use brain_core::{EntityId, PredicateId};
use brain_index::RankedItemId;
use brain_metadata::schema::predicate::predicate_lookup_by_qname;
use brain_metadata::schema::store::schema_active;
use brain_planner::hybrid::executor::{
    execute, ExecutionError, HybridExecutorContext, QueryMetadata, QueryResult, RetrieverStatus,
};
use brain_planner::hybrid::explain::{render_plan, render_trace};
use brain_planner::hybrid::fusion::FusedItem;
use brain_planner::hybrid::planner::{plan, PlanError};
use brain_planner::hybrid::router::{
    FusionConfig, PerRetrieverWeights, QueryRequest as PlannerQueryRequest, Retriever,
    RetrieverSelection, TimeRange,
};
use brain_protocol::{
    FusionConfigWire, ItemIdWire, MemoryHit, QueryExplainRequest, QueryExplainResponse,
    QueryRequest as WireQueryRequest, QueryResponse, QueryResultItem, QueryTraceRequest,
    QueryTraceResponse, RecallHybridRequest, RecallHybridResponse, RetrieverContributionWire,
    RetrieverOutcomeWire, RetrieverSelectionWire, RetrieverWire, TimeRangeWire,
};

use crate::context::OpsContext;
use crate::error::OpError;

// ---------------------------------------------------------------------------
// Limits.
// ---------------------------------------------------------------------------

/// Max bytes of `text` accepted at handler entry. Mirrors RECALL's
/// existing bound (substrate §09/03 §"Cue text"); keeps a single
/// rkyv decode from amplifying into a huge backing string.
pub const MAX_QUERY_TEXT_BYTES: usize = 16 * 1024;

/// Max entries in `RetrieverSelectionWire::Explicit(_)`. Matches
/// the router's `MAX_RETRIEVERS = 3`.
pub const MAX_EXPLICIT_RETRIEVERS: usize = 3;

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

/// Run a full hybrid query: plan + execute, project to wire.
pub async fn handle_query(
    req: WireQueryRequest,
    ctx: &OpsContext,
) -> Result<QueryResponse, OpError> {
    validate_text_length(&req.text)?;
    let predicate_ids = match resolve_predicate_filter(&req.predicate_filter, ctx)? {
        PredicateResolution::Ok(v) => v,
        PredicateResolution::EmptyResultSet => {
            return Ok(QueryResponse {
                items: Vec::new(),
                total_latency_ms: 0.0,
                retriever_outcomes: Vec::new(),
            });
        }
    };
    let planner_req = wire_to_planner_request(req, predicate_ids)?;
    let qp = plan(&planner_req).map_err(map_plan_error)?;
    let exec_ctx = build_executor_context(ctx)?;
    let result = execute(&qp, &planner_req, &exec_ctx)
        .await
        .map_err(map_executor_error)?;
    Ok(project_query_response(&result))
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
    let planner_req = wire_to_planner_request(req.query, predicate_ids)?;
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
    let planner_req = wire_to_planner_request(req.query, predicate_ids)?;
    let qp = plan(&planner_req).map_err(map_plan_error)?;
    let exec_ctx = build_executor_context(ctx)?;
    let result = execute(&qp, &planner_req, &exec_ctx)
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

/// RECALL_HYBRID — narrow projection over the hybrid path: text → memory ids.
pub async fn handle_recall_hybrid(
    req: RecallHybridRequest,
    ctx: &OpsContext,
) -> Result<RecallHybridResponse, OpError> {
    validate_text_length(&req.text)?;
    let planner_req = PlannerQueryRequest {
        text: Some(req.text),
        entity_anchor: None,
        kind_filter: Vec::new(),
        predicate_filter: Vec::new(),
        time_filter: None,
        confidence_min: None,
        include_tombstoned: false,
        include_superseded: false,
        as_of_record_time_unix_nanos: None,
        limit: req.limit,
        retrievers: RetrieverSelection::Auto,
        fusion_config: None,
    };
    let qp = plan(&planner_req).map_err(map_plan_error)?;
    let exec_ctx = build_executor_context(ctx)?;
    let result = execute(&qp, &planner_req, &exec_ctx)
        .await
        .map_err(map_executor_error)?;
    let items = result
        .items
        .iter()
        .filter_map(memory_hit_from_fused)
        .collect();
    Ok(RecallHybridResponse { items })
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
) -> Result<PlannerQueryRequest, OpError> {
    let text = if req.text.is_empty() {
        None
    } else {
        Some(req.text)
    };
    let entity_anchor = req.entity_anchor.map(EntityId::from_bytes);
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
        confidence_min: req.confidence_min,
        include_tombstoned: req.include_tombstoned,
        include_superseded: req.include_superseded,
        as_of_record_time_unix_nanos: None,
        limit: req.limit,
        retrievers,
        fusion_config,
    })
}

fn statement_kind_from_byte(b: u8) -> Result<StatementKind, OpError> {
    match b {
        0 => Ok(StatementKind::Fact),
        1 => Ok(StatementKind::Preference),
        2 => Ok(StatementKind::Event),
        other => Err(OpError::InvalidRequest(format!(
            "unknown StatementKind byte: {other}",
        ))),
    }
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

fn retriever_to_wire(r: Retriever) -> RetrieverWire {
    match r {
        Retriever::Semantic => RetrieverWire::Semantic,
        Retriever::Lexical => RetrieverWire::Lexical,
        Retriever::Graph => RetrieverWire::Graph,
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

fn build_executor_context(ctx: &OpsContext) -> Result<HybridExecutorContext, OpError> {
    Ok(HybridExecutorContext {
        semantic: ctx.semantic_retriever.clone(),
        lexical: ctx.lexical_retriever.clone(),
        graph: ctx.graph_retriever.clone(),
        metadata: ctx.executor.metadata.clone(),
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
// Result projection.
// ---------------------------------------------------------------------------

fn project_query_response(result: &QueryResult) -> QueryResponse {
    let items = result.items.iter().map(project_item).collect();
    let retriever_outcomes = project_outcomes(&result.metadata);
    QueryResponse {
        items,
        total_latency_ms: result.metadata.total_latency_ms,
        retriever_outcomes,
    }
}

fn project_item(item: &FusedItem) -> QueryResultItem {
    let id = item_id_to_wire(item.id);
    let contributing = item
        .contributing
        .iter()
        .map(|c| RetrieverContributionWire {
            retriever: retriever_to_wire(c.retriever),
            rank: c.rank,
            raw_score: c.raw_score,
        })
        .collect();
    QueryResultItem {
        id,
        fused_score: item.fused_score,
        contributing,
    }
}

fn item_id_to_wire(id: RankedItemId) -> ItemIdWire {
    match id {
        RankedItemId::Memory(m) => ItemIdWire {
            kind: 0,
            bytes: u128_to_be_bytes(m.raw()),
        },
        RankedItemId::Statement(s) => ItemIdWire {
            kind: 1,
            bytes: s.to_bytes(),
        },
        RankedItemId::Entity(e) => ItemIdWire {
            kind: 2,
            bytes: e.to_bytes(),
        },
        RankedItemId::Relation(r) => ItemIdWire {
            kind: 3,
            bytes: r.to_bytes(),
        },
    }
}

fn u128_to_be_bytes(v: u128) -> [u8; 16] {
    v.to_be_bytes()
}

fn project_outcomes(meta: &QueryMetadata) -> Vec<RetrieverOutcomeWire> {
    meta.retriever_outcomes
        .iter()
        .map(|o| {
            let latency_ms = meta
                .retriever_latencies_ms
                .iter()
                .find(|(r, _)| *r == o.retriever)
                .map(|(_, ms)| *ms)
                .unwrap_or(0.0);
            let result_count = meta
                .retriever_total_results
                .iter()
                .find(|(r, _)| *r == o.retriever)
                .map(|(_, c)| *c as u32)
                .unwrap_or(0);
            let (status, message) = status_to_wire(&o.status);
            RetrieverOutcomeWire {
                retriever: retriever_to_wire(o.retriever),
                status,
                message,
                latency_ms,
                result_count,
            }
        })
        .collect()
}

fn status_to_wire(s: &RetrieverStatus) -> (u8, String) {
    match s {
        RetrieverStatus::Success => (0, String::new()),
        RetrieverStatus::Skipped(reason) => (1, (*reason).to_string()),
        RetrieverStatus::Timeout => (2, String::new()),
        RetrieverStatus::Failure(msg) => (3, msg.clone()),
    }
}

fn memory_hit_from_fused(item: &FusedItem) -> Option<MemoryHit> {
    match item.id {
        RankedItemId::Memory(m) => Some(MemoryHit {
            memory_id: u128_to_be_bytes(m.raw()),
            fused_score: item.fused_score,
        }),
        _ => None,
    }
}

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
    }
}
