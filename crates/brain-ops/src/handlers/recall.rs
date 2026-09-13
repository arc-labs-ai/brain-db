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

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use brain_core::{EntityId, MemoryId, SessionId, Slot, SubjectRef};
use brain_index::RankedItemId;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_planner::retrieval::executor::{
    execute as retrieval_execute, ExecutionError, QueryMetadata, QueryResult, RerankOutcome,
    RetrievalExecutorContext, RetrieverStatus,
};
use brain_planner::retrieval::planner::{plan as retrieval_plan, PlanError};
use brain_planner::retrieval::router::{
    QueryRequest as PlannerQueryRequest, Retriever, RetrieverSelection,
};
use brain_protocol::envelope::request::{MemoryKindWire, RecallRequest, RecallScopeWire};
use brain_protocol::envelope::response::{
    AnswerKindWire, MemoryResult, RankedItemKindWire, RecallResponseFrame, RecallTrace,
    RecallTraceCandidate, RecallTraceDroppedId, RecallTraceFilterChain, RecallTraceFusion,
    RecallTraceFusionItem, RecallTraceRerank, RecallTraceRetriever, RecallTraceRetrieverStatus,
};
use brain_protocol::RetrieverNameWire;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::grounded::{
    grounded_answer_walk, project_statement_slot, AnswerKind, GroundedAnswer, GroundedValue,
    SLOT_PROJECTION_STRONG_FLOOR,
};
use crate::metrics::{QueryOutcome, RetrieverKind};
use crate::txn::BufferedEncode;

/// Upper bound on the safety cap for returned items (`max_results`).
/// Bounds the fan-out + result-buffer allocation so a crafted request
/// can't drive an unbounded allocation. Matches the statement/relation
/// list cap (`LIST_LIMIT_MAX`).
pub const MAX_RECALL_RESULTS: u32 = 1000;

/// Server default applied when `max_results == 0`. The cap is a safety
/// bound, not a ranking knob — the grounded answer's shape comes from
/// the data, and the episodic path is similarity-ordered, so a generous
/// default keeps "I didn't ask for a count" working sensibly.
pub const DEFAULT_RECALL_RESULTS: u32 = 50;

/// Candidate-pool budget fed to the retrieval executor and the projection loop,
/// decoupled from `max_results` (the answer-size safety cap). The membership
/// band in `build_membership` must decide the answer over the FULL filtered
/// pool — not a window pre-truncated to the caller's cap — or a genuine answer
/// ranked below that window would be invisible to the band (the top-K trap this
/// removes). The pool is still bounded: the per-lane `top_n` clamps cap the
/// fused set well under this ceiling, and `max_results` caps the returned
/// members after the band runs. Sized to the hard allocation guard so the
/// executor never truncates a realistic per-space fused pool.
pub const RECALL_CANDIDATE_POOL: u32 = MAX_RECALL_RESULTS;

/// Upper bound on the entry count of any one recall filter list
/// (`session_filter`, `kind_filter`). These are only bounded by the 16 MiB
/// payload cap otherwise; an explicit cap turns a crafted oversized filter
/// into a clear `InvalidRequest` instead of silently building a large
/// `HashSet` for the post-filter pass. The bound is generous — far above any
/// legitimate scoping need.
pub const MAX_RECALL_FILTER_ENTRIES: usize = 1024;

/// Upper bound on the number of subject candidates resolved from a single
/// cue. Each candidate costs a few redb point lookups plus a grounded
/// scan; capping bounds the per-call work so a cue packed with proper-noun
/// surfaces can't fan out unboundedly. Generous — real cues name a handful
/// of subjects at most.
pub const MAX_SUBJECT_CANDIDATES: usize = 8;

/// Upper bound on the number of DISTINCT surfaces mined from a cue that are
/// handed to the (heavy) entity resolver. `MAX_SUBJECT_CANDIDATES` bounds the
/// resolver's OUTPUT; this bounds its INVOCATIONS. Without it a cue-only RECALL
/// (empty `subject_name`) calls `entity_resolve_scored` once per distinct token
/// — a heavy trigram scan over every entity type — so a multi-megabyte cue of
/// distinct non-resolving tokens (the wire cap is 16 MiB) would starve the
/// single-writer shard core with resolver calls even though few or none
/// resolve. Real cues name a handful of subjects; 32 is generous headroom while
/// keeping the per-call resolver work strictly bounded.
pub const MAX_CUE_SURFACES: usize = 32;

/// Upper bound on how many whitespace tokens of the cue are SCANNED while
/// mining surfaces. Bounds the token walk itself so a cue that is millions of
/// copies of a handful of distinct tokens (which would never fill
/// `MAX_CUE_SURFACES` and so never trip that cap) still can't force an O(cue)
/// scan on the shard core. Far above any real cue length.
pub const MAX_CUE_TOKENS_SCANNED: usize = 256;

/// The read-scope width this request runs under, as the centralized
/// [`brain_metadata::ScopeMode`] the `admits` predicate takes. `Space` (the
/// default) pins reads to the caller's single `(namespace, space)`;
/// `Namespace` widens the space half within the caller's own namespace.
///
/// While `handle_recall` still refuses `scope = Namespace` at the door (the
/// cross-shard fan-out lands in a later phase), this is the single point
/// that maps the wire scope onto the retrieval mode, so the whole read path
/// is already threaded for when the gate lifts.
fn recall_scope_mode(req: &RecallRequest) -> brain_metadata::ScopeMode {
    match req.scope {
        RecallScopeWire::Namespace => brain_metadata::ScopeMode::Namespace,
        RecallScopeWire::Space => brain_metadata::ScopeMode::Space,
    }
}

pub async fn handle_recall(
    mut req: RecallRequest,
    ctx: &OpsContext,
) -> Result<RecallResponseFrame, OpError> {
    // Namespace-wide recall (scope = Namespace) requires cross-shard fan-out +
    // global merge at the connection layer (spec §"Recall scope"). A single
    // shard's handler cannot serve it — it would see only this shard's spaces —
    // so it is refused here until that fan-out path lands. Single-space recall
    // (the default) is unaffected. Lifts when the router-level fan-out is wired.
    if matches!(req.scope, RecallScopeWire::Namespace) {
        return Err(OpError::InvalidRequest(
            "namespace-wide recall (scope=Namespace) is not yet supported".into(),
        ));
    }

    // End-to-end wall clock for the query metric family. One `Instant`
    // per recall (cheap), covering the whole read — fan-out, grounding
    // overlay, membership shaping, and abstention — not just the
    // executor's fan-out window. Recorded once, at each return point.
    let recall_started = Instant::now();
    // Did the caller ask for a specific result count? `0` means "no count, use
    // the server default"; any non-zero value is an explicit caller cap. We
    // capture this BEFORE normalising `max_results` below, because the keyed
    // (exact-anchor) path must not clip the intrinsic belonging set to the fuzzy
    // default window when the caller never asked for a count — and the
    // normalisation overwrites `0` with the default, erasing the distinction.
    let client_requested_count = req.max_results != 0;
    normalize_recall_request(&mut req)?;

    // Brain is a memory database: a recall returns one memory, an array of
    // memories, or none — never raw retrieval lanes. There is ONE unified read
    // path, no flag:
    //
    //   1. Associative fan-out (semantic + lexical), RRF-fused — the recall
    //      base. Robust at any scale; owns "find the relevant memories".
    //   2. Typed-graph grounding overlay — ALWAYS consulted (no flag). The
    //      cue is embedded once and matched by cosine against the resolved
    //      subject's stored predicates/relations (purely semantic, no string
    //      heuristics). A confident match BOOSTS its source memory to the top
    //      of the combined results (and prepends it when the fan-out missed
    //      it), but NEVER replaces the fan-out — a wrong grounded match costs
    //      ordering, never the real answer. This is why the typed graph can be
    //      always-on without the subject-dump flooding that sinks recall.
    //   3. The answer's shape (Single / Many / None) follows the count.

    // The read runs in two composable stages so namespace-wide RECALL reuses the
    // EXACT same shaping as single-space RECALL (no second copy of the answer
    // logic). `gather_recall` is the associative + typed-graph fan-out over the
    // caller's read scope, producing a raw candidate pool; `shape_recall` turns a
    // pool into the shaped answer (grounding overlay + membership + abstention).
    // A single-space recall runs both here on one shard. A namespace-wide recall
    // fans `gather_recall` out to every shard (`recall_gather_namespace`), merges
    // the pools at the connection layer, and runs `shape_recall` once over the
    // merged pool on the bound shard (`recall_shape_namespace`).
    let gathered = gather_recall(&req, ctx).await?;
    shape_recall(gathered, &req, ctx, client_requested_count, recall_started)
}

/// The raw candidate pool a single shard produced for a recall, plus the
/// per-request shaping signals it computed locally (`grounded`, `hype_scores`).
/// For namespace-wide recall the connection layer collects one of these per
/// shard and merges all three signals ([`merge_recall_pools`] for the pool,
/// plus the HyPE union and best-grounded pick) before a single global shaping
/// pass — so a grounded commit and the HyPE answer-lead work ACROSS shards, not
/// just on the coordinator.
struct RecallGathered {
    memories: Vec<MemoryResult>,
    trace: Option<RecallTrace>,
    metric_sample: RecallMetricSample,
    cue_vec: Option<[f32; brain_embed::VECTOR_DIM]>,
    anchor: Option<EntityId>,
    /// The typed-graph grounded answer computed on this shard (over its own
    /// spaces). `NoAnswer` when no cue vector, or no subject/predicate cleared
    /// the match floor here.
    grounded: GroundedOutcome,
    /// HyPE answer-lead scores (memory_id → best question-bridge cosine) from
    /// this shard's HyPE index. Empty when no cue vector.
    hype_scores: HashMap<u128, f32>,
}

/// Stage 1 — the associative (semantic + lexical) + typed-graph fan-out over the
/// caller's read scope, RRF-fused and reranked into a raw candidate pool. No
/// membership shaping happens here, so the pool is safe to merge with other
/// shards' pools before a single global shaping pass.
async fn gather_recall(req: &RecallRequest, ctx: &OpsContext) -> Result<RecallGathered, OpError> {
    // Embed the cue once for the grounding overlay. A failed embed degrades to
    // the plain fan-out — grounding is an overlay, never a reason to fail the
    // read.
    let cue_vec = ctx.executor.embedder.embed(&req.cue_text).ok();
    // The entity-graph traversal lane is anchored on the cue's resolved subject;
    // `retrieve_memories` cue-conditions the graph candidates so the structural
    // walk can't re-introduce the subject-dump flood.
    let anchor = resolve_graph_anchor(req, ctx);
    let (memories, trace, metric_sample) =
        retrieve_memories(req, ctx, anchor, cue_vec.as_ref()).await?;
    // Compute the grounding overlay + HyPE answer-lead HERE (per shard), not in
    // `shape_recall`, so a namespace-wide fan-out can gather each shard's local
    // signals and merge them — letting the grounded commit and the HyPE ordering
    // reflect the whole namespace, not just the coordinator's spaces.
    let (grounded, hype_scores) = compute_grounding(req, ctx, cue_vec.as_ref())?;
    Ok(RecallGathered {
        memories,
        trace,
        metric_sample,
        cue_vec,
        anchor,
        grounded,
        hype_scores,
    })
}

/// Compute the typed-graph grounded answer + the HyPE answer-lead scores for a
/// cue on THIS shard (over its own spaces). Factored out of the read stages so
/// both the single-shard gather and any future re-computation share one
/// definition. Returns `(NoAnswer, empty)` when there is no cue vector.
fn compute_grounding(
    req: &RecallRequest,
    ctx: &OpsContext,
    cue_vec: Option<&[f32; brain_embed::VECTOR_DIM]>,
) -> Result<(GroundedOutcome, HashMap<u128, f32>), OpError> {
    let Some(cue_vec) = cue_vec else {
        return Ok((GroundedOutcome::NoAnswer, HashMap::new()));
    };
    let grounded = best_grounded_for_cue(req, ctx, cue_vec)?;
    // HyPE questions are stored query-side (embed_query, BGE query prefix), so
    // the cue must be embedded the SAME way for the answer-lead cosine to be
    // in-distribution; fall back to the plain cue if the prefixed embed fails.
    let hype_cue_vec = ctx
        .executor
        .embedder
        .embed_query(&req.cue_text)
        .unwrap_or(*cue_vec);
    let hype_scores: HashMap<u128, f32> = ctx
        .semantic_retriever
        .hype_scores_for_query(&hype_cue_vec, RECALL_CANDIDATE_POOL as usize)
        .into_iter()
        .map(|(id, s)| (id.raw(), s))
        .collect();
    Ok((grounded, hype_scores))
}

/// Stage 2 — turn a candidate pool (this shard's, or the cross-shard merge)
/// into the shaped answer: typed-graph grounding overlay, the membership set,
/// and the two abstention gates. Cross-shard members degrade gracefully — the
/// per-shard signals (`vector_for`, grounding, HyPE) simply miss for a member
/// owned by another shard and fall back to the score already on the
/// `MemoryResult`, so the merged answer is coherent without those refinements.
fn shape_recall(
    gathered: RecallGathered,
    req: &RecallRequest,
    ctx: &OpsContext,
    client_requested_count: bool,
    recall_started: Instant,
) -> Result<RecallResponseFrame, OpError> {
    let RecallGathered {
        memories,
        trace,
        metric_sample,
        cue_vec,
        anchor,
        grounded,
        hype_scores,
    } = gathered;

    let Some(cue_vec) = cue_vec else {
        // No cue embedding → no grounding overlay, so no committed shape; the
        // answer cardinality falls back to the member count.
        let frame = recall_frame(memories, None, trace);
        record_recall_metrics(ctx, recall_started, &metric_sample, frame.answer_kind);
        return Ok(frame);
    };

    // `grounded` + `hype_scores` are supplied by the gather stage (single shard),
    // or merged across shards for a namespace-wide read — shaping never
    // recomputes them, so the grounded commit + HyPE ordering are global.

    // MEMBERSHIP MODEL — recall is the SET of memories that belong to this cue,
    // unioning the typed-graph grounded answer (S_struct) with the associative
    // fan-out cut at its natural score cliff (S_sem). The answer SHAPE is the
    // grounded commit's shape when one fired, else it follows the set's
    // cardinality: 0 → None, 1 → Single, N → Many.
    let (membership, committed_shape, any_belongs) = build_membership(
        memories,
        &grounded,
        req,
        ctx,
        &cue_vec,
        anchor,
        client_requested_count,
        &hype_scores,
    );

    // ── ABSTENTION PIPELINE ─────────────────────────────────────────────────
    // Two structural abstention gates keyed on the cue having no real anchor for
    // its answer. In-txn reads are exempt from BOTH (read-your-writes: a pending
    // write carries no retrieval-lane confirmation only because it isn't indexed
    // yet, so abstaining it would break the guarantee).
    let membership = if req.txn_id.is_some() {
        membership
    } else {
        // 1. No subject resolved at all.
        let membership = apply_anchor_abstention(membership, anchor, &grounded, any_belongs);
        // 2. Subject resolved but no fact of the matching KIND/role for it.
        apply_kind_presence_abstention(membership, anchor, &grounded, any_belongs)
    };

    let frame = recall_frame(membership, committed_shape, trace);
    record_recall_metrics(ctx, recall_started, &metric_sample, frame.answer_kind);
    Ok(frame)
}

/// Normalise a recall request in place: clamp the safety cap into
/// `[DEFAULT, MAX]` (`0` = server default), force `include_text` on (a memory
/// without its text is useless), and bound the filter list sizes. Shared by the
/// single-space handler and the namespace-wide fan-out entry points so both
/// apply identical bounds. Capture `client_requested_count = req.max_results
/// != 0` BEFORE calling this — it overwrites a `0` cap.
fn normalize_recall_request(req: &mut RecallRequest) -> Result<(), OpError> {
    if req.max_results == 0 {
        req.max_results = DEFAULT_RECALL_RESULTS;
    }
    if req.max_results > MAX_RECALL_RESULTS {
        req.max_results = MAX_RECALL_RESULTS;
    }
    req.include_text = true;
    if let Some(ref ctxs) = req.session_filter {
        if ctxs.len() > MAX_RECALL_FILTER_ENTRIES {
            return Err(OpError::InvalidRequest(format!(
                "recall: session_filter must have <= {MAX_RECALL_FILTER_ENTRIES} entries"
            )));
        }
    }
    if let Some(ref kinds) = req.kind_filter {
        if kinds.len() > MAX_RECALL_FILTER_ENTRIES {
            return Err(OpError::InvalidRequest(format!(
                "recall: kind_filter must have <= {MAX_RECALL_FILTER_ENTRIES} entries"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Namespace-wide fan-out (cross-shard) — Phase C.
//
// A namespace's spaces are spread across shards, so a namespace-wide RECALL
// must run on EVERY shard and merge. The connection layer orchestrates it:
//   1. `recall_gather_namespace` on each shard → that shard's raw candidate
//      pool (its own spaces, widened by Phase B's namespace read scope).
//   2. `merge_recall_pools` at the connection layer → one global pool (RRF by
//      within-shard rank, deduped, bounded).
//   3. `recall_shape_namespace` on the bound (coordinator) shard → the single
//      global shaping pass over the merged pool, reusing the identical
//      `shape_recall` used by single-space RECALL.
// The per-shard gather never shapes, so no shard decides Single/Many/None over
// a partial view; the shape is decided once, globally. The `handle_recall`
// door still refuses `scope = Namespace` — a single shard cannot serve it — so
// this path is only ever reached through the connection-layer fan-out.
// ---------------------------------------------------------------------------

/// RRF smoothing constant for the cross-shard merge. Matches the intra-shard
/// fusion `k` so a rank-1 hit from one shard and a rank-1 hit from another are
/// weighted identically.
const CROSS_SHARD_RRF_K: f32 = 60.0;

/// One shard's namespace-wide gather partial: its raw candidate pool plus the
/// shaping signals it computed locally (the grounded answer over its spaces, its
/// HyPE answer-lead scores, and the graph anchor it resolved). The connection
/// layer collects one per shard and merges them ([`merge_namespace_partials`]);
/// brain-server holds these opaquely and never inspects the fields.
pub struct NamespaceRecallPartial {
    memories: Vec<MemoryResult>,
    grounded: GroundedOutcome,
    hype_scores: HashMap<u128, f32>,
    anchor: Option<EntityId>,
}

/// The merged cross-shard shaping inputs for a namespace-wide recall: the global
/// candidate pool plus the winning grounded answer, unioned HyPE scores, and the
/// anchor from the shard that produced the grounded answer. Fed to
/// [`recall_shape_namespace`] for the single global shaping pass. Opaque to
/// brain-server.
pub struct MergedNamespaceRecall {
    memories: Vec<MemoryResult>,
    grounded: GroundedOutcome,
    hype_scores: HashMap<u128, f32>,
    anchor: Option<EntityId>,
}

/// Phase-C/D stage 1 (per shard): produce this shard's namespace-wide gather
/// partial — its raw candidate pool AND the grounding/HyPE signals it computed
/// over its own spaces. Normalises the request and forces the widened read
/// scope, then runs the shared `gather_recall` (no shaping).
pub async fn recall_gather_namespace(
    mut req: RecallRequest,
    ctx: &OpsContext,
) -> Result<NamespaceRecallPartial, OpError> {
    normalize_recall_request(&mut req)?;
    // Force the widened read scope regardless of the wire value that reached
    // this shard — this entry point IS the namespace fan-out.
    req.scope = RecallScopeWire::Namespace;
    let gathered = gather_recall(&req, ctx).await?;
    Ok(NamespaceRecallPartial {
        memories: gathered.memories,
        grounded: gathered.grounded,
        hype_scores: gathered.hype_scores,
        anchor: gathered.anchor,
    })
}

/// Merge every shard's namespace-wide gather partial into one set of global
/// shaping inputs:
///   - **pool**: [`merge_recall_pools`] (RRF by within-shard rank, deduped, bounded);
///   - **HyPE**: union of the per-shard score maps (a memory is owned by exactly
///     one shard, so the keys are disjoint) — the answer-lead is now global;
///   - **grounded**: the best-scoring [`GroundedOutcome::Answer`] across shards
///     (a namespace-wide subject typically resolves on one shard), preferring an
///     anchor-scoped answer on a near-tie — so a grounded commit fires even when
///     the subject's facts live on a shard other than the coordinator.
/// The anchor travels with the winning grounded answer's shard.
pub fn merge_namespace_partials(partials: Vec<NamespaceRecallPartial>) -> MergedNamespaceRecall {
    let mut pools: Vec<Vec<MemoryResult>> = Vec::with_capacity(partials.len());
    let mut hype_scores: HashMap<u128, f32> = HashMap::new();
    // Best grounded answer across shards, with the anchor from the same shard.
    let mut best_grounded: GroundedOutcome = GroundedOutcome::NoAnswer;
    let mut best_score = f32::NEG_INFINITY;
    let mut best_anchor: Option<EntityId> = None;
    // A Some anchor from any shard, used as a fallback when no shard grounded.
    let mut fallback_anchor: Option<EntityId> = None;

    for p in partials {
        pools.push(p.memories);
        for (id, s) in p.hype_scores {
            // Disjoint keys across shards; `max` is a harmless tie-break if a
            // memory ever appeared under two shards' HyPE maps.
            hype_scores
                .entry(id)
                .and_modify(|cur| {
                    if s > *cur {
                        *cur = s;
                    }
                })
                .or_insert(s);
        }
        if fallback_anchor.is_none() {
            fallback_anchor = p.anchor;
        }
        if let GroundedOutcome::Answer(ref answer, anchor_scoped) = p.grounded {
            let score = grounded_answer_score(answer);
            // Prefer a strictly higher score; on a near-tie prefer an
            // anchor-scoped answer (it is the one allowed to COMMIT the lead).
            let take = score > best_score + GROUNDED_MERGE_TIE_EPS
                || ((score - best_score).abs() <= GROUNDED_MERGE_TIE_EPS
                    && anchor_scoped
                    && !matches!(best_grounded, GroundedOutcome::Answer(_, true)));
            if take {
                best_score = score;
                best_anchor = p.anchor;
                best_grounded = p.grounded;
            }
        }
    }

    let anchor = if matches!(best_grounded, GroundedOutcome::Answer(..)) {
        best_anchor
    } else {
        fallback_anchor
    };
    MergedNamespaceRecall {
        memories: merge_recall_pools(pools),
        grounded: best_grounded,
        hype_scores,
        anchor,
    }
}

/// Phase-C/D stage 3 (coordinator shard): shape the MERGED cross-shard inputs
/// into the final answer, reusing the identical `shape_recall` single-space
/// RECALL uses. The cue vector is recomputed here (deterministic); the grounded
/// answer, HyPE scores, and anchor come pre-merged from every shard, so the
/// grounded commit + answer-lead reflect the whole namespace. Members owned by
/// other shards still degrade gracefully in `shape_recall`'s per-member
/// `vector_for` re-score (they fall back to the score on the `MemoryResult`).
pub async fn recall_shape_namespace(
    merged: MergedNamespaceRecall,
    mut req: RecallRequest,
    ctx: &OpsContext,
) -> Result<RecallResponseFrame, OpError> {
    let recall_started = Instant::now();
    let client_requested_count = req.max_results != 0;
    normalize_recall_request(&mut req)?;
    req.scope = RecallScopeWire::Namespace;
    let cue_vec = ctx.executor.embedder.embed(&req.cue_text).ok();
    let gathered = RecallGathered {
        memories: merged.memories,
        // Per-shard trace/metrics don't compose across a fan-out; the merged
        // answer records a minimal sample (namespace-wide observability is a
        // follow-up).
        trace: None,
        metric_sample: RecallMetricSample::empty(),
        cue_vec,
        anchor: merged.anchor,
        grounded: merged.grounded,
        hype_scores: merged.hype_scores,
    };
    shape_recall(gathered, &req, ctx, client_requested_count, recall_started)
}

/// Near-tie epsilon for picking the best grounded answer across shards. Mirrors
/// the intra-shard `TIE_EPS` used by `best_grounded_for_cue`.
const GROUNDED_MERGE_TIE_EPS: f32 = 1e-4;

/// The comparable score of a grounded answer — its top value's match cosine,
/// the same key `best_grounded_for_cue` ranks candidates by within a shard.
fn grounded_answer_score(answer: &GroundedAnswer) -> f32 {
    answer.values.first().map(|v| v.match_score).unwrap_or(0.0)
}

/// Merge per-shard candidate pools into one global pool for namespace-wide
/// recall. Each pool is that shard's fan-out result, best-first. A memory is
/// owned by exactly one shard, so there are normally no cross-shard duplicates;
/// this dedups defensively by id (keeping the higher RRF score) and orders the
/// union by Reciprocal-Rank-Fusion over each hit's WITHIN-SHARD rank — the one
/// cross-shard-comparable signal (raw fused scores are normalised per pool and
/// are not comparable across shards). The result is truncated to the candidate
/// pool budget so the downstream shaping pass stays bounded regardless of the
/// shard count.
///
/// This ordering primarily bounds and assembles the pool; `shape_recall`
/// re-scores members by cue cosine, so the exact merge order only decides the
/// no-cue fast path and the truncation cut.
pub fn merge_recall_pools(pools: Vec<Vec<MemoryResult>>) -> Vec<MemoryResult> {
    let mut best: HashMap<u128, (MemoryResult, f32)> = HashMap::new();
    for pool in pools {
        for (rank0, m) in pool.into_iter().enumerate() {
            let rrf = 1.0 / (CROSS_SHARD_RRF_K + (rank0 as f32) + 1.0);
            match best.get_mut(&m.memory_id) {
                Some((_, score)) => {
                    // Same memory from two shards (should not happen — one owner
                    // — but be defensive): keep the higher-ranked appearance.
                    if rrf > *score {
                        *score = rrf;
                    }
                }
                None => {
                    best.insert(m.memory_id, (m, rrf));
                }
            }
        }
    }
    let mut merged: Vec<(MemoryResult, f32)> = best.into_values().collect();
    // Descending RRF; ties broken on memory_id so the merge is deterministic
    // across runs and shard-arrival order.
    merged.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.memory_id.cmp(&b.0.memory_id))
    });
    merged.truncate(RECALL_CANDIDATE_POOL as usize);
    merged.into_iter().map(|(m, _)| m).collect()
}

/// Lightweight always-on read-path stats the executor already MEASURED,
/// extracted from `QueryMetadata` so the RECALL handler can RECORD them
/// into the retriever / query metric families after the answer is
/// shaped. Kept small (one `Vec` of at most three lanes, no per-item
/// detail) so surfacing it up the call adds no hot-path allocation of
/// consequence — the fan-out loop itself is untouched.
struct RecallMetricSample {
    /// One entry per retriever lane that was actually invoked (skipped
    /// lanes are omitted — they never ran): `(kind, elapsed_ms,
    /// candidates)`.
    per_lane: Vec<(RetrieverKind, f64, u64)>,
    /// The effective fusion `k` the engine fused at this execution.
    effective_fusion_k: u32,
    /// Whether the cross-encoder rerank stage actually reordered the
    /// fused list (loaded and applied — not merely present).
    rerank_invoked: bool,
}

impl RecallMetricSample {
    /// A no-signal sample for the namespace-wide merged-shape path, where the
    /// per-shard executor metadata does not compose across the fan-out. Records
    /// the query outcome (latency + answer kind) with no per-lane detail.
    fn empty() -> Self {
        Self {
            per_lane: Vec::new(),
            effective_fusion_k: 0,
            rerank_invoked: false,
        }
    }

    /// Extract the always-on stats from the executor's returned
    /// metadata. Populated on every recall (not gated on `trace_detail`)
    /// — the per-lane latency / total / outcome vectors and the
    /// `effective_fusion_k` / `rerank` fields are always filled.
    fn from_metadata(meta: &QueryMetadata) -> Self {
        let mut per_lane = Vec::with_capacity(meta.retriever_outcomes.len());
        for outcome in &meta.retriever_outcomes {
            // A skipped lane never ran, so it isn't an invocation.
            if matches!(outcome.status, RetrieverStatus::Skipped(_)) {
                continue;
            }
            let elapsed_ms = meta
                .retriever_latencies_ms
                .iter()
                .find(|(r, _)| *r == outcome.retriever)
                .map(|(_, ms)| *ms)
                .unwrap_or(0.0);
            let candidates = meta
                .retriever_total_results
                .iter()
                .find(|(r, _)| *r == outcome.retriever)
                .map(|(_, c)| *c as u64)
                .unwrap_or(0);
            per_lane.push((retriever_kind(outcome.retriever), elapsed_ms, candidates));
        }
        let rerank_invoked = matches!(meta.rerank, Some(RerankOutcome::Applied { .. }));
        Self {
            per_lane,
            effective_fusion_k: meta.effective_fusion_k,
            rerank_invoked,
        }
    }
}

/// Map the planner's retriever discriminant onto the metric-family
/// label enum. Total — the planner has exactly these three lanes.
fn retriever_kind(retriever: Retriever) -> RetrieverKind {
    match retriever {
        Retriever::Semantic => RetrieverKind::Semantic,
        Retriever::Lexical => RetrieverKind::Lexical,
        Retriever::Graph => RetrieverKind::Graph,
    }
}

/// Map the wire answer shape onto the query-metric outcome label.
fn query_outcome(answer_kind: AnswerKindWire) -> QueryOutcome {
    match answer_kind {
        AnswerKindWire::Single => QueryOutcome::Single,
        AnswerKindWire::Many => QueryOutcome::Many,
        AnswerKindWire::None => QueryOutcome::None,
    }
}

/// Record the read-path metrics for one served recall. Runs on EVERY
/// recall (both return paths), post-hoc: a handful of atomic adds plus
/// a per-lane and two query histogram observes. No lock, no allocation
/// beyond the already-built `per_lane` sample.
fn record_recall_metrics(
    ctx: &OpsContext,
    started: Instant,
    sample: &RecallMetricSample,
    answer_kind: AnswerKindWire,
) {
    for &(kind, elapsed_ms, candidates) in &sample.per_lane {
        ctx.retriever_metrics.record(kind, elapsed_ms, candidates);
    }
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    ctx.query_metrics.record(
        latency_ms,
        sample.effective_fusion_k,
        sample.rerank_invoked,
        query_outcome(answer_kind),
    );
}

/// Honest abstention by structural anchor — unconditional, no flag/knob (FIX C).
/// This gate owns the NO-ANCHOR case: the cue resolved no subject entity and the
/// grounded layer produced no answer. It drops to an empty (None) answer when NO
/// surviving member belongs (`!any_belongs`) — i.e. no member carries a belonging
/// signal beyond the raw passage cosine (strong HyPE / lexical / graph /
/// grounded). When some member does, the facts ship: the read never emits an
/// empty answer over corroborated belonging. Substrate-safe: a stored subject's
/// own memory is confirmed by a real lane (lexical/graph/grounded), so it belongs
/// and this never fires; it triggers only when the cue's answer is nowhere in the
/// store and only a lone passage-cosine echo remains.
fn apply_anchor_abstention(
    members: Vec<MemoryResult>,
    anchor: Option<EntityId>,
    grounded: &GroundedOutcome,
    any_belongs: bool,
) -> Vec<MemoryResult> {
    if anchor.is_some() || matches!(grounded, GroundedOutcome::Answer(..)) {
        return members;
    }
    if any_belongs {
        members
    } else {
        Vec::new()
    }
}

/// Kind-presence abstention — the adversarial-question gate (FIX C). This gate
/// owns the SUBJECT-RESOLVED case: the cue resolved a subject (`anchor`) but the
/// grounded typed-graph layer produced NO fact of the matching kind/role for it
/// (`GroundedOutcome::NoAnswer`). The honest answer is often `None`, not a
/// loosely-related memory. Two traps this closes:
///   * subject-mismatch — "when did MELANIE run a race" when only Caroline did:
///     the topical race memory is about Caroline, never linked to Melanie.
///   * wrong-kind — the subject has facts, but none of the kind the cue asks
///     for, so its own memories don't actually answer the question.
///
/// Keyed on `any_belongs`: the set is KEPT whenever some surviving member carries
/// a belonging signal beyond the raw passage cosine (strong HyPE / lexical /
/// graph / grounded), and ABSTAINS (→ `None`) only when none does. A truly
/// adversarial cue — a resolved subject whose only surfaced memories are off-cue
/// buried facts (no lexical/graph lane, no grounded source, no strong HyPE, only
/// a lone passage cosine) — does not belong and abstains; a real episodic answer
/// grounded simply hadn't extracted lands in some independent lane, so it belongs
/// and is kept.
///
/// Never fires when the grounded layer DID answer (the typed graph has the
/// fact) or when no subject resolved (that is [`apply_anchor_abstention`]'s
/// job). Pure: unit-testable.
fn apply_kind_presence_abstention(
    members: Vec<MemoryResult>,
    anchor: Option<EntityId>,
    grounded: &GroundedOutcome,
    any_belongs: bool,
) -> Vec<MemoryResult> {
    // Only the "subject resolved, but no matching-kind fact" case is ours.
    if anchor.is_none() || matches!(grounded, GroundedOutcome::Answer(..)) {
        return members;
    }
    if any_belongs {
        members
    } else {
        Vec::new()
    }
}

/// Unique multi-lane consensus collapse (model-free, no per-read model). Uses the
/// per-lane contributions the fan-out already recorded: a memory found by MORE
/// independent lanes (semantic / lexical / graph) is a stronger belonging signal.
///
/// Collapse the set to a crisp Single ONLY when BOTH agree:
///   * exactly one member has the maximum lane count, and that maximum is ≥ 2
///     (a unique multi-lane consensus), AND
///   * that same member is the highest-belonging member (`top_member_id`).
///
/// Requiring both is what protects recall on paraphrase / lexical cues: a lexical
/// term-matcher can hit two cheap lanes and win the lane count while NOT being the
/// real answer; without the score-agreement guard the collapse would discard the
/// true answer. When the two signals disagree (or there is no unique consensus, or
/// the max is a single lane), the full set is returned unchanged — recall is never
/// reduced. Pure (no `ctx`): unit-testable.
fn consensus_collapse(
    mut out: Vec<MemoryResult>,
    top_member_id: Option<u128>,
) -> Vec<MemoryResult> {
    let lanes = |m: &MemoryResult| m.contributing_retrievers.len();
    if out.is_empty() {
        return out;
    }
    let max_lanes = out.iter().map(&lanes).max().unwrap_or(0);
    if max_lanes < 2 {
        return out;
    }
    let consensus: Vec<usize> = out
        .iter()
        .enumerate()
        .filter(|(_, m)| lanes(m) == max_lanes)
        .map(|(i, _)| i)
        .collect();
    if consensus.len() == 1
        && out
            .get(consensus[0])
            .is_some_and(|m| Some(m.memory_id) == top_member_id)
    {
        let m = out.swap_remove(consensus[0]);
        out.clear();
        out.push(m);
    }
    out
}

/// The support count at which a memory is CROSS-LANE CORROBORATED — two or more
/// independent lanes agree it belongs to the cue. This is the single
/// corroboration gate the grounded commit (FIX B) requires before a value may
/// lead: grounded alone (support 1) is not enough; a second independent lane
/// (semantic / lexical / graph / HyPE) must confirm it.
const SUPPORT_CORROBORATED: u8 = 2;

/// Belonging-cosine floor at which the SEMANTIC lane counts as corroborating
/// [`support`] — deliberately far above the membership band's `MEMBERSHIP_ABS_FLOOR`
/// (0.20). The band floor is a junk cutoff for RECALL (which passages to keep);
/// this is a PRECISION cutoff for ABSTENTION (which passages actually *confirm*
/// belonging). They must differ because BGE-small cosines are compressed: an
/// off-topic cue scores ~0.45 against everything, clearing the recall floor but
/// confirming nothing. Set at the grounded strong-match sibling (`0.6`) so a
/// nonsense cue's ~0.45 top no longer counts as support (→ `None`) while a real
/// cue's strong top (or its HyPE / lexical / graph lane) still does. Calibrated
/// against the read fixtures; NOT independently tuned per corpus (see the
/// full-eval follow-up).
const STRONG_SEMANTIC_SUPPORT: f32 = 0.6;

/// HyPE answer-lead floor at which the HyPE lane counts as corroborating
/// [`support`]. HyPE cosine is cue↔hypothetical-question and suffers the same BGE
/// compression as the passage lane: an off-topic cue still matches *some*
/// generated question at ~0.5, so — once the semantic lane was strong-gated —
/// a loose HyPE match was the remaining signal keeping the abstention gate from
/// ever reaching `None`. Only a genuinely strong lead now CORROBORATES (blocks
/// abstention / feeds a grounded commit). This refines the original "every lane's
/// bar is loose; trust agreement across lanes" design for the COSINE lanes only:
/// under BGE compression a loose cosine lane is not real evidence, whereas the
/// discrete lanes (lexical / graph / grounded) still corroborate at their natural
/// bar. Answer-relevance ORDERING is unaffected — it sorts by raw HyPE score with
/// no floor. Same value as the semantic bar (one compressed-cosine problem);
/// calibrated against the read fixtures, full-eval sweep is the follow-up.
const STRONG_HYPE_SUPPORT: f32 = 0.6;

/// The cosine gap below which BGE-small cannot reliably distinguish two
/// passages (or two cue↔hypothetical-question matches). BGE-small cosines are
/// compressed, so a corpus of near-duplicate passages that differ only in one
/// exact token scores within a hair on cosine (measured ~0.727–0.739 across a
/// 50-way near-duplicate probe). Ordering such a set by that cosine alone is
/// query-independent and strands the one document the cue's exact token
/// matches. `order_by_answer_relevance` therefore quantises the two cosine-
/// scale signals (HyPE answer-relevance, passage cosine) at this resolution:
/// members within half a bucket of the best are treated as indistinguishable,
/// and the RRF `fused_score` — which carries the lexical / graph evidence a
/// cosine cannot — breaks the tie. A genuine cosine gap (larger than this)
/// still decides, so a real topical / answer signal is never overridden by
/// lexical coverage. Not per-corpus tuned: it is the model's discriminative
/// floor, comfortably above the observed near-duplicate spread.
const TOPICAL_COSINE_RESOLUTION: f32 = 0.05;

/// Count the INDEPENDENT lanes that confirm a memory belongs to the cue — the
/// ONE unifying corroboration signal the read path keys its belonging decisions
/// on (FIX A/B/C). Each lane is a distinct, independently-computed source of
/// evidence, so a memory two of them agree on is corroborated in a way no single
/// lane (however strong) can be:
///   * semantic — a STRONG semantic match (`strong_semantic`): the belonging
///     cosine clears `STRONG_SEMANTIC_SUPPORT`, NOT merely the membership band's
///     junk floor. This is deliberate. The old rule counted any band member or
///     any Semantic fan-out lane, both true above the 0.20 floor — but BGE-small
///     cosines are compressed, so an off-topic cue still scores ~0.45 against
///     everything and would count as "supported", defeating the abstention gate
///     (a nonsense cue could never fall to `None`). Requiring a strong cosine
///     makes the semantic lane a real belonging signal; genuinely answerable
///     paraphrase cues that sit below the strong bar are caught by the HyPE
///     answer-lead / lexical / graph lanes below, which are unchanged.
///   * lexical  — a Lexical fan-out lane (keyword / paraphrase surface match);
///   * graph    — a Graph fan-out lane (reached by the entity-graph walk);
///   * grounded — the memory is a source of the grounded typed-graph answer;
///   * HyPE     — its write-time hypothetical question answers the cue with a
///     STRONG lead (`hype >= STRONG_HYPE_SUPPORT`), for the same reason.
///
/// `lanes` MUST be the REAL fan-out contributions, not the synthetic `Graph`
/// lane that structured / anchor-direct hydration stamps on a row it fabricated
/// — otherwise the graph lane would double-count the grounded signal. The caller
/// (`build_membership`) passes `by_id`'s real lanes and leaves `lanes` empty for
/// a hydrate-only row, so such a row is supported only by its grounded / HyPE /
/// strong-semantic signals. Pure: unit-testable.
fn support(
    id: u128,
    lanes: &[RetrieverNameWire],
    strong_semantic: bool,
    grounded_sources: &HashSet<u128>,
    hype: &HashMap<u128, f32>,
) -> u8 {
    let has = |want: RetrieverNameWire| lanes.contains(&want);
    let mut n = 0u8;
    if strong_semantic {
        n += 1;
    }
    if has(RetrieverNameWire::Lexical) {
        n += 1;
    }
    if has(RetrieverNameWire::Graph) {
        n += 1;
    }
    if grounded_sources.contains(&id) {
        n += 1;
    }
    if hype.get(&id).copied().unwrap_or(0.0) >= STRONG_HYPE_SUPPORT {
        n += 1;
    }
    n
}

/// Order the membership set by ANSWER RELEVANCE, unconditionally (Phase 1).
///
/// The two signals the read conflates are distinct: `cos` (passage↔cue cosine)
/// measures TOPICAL adjacency ("is this memory about the topic"), while `hype`
/// (the best cosine between the cue and any hypothetical question generated FROM
/// the memory at write time) measures ANSWER RELEVANCE ("does this memory ANSWER
/// the cue"). The dominant read failure was a topically-adjacent memory LEADING
/// the list while the answering memory sat below it. So answer-relevance is the
/// PRIMARY sort key and topical cosine only the SECONDARY tiebreak — always, not
/// conditionally.
///
/// Membership is never changed: this only reorders the members already admitted,
/// so recall is untouched. When the HyPE signal is ABSENT or FLAT (no member's
/// answer-lead discriminates it from any other — e.g. no HyPE index, or a purely
/// lexical / paraphrase cue the question-bridge doesn't separate) there is no
/// answer-relevance signal to order by, so the existing (cosine / assembly) order
/// is kept verbatim — no regression on those cues. Pure: unit-testable.
fn order_by_answer_relevance(
    mut out: Vec<MemoryResult>,
    hype: &HashMap<u128, f32>,
    cos: &HashMap<u128, f32>,
) -> Vec<MemoryResult> {
    if out.len() < 2 {
        return out;
    }
    let h = |m: &MemoryResult| hype.get(&m.memory_id).copied().unwrap_or(0.0);
    let c = |m: &MemoryResult| {
        cos.get(&m.memory_id)
            .copied()
            .unwrap_or_else(|| m.similarity_score.max(0.0))
    };
    let f = |m: &MemoryResult| m.fused_score;

    // ── EXACT-TOKEN LEAD ────────────────────────────────────────────────────
    // A UNIQUE original-query lexical hit — exactly one member carries the cue's
    // exact token — is a high-precision exact-match signal that neither cosine,
    // HyPE, nor even the RRF fused_score reliably surfaces. On a dense near-
    // duplicate corpus the one document carrying the cue's exact token scores
    // within a hair of its neighbours on cosine (measured: 50 near-duplicates all
    // at ~0.73), its HyPE is sub-floor noise, and the semantic-rank spread buries
    // its small lexical bump in fused_score — yet it is the ONE document that
    // answers the cue. Lead with it, then the RRF fused order, then cosine.
    //
    // Three guards confine this to the genuine exact-match case so diverse
    // corpora never regress:
    //   * UNIQUENESS — a low-specificity term matches many members (all tagged
    //     Lexical), so `sole` is `None` and this path is skipped; PRF-expanded
    //     lexical tags are already stripped from `contributing_retrievers`, so
    //     only a genuine original-query match counts.
    //   * NO STANDOUT ANSWER-LEAD — no member's HyPE clearly leads the rest. This
    //     is measured as FLATNESS (top HyPE minus second-best HyPE below the
    //     resolution), NOT magnitude: BGE-small cosines are so compressed that
    //     even a random nonce cue scores ≥ the support floor against SOME
    //     generated question, so an absolute floor would never engage. A genuine
    //     answerable cue instead makes one member's answer-lead STAND OUT from
    //     the pack; when it does, the standard answer-relevance ordering below
    //     leads with that member instead.
    //   * TOPICALLY COMPETITIVE — the unique hit's own passage cosine is within
    //     BGE's discriminative resolution of the best member, so a real topical
    //     answer that clearly out-cosines a stray one-word lexical coincidence
    //     still wins (the coverage-bias guard), computed per-member rather than
    //     on the whole set's spread (robust to a few low-cosine members).
    let best_c = out.iter().map(&c).fold(f32::NEG_INFINITY, f32::max);
    let (best_h, second_h) = {
        let (mut b, mut s) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        for m in &out {
            let v = h(m);
            if v > b {
                s = b;
                b = v;
            } else if v > s {
                s = v;
            }
        }
        (b, s)
    };
    let answer_lead_stands_out = (best_h - second_h) >= TOPICAL_COSINE_RESOLUTION;
    if !answer_lead_stands_out {
        let lexical_members: Vec<u128> = out
            .iter()
            .filter(|m| {
                m.contributing_retrievers
                    .contains(&RetrieverNameWire::Lexical)
            })
            .map(|m| m.memory_id)
            .collect();
        if let [sole] = lexical_members.as_slice() {
            let sole = *sole;
            let sole_cos = out
                .iter()
                .find(|m| m.memory_id == sole)
                .map(&c)
                .unwrap_or(0.0);
            if (best_c - sole_cos) < TOPICAL_COSINE_RESOLUTION {
                out.sort_by(|a, b| {
                    // The unique exact-token match (`true`) ranks first.
                    (b.memory_id == sole)
                        .cmp(&(a.memory_id == sole))
                        .then_with(|| f(b).partial_cmp(&f(a)).unwrap_or(std::cmp::Ordering::Equal))
                        .then_with(|| c(b).partial_cmp(&c(a)).unwrap_or(std::cmp::Ordering::Equal))
                });
                return out;
            }
        }
        // No qualifying unique lexical hit — fall through to standard ordering.
    }

    // ── STANDARD ANSWER-RELEVANCE ORDERING ──────────────────────────────────
    // Answer-relevance (HyPE) is the PRIMARY key, topical cosine the SECONDARY
    // tiebreak. Flat / absent HyPE carries no answer-relevance signal, so the
    // incoming (cosine / assembly) order is preserved verbatim — the
    // lexical/paraphrase no-regression guarantee.
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for m in &out {
        let v = h(m);
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if (hi - lo) <= f32::EPSILON {
        return out;
    }
    // Stable sort: equal (hype, cos) members keep their incoming relative order.
    out.sort_by(|a, b| {
        h(b).partial_cmp(&h(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| c(b).partial_cmp(&c(a)).unwrap_or(std::cmp::Ordering::Equal))
    });
    out
}

/// The strong-match floor a grounded typed-graph value must clear before it may
/// be COMMITTED as the answer lead — deliberately above the grounded overlay's
/// loose `GROUNDED_MATCH_FLOOR` (0.5). At the floor a grounded match is close
/// enough to BOOST ordering but far too loose to lead the answer: it is the
/// cosine-vs-predicate-name match that once hijacked whole benchmarks. Requiring
/// 0.6 means only a clearly-answering typed-graph value takes the lead.
const GROUNDED_SINGLE_STRONG_MATCH: f32 = 0.6;

/// The lead the grounded answer commits: the source-memory ids that constitute
/// the answer, in grounded order (Single → one; Set → each distinct member, most
/// recent first), and the answer SHAPE they set.
struct CommitLead {
    ids: Vec<u128>,
    shape: AnswerKindWire,
}

/// Decide whether the grounded typed-graph answer should be COMMITTED as the
/// lead (Phase 2/3), returning the ordered lead ids and the answer shape, or
/// `None` to fall through to plain answer-relevance ordering.
///
/// The old `grounded_single_collapse` required full sem-lane consensus (the
/// grounded and semantic lanes agreeing on the same top-belonging memory) — a
/// deliberately conservative guard against the grounded-first regression (recall
/// 0.75→0.10, a loose predicate-name match on the WRONG subject hijacking the
/// answer with no episodic fallback). Two things make an aggressive commit safe
/// now and both are preserved:
///   1. ANCHOR-SCOPE — the grounded answer's subject is a resolved NAMED anchor
///      (`GroundedOutcome::Answer(_, anchor_scoped=true)`; see the enum doc). A
///      wrong-subject / self-loose match is never anchor-scoped, so it can never
///      commit.
///   2. EPISODIC RETENTION — the caller keeps the full membership list appended
///      BELOW the committed lead (see `apply_grounded_commit`). A wrong commit
///      can only MIS-ORDER; it can never drop the real answer from the set.
///
/// Together with the ≥0.6 strong-match floor AND the corroboration gate below,
/// these replace the sem-lane consensus gate: grounded commits often enough to
/// raise crisp-answer coverage, but only for a value an independent lane also
/// confirms, so a lone topical neighbor can never hijack the lead.
///
/// FIX B — CORROBORATION-GATED COMMIT. The ≥0.6 match floor is a NECESSARY floor,
/// not the whole gate: a grounded value may lead only when its source memory is
/// cross-lane CORROBORATED (`support >= SUPPORT_CORROBORATED`, i.e. grounded plus
/// at least one independent lane). `support_of` is the unifying [`support`]
/// lookup the caller passes; grounded itself already contributes one lane, so
/// this demands a second. This demotes a lone topical-cosine neighbor (grounded-
/// only, support 1) to the answer-relevance-ordered list while a genuinely-
/// answering fact multiple lanes agree on still commits — preserving the
/// `other` / `single_hop` gains, which are corroborated by construction.
///
/// Shapes:
///   * `AnswerKind::Single` (or a lone Set member) whose value clears 0.6 with a
///     real, corroborated source memory → commit that ONE memory, shape `Single`.
///   * `AnswerKind::Set` whose representative clears 0.6 and at least one member
///     is corroborated → commit ALL distinct source memories, shape `Many`. This
///     is the enumeration-completeness case ("what did X research?" → both facts).
///
/// Pure (no `ctx`): unit-testable.
fn grounded_commit(
    grounded: &GroundedOutcome,
    support_of: &impl Fn(u128) -> u8,
) -> Option<CommitLead> {
    let GroundedOutcome::Answer(a, anchor_scoped) = grounded else {
        return None;
    };
    // The WS-B subject-scope guard: only a value about the resolved named anchor
    // may lead.
    if !*anchor_scoped {
        return None;
    }
    match a.kind {
        AnswerKind::None => None,
        AnswerKind::Single => {
            let v = a.values.first()?;
            if v.match_score < GROUNDED_SINGLE_STRONG_MATCH {
                return None;
            }
            let raw = v.source_memory?.raw();
            if raw == 0 {
                return None;
            }
            // FIX B: the one source must be cross-lane corroborated to lead.
            if support_of(raw) < SUPPORT_CORROBORATED {
                return None;
            }
            Some(CommitLead {
                ids: vec![raw],
                shape: AnswerKindWire::Single,
            })
        }
        AnswerKind::Set => {
            // The representative (recency head) must clear the strong floor; the
            // whole set shares one match_score, so this gates the group.
            let head = a.values.first()?;
            if head.match_score < GROUNDED_SINGLE_STRONG_MATCH {
                return None;
            }
            let mut ids: Vec<u128> = Vec::new();
            let mut seen: HashSet<u128> = HashSet::new();
            for v in &a.values {
                if let Some(mid) = v.source_memory {
                    let raw = mid.raw();
                    if raw != 0 && seen.insert(raw) {
                        ids.push(raw);
                    }
                }
            }
            if ids.is_empty() {
                return None;
            }
            // FIX B: at least one member of the set must be cross-lane
            // corroborated. A whole set of grounded-only neighbors (none confirmed
            // by another lane) must not lead; a set the answer lanes agree on does.
            if !ids.iter().any(|id| support_of(*id) >= SUPPORT_CORROBORATED) {
                return None;
            }
            Some(CommitLead {
                ids,
                shape: AnswerKindWire::Many,
            })
        }
    }
}

/// Apply a grounded commit: the committed lead memories move to the FRONT (in
/// grounded order), and the rest of the membership is answer-relevance ordered
/// and RETAINED below — never cleared. This is the standing-guardrail contract:
/// the grounded value leads, but a wrong commit can only mis-order because the
/// real answer is still somewhere in the retained set. Pure: unit-testable.
fn apply_grounded_commit(
    out: Vec<MemoryResult>,
    lead: &CommitLead,
    hype: &HashMap<u128, f32>,
    cos: &HashMap<u128, f32>,
) -> Vec<MemoryResult> {
    let lead_set: HashSet<u128> = lead.ids.iter().copied().collect();
    let (mut leads, rest): (Vec<MemoryResult>, Vec<MemoryResult>) = out
        .into_iter()
        .partition(|m| lead_set.contains(&m.memory_id));
    // Leads in grounded (recency) order: index into `lead.ids`.
    leads.sort_by_key(|m| {
        lead.ids
            .iter()
            .position(|id| *id == m.memory_id)
            .unwrap_or(usize::MAX)
    });
    let mut result = leads;
    result.extend(order_by_answer_relevance(rest, hype, cos));
    result
}

/// Build the membership set for a cue: `S_struct ∪ S_sem`, deduped by memory id,
/// then ORDERED and (optionally) LED by the grounded commit.
///
/// Decision order (documented once, the single source of truth — every belonging
/// decision keys on the ONE unifying [`support`] signal):
///   0. Assemble the union — intersection (both lanes) ∪ structured-only
///      (hydrated) ∪ verified-semantic — then graph-anchor injection for
///      buried-fact recall. This fixes MEMBERSHIP (which memories belong) and is
///      never reduced downstream: recall is untouched.
///   1. COMPUTE SUPPORT — for every member, count the independent lanes that
///      confirm it (semantic / lexical / graph / grounded / HyPE) via `support_of`
///      over the REAL fan-out lanes. This single count drives steps 2 and the
///      caller's abstention (FIX C).
///   2. GROUNDED COMMIT (FIX B): if the grounded answer is anchor-scoped, clears
///      the strong floor, AND its source memory is cross-lane corroborated
///      (`support >= SUPPORT_CORROBORATED`), its value(s) become the LEAD and set
///      the answer SHAPE — Single (one source, FIX A keeps it Single) or a
///      cue-scoped Set (FIX A: distinct on-cue objects). An uncorroborated
///      grounded value does NOT commit; the set falls to answer-relevance
///      ordering. The rest of the membership is RETAINED below, never cleared (the
///      standing guardrail: a wrong commit can only mis-order, never drop the
///      real answer).
///   3. ANSWER-RELEVANCE ordering: the remaining / episodic members are ordered by
///      the HyPE answer-lead (PRIMARY) with passage cosine (SECONDARY) — the
///      answering memory leads over a topical neighbor. When no commit fires this
///      orders the whole set, then the lane-consensus collapse may crisp it.
///   4. Episodic is ALWAYS retained below the committed lead.
///   5. ABSTENTION (FIX C, applied by the caller): abstain only when the max
///      support across members is 0 — hence this returns that scalar.
///
/// Returns `(members, committed_shape, any_belongs)`: `committed_shape` is `Some`
/// only when a grounded commit fired; `any_belongs` is true when at least one
/// returned member carries a non-passage-cosine belonging signal (strong HyPE,
/// lexical, graph, or grounded) — the corroboration the caller's abstention gates
/// key on. A lone semantic-cosine member does NOT set it.
#[allow(clippy::too_many_arguments)]
fn build_membership(
    ranked: Vec<MemoryResult>,
    grounded: &GroundedOutcome,
    req: &RecallRequest,
    ctx: &OpsContext,
    cue_vec: &[f32; brain_embed::VECTOR_DIM],
    anchor: Option<EntityId>,
    client_requested_count: bool,
    hype_scores: &HashMap<u128, f32>,
) -> (Vec<MemoryResult>, Option<AnswerKindWire>, bool) {
    // Membership = candidates within the query-relative cosine band of the best
    // match. Recall-safe; shape-loose on dense single-subject corpora and cannot
    // abstain (BGE cosines too compressed). A reliable belonging/abstention
    // signal must NOT depend on a per-read model (cross-encoder = latency) — it
    // has to come from structure already computed in the fan-out; tracked below.
    let mut scored: Vec<(MemoryResult, f32)> = ranked
        .into_iter()
        .map(|m| {
            // Belonging score = the STRONGER of the direct passage cosine and the
            // score the fan-out already assigned this hit. The fan-out score
            // carries signals the raw passage vector does NOT: a HyPE question-
            // vector match (the cue matched a hypothetical question generated FROM
            // this memory — the paraphrase bridge), the best-of-lanes union, and
            // the in-txn overlay cosine. Re-scoring on the passage vector alone
            // would discard those and drop a paraphrase-/lexical-surfaced answer
            // below the membership band (measured: passage cosine ~0.5 on indirect
            // cues while HyPE surfaced the gold). Taking the max keeps such a hit
            // in the set and leaves direct-cosine hits unchanged.
            //
            // A pending in-txn write isn't in the HNSW yet (`vector_for` misses),
            // so its score comes entirely from `similarity_score` — preserving the
            // read-your-writes guarantee.
            let passage = ctx
                .semantic_retriever
                .vector_for(MemoryId::from_raw(m.memory_id))
                .map(|v| cosine(cue_vec, &v).max(0.0))
                .unwrap_or(0.0);
            let cos = passage.max(m.similarity_score.max(0.0));
            (m, cos)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Per-member belonging cosine — the SECONDARY key for answer-relevance
    // ordering (Phase 1). Captured before `scored` is consumed into `by_id`.
    let cos_by_id: HashMap<u128, f32> = scored.iter().map(|(m, c)| (m.memory_id, *c)).collect();

    let top = scored.first().map(|(_, c)| *c).unwrap_or(0.0);
    let band = top * MEMBERSHIP_REL_BAND;
    let mut s_sem: Vec<MemoryResult> = Vec::new();
    let mut sem_ids: HashSet<u128> = HashSet::new();
    if top >= MEMBERSHIP_ABS_FLOOR {
        for (m, c) in &scored {
            if *c >= MEMBERSHIP_ABS_FLOOR && *c >= band {
                sem_ids.insert(m.memory_id);
                s_sem.push(m.clone());
            }
        }
    }
    // Snapshot the VERIFIED-SEMANTIC set (the cosine-band members only) before the
    // lexical/graph loop widens `sem_ids` for membership assembly. The [`support`]
    // signal's semantic lane must count only a genuine semantic-band confirmation,
    // never a member that entered the membership set on a lexical/graph lane — that
    // lane is counted on its own, so mixing it in here would double-count.
    let verified_sem_ids = sem_ids.clone();
    // Lexical / graph belonging. A hit independently confirmed by a
    // non-semantic lane — it surfaced in the lexical or graph fan-out, or in
    // two lanes at once — belongs to the cue even when its embedding cosine
    // sits below the semantic floor. Keyword and paraphrase cues match
    // lexically at a low cosine; the fan-out already did that matching, so the
    // membership set must not discard it. This is the same cross-lane
    // confirmation the abstention gate trusts, applied here at construction so
    // a real lexical hit survives to be returned instead of being gated out by
    // the cosine floor.
    for (m, _c) in &scored {
        if sem_ids.contains(&m.memory_id) {
            continue;
        }
        let lane_confirmed = m.contributing_retrievers.len() >= 2
            || m.contributing_retrievers
                .iter()
                .any(|r| !matches!(r, RetrieverNameWire::Semantic));
        if lane_confirmed {
            sem_ids.insert(m.memory_id);
            s_sem.push(m.clone());
        }
    }
    let by_id: HashMap<u128, MemoryResult> =
        scored.into_iter().map(|(m, _)| (m.memory_id, m)).collect();

    // S_struct: the precise typed-graph answer — grounded value source memories.
    let mut struct_ids: Vec<MemoryId> = Vec::new();
    let mut struct_seen: HashSet<u128> = HashSet::new();
    if let GroundedOutcome::Answer(answer, _) = grounded {
        for v in &answer.values {
            if let Some(mid) = v.source_memory {
                if mid.raw() != 0 && struct_seen.insert(mid.raw()) {
                    struct_ids.push(mid);
                }
            }
        }
    }

    let mut out: Vec<MemoryResult> = Vec::new();
    let mut placed: HashSet<u128> = HashSet::new();

    // 1. Intersection: structured sources the verified-semantic lane also kept —
    //    both signals agree, strongest confirmation.
    for mid in &struct_ids {
        let raw = mid.raw();
        if sem_ids.contains(&raw) {
            if let Some(m) = by_id.get(&raw) {
                if placed.insert(raw) {
                    out.push(m.clone());
                }
            }
        }
    }
    // 2. Structured-only: precise answer the verification band dropped or the
    //    fan-out missed. Hydrate it — the typed graph reaching a memory the
    //    vector lane missed is the whole point.
    let mut hydrate_missing: Vec<MemoryId> = Vec::new();
    for mid in &struct_ids {
        let raw = mid.raw();
        if placed.contains(&raw) {
            continue;
        }
        match by_id.get(&raw) {
            Some(m) => {
                if placed.insert(raw) {
                    out.push(m.clone());
                }
            }
            None => hydrate_missing.push(*mid),
        }
    }
    if !hydrate_missing.is_empty() {
        if let Ok(rtxn) = ctx.executor.metadata.read_txn() {
            if let Ok(extra) = hydrate_memories_by_id(&rtxn, &hydrate_missing, req, ctx) {
                for e in extra {
                    if placed.insert(e.memory_id) {
                        out.push(e);
                    }
                }
            }
        }
    }
    // 3. Verified-semantic-only, in cosine-descending order.
    for m in s_sem {
        if placed.insert(m.memory_id) {
            out.push(m);
        }
    }

    // ── GRAPH-ANCHORED INJECTION (buried-fact recall) ──────────────────────
    // When the cue resolved to a real subject entity, pull that entity's OWN
    // facts directly — bypassing the cosine cutoff that governs S_sem. A buried
    // fact is one whose stored phrasing is semantically distant from the cue's
    // phrasing: it never enters any lane's top-K, and (when its predicate also
    // fails the grounded match floor) S_struct never reaches it either, so it
    // can't be recalled at all. But if the cue's subject is correctly resolved,
    // every fact ABOUT that subject is a candidate answer regardless of cosine —
    // belonging here is decided by the graph edge, not the embedding distance.
    //
    // Two direct sources, both via existing accessors (no new tables):
    //   * the subject's current statements → their first evidence memory, and
    //   * memories with a Mentions edge to the subject (Memory→Entity, so we
    //     walk it in reverse from the entity).
    // Hydration reuses `hydrate_memories_by_id`, which re-applies the same
    // space/kind/context/salience/age/tombstone filters as every other path, so
    // this never leaks a memory the caller couldn't otherwise see.
    //
    // Strictly additive and bounded: only ids NOT already placed are appended,
    // and the pull is capped at the per-read result scale before the safety
    // ceiling below trims the whole set. It can only ADD recall — it runs BEFORE
    // ordering so an injected buried fact that genuinely answers the cue is
    // answer-relevance ordered up rather than stranded at the tail. It never
    // reorders or drops an existing member.
    if let Some(anchor) = anchor {
        let already: HashSet<u128> = placed.clone();
        if let Some(extra) = anchor_direct_memories(anchor, &already, req, ctx) {
            for e in extra {
                if placed.insert(e.memory_id) {
                    out.push(e);
                }
            }
        }
    }

    // ── SUPPORT (the one unifying corroboration signal) ─────────────────────
    // For any memory id, count the INDEPENDENT lanes confirming it. Computed over
    // the REAL fan-out lanes (`by_id`) — NOT the synthetic `Graph` lane that
    // structured / anchor-direct hydration stamps on rows it fabricated — plus the
    // verified-semantic band, the grounded sources, and the HyPE answer-lead. This
    // is the single seam FIX B (commit corroboration) and FIX C (abstention) both
    // key on. A hydrate-only row is absent from `by_id`, so its lanes are empty and
    // it is supported only by its grounded / HyPE / semantic-band signals.
    let grounded_sources = &struct_seen;
    let support_of = |id: u128| -> u8 {
        let lanes: &[RetrieverNameWire] = by_id
            .get(&id)
            .map(|b| b.contributing_retrievers.as_slice())
            .unwrap_or(&[]);
        // Semantic counts as SUPPORT only when the belonging cosine is genuinely
        // strong — not merely inside the recall band. `verified_sem_ids` admits
        // anything above the 0.20 junk floor, which BGE compression makes true
        // even for an off-topic cue; gating on `STRONG_SEMANTIC_SUPPORT` here is
        // what lets the abstention gate reach `None`. Membership (recall) is
        // untouched: the member still belongs; it just doesn't *corroborate*.
        let strong_semantic = verified_sem_ids.contains(&id)
            && cos_by_id.get(&id).copied().unwrap_or(0.0) >= STRONG_SEMANTIC_SUPPORT;
        support(id, lanes, strong_semantic, grounded_sources, hype_scores)
    };

    // ── LEAD + ORDER (grounded commit, then answer-relevance) ───────────────
    // Grounded commit decides the lead + shape when it fires; otherwise the whole
    // set is answer-relevance ordered and the lane-consensus collapse may crisp
    // it. In BOTH branches the episodic set is retained below the lead — recall is
    // never discarded (the standing grounded-first tripwire).
    let no_commit = |out: Vec<MemoryResult>| -> (Vec<MemoryResult>, Option<AnswerKindWire>) {
        let out = order_by_answer_relevance(out, hype_scores, &cos_by_id);
        // The lane-consensus collapse keys on the NEW answer-relevance leader
        // (out[0] after ordering), so it only crisps to a Single when the
        // best-answering member is also the unique multi-lane consensus.
        let top_member_id = out.first().map(|m| m.memory_id);
        (consensus_collapse(out, top_member_id), None)
    };
    // FIX B: `grounded_commit` now enforces corroboration internally (a lead
    // source must clear `SUPPORT_CORROBORATED`), so an uncorroborated grounded
    // value returns `None` here and falls through to `no_commit`.
    let (mut out, committed_shape, committed_lead_count) =
        match grounded_commit(grounded, &support_of) {
            // Honor the commit only when at least one lead memory is actually present
            // in the visible set: a grounded source filtered out by the visibility
            // pass (space/kind/context/tombstone) must not set a shape with no backing
            // member. Otherwise fall through to plain answer-relevance ordering.
            Some(lead) if lead.ids.iter().any(|id| placed.contains(id)) => {
                let shape = lead.shape;
                let lead_n = lead.ids.iter().filter(|id| placed.contains(id)).count();
                (
                    apply_grounded_commit(out, &lead, hype_scores, &cos_by_id),
                    Some(shape),
                    lead_n,
                )
            }
            _ => {
                let (out, shape) = no_commit(out);
                (out, shape, 0)
            }
        };

    // ── EXACT-PATH INTRINSIC CARDINALITY ───────────────────────────────────
    // Ablatable block (revert by deleting it and keeping the plain
    // `membership_ceiling` truncation below): for a KEYED query — one whose cue
    // resolved to a subject anchor OR produced a grounded answer — the exact set
    // (grounded source memories ∪ anchor-direct statements/mentions) IS the
    // answer, with its own intrinsic size. Clipping it to the fuzzy
    // `DEFAULT_RECALL_RESULTS` window would drop true belonging-set members, so
    // the keyed ceiling is the hard allocation guard (`MAX_RECALL_RESULTS`),
    // honouring an explicit client `max_results` only when the caller actually
    // asked for one. A KEYLESS query has no exact key, so it keeps the fuzzy
    // default window unchanged.
    let keyed = anchor.is_some() || matches!(grounded, GroundedOutcome::Answer(..));
    let ceiling = if keyed {
        keyed_membership_ceiling(req, client_requested_count)
    } else {
        membership_ceiling(req)
    };
    // Safety ceiling only — never the answer size on the keyed path; on the
    // keyless path the verification band already bounds the set and this guards
    // a pathological flat distribution.
    out.truncate(ceiling as usize);

    tracing::debug!(
        target: "brain_ops::recall_trace",
        cue = %req.cue_text,
        path = "membership_cosine",
        top_cosine = top,
        structured = struct_ids.len(),
        semantic = sem_ids.len(),
        members = out.len(),
        committed_shape = ?committed_shape,
        "recall: membership set"
    );

    // FIX C: the caller abstains only when NO returned member has any cross-lane
    // support. Compute the max support over the FINAL member set with the same
    // corroboration seam the commit used (real fan-out lanes, not synthetic).
    // Belonging for the abstention decision: a member belongs to the cue only
    // when it carries evidence BEYOND the raw passage cosine. Under BGE-small
    // compression an off-topic / nonsense cue still passage-cosines a doc into
    // the low 0.6s — indistinguishable by magnitude from a genuinely weak-but-
    // real hit — so a LONE semantic lane cannot establish belonging no matter how
    // strong it looks. A non-passage signal must corroborate: a strong HyPE
    // answer-lead (the write-time question bridge), an ORIGINAL-query lexical hit
    // (the query's own terms matched — PRF echoes were already stripped from
    // `contributing_retrievers`), a graph hit, or a grounded typed-graph fact.
    // This is the structural discriminator the data shows (real cues corroborate
    // across independent lanes; nonsense gets one lone cosine), and it needs no
    // corpus-specific cosine bar.
    let belongs_of = |id: u128| -> bool {
        let lanes: &[RetrieverNameWire] = by_id
            .get(&id)
            .map(|b| b.contributing_retrievers.as_slice())
            .unwrap_or(&[]);
        let strong_hype = hype_scores.get(&id).copied().unwrap_or(0.0) >= STRONG_HYPE_SUPPORT;
        strong_hype
            || lanes.contains(&RetrieverNameWire::Lexical)
            || lanes.contains(&RetrieverNameWire::Graph)
            || grounded_sources.contains(&id)
    };
    let any_belongs = out.iter().any(|m| belongs_of(m.memory_id));

    // ── PRECISION DECISION ──────────────────────────────────────────────────
    // Calibrated selective shaping over the assembled set: commit a Single/Many
    // only when the lead is corroborated enough, else abstain with an honest None;
    // keep a committed Many minimal. The confidence signal is the SAME cross-lane
    // `support` the commit and abstention gates already use — the one signal that
    // separates correct from wrong answers; retrieval score is deliberately not an
    // input. Defaults are no-ops (commit_min_support = many_min_support = 0), so an
    // uncalibrated deploy reproduces the prior shape and abstention exactly; a
    // fitted `[precision]` calibration is what makes None reachable and Many tight.
    //
    // In-txn reads are EXEMPT — exactly as the caller's `any_belongs` abstention
    // is. An in-txn read is read-your-writes: a write the caller just made isn't
    // indexed yet, so it carries no cross-lane support and would be wrongly
    // abstained. The precision decision therefore never runs on the txn path.
    let (out, committed_shape) = if req.txn_id.is_some() {
        (out, committed_shape)
    } else {
        let supports: Vec<u8> = out.iter().map(|m| support_of(m.memory_id)).collect();
        let decision = crate::precision::decide(
            &crate::precision::DecisionInput {
                supports: &supports,
                committed_shape,
                committed_lead_count,
            },
            brain_core::PrecisionTuning::active(),
        );
        // Observability: emit the decision + every member's support (leads first) so
        // a calibration can be fit from a single eval run's logs — no threshold
        // sweep needed. This is the raw material for choosing the thresholds offline.
        tracing::debug!(
            target: "brain_ops::precision",
            cue = %req.cue_text,
            lead_support = supports.first().copied().unwrap_or(0),
            shape = ?decision.shape,
            lead_count = decision.lead_count,
            abstained = decision.abstained,
            members = out.len(),
            support_hist = ?supports,
            "recall: precision decision"
        );
        match decision.shape {
            // Calibrated abstention: drop the set so `recall_frame` yields an
            // explicit None. Only reachable when a `commit_min_support` is set.
            AnswerKindWire::None => (Vec::new(), None),
            shape => {
                // Minimal-Many trim: for an UNCOMMITTED decision, drop the members
                // below the committed lead set so a `Many` returns only its tight
                // answer (fewer, higher-consensus memories). Guarded: grounded
                // commits keep their full retained tail (their guardrail is that a
                // wrong commit can only mis-order); and with the default
                // many_min_support = 0 lead_count == out.len(), so nothing is
                // dropped. Only fires when an operator calibrates a trim.
                let out = if committed_shape.is_none() && decision.lead_count < out.len() {
                    out.into_iter().take(decision.lead_count).collect()
                } else {
                    out
                };
                (out, Some(shape))
            }
        }
    };
    let any_belongs = any_belongs && !out.is_empty();

    (out, committed_shape, any_belongs)
}

/// Runaway guard on the anchor-direct EXACT pull (entity→statements +
/// incoming Mentions). The exact path is keyed: when the cue resolves to a
/// subject entity, every fact ABOUT that subject belongs to the answer, so its
/// size is INTRINSIC (single value / list / range) — it must not be clipped to
/// the fuzzy `DEFAULT_RECALL_RESULTS` window the associative lane uses. The only
/// bound here is the same hard allocation ceiling that bounds every other path
/// ([`MAX_RECALL_RESULTS`]), so a hub entity with thousands of mentions still
/// can't blow up the candidate set or the latency budget. This is the runaway
/// guard, NOT the answer size.
const ANCHOR_DIRECT_PULL_CAP: usize = MAX_RECALL_RESULTS as usize;

/// Collect memories that are DIRECTLY about the resolved subject entity, for the
/// graph-anchored injection in [`build_membership`]: the first evidence memory
/// of each of the subject's current statements, plus memories that mention the
/// subject via a `Mentions` edge. Ids already in `exclude` are skipped before
/// any redb work. The collected ids are hydrated through
/// [`hydrate_memories_by_id`] so they carry the same visibility filters as the
/// rest of the set. Returns `None` only when the read txn can't be opened —
/// degrading silently to the fan-out, never failing the read.
fn anchor_direct_memories(
    anchor: EntityId,
    exclude: &HashSet<u128>,
    req: &RecallRequest,
    ctx: &OpsContext,
) -> Option<Vec<MemoryResult>> {
    use brain_core::NodeRef;
    use brain_metadata::tables::edge::walk_incoming;
    use brain_metadata::{statement_list, StatementListFilter};

    let rtxn = ctx.executor.metadata.read_txn().ok()?;

    let mut ids: Vec<MemoryId> = Vec::new();
    let mut seen: HashSet<u128> = HashSet::new();
    let mut push = |mid: MemoryId, ids: &mut Vec<MemoryId>| {
        let raw = mid.raw();
        if raw != 0 && !exclude.contains(&raw) && seen.insert(raw) {
            ids.push(mid);
        }
    };

    // 1. The subject's own current statements → their first evidence memory.
    let scope =
        brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
    if let Ok(stmts) = statement_list(
        &rtxn,
        scope,
        &StatementListFilter {
            subject: Some(anchor),
            current_only: true,
            limit: ANCHOR_DIRECT_PULL_CAP,
            ..Default::default()
        },
    ) {
        for s in stmts {
            if let brain_core::EvidenceRef::Inline(ev) = &s.evidence {
                if let Some(first) = ev.first() {
                    push(first.memory_id, &mut ids);
                    if ids.len() >= ANCHOR_DIRECT_PULL_CAP {
                        break;
                    }
                }
            }
        }
    }

    // 2. Memories that mention the subject. The Mentions edge is Memory→Entity,
    //    so the mentioning memories are the anchor's INCOMING Mentions edges.
    if ids.len() < ANCHOR_DIRECT_PULL_CAP {
        if let Ok(rows) = walk_incoming(
            &rtxn,
            NodeRef::Entity(anchor),
            Some(brain_core::EdgeKindRef::Mentions),
        ) {
            for (_, from, _, _) in rows {
                if let NodeRef::Memory(mid) = from {
                    push(mid, &mut ids);
                    if ids.len() >= ANCHOR_DIRECT_PULL_CAP {
                        break;
                    }
                }
            }
        }
    }

    if ids.is_empty() {
        return Some(Vec::new());
    }
    hydrate_memories_by_id(&rtxn, &ids, req, ctx).ok()
}

/// Low junk floor: cue↔memory cosine below this is clearly irrelevant. NOT an
/// abstention mechanism — BGE-small cosines are too compressed for that (see
/// `build_membership`); robust abstention needs a cross-encoder verifier.
const MEMBERSHIP_ABS_FLOOR: f32 = 0.20;

/// A memory belongs only if its cue cosine is within this fraction of the best
/// match's — query-relative, so a lone strong match → Single and a co-relevant
/// cluster → Many. "Within ~15% of the best." Principled default, not tuned.
const MEMBERSHIP_REL_BAND: f32 = 0.85;

/// Internal safety ceiling on the membership set size for the KEYLESS (fuzzy)
/// path. Not a ranking knob and not caller intent — the adaptive gap decides the
/// real set; this only caps a degenerate flat-distribution result so the
/// response can't balloon.
/// Whether a memory passes the `age_bound` filter. `age_bound` is an
/// event-time lower bound: a memory passes iff its event time
/// (`occurred_at`, falling back to `created_at` when unset) is at or after
/// the bound. A `None` bound admits everything.
fn passes_age_bound(bound: Option<u64>, occurred_at: Option<u64>, created_at: u64) -> bool {
    match bound {
        None => true,
        Some(b) => occurred_at.unwrap_or(created_at) >= b,
    }
}

fn membership_ceiling(req: &RecallRequest) -> u32 {
    let cap = if req.max_results == 0 {
        DEFAULT_RECALL_RESULTS
    } else {
        req.max_results
    };
    cap.min(MAX_RECALL_RESULTS)
}

/// Ceiling for the KEYED (exact-anchor / grounded) path. The belonging set has
/// an intrinsic cardinality, so when the caller did NOT ask for a count
/// (`client_requested_count == false`, the common "I didn't ask for a count"
/// case) the set is NOT clipped to the fuzzy default-50 window — it is bounded
/// only by the hard allocation guard. An explicit client `max_results` is still
/// honoured as a caller cap. This is the runaway guard, never the answer size.
///
/// `req.max_results` has already been normalised by the time this runs (a `0`
/// became [`DEFAULT_RECALL_RESULTS`]), which is exactly why the caller's original
/// intent is threaded in separately as `client_requested_count`.
fn keyed_membership_ceiling(req: &RecallRequest, client_requested_count: bool) -> u32 {
    if client_requested_count {
        req.max_results.min(MAX_RECALL_RESULTS)
    } else {
        MAX_RECALL_RESULTS
    }
}

/// Build the response frame from the router's chosen memories.
///
/// The answer SHAPE is `committed_shape` when a grounded commit fired — the
/// committed value LEADS the list and sets the shape (Single / Many-as-Set),
/// while the full retained membership still ships beneath it (episodic is never
/// discarded). Otherwise the shape is derived from the member count: none / one /
/// many. An empty list is always `None`, whatever the commit intended (abstention
/// only ever empties a set the grounded layer did not answer, so this is a
/// defensive belt-and-braces rather than a live path).
fn recall_frame(
    memories: Vec<MemoryResult>,
    committed_shape: Option<AnswerKindWire>,
    trace: Option<RecallTrace>,
) -> RecallResponseFrame {
    let answer_kind = if memories.is_empty() {
        AnswerKindWire::None
    } else {
        committed_shape.unwrap_or(match memories.len() {
            1 => AnswerKindWire::Single,
            _ => AnswerKindWire::Many,
        })
    };
    let cumulative_count = u32::try_from(memories.len()).unwrap_or(u32::MAX);
    RecallResponseFrame {
        answer_kind,
        memories,
        is_final: true,
        cumulative_count,
        estimated_remaining: None,
        trace,
    }
}

/// Result of the grounded attempt: either a confident precise answer, or
/// none — in which case the unified read path degrades to episodic.
enum GroundedOutcome {
    /// A confident precise answer (`Single`/`Set`). The bool records whether the
    /// answer is ANCHOR-SCOPED — its subject is a resolved NAMED (non-self)
    /// anchor. Only an anchor-scoped answer may be COMMITTED as the lead
    /// (`grounded_commit`): this is the WS-B subject-scope guard that makes the
    /// aggressive commit safe. A loose match on the WRONG subject (the old
    /// grounded-first regression, recall 0.75→0.10) can never hijack the lead
    /// because its subject is not the resolved anchor, so it never commits.
    Answer(GroundedAnswer, bool),
    /// No confident grounded answer (no subject resolved, or no predicate
    /// cleared the match floor).
    NoAnswer,
}

/// Resolve candidate subject entities from the cue text and pick a grounded
/// answer across them. The grounded match is SEMANTIC — `cue_vec` cosine
/// against each subject's stored predicate / relation-type embeddings (see
/// `grounded_answer`).
///
/// We pick the **globally best-scoring** match across ALL candidates, not the
/// first candidate that clears the floor. First-match-wins was a bug: the space
/// self-entity is always candidate[0], and a loose self-predicate (e.g. the
/// space's `usually_reviews` against "who does Niraj report to") could clear the
/// floor and short-circuit before the actually-named subject's exact predicate
/// (`reports_to`, a far higher cosine) was ever tried. Comparing all candidates
/// by cosine lets the strong, specific match win. On a near-tie we prefer a
/// named (non-self) subject, since a cue that names someone is asking about
/// them, not the writer.
/// How many statement-question hits to probe for the slot-projection overlay.
/// A handful is plenty: the best hit that clears the floor and can project its
/// slot wins; the small window lets a strong-but-unanswerable hit (e.g. a Time
/// slot on a statement whose `event_at` is absent) yield to the next candidate.
const SLOT_PROJECTION_PROBE_K: usize = 8;

/// Whether a slot-projection candidate's subject is inside the cue's resolved
/// anchor scope.
///
/// The statement-question bridge index is GLOBAL — it is not partitioned by
/// subject, so a probe with the cue vector can match a question generated from
/// ANY person's fact. Without this check a "when did Melanie run a charity
/// race" cue would happily match a `Slot::Time` question generated from
/// CAROLINE's event and project Caroline's race time as the confident answer
/// (which also defeats abstention, since a spurious `Answer` switches the
/// gates off).
///
/// Keyed strictly on `EntityId` equality: coreference fragmentation (Mel vs
/// Melanie stored as separate nodes) is fixed at the data layer, and once those
/// merge `subject == anchor` holds naturally — this code needs no nickname
/// knowledge. An EMPTY anchor set means the cue named no subject (only the
/// always-present self fallback resolved, e.g. "when was the trip"): there is
/// nothing to check against, so the projection stays global (its historical
/// behavior). A non-`Entity` subject (`Memory` / `Pending`) can never equal a
/// named anchor, so it survives only under an empty scope.
fn statement_subject_in_scope(subject: SubjectRef, anchors: &HashSet<EntityId>) -> bool {
    if anchors.is_empty() {
        return true;
    }
    matches!(subject, SubjectRef::Entity(id) if anchors.contains(&id))
}

/// Whether a slot-projection hit may be returned as a grounded answer, given
/// the resolved anchor scope. Combines the subject-scope check with the
/// no-anchor Time-safety policy.
///
/// A subjectless `Slot::Time` hit is REFUSED. When the cue resolved no named
/// subject (`anchors` empty), the global bridge probe can confidently project an
/// unrelated statement's event time — e.g. a "when was the trip" cue matching a
/// `Slot::Time` question generated from some OTHER person's trip and returning
/// their date as the answer. A "when" with no grounded subject cannot be
/// answered safely, so it falls through to the episodic path rather than risk a
/// wrong-answer. `Slot::Object` / `Slot::Subject` may still project globally
/// (they resolve a value the cue named directly and must still clear the strong
/// floor), so only Time is gated here; with a named anchor the ordinary
/// subject-scope check governs every slot.
fn slot_hit_projectable(slot: Slot, subject: SubjectRef, anchors: &HashSet<EntityId>) -> bool {
    if anchors.is_empty() && matches!(slot, Slot::Time) {
        return false;
    }
    statement_subject_in_scope(subject, anchors)
}

/// Whether a statement id belongs to the caller's `(namespace, space)` scope.
///
/// The statement-question bridge index probed by the slot-projection path is
/// shard-GLOBAL — it carries no tenant partition, so a probe with the caller's
/// cue vector can match a question generated from ANY tenant's fact. Loading the
/// hit's row via `statement_get` does NOT re-check scope (the `Statement` value
/// carries no scope), so without this guard a foreign-space statement whose
/// object/subject-slot question matched could project as a confident answer AND
/// (worse) flip the abstention gates off (a spurious `Answer` disables both
/// `apply_anchor_abstention` and `apply_kind_presence_abstention`). The
/// predicate walk is immune because it reads scope-prefixed secondary indexes;
/// this restores the same isolation for the global bridge probe by loading the
/// primary row's stamped scope and comparing it to the caller's.
///
/// A missing row is out of scope (`false`) — never fabricate an answer.
fn statement_in_caller_scope(
    rtxn: &redb::ReadTransaction,
    sid: brain_core::StatementId,
    caller_scope: brain_metadata::RowScope,
) -> Result<bool, OpError> {
    use brain_metadata::tables::statement::STATEMENTS_TABLE;
    let table = rtxn
        .open_table(STATEMENTS_TABLE)
        .map_err(|e| OpError::Internal(format!("slot-projection scope open: {e}")))?;
    let row = table
        .get(&sid.to_bytes())
        .map_err(|e| OpError::Internal(format!("slot-projection scope get: {e}")))?;
    Ok(matches!(row, Some(guard) if guard.value().scope() == caller_scope))
}

/// Build the cue-scoped OBJECT set for a `Slot::Object` slot-projection match
/// (FIX A). The set is the DISTINCT objects of the statement-question bridge
/// hits that (a) probe the Object slot, (b) clear the strong floor against THIS
/// cue, (c) are current (not tombstoned / superseded), (d) are about a resolved
/// anchor (see [`slot_hit_projectable`]), and (e) project to a meaningful value.
///
/// This REPLACES `(subject, predicate)` enumeration: it is PRECISE (a member is
/// included only when its own slot-question is cue-relevant, so unrelated
/// neighborhood facts are dropped) and COMPLETE across predicates (the bridge
/// hits span predicates, so a synonym / related predicate folds into the same
/// set). Deduped by object; score-descending, so the strongest, most cue-relevant
/// member heads the set. Reads only current, visible rows — never fabricates.
fn cue_scoped_object_set(
    rtxn: &redb::ReadTransaction,
    hits: &[(brain_core::StatementId, Slot, f32)],
    anchors: &HashSet<EntityId>,
    caller_scope: brain_metadata::RowScope,
) -> Result<Vec<GroundedValue>, OpError> {
    let mut values: Vec<GroundedValue> = Vec::new();
    for &(sid, slot, score) in hits {
        // Hits arrive globally score-descending; once below the floor nothing
        // later can qualify, on any slot.
        if score < SLOT_PROJECTION_STRONG_FLOOR {
            break;
        }
        if !matches!(slot, Slot::Object) {
            continue;
        }
        // Tenant isolation for the GLOBAL bridge probe: drop any hit whose
        // statement is not in the caller's scope before it can become a member.
        if !statement_in_caller_scope(rtxn, sid, caller_scope)? {
            continue;
        }
        let Some(statement) = brain_metadata::statement_get(rtxn, sid)
            .map_err(|e| OpError::Internal(format!("cue-scoped object set statement_get: {e}")))?
        else {
            continue;
        };
        if statement.tombstoned || statement.superseded_by.is_some() {
            continue;
        }
        if !slot_hit_projectable(Slot::Object, statement.subject, anchors) {
            continue;
        }
        let Some(v) = project_statement_slot(rtxn, &statement, Slot::Object, score)
            .map_err(|e| OpError::Internal(format!("cue-scoped object set project: {e}")))?
        else {
            continue;
        };
        // Dedup by object — the same object asserted by two statements is one
        // member.
        if values.iter().any(|e| e.object == v.object) {
            continue;
        }
        values.push(v);
    }
    Ok(values)
}

/// Reified slot-projection grounded answer.
///
/// Probes the per-statement question-bridge index with the cue vector, then, in
/// descending score order, loads the first CURRENT statement whose matched slot
/// projects to a value, clears [`SLOT_PROJECTION_STRONG_FLOOR`], AND is about one
/// of the resolved `anchors` (see [`statement_subject_in_scope`]). The result is
/// a grounded value (or, for the OBJECT slot, a cue-scoped SET — see
/// [`cue_scoped_object_set`]) carrying the projected slot (object / event time /
/// subject name). Returns `None` when no hit clears the floor, is subject-scoped
/// out, or fails to project — the caller then falls through to the
/// predicate-embedding walk, so this never fabricates an answer and never
/// regresses the episodic fallback.
fn slot_projection_grounded(
    rtxn: &redb::ReadTransaction,
    ctx: &OpsContext,
    cue_vec: &[f32; brain_embed::VECTOR_DIM],
    anchors: &HashSet<EntityId>,
    caller_scope: brain_metadata::RowScope,
    req: &RecallRequest,
) -> Result<Option<GroundedAnswer>, OpError> {
    let hits = ctx
        .semantic_retriever
        .statement_slot_hits_for_query(cue_vec, SLOT_PROJECTION_PROBE_K);

    // Compact top-hit summary for the grounded trace. Built once and logged
    // once (never per-hit) so a large probe window can't spam the log: the first
    // few (statement, slot, score) triples the bridge returned, strongest first.
    let hit_summary: Vec<String> = hits
        .iter()
        .take(3)
        .map(|(sid, slot, score)| format!("{sid:?}/{slot:?}@{score:.3}"))
        .collect();

    for &(sid, slot, score) in &hits {
        // Hits arrive descending by score; once below the floor nothing later
        // can clear it, so stop rather than scan the tail.
        if score < SLOT_PROJECTION_STRONG_FLOOR {
            break;
        }
        // Tenant isolation for the GLOBAL bridge probe: a hit whose statement is
        // not in the caller's scope must never project — it would both leak a
        // foreign fact AND, as a spurious `Answer`, disable the abstention gates
        // (`apply_anchor_abstention` / `apply_kind_presence_abstention`). Checked
        // BEFORE loading/projecting so a foreign row can produce nothing.
        if !statement_in_caller_scope(rtxn, sid, caller_scope)? {
            continue;
        }
        let Some(statement) = brain_metadata::statement_get(rtxn, sid)
            .map_err(|e| OpError::Internal(format!("slot projection statement_get: {e}")))?
        else {
            continue;
        };
        // A superseded / tombstoned fact must never surface as the answer.
        if statement.tombstoned || statement.superseded_by.is_some() {
            continue;
        }
        // Subject-scope the GLOBAL bridge probe: when the cue named a subject the
        // projected statement MUST be about one of the resolved anchors, so a
        // hit generated from a different person's fact can't answer here. Empty
        // scope (no named subject) leaves Object/Subject projections global but
        // REFUSES a subjectless Time projection (a "when" with no resolved
        // subject cannot be grounded safely — see `slot_hit_projectable`).
        if !slot_hit_projectable(slot, statement.subject, anchors) {
            continue;
        }
        let projected: Option<GroundedValue> =
            project_statement_slot(rtxn, &statement, slot, score)
                .map_err(|e| OpError::Internal(format!("slot projection: {e}")))?;
        if let Some(value) = projected {
            // FIX A — CUE-SCOPED SET MEMBERSHIP. For the OBJECT slot the answer's
            // SET is built from the SQ Object hits that themselves clear the strong
            // floor against THIS cue (see `cue_scoped_object_set`), NOT by
            // enumerating every current statement sharing the matched fact's
            // (subject, predicate). That old enumeration was over-broad (it pulled
            // unrelated neighborhood facts) AND incomplete (it missed members
            // stored under a related predicate). Cue-scoped is PRECISE — a member
            // is included only when its own slot-question is cue-relevant — and
            // COMPLETE — the bridge hits span predicates, so a synonym / related
            // predicate folds into one set. Distinct on-cue objects → Set; one
            // distinct object → Single. Time/Subject slots stay Single (a fact has
            // one event time / one subject).
            let answer = if matches!(slot, Slot::Object) {
                let mut values = cue_scoped_object_set(rtxn, &hits, anchors, caller_scope)?;
                // The chosen hit projected, so its object is always a member; keep
                // it as the sole member if the scan somehow produced nothing.
                if values.is_empty() {
                    values.push(value);
                }
                let kind = if values.len() > 1 {
                    AnswerKind::Set
                } else {
                    AnswerKind::Single
                };
                GroundedAnswer { kind, values }
            } else {
                GroundedAnswer {
                    kind: AnswerKind::Single,
                    values: vec![value],
                }
            };
            tracing::info!(
                target: "brain_ops::grounded_trace",
                cue = %req.cue_text,
                anchors = anchors.len(),
                hits = ?hit_summary,
                chosen_statement = ?sid,
                chosen_slot = ?slot,
                chosen_score = score,
                subject_scoped = !anchors.is_empty(),
                event_at_present = statement.event_at_unix_nanos.is_some(),
                answer_kind = ?answer.kind,
                members = answer.values.len(),
                outcome = "answer",
                "grounded: slot-projection answer"
            );
            return Ok(Some(answer));
        }
        // Slot couldn't project (e.g. Time with no event_at) — try the next hit.
    }
    tracing::info!(
        target: "brain_ops::grounded_trace",
        cue = %req.cue_text,
        anchors = anchors.len(),
        hits = ?hit_summary,
        outcome = "no_slot_projection",
        "grounded: slot-projection produced no answer"
    );
    Ok(None)
}

fn best_grounded_for_cue(
    req: &RecallRequest,
    ctx: &OpsContext,
    cue_vec: &[f32; brain_embed::VECTOR_DIM],
) -> Result<GroundedOutcome, OpError> {
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("recall grounded read_txn: {e}")))?;

    // Resolve subject candidates FIRST — the slot-projection overlay below must
    // be scoped to them. `subject_candidates_from_cue` always yields the caller
    // self-entity as candidate[0]; the NAMED subjects mined from the cue are the
    // remaining (non-self) ids. Those named subjects are the anchor scope: a
    // cue that names someone is asking about them, so the global bridge probe
    // must not project a different person's fact (see `slot_projection_grounded`
    // / `statement_subject_in_scope` for the wrong-subject trap this closes).
    // When the cue names no subject (only the self fallback resolves, e.g. "when
    // was the trip") the scope is empty and the projection stays global — it
    // can't subject-check, but there is no named anchor to check against.
    let candidates = subject_candidates_from_cue(&rtxn, req, ctx)?;
    let self_id = EntityId::from(ctx.executor.caller_space.0.into_bytes());
    let anchors: HashSet<EntityId> = candidates
        .iter()
        .copied()
        .filter(|id| *id != self_id)
        .collect();

    // Reified slot-projection overlay: probe the per-statement question-bridge
    // index directly with the cue and project the matched slot of the best
    // (statement, slot) hit that clears the strong floor AND is about a resolved
    // anchor. The slot tag names EXACTLY which slot the question asked for —
    // object, event time, or subject — so a "when …" / "who …" cue is answered
    // by the fact's time / subject slot. This is a SINGLE-HOP (depth-0)
    // projection about the anchor by construction; we keep it as a candidate but
    // do NOT return early, so a genuine multi-hop walk answer can override it
    // below (a shallow slot answer must not short-circuit a real chain).
    let caller_scope =
        brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
    let slot_answer = slot_projection_grounded(&rtxn, ctx, cue_vec, &anchors, caller_scope, req)?;

    // Captured before the loop consumes `candidates`, for the decision trace.
    let candidate_count = candidates.len();

    // Multi-hop walk over the resolved subjects: a bounded depth-discounted beam
    // walk running the 1-hop matcher at every reachable node (nearest answer wins
    // unless a deeper one out-scores the per-hop discount). Reduces to the 1-hop
    // answer (depth 0) when no edge embeds close to the cue, so single-hop
    // questions are unaffected — no read LLM. The chosen hop DEPTH is tracked so
    // a genuine chain (depth >= 1) can be told apart from a shallow answer.
    // Near-tie band (`TIE_EPS`) within which we prefer a named subject over self.
    const TIE_EPS: f32 = 0.02;
    // (answer, score, is_self, depth)
    let mut best: Option<(GroundedAnswer, f32, bool, usize)> = None;
    for subject in candidates {
        let grounded_scope =
            brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
        let (answer, depth) = grounded_answer_walk(&rtxn, grounded_scope, subject, cue_vec)
            .map_err(|e| OpError::Internal(format!("recall grounded walk: {e}")))?;
        if matches!(answer.kind, AnswerKind::None) {
            continue;
        }
        let score = answer.values.first().map(|v| v.match_score).unwrap_or(0.0);
        let is_self = subject == self_id;
        let take = match &best {
            None => true,
            Some((_, best_score, best_is_self, _)) => {
                score > *best_score + TIE_EPS
                    || ((score - *best_score).abs() <= TIE_EPS && *best_is_self && !is_self)
            }
        };
        if take {
            best = Some((answer, score, is_self, depth));
        }
    }

    // Prefer a genuine MULTI-HOP walk answer over the shallow slot projection.
    // The slot projection is always a single-hop (depth-0) statement about the
    // anchor; when the walk DESCENDED to a deeper node (`depth >= 1`) AND that
    // deep answer is itself a STRONG grounded match (its depth-discounted score
    // clears the same strong floor the slot projection required,
    // `SLOT_PROJECTION_STRONG_FLOOR`), it expresses a chain the projection
    // structurally cannot — so it wins. This never fires for a single-hop cue:
    // the walk only returns `depth >= 1` when a deeper node out-competed the
    // anchor's own answer after the per-hop discount, which a single-hop cue
    // (strongest at the anchor) never produces. The two score scales
    // (question-bridge cosine vs predicate cosine) are never compared against
    // each other — the deep walk answer must independently clear the strong floor
    // before it may override. When the walk found nothing deep-and-strong the
    // slot projection is used unchanged; when neither fired the read falls
    // through to episodic, leaving the answers grounded simply hadn't extracted
    // untouched.
    // ANCHOR-SCOPE flag for the commit guard (`grounded_commit`). An answer is
    // anchor-scoped when its subject is a resolved NAMED (non-self) anchor:
    //   * the slot projection was subject-scoped to a named anchor iff the cue
    //     resolved one (`!anchors.is_empty()`); a subjectless/self projection is
    //     NOT a named anchor and must not aggressively commit.
    //   * a walk answer is anchor-scoped iff the winning subject was not self
    //     (`!is_self`).
    // Only an anchor-scoped answer may be COMMITTED as the lead — a self / loose
    // match can still BOOST via S_struct but never hijacks the answer shape.
    let slot_scoped = !anchors.is_empty();
    let outcome = match (slot_answer, best) {
        (Some(slot), Some((answer, score, is_self, depth))) => {
            if depth >= 1 && score >= SLOT_PROJECTION_STRONG_FLOOR {
                tracing::info!(
                    target: "brain_ops::grounded_trace",
                    cue = %req.cue_text,
                    anchors = anchors.len(),
                    candidates = candidate_count,
                    score,
                    depth,
                    outcome = "walk_override_slot",
                    "grounded: multi-hop walk overrides shallow slot projection"
                );
                GroundedOutcome::Answer(answer, !is_self)
            } else {
                // The slot projection already traced its own answer.
                GroundedOutcome::Answer(slot, slot_scoped)
            }
        }
        (Some(slot), None) => GroundedOutcome::Answer(slot, slot_scoped),
        (None, Some((answer, score, is_self, depth))) => {
            tracing::info!(
                target: "brain_ops::grounded_trace",
                cue = %req.cue_text,
                anchors = anchors.len(),
                candidates = candidate_count,
                score,
                depth,
                outcome = "answer",
                "grounded: predicate-walk decision"
            );
            GroundedOutcome::Answer(answer, !is_self)
        }
        (None, None) => {
            tracing::info!(
                target: "brain_ops::grounded_trace",
                cue = %req.cue_text,
                anchors = anchors.len(),
                candidates = candidate_count,
                outcome = "no_answer",
                "grounded: predicate-walk decision"
            );
            GroundedOutcome::NoAnswer
        }
    };
    Ok(outcome)
}

/// Hydrate `MemoryResult`s straight from `MEMORIES_TABLE` (+ `TEXTS_TABLE`
/// when `include_text`) for a set of memory ids — the structured branch's
/// projector, which answers from stored ids rather than a retrieval result.
/// Applies the same post-filters as the fan-out projector (space scope, kind,
/// context, salience, age, tombstone) so a structured answer never leaks a
/// memory the caller could not otherwise see. A structured hit carries no
/// retrieval score; its `similarity_score`/`confidence`/`fused_score` are
/// `1.0` (an exact stored match) and its contributing lane is `Graph`.
fn hydrate_memories_by_id(
    rtxn: &redb::ReadTransaction,
    ids: &[MemoryId],
    req: &RecallRequest,
    ctx: &OpsContext,
) -> Result<Vec<MemoryResult>, OpError> {
    use brain_metadata::tables::memory::MEMORIES_TABLE as MEM_T;

    // Space scope of the structured projector. The namespace (tenant) wall is
    // applied separately and unconditionally on every row below; this is only
    // the inner space narrowing. Space-scoped recall pins the caller's own
    // space; namespace-wide recall drops it (`None` = any space the caller
    // owns within its namespace), so a namespace-wide answer spans the
    // caller's spaces but never crosses the namespace wall.
    let space_scope: Option<HashSet<[u8; 16]>> = match recall_scope_mode(req) {
        brain_metadata::ScopeMode::Space => Some(
            [<[u8; 16]>::from(ctx.executor.caller_space)]
                .into_iter()
                .collect(),
        ),
        brain_metadata::ScopeMode::Namespace => None,
    };
    let kind_filter: Option<HashSet<MemoryKindWire>> = req
        .kind_filter
        .as_ref()
        .map(|v| v.iter().copied().collect());
    let session_filter: Option<HashSet<u64>> = req
        .session_filter
        .as_ref()
        .map(|v| v.iter().copied().collect());

    let table = rtxn
        .open_table(MEM_T)
        .map_err(|e| OpError::Internal(format!("structured recall open MEMORIES_TABLE: {e}")))?;
    let texts_table =
        if req.include_text {
            Some(rtxn.open_table(TEXTS_TABLE).map_err(|e| {
                OpError::Internal(format!("structured recall open TEXTS_TABLE: {e}"))
            })?)
        } else {
            None
        };

    let mut out: Vec<MemoryResult> = Vec::with_capacity(ids.len());
    for &memory_id in ids {
        let row = match table.get(&memory_id.to_be_bytes()) {
            Ok(Some(guard)) => guard.value(),
            Ok(None) => continue,
            Err(e) => {
                return Err(OpError::Internal(format!(
                    "structured recall MEMORIES_TABLE get: {e}"
                )))
            }
        };
        if row.is_tombstoned() {
            continue;
        }
        // Namespace (tenant) wall — unconditional. A caller can never see
        // another namespace's memories; space scope only ever narrows further
        // WITHIN the caller's own namespace.
        if row.namespace_id != ctx.executor.caller_namespace.raw() {
            continue;
        }
        if let Some(ref scope) = space_scope {
            if !scope.contains(&row.space_id_bytes) {
                continue;
            }
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
        if let Some(allowed) = &session_filter {
            if !allowed.contains(&row.session().raw()) {
                continue;
            }
        }
        if row.salience < req.salience_floor {
            continue;
        }
        if !passes_age_bound(
            req.age_bound_unix_nanos,
            row.occurred_at_unix_nanos,
            row.created_at_unix_nanos,
        ) {
            continue;
        }

        let text = if let Some(texts) = texts_table.as_ref() {
            match texts.get(&memory_id.to_be_bytes()) {
                Ok(Some(guard)) => std::str::from_utf8(guard.value())
                    .map(str::to_owned)
                    .map_err(|e| {
                        OpError::Internal(format!(
                            "structured recall TEXTS_TABLE non-UTF-8 for {memory_id:?}: {e}"
                        ))
                    })?,
                Ok(None) => String::new(),
                Err(e) => {
                    return Err(OpError::Internal(format!(
                        "structured recall TEXTS_TABLE get: {e}"
                    )))
                }
            }
        } else {
            String::new()
        };

        out.push(MemoryResult {
            memory_id: memory_id.raw(),
            text,
            similarity_score: 1.0,
            confidence: 1.0,
            salience: row.salience,
            kind: wire_kind,
            space_id: row.space_id_bytes,
            session_id: SessionId(row.session_id).into(),
            created_at_unix_nanos: row.created_at_unix_nanos,
            last_accessed_at_unix_nanos: row.last_accessed_at_unix_nanos,
            edges: if req.include_edges {
                Some(Vec::new())
            } else {
                None
            },
            graph: None,
            contributing_retrievers: vec![RetrieverNameWire::Graph],
            fused_score: 1.0,
            rerank_score: None,
            salience_initial: row.salience_initial,
            access_count: row.access_count,
            lsn: row.encoded_at_lsn,
            flags: row.flags,
            consolidated_at_unix_nanos: row.consolidated_at_unix_nanos,
            occurred_at_unix_nanos: row.occurred_at_unix_nanos,
            edges_out_count: row.edges_out_count,
            edges_in_count: row.edges_in_count,
        });
        ctx.access_buffer.record(memory_id);
    }
    Ok(out)
}

/// Derive candidate subject entities from the cue alone — no client
/// `subject_name` required. Language-general by construction: every
/// candidate surface is resolved through the exact canonical-name index
/// (`entity_resolve_canonical_all_types`, which NFC-normalizes), so common
/// words simply fail to resolve and are harmless. There is no hardcoded
/// pronoun / stopword list.
///
/// Sources, in order:
///   1. The caller's space self-entity — covers every first-person
///      "what are my X" query with zero pronoun parsing.
///   2. An explicit `subject_name`, when the client did pass one.
///   3. Surfaces mined from the cue: capitalized multi-word runs (Latin
///      proper nouns) and individual whitespace tokens of length ≥ 2
///      (catches CJK single-token names and lowercase entity names).
///
/// Deduped and capped at `MAX_SUBJECT_CANDIDATES` to bound the per-call
/// grounded work (each candidate is a few redb point lookups).
/// The cue's subject entity, used as the always-on graph-lane anchor. We take
/// the strongest NON-self candidate — the space self-entity is too broad to
/// anchor a walk (everything the space ever said connects to it). `None` when
/// nothing resolves, in which case the graph lane simply has no seed.
fn resolve_graph_anchor(req: &RecallRequest, ctx: &OpsContext) -> Option<EntityId> {
    let rtxn = ctx.executor.metadata.read_txn().ok()?;
    let self_id = EntityId::from(ctx.executor.caller_space.0.into_bytes());
    subject_candidates_from_cue(&rtxn, req, ctx)
        .ok()?
        .into_iter()
        .find(|id| *id != self_id)
}

fn subject_candidates_from_cue(
    rtxn: &redb::ReadTransaction,
    req: &RecallRequest,
    ctx: &OpsContext,
) -> Result<Vec<EntityId>, OpError> {
    let mut out: Vec<EntityId> = Vec::new();
    let mut seen: HashSet<EntityId> = HashSet::new();
    let push = |id: EntityId, out: &mut Vec<EntityId>, seen: &mut HashSet<EntityId>| {
        if seen.insert(id) {
            out.push(id);
        }
    };

    // 1. Space self-entity (same derivation as MATERIALIZE_PROCEDURAL).
    push(
        EntityId::from(ctx.executor.caller_space.0.into_bytes()),
        &mut out,
        &mut seen,
    );

    // The surfaces to resolve against the canonical-name index. DISTINCT and
    // capped at `MAX_CUE_SURFACES` so the resolver (heavy per call) runs a
    // bounded number of times regardless of cue length (see `mine_cue_surfaces`).
    let surfaces = mine_cue_surfaces(&req.subject_name, &req.cue_text);

    for surface in surfaces {
        if out.len() >= MAX_SUBJECT_CANDIDATES {
            break;
        }
        // A short cue ("Niraj") must reach the entity stored under its full
        // name ("Niraj Georgian"): the scored resolver's partial-name tier maps
        // an unambiguous token-subset to the full entity at 0.9, so a short cue
        // anchors the full subject's whole graph even when write-time coref left
        // the forms as separate nodes. Take exact + alias + partial-name matches
        // (score >= 0.9); trigram-fuzzy is too loose for a grounded anchor (a
        // wrong subject yields a wrong fact).
        let scope =
            brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
        let ids = brain_metadata::entity_resolve_scored(rtxn, scope, &surface, 5)
            .map_err(OpError::from)?
            .into_iter()
            .filter(|(_, score)| *score >= 0.9)
            .map(|(id, _)| id);
        for id in ids {
            push(id, &mut out, &mut seen);
            if out.len() >= MAX_SUBJECT_CANDIDATES {
                break;
            }
        }
    }

    out.truncate(MAX_SUBJECT_CANDIDATES);
    Ok(out)
}

/// Mine the DISTINCT candidate surfaces to resolve against the canonical-name
/// index, bounded so the (heavy) entity resolver runs a strictly bounded number
/// of times regardless of cue length.
///
/// When the client passed an explicit `subject_name` it is the sole surface.
/// Otherwise surfaces are mined from the cue: capitalized proper-noun runs
/// first (the strongest anchors), then individual whitespace tokens of length
/// ≥ 2 (CJK single-token names, lowercase entity names). Deduped, the token walk
/// clamped to `MAX_CUE_TOKENS_SCANNED`, and the whole set capped at
/// `MAX_CUE_SURFACES`. A normal-length cue is unaffected (it names far fewer than
/// `MAX_CUE_SURFACES` distinct surfaces); a pathological multi-megabyte cue can
/// no longer drive an unbounded number of resolver calls on the shard core.
fn mine_cue_surfaces(subject_name: &str, cue_text: &str) -> Vec<String> {
    let subject = subject_name.trim();
    if !subject.is_empty() {
        // Explicit subject still works — single surface, no mining.
        return vec![subject.to_string()];
    }

    let mut surfaces: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Capitalized proper-noun runs first — the strongest anchors, so they get
    // the scarce surface budget before generic tokens.
    for run in capitalized_runs(cue_text) {
        if surfaces.len() >= MAX_CUE_SURFACES {
            return surfaces;
        }
        if seen.insert(run.clone()) {
            surfaces.push(run);
        }
    }

    // Then individual tokens, with the token walk itself clamped so a cue that
    // is millions of copies of a few distinct tokens can't force an O(cue) scan.
    for tok in cue_text.split_whitespace().take(MAX_CUE_TOKENS_SCANNED) {
        if surfaces.len() >= MAX_CUE_SURFACES {
            break;
        }
        if tok.chars().count() < 2 {
            continue;
        }
        if seen.insert(tok.to_string()) {
            surfaces.push(tok.to_string());
        }
    }

    surfaces
}

/// Extract capitalized multi-word (or single-word) runs from the cue —
/// Latin proper-noun surfaces like "NeuraCorp" or "Web Summit". A run is a
/// maximal sequence of whitespace-split tokens whose first character is
/// uppercase. Possessive `'s` and surrounding punctuation are trimmed so
/// "NeuraCorp's" resolves as "NeuraCorp". This is a candidate generator,
/// not a parser: a surface that isn't a real entity simply fails to
/// resolve.
fn capitalized_runs(cue: &str) -> Vec<String> {
    let mut runs: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let flush = |current: &mut Vec<&str>, runs: &mut Vec<String>| {
        if !current.is_empty() {
            runs.push(current.join(" "));
            current.clear();
        }
    };
    // Bound the token walk (see `MAX_CUE_TOKENS_SCANNED`) so a pathological
    // multi-megabyte cue can't force an O(cue) scan on the shard core.
    for raw in cue.split_whitespace().take(MAX_CUE_TOKENS_SCANNED) {
        // Trim leading/trailing punctuation and a trailing possessive so
        // the surface matches the stored canonical name.
        let trimmed = raw
            .trim_matches(|c: char| !c.is_alphanumeric())
            .trim_end_matches("'s")
            .trim_end_matches("’s");
        let starts_upper = trimmed.chars().next().is_some_and(char::is_uppercase);
        if starts_upper {
            current.push(trimmed);
        } else {
            flush(&mut current, &mut runs);
        }
    }
    flush(&mut current, &mut runs);
    runs
}

/// The retrieval engine — plan → fan-out (semantic / lexical / graph) →
/// fuse → filter → enrich → project, with in-txn read-your-writes overlay.
/// Returns the ranked candidate memories. This is INTERNAL plumbing the
/// router uses to find answering memories; the lanes it fuses are never
/// surfaced to the caller (there is no "episodic" answer or mode).
async fn retrieve_memories(
    req: &RecallRequest,
    ctx: &OpsContext,
    entity_anchor: Option<EntityId>,
    cue_vec: Option<&[f32; brain_embed::VECTOR_DIM]>,
) -> Result<(Vec<MemoryResult>, Option<RecallTrace>, RecallMetricSample), OpError> {
    let planner_req = build_planner_request(req, ctx.executor.caller_space, entity_anchor);

    let plan = retrieval_plan(&planner_req).map_err(map_plan_error)?;
    let exec_ctx = RetrievalExecutorContext {
        semantic: ctx.semantic_retriever.clone(),
        lexical: ctx.lexical_retriever.clone(),
        graph: ctx.graph_retriever.clone(),
        metadata: ctx.executor.metadata.clone(),
        caller_namespace: ctx.executor.caller_namespace.raw(),
        caller_space: ctx.executor.caller_space,
        scope_mode: recall_scope_mode(req),
        cross_encoder: ctx.cross_encoder.as_arc().cloned(),
        space_vectors: ctx.executor.space_vectors.clone(),
    };
    // The statement corpus (statement HNSW + statements.tantivy) is ALWAYS
    // searched — it is a cue-driven lane like memory semantic/lexical (it
    // matches statement text against the query), so there is no reason to
    // ever gate it off. RECALL stays memory-centric: a statement hit surfaces
    // its SOURCE memory (the projector maps `Statement` items back through
    // evidence). The flooding risk was only ever the subject-anchored graph
    // walk, never these cue-conditioned statement lanes.
    // `trace_detail` mirrors `req.trace` exactly: `trace: true` is the one
    // opt-in knob for full per-stage observability, so the caller that asked
    // for a `RecallTrace` at all is the same caller who wants the per-item
    // detail inside it. The fast (untraced) default pays nothing extra.
    let mut result = retrieval_execute(&plan, &planner_req, true, req.trace, &exec_ctx)
        .await
        .map_err(map_execution_error)?;

    // Cue-condition the entity-graph lane. A candidate reached ONLY by the
    // structural graph walk (not also by the semantic/lexical lanes) is kept
    // only insofar as it is relevant to the query: its score is scaled by the
    // cosine of the cue to that memory's own embedding. An off-topic neighbour
    // of the anchor collapses to ~0 and drops out; a memory that is BOTH
    // graph-connected AND on-topic survives and rises. This is precisely what
    // lets the entity-graph lane be always-on without the subject-dump flood —
    // items the semantic/lexical lanes also surfaced are already cue-relevant,
    // so they are left untouched. Skipped only when the cue failed to embed.
    if let Some(cue) = cue_vec {
        for item in &mut result.items {
            let mid = match item.id {
                RankedItemId::Memory(m) => m,
                _ => continue,
            };
            let graph_only = !item.contributing.is_empty()
                && item
                    .contributing
                    .iter()
                    .all(|c| matches!(c.retriever, Retriever::Graph));
            if graph_only {
                let relevance = ctx
                    .semantic_retriever
                    .vector_for(mid)
                    .map(|v| cosine(cue, &v).max(0.0))
                    .unwrap_or(0.0);
                item.fused_score *= f64::from(relevance);
            }
        }
        result.items.sort_by(|a, b| {
            b.fused_score
                .partial_cmp(&a.fused_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    // Per-lane contribution trace: how many fused items each retriever lane
    // contributed, and the kind of each fused item (memory vs statement vs
    // entity/relation). This is the single most useful "where is matching
    // off" signal — it shows whether the graph/statement lanes are flooding
    // the top-K with non-semantic hits.
    if tracing::enabled!(target: "brain_ops::recall_trace", tracing::Level::DEBUG) {
        let mut lane: std::collections::HashMap<&'static str, usize> =
            std::collections::HashMap::new();
        let (mut n_mem, mut n_stmt, mut n_other) = (0usize, 0usize, 0usize);
        for f in &result.items {
            match f.id {
                RankedItemId::Memory(_) => n_mem += 1,
                RankedItemId::Statement(_) => n_stmt += 1,
                _ => n_other += 1,
            }
            for c in &f.contributing {
                *lane
                    .entry(match c.retriever {
                        Retriever::Semantic => "semantic",
                        Retriever::Lexical => "lexical",
                        Retriever::Graph => "graph",
                    })
                    .or_default() += 1;
            }
        }
        tracing::debug!(
            target: "brain_ops::recall_trace",
            cue = %req.cue_text,
            statements_searched = true,
            anchor = ?entity_anchor,
            fused_total = result.items.len(),
            items_memory = n_mem,
            items_statement = n_stmt,
            items_other = n_other,
            lane_semantic = lane.get("semantic").copied().unwrap_or(0),
            lane_lexical = lane.get("lexical").copied().unwrap_or(0),
            lane_graph = lane.get("graph").copied().unwrap_or(0),
            "recall fan-out: per-lane contribution"
        );
    }

    // Structured per-stage trace, opt-in. The executor already computed
    // `result.metadata` (per-lane latencies/outcomes/counts, filter-chain
    // survivor counts, rerank outcome, total wall-time) for its own
    // observability; without `req.trace` we drop it exactly as before, so the
    // common path pays nothing. When asked, we hand it to the final frame.
    let trace = if req.trace {
        Some(build_recall_trace(
            &result.metadata,
            ctx,
            recall_scope_mode(req),
        )?)
    } else {
        None
    };

    // Always-on read-path metric sample. Extracted from the same
    // `result.metadata` the executor already produced (per-lane
    // latency / totals / outcomes, effective fusion k, rerank flag) —
    // populated on every recall, not just `req.trace`. The RECALL
    // handler records it into the retriever / query metric families
    // once the answer shape is known.
    let metric_sample = RecallMetricSample::from_metadata(&result.metadata);

    let memory_results = project_memory_results(&result, req, ctx)?;

    // In-txn read-your-writes: overlay the txn's pending ENCODE
    // buffer on top of the committed retrieval result. Without this,
    // an in-txn RECALL would never see writes the same transaction
    // has buffered but not yet committed.
    let memory_results = if let Some(txn_id) = req.txn_id {
        overlay_txn_buffer(memory_results, txn_id, req, ctx)?
    } else {
        memory_results
    };

    // Autocut: adapt the returned count to the score distribution rather
    // than a constant. A tight score cluster keeps the whole window; a sharp
    // cliff cuts at the cliff — so a tiny store returns its few real hits
    // without phantom neighbours, and a huge store isn't truncated above the
    // answer. Gated default-off (changes the returned count) until measured.
    let memory_results = if autocut_enabled() {
        apply_autocut(memory_results)
    } else {
        memory_results
    };

    for r in &memory_results {
        ctx.access_buffer.record(MemoryId::from_raw(r.memory_id));
    }

    Ok((memory_results, trace, metric_sample))
}

/// Structure the executor's `QueryMetadata` into the wire `RecallTrace` the
/// `trace = true` caller receives. Pure re-shape — the same per-lane
/// latencies/outcomes/counts, filter-chain survivor counts, rerank outcome,
/// and total wall-time the read pipeline already produced, surfaced as data
/// instead of the rendered text `QUERY_TRACE` emits.
///
/// `trace = true` also means full-detail mode (there is no third knob — see
/// the module doc on [`RecallRequest::trace`]), so this additionally
/// surfaces the per-item fields: each lane's raw candidates (with text,
/// mirroring the `include_text` final-result fetch), which specific memory
/// id each filter step dropped, the per-fused-item lane-score breakdown, and
/// the rerank stage's before/after order. `meta`'s full-detail fields are
/// empty `Vec`s when the executor ran with `trace_detail = false`, so this
/// degrades to the count-only shape automatically — it never has to guess
/// which mode produced `meta`.
fn build_recall_trace(
    meta: &QueryMetadata,
    ctx: &OpsContext,
    mode: brain_metadata::ScopeMode,
) -> Result<RecallTrace, OpError> {
    let latency_of = |r: Retriever| -> f64 {
        meta.retriever_latencies_ms
            .iter()
            .find(|(rr, _)| *rr == r)
            .map(|(_, ms)| *ms)
            .unwrap_or(0.0)
    };
    let count_of = |r: Retriever| -> u32 {
        meta.retriever_total_results
            .iter()
            .find(|(rr, _)| *rr == r)
            .map(|(_, c)| u32::try_from(*c).unwrap_or(u32::MAX))
            .unwrap_or(0)
    };

    // Full-detail mode only: one batched text fetch for every memory id any
    // retriever lane surfaced pre-fusion, mirroring the existing
    // `include_text` per-final-result fetch (`project_memory_results`)
    // applied here to the (larger) per-stage candidate set. `meta
    // .retriever_candidates` stays empty when `trace_detail = false`, so
    // this is a no-op (no read txn opened) on the fast path.
    let candidate_ids: HashSet<MemoryId> = meta
        .retriever_candidates
        .iter()
        .flat_map(|(_, cands)| cands.iter())
        .filter_map(|(id, _)| match id {
            RankedItemId::Memory(mid) => Some(*mid),
            _ => None,
        })
        .collect();
    // Tenant wall for the diagnostic surface. The lexical / graph / statement-
    // semantic lanes don't push the `(namespace, space)` scope down, so their
    // pre-fusion candidate sets can carry foreign-tenant ids. The trace must not
    // render another tenant's content: every candidate id is re-verified against
    // the caller's scope BEFORE its text / label is fetched — a foreign or
    // missing candidate surfaces as an opaque id with no content, mirroring the
    // answer path's per-row `(namespace_id, space_id)` re-check.
    let caller_scope =
        brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
    let candidate_texts = fetch_candidate_texts(&candidate_ids, caller_scope, mode, ctx)?;

    // The graph lane surfaces typed items (entities / relations), not memories —
    // resolving those to display labels needs the metadata tables. Open one read
    // txn for the whole trace build, but only when a non-memory candidate is
    // actually present (so the memory-only fast case opens nothing extra).
    let has_typed = meta.retriever_candidates.iter().any(|(_, cands)| {
        cands
            .iter()
            .any(|(id, _)| !matches!(id, RankedItemId::Memory(_)))
    });
    let typed_rtxn = if has_typed {
        ctx.executor.metadata.read_txn().ok()
    } else {
        None
    };

    let retrievers = meta
        .retriever_outcomes
        .iter()
        .map(|o| {
            let (status, status_detail) = match &o.status {
                RetrieverStatus::Success => (RecallTraceRetrieverStatus::Success, String::new()),
                RetrieverStatus::Skipped(reason) => {
                    (RecallTraceRetrieverStatus::Skipped, (*reason).to_string())
                }
                RetrieverStatus::Timeout => (RecallTraceRetrieverStatus::Timeout, String::new()),
                RetrieverStatus::Failure(msg) => (RecallTraceRetrieverStatus::Failure, msg.clone()),
            };
            let candidates = meta
                .retriever_candidates
                .iter()
                .find(|(rr, _)| *rr == o.retriever)
                .map(|(_, cands)| {
                    cands
                        .iter()
                        .map(|(id, score)| {
                            candidate_from_ranked(
                                typed_rtxn.as_ref(),
                                caller_scope,
                                id,
                                *score,
                                &candidate_texts,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            RecallTraceRetriever {
                name: retriever_name_wire(o.retriever),
                status,
                status_detail,
                latency_ms: latency_of(o.retriever),
                candidate_count: count_of(o.retriever),
                candidates,
            }
        })
        .collect();

    let s = &meta.filter_stats;
    let filter_chain = RecallTraceFilterChain {
        before: s.before,
        after_type: s.after_type,
        after_temporal: s.after_temporal,
        after_confidence: s.after_confidence,
        after_tombstone: s.after_tombstone,
        after_supersession: s.after_supersession,
        after_as_of: s.after_as_of,
        after_limit: s.after_limit,
        dropped_by_type: memory_ids_wire(&s.dropped_by_type),
        dropped_by_temporal: memory_ids_wire(&s.dropped_by_temporal),
        dropped_by_confidence: memory_ids_wire(&s.dropped_by_confidence),
        dropped_by_tombstone: memory_ids_wire(&s.dropped_by_tombstone),
        dropped_by_supersession: dropped_ids_wire(&s.dropped_by_supersession),
        dropped_by_as_of: dropped_ids_wire(&s.dropped_by_as_of),
        dropped_by_limit: dropped_ids_wire(&s.dropped_by_limit),
    };

    let rerank = meta.rerank.as_ref().map(|r| {
        let (applied, candidates, latency_ms) = match r {
            RerankOutcome::Applied {
                candidates,
                latency_ms,
            } => (
                true,
                u32::try_from(*candidates).unwrap_or(u32::MAX),
                *latency_ms,
            ),
            RerankOutcome::SkippedNoCandidates => (false, 0, 0.0),
        };
        RecallTraceRerank {
            applied,
            candidates,
            latency_ms,
            before_order: memory_ids_wire(&meta.rerank_before_order),
            after_order: memory_ids_wire(&meta.rerank_after_order),
        }
    });

    // Full-detail mode only: per-fused-item lane-score breakdown. `None`
    // when `trace_detail` wasn't requested (empty `fusion_breakdown`) or
    // fusion produced nothing.
    let fusion = if meta.fusion_breakdown.is_empty() {
        None
    } else {
        Some(RecallTraceFusion {
            items: meta
                .fusion_breakdown
                .iter()
                .filter_map(|(id, rrf_score, lane_scores)| match id {
                    RankedItemId::Memory(mid) => Some(RecallTraceFusionItem {
                        memory_id: mid.raw(),
                        #[allow(clippy::cast_possible_truncation)]
                        rrf_score: *rrf_score as f32,
                        lane_scores: lane_scores
                            .iter()
                            .map(|(r, score)| (retriever_name_wire(*r), *score))
                            .collect(),
                    }),
                    _ => None,
                })
                .collect(),
        })
    };

    Ok(RecallTrace {
        retrievers,
        filter_chain,
        rerank,
        total_latency_ms: meta.total_latency_ms,
        fusion,
    })
}

/// Filter a filter-chain / rerank-order list of the union `RankedItemId`
/// down to memory ids only, mapped to their `u128` wire form. This trace
/// path only ever needs to bridge the executor's cross-kind id union
/// (memory / statement / entity / relation, since the same executor also
/// serves the typed-graph QUERY path) down to RECALL's memory-only wire
/// shape — non-`Memory` variants are silently dropped here exactly as
/// `project_memory_results` already drops them from the answer set.
fn memory_ids_wire(ids: &[RankedItemId]) -> Vec<u128> {
    ids.iter()
        .filter_map(|id| match id {
            RankedItemId::Memory(mid) => Some(mid.raw()),
            _ => None,
        })
        .collect()
}

/// Kind-preserving counterpart to [`memory_ids_wire`] for filter steps that
/// can drop non-`Memory` items (supersession, as-of, and the final limit
/// truncation all operate on the fused `RankedItemId` set, which includes
/// `Statement`/`Entity`/`Relation` alongside `Memory`). Unlike
/// `memory_ids_wire`, nothing is discarded here — every variant maps to a
/// tagged `RecallTraceDroppedId` so the wire trace can tell a dropped
/// statement from a dropped memory.
fn dropped_ids_wire(ids: &[RankedItemId]) -> Vec<RecallTraceDroppedId> {
    ids.iter()
        .map(|id| match id {
            RankedItemId::Memory(mid) => RecallTraceDroppedId {
                kind: RankedItemKindWire::Memory,
                id: mid.raw(),
            },
            RankedItemId::Statement(sid) => RecallTraceDroppedId {
                kind: RankedItemKindWire::Statement,
                id: sid.0.as_u128(),
            },
            RankedItemId::Entity(eid) => RecallTraceDroppedId {
                kind: RankedItemKindWire::Entity,
                id: eid.0.as_u128(),
            },
            RankedItemId::Relation(rid) => RecallTraceDroppedId {
                kind: RankedItemKindWire::Relation,
                id: rid.0.as_u128(),
            },
        })
        .collect()
}

/// Fetch stored text for a set of memory ids in one batched redb read,
/// mirroring the `include_text` final-result fetch in
/// `project_memory_results` — applied here to the (larger, opt-in)
/// full-detail trace candidate set. A missing/tombstoned-since-fusion row
/// maps to an empty string rather than a fatal error: an observability
/// payload losing one candidate's text is not the same failure class as a
/// missing final answer row.
/// Turn one ranked retriever candidate into its wire trace form, resolving a
/// display label per item kind. Memory labels come from the batched
/// `memory_texts`; the graph lane's typed items (entity / relation / statement)
/// are resolved against `typed_rtxn` (opened once by the caller, `None` on the
/// memory-only fast path). Every lookup is best-effort — a missing row degrades
/// to an empty/partial label rather than dropping the candidate, so the count
/// shown in the pipeline always matches the list.
fn candidate_from_ranked(
    typed_rtxn: Option<&redb::ReadTransaction>,
    caller_scope: brain_metadata::RowScope,
    id: &RankedItemId,
    score: f32,
    memory_texts: &HashMap<MemoryId, String>,
) -> RecallTraceCandidate {
    use brain_protocol::ops::memory::RecallCandidateKind;
    match id {
        // Memory scope is enforced upstream: `fetch_candidate_texts` only
        // populates `memory_texts` for in-scope rows, so a foreign / missing
        // memory falls back to an empty (contentless) label here.
        RankedItemId::Memory(mid) => RecallTraceCandidate {
            item_id: mid.raw(),
            kind: RecallCandidateKind::Memory,
            text: memory_texts.get(mid).cloned().unwrap_or_default(),
            score,
        },
        // Typed items are re-scoped here before their label is rendered: an
        // out-of-scope (or missing) row yields an empty label, so a foreign
        // tenant's entity name / relation / statement never reaches the trace.
        RankedItemId::Entity(eid) => RecallTraceCandidate {
            item_id: u128::from_be_bytes(eid.to_bytes()),
            kind: RecallCandidateKind::Entity,
            text: typed_rtxn
                .filter(|r| entity_in_caller_scope(r, *eid, caller_scope))
                .and_then(|r| brain_metadata::entity_get(r, *eid).ok().flatten())
                .map(|e| e.canonical_name)
                .unwrap_or_default(),
            score,
        },
        RankedItemId::Relation(rid) => RecallTraceCandidate {
            item_id: u128::from_be_bytes(rid.to_bytes()),
            kind: RecallCandidateKind::Relation,
            text: typed_rtxn
                .filter(|r| relation_in_caller_scope(r, *rid, caller_scope))
                .map(|r| render_relation_label(r, *rid))
                .unwrap_or_default(),
            score,
        },
        RankedItemId::Statement(sid) => RecallTraceCandidate {
            item_id: u128::from_be_bytes(sid.to_bytes()),
            kind: RecallCandidateKind::Statement,
            text: typed_rtxn
                .filter(|r| statement_in_caller_scope(r, *sid, caller_scope).unwrap_or(false))
                .map(|r| render_statement_label(r, *sid))
                .unwrap_or_default(),
            score,
        },
    }
}

/// Whether entity `eid`'s row belongs to the caller's `(namespace, space)`
/// scope, read on the shared trace txn. Fail-closed: a missing row or read
/// error denies, so a foreign / vanished entity never renders its canonical
/// name into the trace. Mirrors [`statement_in_caller_scope`] for the graph
/// lane's entity candidates.
fn entity_in_caller_scope(
    rtxn: &redb::ReadTransaction,
    eid: EntityId,
    caller_scope: brain_metadata::RowScope,
) -> bool {
    use brain_metadata::tables::entity::{EntityMetadata, ENTITIES_TABLE};
    let Ok(t) = rtxn.open_table(ENTITIES_TABLE) else {
        return false;
    };
    let row: Option<EntityMetadata> = t.get(&eid.to_bytes()).ok().flatten().map(|g| g.value());
    matches!(row, Some(m) if m.scope() == caller_scope)
}

/// Whether relation `rid`'s sidecar row belongs to the caller's `(namespace,
/// space)` scope, read on the shared trace txn. Fail-closed: a missing row or
/// read error denies. Mirrors [`entity_in_caller_scope`] for the graph lane's
/// relation candidates.
fn relation_in_caller_scope(
    rtxn: &redb::ReadTransaction,
    rid: brain_core::RelationId,
    caller_scope: brain_metadata::RowScope,
) -> bool {
    use brain_metadata::tables::relation::{RelationMetadata, RELATION_METADATA_TABLE};
    let Ok(t) = rtxn.open_table(RELATION_METADATA_TABLE) else {
        return false;
    };
    let row: Option<RelationMetadata> = t.get(&rid.to_bytes()).ok().flatten().map(|g| g.value());
    matches!(row, Some(m) if m.scope() == caller_scope)
}

/// "From —namespace:name→ To" for a relation candidate; partial when a lookup
/// misses.
fn render_relation_label(rtxn: &redb::ReadTransaction, rid: brain_core::RelationId) -> String {
    let Ok(Some(rel)) = brain_metadata::relation_get(rtxn, rid) else {
        return String::new();
    };
    let name = |eid| {
        brain_metadata::entity_get(rtxn, eid)
            .ok()
            .flatten()
            .map(|e| e.canonical_name)
            .unwrap_or_default()
    };
    let pred = brain_metadata::relation_type_get(rtxn, rel.relation_type)
        .ok()
        .flatten()
        .map(|rt| format!("{}:{}", rt.namespace, rt.name))
        .unwrap_or_else(|| "related_to".to_string());
    format!(
        "{} —{}→ {}",
        name(rel.from_entity),
        pred,
        name(rel.to_entity)
    )
}

/// "Subject predicate Object" for a statement candidate; partial when a lookup
/// misses.
fn render_statement_label(rtxn: &redb::ReadTransaction, sid: brain_core::StatementId) -> String {
    use brain_core::nodes::statement::{StatementObject, StatementValue, SubjectRef};
    let Ok(Some(st)) = brain_metadata::statement_get(rtxn, sid) else {
        return String::new();
    };
    let ent_name = |eid| {
        brain_metadata::entity_get(rtxn, eid)
            .ok()
            .flatten()
            .map(|e| e.canonical_name)
            .unwrap_or_default()
    };
    let subject = match st.subject {
        SubjectRef::Entity(e) => ent_name(e),
        SubjectRef::Memory(_) => "(memory)".to_string(),
        _ => String::new(),
    };
    let predicate = brain_metadata::predicate_get(rtxn, st.predicate)
        .ok()
        .flatten()
        .map(|p| format!("{}:{}", p.namespace, p.name))
        .unwrap_or_default();
    let object = match st.object {
        StatementObject::Entity(e) => ent_name(e),
        StatementObject::Value(v) => match v {
            StatementValue::Text(s) => s,
            StatementValue::Integer(i) => i.to_string(),
            StatementValue::Float(f) => f.to_string(),
            StatementValue::Bool(b) => b.to_string(),
            StatementValue::UnixNanos(n) => n.to_string(),
            StatementValue::Blob(_) => "(blob)".to_string(),
        },
        StatementObject::Memory(_) => "(memory)".to_string(),
        StatementObject::Statement(_) => "(statement)".to_string(),
    };
    format!("{subject} {predicate} {object}").trim().to_string()
}

fn fetch_candidate_texts(
    ids: &HashSet<MemoryId>,
    caller_scope: brain_metadata::RowScope,
    mode: brain_metadata::ScopeMode,
    ctx: &OpsContext,
) -> Result<HashMap<MemoryId, String>, OpError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("recall trace read_txn: {e}")))?;
    let texts_table = rtxn
        .open_table(TEXTS_TABLE)
        .map_err(|e| OpError::Internal(format!("recall trace open TEXTS_TABLE: {e}")))?;
    // The memory table carries each row's owner scope; the text table is keyed
    // by global id with no scope, so text is fetched only after the row's
    // `(namespace, space)` clears the caller's scope. A foreign / missing row
    // is left out of the map entirely, so its trace candidate renders with no
    // text — mirroring `project_memory_results`' per-row re-check.
    let memories_table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| OpError::Internal(format!("recall trace open MEMORIES_TABLE: {e}")))?;

    let mut out = HashMap::with_capacity(ids.len());
    for &id in ids {
        let in_scope = match memories_table.get(&id.to_be_bytes()) {
            Ok(Some(guard)) => {
                let row = guard.value();
                caller_scope.admits(row.namespace_id, &row.space_id_bytes, mode)
            }
            Ok(None) => false,
            Err(e) => {
                return Err(OpError::Internal(format!(
                    "recall trace MEMORIES_TABLE get: {e}"
                )));
            }
        };
        if !in_scope {
            continue;
        }
        let text = match texts_table.get(&id.to_be_bytes()) {
            Ok(Some(guard)) => std::str::from_utf8(guard.value())
                .map(str::to_owned)
                .map_err(|e| {
                    OpError::Internal(format!(
                        "recall trace TEXTS_TABLE non-UTF-8 for {id:?}: {e}"
                    ))
                })?,
            Ok(None) => String::new(),
            Err(e) => {
                return Err(OpError::Internal(format!(
                    "recall trace TEXTS_TABLE get: {e}"
                )));
            }
        };
        out.insert(id, text);
    }
    Ok(out)
}

/// Map the planner's internal `Retriever` discriminant to the wire lane name.
fn retriever_name_wire(r: Retriever) -> RetrieverNameWire {
    match r {
        Retriever::Semantic => RetrieverNameWire::Semantic,
        Retriever::Lexical => RetrieverNameWire::Lexical,
        Retriever::Graph => RetrieverNameWire::Graph,
    }
}

/// Deploy-time gate for autocut (`[retrieval] autocut`). Default OFF.
/// Sourced from the parsed config installed at boot.
fn autocut_enabled() -> bool {
    brain_core::RetrievalTuning::active().autocut
}

/// Smallest count autocut will ever return when there is at least one hit —
/// below this the "distribution" is too small to read a meaningful cliff, so
/// we never cut into the very top.
const AUTOCUT_MIN_KEEP: usize = 1;

/// Relative-drop threshold: a consecutive `fused_score` ratio at or below
/// this (the next hit scores ≤ 55% of the current one) is a cliff — autocut
/// stops the result list there. Conservative so it only fires on a real gap.
const AUTOCUT_CLIFF_RATIO: f32 = 0.55;

/// Cut the ranked results at the first sharp relative drop in `fused_score`
/// after `AUTOCUT_MIN_KEEP` hits. Results arrive ranked (descending); a cut
/// keeps the head up to and including the hit before the cliff. No cut when
/// the list is short, the scores are flat, or no cliff is found — autocut
/// only ever trims a clearly-separated tail, never the answer.
fn apply_autocut(mut results: Vec<MemoryResult>) -> Vec<MemoryResult> {
    if results.len() <= AUTOCUT_MIN_KEEP {
        return results;
    }
    let mut cut_at: Option<usize> = None;
    for i in AUTOCUT_MIN_KEEP..results.len() {
        let prev = results[i - 1].fused_score;
        let cur = results[i].fused_score;
        // Only reason about positive, ordered scores; a non-positive or
        // out-of-order score is no signal, so leave the tail intact.
        if prev <= 0.0 || cur <= 0.0 || cur > prev {
            continue;
        }
        if cur / prev <= AUTOCUT_CLIFF_RATIO {
            cut_at = Some(i);
            break;
        }
    }
    if let Some(i) = cut_at {
        results.truncate(i);
    }
    results
}

/// Merge the txn's pending writes into the committed retrieval result.
/// Drops tombstoned ids on the committed side, scores each pending
/// encode against the cue, applies the post-filters (kind, context,
/// salience, age), then re-sorts by score and trims to `top_k`.
fn overlay_txn_buffer(
    committed: Vec<MemoryResult>,
    txn_id: [u8; 16],
    req: &RecallRequest,
    ctx: &OpsContext,
) -> Result<Vec<MemoryResult>, OpError> {
    let _ = ctx
        .txn_store
        .validate_active(txn_id, ctx.caller_connection_id)?;
    let (pending, tombstoned) =
        ctx.txn_store
            .with_buffer(txn_id, ctx.caller_connection_id, |buf| {
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
        // tombstoned) feeds membership. Bound by the candidate pool, not the
        // answer cap; `build_membership` applies `max_results` after the band.
        merged.truncate(RECALL_CANDIDATE_POOL as usize);
        return Ok(merged);
    }

    let cue_vec = ctx
        .executor
        .embedder
        .embed_query(&req.cue_text)
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::EmbedFailed(e)))?;

    let kind_filter: Option<HashSet<MemoryKindWire>> = req
        .kind_filter
        .as_ref()
        .map(|v| v.iter().copied().collect());
    let session_filter: Option<HashSet<u64>> = req
        .session_filter
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
        if let Some(ref sessions) = session_filter {
            if !sessions.contains(&p.session_id.raw()) {
                continue;
            }
        }
        if p.salience_initial < req.salience_floor {
            continue;
        }
        if !passes_age_bound(
            req.age_bound_unix_nanos,
            p.occurred_at_unix_nanos,
            p.created_at_unix_nanos,
        ) {
            continue;
        }
        // `confidence_threshold` is a SALIENCE floor on the committed path
        // (brain-planner `filter_confidence` gates memory hits by
        // `salience >= confidence_min`, deliberately — see the owner note there;
        // the surfaced `confidence` field remains cosine). Apply the SAME
        // semantic to buffered hits so read-your-writes is consistent: filter by
        // the pending item's salience, never by cosine. A fresh buffered write
        // has no decay, so `salience_initial` is its current salience.
        if !pending_clears_confidence(p.salience_initial, req.confidence_threshold) {
            continue;
        }
        let score = cosine(&cue_vec, &p.vector);
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
    // Candidate pool for membership, not the answer cap (applied downstream).
    merged.truncate(RECALL_CANDIDATE_POOL as usize);
    Ok(merged)
}

/// Whether a buffered (pending) hit clears the `confidence_threshold` gate,
/// using the SAME semantic as the committed path.
///
/// On the committed path `confidence_threshold` becomes `FilterChain::confidence_min`
/// and gates memory hits as a SALIENCE floor (`salience >= min`), NOT a cosine
/// floor (intentional; the surfaced `confidence` field is cosine but the FILTER
/// is salience). The overlay historically applied it as a cosine floor, so the
/// one wire field filtered committed vs pending on different quantities and
/// read-your-writes was inconsistent. This makes the pending gate salience-based
/// too. A zero threshold admits everything (committed leaves `confidence_min`
/// unset at `0.0`), which `salience >= 0.0` reproduces.
fn pending_clears_confidence(salience: f32, confidence_threshold: f32) -> bool {
    salience >= confidence_threshold
}

fn pending_to_memory_result(p: &BufferedEncode, req: &RecallRequest, score: f32) -> MemoryResult {
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
        space_id: p.space_id.into(),
        session_id: p.session_id.into(),
        created_at_unix_nanos: p.created_at_unix_nanos,
        last_accessed_at_unix_nanos: p.created_at_unix_nanos,
        // Edges and graph enrichment from buffered writes aren't
        // visible until commit — the typed-graph tables they'd
        // resolve against don't have the buffered rows yet.
        edges: if req.include_edges {
            Some(Vec::new())
        } else {
            None
        },
        graph: None,
        contributing_retrievers: Vec::new(),
        fused_score: score,
        // Buffered txn writes never go through the retrieval rerank stage.
        rerank_score: None,
        salience_initial: p.salience_initial,
        access_count: 0,
        // Buffered writes haven't been WAL'd yet; LSN is assigned
        // at TXN_COMMIT.
        lsn: 0,
        flags: 0,
        consolidated_at_unix_nanos: None,
        occurred_at_unix_nanos: p.occurred_at_unix_nanos,
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

/// Per-hit opaque-body enrichment populated when the request
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
pub(crate) fn fetch_enrichment_for(
    memory_ids: &[MemoryId],
    scope: brain_metadata::RowScope,
    session_filter: Option<&HashSet<u64>>,
    rtxn: &redb::ReadTransaction,
) -> Result<Vec<brain_protocol::envelope::response::GraphEnrichment>, OpError> {
    use brain_core::{EdgeKindRef, NodeRef};
    use brain_core::{EntityId, StatementId, SubjectRef};
    use brain_metadata::entity::ops::entity_get;
    use brain_metadata::relation::types::relation_type_get;
    use brain_metadata::schema::predicate::predicate_get;
    use brain_metadata::tables::edge::{walk_incoming, walk_outgoing};
    use brain_metadata::tables::entity_type::ENTITY_TYPES_TABLE;
    use brain_metadata::tables::relation::{RelationMetadata, RELATION_METADATA_TABLE};
    use brain_metadata::tables::statement::{
        statement_from_metadata, StatementMetadata, STATEMENTS_BY_EVIDENCE_TABLE, STATEMENTS_TABLE,
    };
    use brain_protocol::envelope::response::{
        EnrichedEntity, EnrichedRelation, EnrichedStatement, GraphEnrichment,
    };

    const ENTITY_CAP: usize = 16;
    const STATEMENT_CAP: usize = 5;
    const RELATION_CAP: usize = 5;
    let entity_types = rtxn.open_table(ENTITY_TYPES_TABLE).ok();
    let evidence_table = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).map_err(|e| {
        OpError::Internal(format!(
            "include_graph: open STATEMENTS_BY_EVIDENCE_TABLE: {e}"
        ))
    })?;
    // The statement/relation sidecars carry the per-utterance `session_id`;
    // read them directly so `session_filter` (already applied to memories)
    // also gates the graph rows — "session N" shows its memories AND its
    // graph. Session is a grouping column, never a key.
    let statements_table = rtxn
        .open_table(STATEMENTS_TABLE)
        .map_err(|e| OpError::Internal(format!("include_graph: open STATEMENTS_TABLE: {e}")))?;
    let relation_meta_table = rtxn.open_table(RELATION_METADATA_TABLE).map_err(|e| {
        OpError::Internal(format!("include_graph: open RELATION_METADATA_TABLE: {e}"))
    })?;

    let mut out: Vec<GraphEnrichment> = Vec::with_capacity(memory_ids.len());
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
        {
            let mid = memory_id.to_be_bytes();
            // STATEMENTS_BY_EVIDENCE is now scoped: the key is
            // `(namespace_id, space_id_bytes, MemoryId, StatementId)`.
            // Restrict the range to the caller's scope so the evidence
            // scan can never cross the tenant boundary.
            let lo = (scope.namespace_id, scope.space_id_bytes, mid, [0u8; 16]);
            let hi = (scope.namespace_id, scope.space_id_bytes, mid, [0xFFu8; 16]);
            let mut stmts: Vec<brain_core::Statement> = Vec::new();
            for entry in evidence_table
                .range(lo..=hi)
                .map_err(|e| OpError::Internal(format!("include_graph: evidence range: {e}")))?
            {
                let (k, _v) = entry
                    .map_err(|e| OpError::Internal(format!("include_graph: evidence row: {e}")))?;
                let (_ns, _space, _mem_bytes, sid_bytes) = k.value();
                let sid = StatementId::from_bytes(sid_bytes);
                let row: Option<StatementMetadata> = statements_table
                    .get(&sid.to_bytes())
                    .map_err(|e| OpError::Internal(format!("include_graph: statement row: {e}")))?
                    .map(|g| g.value());
                let Some(m) = row else {
                    continue;
                };
                // Session coherence: drop statements outside the requested
                // session(s) so a session-scoped read's graph matches its
                // memories.
                if let Some(sf) = session_filter {
                    if !sf.contains(&m.session_id) {
                        continue;
                    }
                }
                if let Some(stmt) = statement_from_metadata(&m) {
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
                    // Render the inner value, not its Debug form: a bare
                    // `Text("acceptance")` wrapper must never leak into the
                    // client-facing enrichment label (or the eval report).
                    brain_core::StatementObject::Value(v) => match v {
                        brain_core::StatementValue::Text(s) => s.clone(),
                        brain_core::StatementValue::Integer(n) => n.to_string(),
                        brain_core::StatementValue::Float(f) => f.to_string(),
                        brain_core::StatementValue::Bool(b) => b.to_string(),
                        brain_core::StatementValue::UnixNanos(t) => t.to_string(),
                        brain_core::StatementValue::Blob(b) => format!("<{} bytes>", b.len()),
                    },
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
                    event_at_unix_nanos: stmt.event_at_unix_nanos,
                });
            }
        }

        // 3. Typed relations incident to any mentioned entity. Both
        // directions; top RELATION_CAP by created_at desc across the
        // pool. A relation whose BOTH endpoints are mentioned by this
        // memory is reachable twice — once as an outgoing edge from one
        // endpoint, once as an incoming edge to the other — so dedup on
        // the edge identity `(from, type, to)` to emit each relation once.
        let mut all_rels: Vec<(u64, EnrichedRelation)> = Vec::new();
        let mut seen_rels: std::collections::HashSet<([u8; 16], u32, [u8; 16])> =
            std::collections::HashSet::new();
        for eid in &entity_ids {
            for outgoing in [true, false] {
                let rows = if outgoing {
                    walk_outgoing(rtxn, NodeRef::Entity(*eid), None)
                } else {
                    walk_incoming(rtxn, NodeRef::Entity(*eid), None)
                }
                .map_err(|e| OpError::Internal(format!("include_graph: walk relation: {e}")))?;
                for (kind, other, disamb, data) in rows {
                    let typed_id = match kind {
                        EdgeKindRef::Typed(rt_id) => rt_id,
                        _ => continue,
                    };
                    let other_entity = match other {
                        NodeRef::Entity(oid) => oid,
                        _ => continue,
                    };
                    // Session coherence: the typed-edge disambiguator is the
                    // relation id, keying its sidecar (which carries the
                    // per-utterance session). Drop relations outside the
                    // requested session(s) so a session-scoped read's graph
                    // matches its memories.
                    if let Some(sf) = session_filter {
                        let sidecar: Option<RelationMetadata> = relation_meta_table
                            .get(&disamb)
                            .map_err(|e| {
                                OpError::Internal(format!("include_graph: relation sidecar: {e}"))
                            })?
                            .map(|g| g.value());
                        match sidecar {
                            Some(rm) if sf.contains(&rm.session_id) => {}
                            // Drop both a foreign-session relation and one
                            // whose sidecar is missing (can't prove it belongs).
                            _ => continue,
                        }
                    }
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
                    // Skip the mirror image of a relation already recorded
                    // from its other endpoint.
                    if !seen_rels.insert((from_id.to_bytes(), typed_id.raw(), to_id.to_bytes())) {
                        continue;
                    }
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

        out.push(GraphEnrichment {
            entities: enriched_entities,
            statements: enriched_statements,
            relations: enriched_relations,
        });
    }
    Ok(out)
}

fn build_planner_request(
    req: &RecallRequest,
    caller_space: brain_core::SpaceId,
    entity_anchor: Option<EntityId>,
) -> PlannerQueryRequest {
    // Space scope of the front-gate prefilter. In the default (space-scoped)
    // recall this pins retrieval to the calling space — every row belongs to
    // exactly one space and there is no client-supplied space filter on the
    // wire, so a key can never reach another space's memories. Under a
    // namespace-wide recall the space filter is left EMPTY, which the
    // retrievers read as "any space"; the namespace wall is enforced
    // separately and unconditionally on every lane (semantic
    // `namespace_id`, graph `admits`, and the projector post-filters), so
    // an empty space filter widens across the caller's own spaces without
    // ever crossing namespaces.
    let space_filter: Vec<brain_core::SpaceId> = match recall_scope_mode(req) {
        brain_metadata::ScopeMode::Space => vec![caller_space],
        brain_metadata::ScopeMode::Namespace => Vec::new(),
    };

    PlannerQueryRequest {
        text: Some(req.cue_text.clone()),
        entity_anchor,
        // RECALL doesn't filter by statement kind; the retrieval
        // planner uses an empty filter to mean "any kind". Substrate
        // post-filters (kind / context / salience) re-apply below.
        kind_filter: Vec::new(),
        predicate_filter: Vec::new(),
        time_filter: None,
        // Push the memory-context scope into the front gate so the
        // retrievers run on the eligible universe instead of pruning
        // post-projection (the historical gap this turn closes).
        session_filter: req.session_filter.as_ref().cloned().unwrap_or_default(),
        space_filter,
        confidence_min: if req.confidence_threshold > 0.0 {
            Some(req.confidence_threshold)
        } else {
            None
        },
        include_tombstoned: false,
        include_superseded: false,
        as_of_record_time_unix_nanos: req.as_of_record_time_unix_nanos,
        // Candidate budget, NOT the answer cap. Feed the full pool so the
        // membership band (not a top-K cut) decides the answer; `max_results`
        // bounds the returned members afterwards in `build_membership`.
        limit: RECALL_CANDIDATE_POOL,
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
    let session_filter: Option<HashSet<u64>> = req
        .session_filter
        .as_ref()
        .map(|v| v.iter().copied().collect());

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("retrieval recall read_txn: {e}")))?;
    let table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| OpError::Internal(format!("retrieval recall open MEMORIES_TABLE: {e}")))?;
    // Opening the texts table costs a redb seek; only do it when the
    // caller asked for text, so the common ids-only path stays cheap.
    let texts_table =
        if req.include_text {
            Some(rtxn.open_table(TEXTS_TABLE).map_err(|e| {
                OpError::Internal(format!("retrieval recall open TEXTS_TABLE: {e}"))
            })?)
        } else {
            None
        };

    // Pre-fetch opaque-body enrichment in one pass if requested.
    // The retrieval path already holds a read txn open for the row hydration
    // below; the helper reuses it so we don't open a second redb snapshot.
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
        let scope =
            brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
        let enriched = fetch_enrichment_for(&ids, scope, session_filter.as_ref(), &rtxn)?;
        Some(ids.into_iter().zip(enriched).collect())
    } else {
        None
    };

    let mut out: Vec<MemoryResult> = Vec::with_capacity(result.items.len());
    // A memory may be reached directly AND via a statement that cites it;
    // keep only its first (highest-ranked) appearance so the answer set has
    // no duplicate memories.
    let mut seen: HashSet<MemoryId> = HashSet::new();
    for fused in &result.items {
        // Resolve the fused item to the memory it answers with. A memory hit
        // is itself; a statement hit surfaces its first evidence memory (the
        // memory-DB contract — a statement is provenance, the memory is the
        // answer). Entity / relation hits carry no memory and are dropped.
        let memory_id = match fused.id {
            RankedItemId::Memory(mid) => mid,
            // Statement lanes are always searched; a statement hit surfaces its
            // first evidence memory (RECALL is memory-centric — the statement is
            // provenance, the memory is the answer). Entity / relation hits
            // carry no memory and are dropped.
            RankedItemId::Statement(sid) => match statement_evidence_memory(&rtxn, sid)? {
                Some(mid) => mid,
                None => continue,
            },
            _ => continue,
        };
        if !seen.insert(memory_id) {
            continue;
        }

        let row = match table.get(&memory_id.to_be_bytes()) {
            Ok(Some(guard)) => guard.value(),
            Ok(None) => continue, // Tombstoned between fusion and projection — drop.
            Err(e) => {
                return Err(OpError::Internal(format!(
                    "retrieval recall MEMORIES_TABLE get: {e}",
                )));
            }
        };

        if row.is_tombstoned() {
            continue;
        }

        // Tenant wall — the authoritative, defense-in-depth scope gate at the
        // projector. The semantic (and per-space brute-force) lane scopes at
        // the index, but the lexical and graph lanes do NOT push the scope
        // down (the tantivy query carries no namespace clause), so a fused hit
        // could otherwise carry a foreign-tenant OR foreign-space memory into
        // the answer. Re-check the row's own owner scope here via the one
        // centralized `admits` predicate: the **namespace half is always
        // enforced** (the tenant wall, never relaxed), while the space half is
        // relaxed only under a namespace-wide recall — so a namespace-wide
        // read spans the caller's own spaces yet can never cross namespaces,
        // even though the lexical lane over-returns when its space filter is
        // dropped. This projector is what makes that widening safe.
        let caller_scope =
            brain_metadata::RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);
        if !caller_scope.admits(
            row.namespace_id,
            &row.space_id_bytes,
            recall_scope_mode(req),
        ) {
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
        if let Some(allowed) = &session_filter {
            if !allowed.contains(&row.session().raw()) {
                continue;
            }
        }
        if row.salience < req.salience_floor {
            continue;
        }
        if !passes_age_bound(
            req.age_bound_unix_nanos,
            row.occurred_at_unix_nanos,
            row.created_at_unix_nanos,
        ) {
            continue;
        }

        let text = if let Some(texts) = texts_table.as_ref() {
            match texts.get(&memory_id.to_be_bytes()) {
                Ok(Some(guard)) => std::str::from_utf8(guard.value())
                    .map(str::to_owned)
                    .map_err(|e| {
                        OpError::Internal(format!(
                            "retrieval recall TEXTS_TABLE non-UTF-8 for {memory_id:?}: {e}",
                        ))
                    })?,
                Ok(None) => String::new(),
                Err(e) => {
                    return Err(OpError::Internal(format!(
                        "retrieval recall TEXTS_TABLE get: {e}",
                    )));
                }
            }
        } else {
            String::new()
        };

        // similarity_score on the retrieval path is the semantic
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
        // edges. typed-graph edges (Mentions / Typed) belong to
        // entity/relation ops, not RECALL. The rtxn opened above
        // serves every hit; one prefix scan per memory.
        let edges = if req.include_edges {
            use brain_core::NodeRef;
            let rows = brain_metadata::tables::edge::walk_outgoing(
                &rtxn,
                NodeRef::Memory(memory_id),
                None,
            )
            .map_err(|e| OpError::Internal(format!("retrieval recall walk_outgoing: {e}")))?;
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
            // `confidence` is the cosine similarity of the hit — a [0,1]
            // quantity, identical to `similarity_score`, consistent across
            // the retrieval and substrate paths. The raw RRF rank-fusion sum
            // is unbounded (it grows with the number of contributing
            // retrievers) and is exposed separately as `fused_score`; it is
            // a ranking diagnostic, not a confidence.
            confidence: semantic_score,
            salience: row.salience,
            kind: wire_kind,
            space_id: row.space_id_bytes,
            session_id: SessionId(row.session_id).into(),
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
            // Present iff the always-on rerank stage scored this hit.
            // When set, the result list is ordered by this score, not
            // `fused_score`; the recall card surfaces it as `rr=`.
            rerank_score: fused.rerank_score,
            salience_initial: row.salience_initial,
            access_count: row.access_count,
            // WAL position the row was originally encoded at.
            lsn: row.encoded_at_lsn,
            flags: row.flags,
            consolidated_at_unix_nanos: row.consolidated_at_unix_nanos,
            occurred_at_unix_nanos: row.occurred_at_unix_nanos,
            edges_out_count: row.edges_out_count,
            edges_in_count: row.edges_in_count,
        });

        // Bound the projected pool by the candidate budget, NOT the answer cap:
        // membership shaping downstream needs the full filtered pool to run its
        // relevance band over; `max_results` is applied after the band.
        if out.len() == RECALL_CANDIDATE_POOL as usize {
            break;
        }
    }

    Ok(out)
}

/// Resolve a statement hit to the memory that is its answer: the first
/// evidence memory of the (current, non-tombstoned) statement. Returns
/// `None` when the statement is gone, tombstoned, or carries no inline
/// evidence — in which case the statement hit contributes no memory.
fn statement_evidence_memory(
    rtxn: &redb::ReadTransaction,
    sid: brain_core::StatementId,
) -> Result<Option<MemoryId>, OpError> {
    let Some(stmt) = brain_metadata::statement::statement_get(rtxn, sid)
        .map_err(|e| OpError::Internal(format!("recall statement_get: {e}")))?
    else {
        return Ok(None);
    };
    if stmt.tombstoned {
        return Ok(None);
    }
    Ok(match &stmt.evidence {
        brain_core::EvidenceRef::Inline(v) => v.first().map(|e| e.memory_id),
        brain_core::EvidenceRef::Overflow(_) => None,
    })
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
        ExecutionError::Filter(inner) => OpError::Internal(format!("retrieval filter: {inner}")),
        ExecutionError::Recency(inner) => OpError::Internal(format!("recency ranking: {inner}")),
    }
}

/// Translate the planner's `Retriever` directly to the substrate
/// `RetrieverNameWire`. Avoids round-tripping through the typed-graph
/// namespace's wire enum (which would require chained `From`s on
/// foreign types, an orphan-rule violation).
fn retriever_to_wire_name(r: brain_planner::retrieval::router::Retriever) -> RetrieverNameWire {
    use brain_planner::retrieval::router::Retriever as R;
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
    use brain_planner::retrieval::router::Retriever;

    /// Minimal `MemoryResult` for merge tests — only `memory_id` matters to the
    /// cross-shard merge (it ranks by within-shard position, dedups by id).
    fn mr_min(id: u128) -> MemoryResult {
        MemoryResult {
            memory_id: id,
            text: String::new(),
            similarity_score: 0.0,
            confidence: 0.0,
            salience: 0.0,
            kind: MemoryKindWire::Semantic,
            space_id: [0u8; 16],
            session_id: 0,
            created_at_unix_nanos: 0,
            last_accessed_at_unix_nanos: 0,
            edges: None,
            graph: None,
            contributing_retrievers: Vec::new(),
            fused_score: 0.0,
            rerank_score: None,
            salience_initial: 0.0,
            access_count: 0,
            lsn: 0,
            flags: 0,
            consolidated_at_unix_nanos: None,
            occurred_at_unix_nanos: None,
            edges_out_count: 0,
            edges_in_count: 0,
        }
    }

    fn ids(pool: &[MemoryResult]) -> Vec<u128> {
        pool.iter().map(|m| m.memory_id).collect()
    }

    #[test]
    fn merge_pools_empty_is_empty() {
        assert!(merge_recall_pools(Vec::new()).is_empty());
        assert!(merge_recall_pools(vec![Vec::new(), Vec::new()]).is_empty());
    }

    #[test]
    fn merge_pools_single_pool_preserves_order() {
        let pool = vec![mr_min(10), mr_min(20), mr_min(30)];
        assert_eq!(ids(&merge_recall_pools(vec![pool])), vec![10, 20, 30]);
    }

    #[test]
    fn merge_pools_interleaves_by_within_shard_rank() {
        // Two shards. RRF by rank: both rank-1 hits outrank both rank-2 hits,
        // etc. Within an equal rank, ties break on ascending memory_id.
        let shard_a = vec![mr_min(1), mr_min(3), mr_min(5)];
        let shard_b = vec![mr_min(2), mr_min(4)];
        // rank1: {1,2} → 1,2 ; rank2: {3,4} → 3,4 ; rank3: {5} → 5
        assert_eq!(
            ids(&merge_recall_pools(vec![shard_a, shard_b])),
            vec![1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn merge_pools_dedups_by_id_keeping_best_rank() {
        // A defensive case: the same memory appears in two pools at different
        // ranks. It must appear ONCE, at its best (highest-RRF = lowest) rank.
        let shard_a = vec![mr_min(1), mr_min(9)]; // 9 at rank 2
        let shard_b = vec![mr_min(9), mr_min(2)]; // 9 at rank 1 (better)
        let out = ids(&merge_recall_pools(vec![shard_a, shard_b]));
        assert_eq!(out.iter().filter(|&&x| x == 9).count(), 1, "9 deduped");
        // 9's best rank is 1 (from shard_b), tying with 1 (rank 1 shard_a) →
        // ordered by id: 1, 9, then 2 (rank 2).
        assert_eq!(out, vec![1, 9, 2]);
    }

    #[test]
    fn merge_pools_truncates_to_candidate_budget() {
        let big: Vec<MemoryResult> = (0..(RECALL_CANDIDATE_POOL as u128 + 50))
            .map(mr_min)
            .collect();
        let out = merge_recall_pools(vec![big]);
        assert_eq!(out.len(), RECALL_CANDIDATE_POOL as usize);
    }

    // ---- merge_namespace_partials (Phase D: cross-shard grounded + HyPE) ----

    use brain_core::{StatementObject, StatementValue};

    fn grounded(score: f32, anchor_scoped: bool) -> GroundedOutcome {
        GroundedOutcome::Answer(
            GroundedAnswer {
                kind: crate::grounded::AnswerKind::Single,
                values: vec![GroundedValue {
                    predicate: "brain:x".into(),
                    object: StatementObject::Value(StatementValue::Text("v".into())),
                    confidence: 1.0,
                    source_memory: None,
                    match_score: score,
                    recency: 0,
                }],
            },
            anchor_scoped,
        )
    }

    fn part(
        memories: Vec<MemoryResult>,
        grounded: GroundedOutcome,
        hype: &[(u128, f32)],
        anchor: Option<EntityId>,
    ) -> NamespaceRecallPartial {
        NamespaceRecallPartial {
            memories,
            grounded,
            hype_scores: hype.iter().copied().collect(),
            anchor,
        }
    }

    #[test]
    fn merge_partials_picks_highest_scoring_grounded_answer() {
        let a1 = EntityId::new();
        let a2 = EntityId::new();
        let merged = merge_namespace_partials(vec![
            part(vec![mr_min(1)], grounded(0.60, true), &[], Some(a1)),
            part(vec![mr_min(2)], grounded(0.80, true), &[], Some(a2)),
        ]);
        match &merged.grounded {
            GroundedOutcome::Answer(ans, _) => {
                assert!(
                    (grounded_answer_score(ans) - 0.80).abs() < 1e-6,
                    "kept the 0.80 answer"
                );
            }
            GroundedOutcome::NoAnswer => panic!("expected an Answer"),
        }
        assert_eq!(
            merged.anchor,
            Some(a2),
            "anchor travels with the winning shard"
        );
    }

    #[test]
    fn merge_partials_prefers_anchor_scoped_on_tie() {
        let a_scoped = EntityId::new();
        let a_unscoped = EntityId::new();
        // Unscoped answer arrives FIRST at an equal score; the anchor-scoped one
        // must still win (only an anchor-scoped answer may commit the lead).
        let merged = merge_namespace_partials(vec![
            part(
                vec![mr_min(1)],
                grounded(0.70, false),
                &[],
                Some(a_unscoped),
            ),
            part(vec![mr_min(2)], grounded(0.70, true), &[], Some(a_scoped)),
        ]);
        match merged.grounded {
            GroundedOutcome::Answer(_, anchor_scoped) => {
                assert!(anchor_scoped, "anchor-scoped wins the tie")
            }
            GroundedOutcome::NoAnswer => panic!("expected an Answer"),
        }
        assert_eq!(merged.anchor, Some(a_scoped));
    }

    #[test]
    fn merge_partials_unions_hype_scores_across_shards() {
        let merged = merge_namespace_partials(vec![
            part(
                vec![mr_min(1)],
                GroundedOutcome::NoAnswer,
                &[(1, 0.5)],
                None,
            ),
            part(
                vec![mr_min(2)],
                GroundedOutcome::NoAnswer,
                &[(2, 0.9)],
                None,
            ),
        ]);
        assert_eq!(merged.hype_scores.get(&1), Some(&0.5));
        assert_eq!(merged.hype_scores.get(&2), Some(&0.9));
        assert_eq!(merged.hype_scores.len(), 2);
    }

    #[test]
    fn merge_partials_no_grounded_uses_first_some_anchor() {
        let a = EntityId::new();
        let merged = merge_namespace_partials(vec![
            part(vec![mr_min(1)], GroundedOutcome::NoAnswer, &[], None),
            part(vec![mr_min(2)], GroundedOutcome::NoAnswer, &[], Some(a)),
        ]);
        assert!(matches!(merged.grounded, GroundedOutcome::NoAnswer));
        assert_eq!(
            merged.anchor,
            Some(a),
            "falls back to a resolved anchor when no shard grounded"
        );
        // Pools still merged across both shards.
        assert_eq!(merged.memories.len(), 2);
    }

    // ---- merge_recall_pools property invariants (Phase E hardening) ----

    mod merge_property {
        use super::{mr_min, RECALL_CANDIDATE_POOL};
        use crate::handlers::recall::merge_recall_pools;
        use proptest::collection::vec as pvec;
        use proptest::prelude::*;
        use std::collections::HashSet;

        proptest! {
            #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

            /// The cross-shard merge is a pure set-and-order operation. Whatever
            /// the per-shard pools (any shard count, any sizes, overlapping ids
            /// included), the merge must never fabricate an id, never duplicate
            /// one, stay within the candidate budget, keep every id when the
            /// union fits the budget, and be deterministic.
            #[test]
            fn merge_pools_invariants(
                // Up to 8 shards, each up to 40 ids drawn from a small space so
                // cross-pool id collisions actually occur and exercise dedup.
                pools_ids in pvec(pvec(0u128..60, 0..40), 0..8),
            ) {
                let union: HashSet<u128> = pools_ids.iter().flatten().copied().collect();
                let pools: Vec<Vec<_>> = pools_ids
                    .iter()
                    .map(|ids| ids.iter().map(|&id| mr_min(id)).collect())
                    .collect();

                let out = merge_recall_pools(pools.clone());
                let out_ids: Vec<u128> = out.iter().map(|m| m.memory_id).collect();
                let out_set: HashSet<u128> = out_ids.iter().copied().collect();

                // No fabrication: every emitted id came from some input pool.
                prop_assert!(out_set.is_subset(&union));
                // No duplicates.
                prop_assert_eq!(out_ids.len(), out_set.len(), "merge must dedup by id");
                // Bounded by the candidate budget.
                prop_assert!(out_ids.len() <= RECALL_CANDIDATE_POOL as usize);
                // Completeness: when the whole union fits the budget, keep it all.
                if union.len() <= RECALL_CANDIDATE_POOL as usize {
                    prop_assert_eq!(out_set, union.clone());
                }
                // Deterministic across runs (same input → identical order).
                let out2: Vec<u128> = merge_recall_pools(pools).iter().map(|m| m.memory_id).collect();
                prop_assert_eq!(out_ids, out2);
            }
        }
    }

    #[test]
    fn age_bound_none_admits_everything() {
        assert!(passes_age_bound(None, Some(1), 2));
        assert!(passes_age_bound(None, None, 0));
    }

    #[test]
    fn age_bound_filters_on_event_time_not_ingest_time() {
        let bound = Some(2025);
        // Ingested in 2026 but the event occurred in 2020: excluded, because
        // its event time (2020) is before the bound.
        assert!(!passes_age_bound(bound, Some(2020), 2026));
        // Back-filled 2026 event with an old created_at: included, because
        // its event time (2026) is at/after the bound.
        assert!(passes_age_bound(bound, Some(2026), 1990));
        // No occurred_at: falls back to created_at.
        assert!(!passes_age_bound(bound, None, 2020));
        assert!(passes_age_bound(bound, None, 2025));
    }

    #[test]
    fn memory_ids_wire_keeps_only_memory_variants() {
        let mem = MemoryId::from_raw(0x42);
        let ids = vec![
            RankedItemId::Memory(mem),
            RankedItemId::Statement(brain_core::StatementId::new()),
            RankedItemId::Entity(EntityId::new()),
            RankedItemId::Relation(brain_core::RelationId::new()),
        ];
        assert_eq!(memory_ids_wire(&ids), vec![mem.raw()]);
    }

    #[test]
    fn memory_ids_wire_empty_input_is_empty_output() {
        assert!(memory_ids_wire(&[]).is_empty());
    }

    #[test]
    fn dropped_ids_wire_kind_tags_every_variant() {
        let mem = MemoryId::from_raw(0x42);
        let sid = brain_core::StatementId::new();
        let eid = EntityId::new();
        let rid = brain_core::RelationId::new();
        let ids = vec![
            RankedItemId::Memory(mem),
            RankedItemId::Statement(sid),
            RankedItemId::Entity(eid),
            RankedItemId::Relation(rid),
        ];
        let wire = dropped_ids_wire(&ids);
        assert_eq!(
            wire,
            vec![
                RecallTraceDroppedId {
                    kind: RankedItemKindWire::Memory,
                    id: mem.raw(),
                },
                RecallTraceDroppedId {
                    kind: RankedItemKindWire::Statement,
                    id: sid.0.as_u128(),
                },
                RecallTraceDroppedId {
                    kind: RankedItemKindWire::Entity,
                    id: eid.0.as_u128(),
                },
                RecallTraceDroppedId {
                    kind: RankedItemKindWire::Relation,
                    id: rid.0.as_u128(),
                },
            ]
        );
    }

    #[test]
    fn dropped_ids_wire_empty_input_is_empty_output() {
        assert!(dropped_ids_wire(&[]).is_empty());
    }

    #[test]
    fn capitalized_runs_extracts_proper_noun_surfaces() {
        // Single capitalized token.
        assert_eq!(capitalized_runs("who founded NeuraCorp"), vec!["NeuraCorp"]);
        // Multi-word run joined; lowercase token breaks the run.
        assert_eq!(
            capitalized_runs("did they speak at Web Summit last year"),
            vec!["Web Summit"]
        );
        // Possessive and surrounding punctuation trimmed.
        assert_eq!(
            capitalized_runs("what is NeuraCorp's mission?"),
            vec!["NeuraCorp"]
        );
        // Two separate runs.
        assert_eq!(capitalized_runs("Alice met Bob"), vec!["Alice", "Bob"]);
        // No capitalized surface → empty (lowercase / CJK handled by the
        // token path, not this one).
        assert!(capitalized_runs("what are my allergies").is_empty());
        assert!(capitalized_runs("李明 去 哪里").is_empty());
    }

    #[test]
    fn slot_hit_no_anchor_time_not_projected() {
        // A subjectless "when" cue must NOT project a Time slot: with no resolved
        // subject the global bridge probe could return an unrelated statement's
        // date, so the hit falls through to episodic instead.
        let empty: HashSet<EntityId> = HashSet::new();
        let subj = SubjectRef::Entity(EntityId::new());
        assert!(!slot_hit_projectable(Slot::Time, subj, &empty));
    }

    #[test]
    fn slot_hit_no_anchor_object_and_subject_still_project() {
        // Object/Subject slots may still project globally with no named anchor —
        // they resolve a value the cue named directly and still must clear the
        // strong floor upstream, so only Time is refused here.
        let empty: HashSet<EntityId> = HashSet::new();
        let subj = SubjectRef::Entity(EntityId::new());
        assert!(slot_hit_projectable(Slot::Object, subj, &empty));
        assert!(slot_hit_projectable(Slot::Subject, subj, &empty));
    }

    #[test]
    fn slot_hit_with_anchor_scopes_every_slot() {
        // With a named anchor the subject-scope check governs ALL slots: an
        // in-scope subject projects (Time included); an out-of-scope one never
        // does, on any slot.
        let id = EntityId::new();
        let other = EntityId::new();
        let anchors: HashSet<EntityId> = [id].into_iter().collect();
        assert!(slot_hit_projectable(
            Slot::Time,
            SubjectRef::Entity(id),
            &anchors
        ));
        assert!(!slot_hit_projectable(
            Slot::Time,
            SubjectRef::Entity(other),
            &anchors
        ));
        assert!(!slot_hit_projectable(
            Slot::Object,
            SubjectRef::Entity(other),
            &anchors
        ));
    }

    /// Minimal `RecallRequest` for ceiling-logic tests. Only `max_results`
    /// matters here; everything else is a benign zero/empty value.
    fn req_with_max(max_results: u32) -> RecallRequest {
        RecallRequest {
            scope: Default::default(),
            cue_text: String::new(),
            subject_name: String::new(),
            max_results,
            confidence_threshold: 0.0,
            session_filter: None,
            age_bound_unix_nanos: None,
            as_of_record_time_unix_nanos: None,
            kind_filter: None,
            salience_floor: 0.0,
            include_edges: false,
            include_graph: false,
            include_text: true,
            request_id: None,
            txn_id: None,
            trace: false,
            act_as: None,
        }
    }

    #[test]
    fn keyed_ceiling_uses_intrinsic_set_not_fuzzy_window() {
        // KEYED + no caller count → bounded only by the hard guard, NOT the
        // fuzzy default-50 window. This is the core of the change: the exact
        // belonging set keeps its intrinsic cardinality.
        let normalized = req_with_max(DEFAULT_RECALL_RESULTS); // 0 → default at the gate
        assert_eq!(
            keyed_membership_ceiling(&normalized, false),
            MAX_RECALL_RESULTS,
            "keyed path with no caller count must not clip to the fuzzy default window"
        );

        // KEYED + explicit caller count → honour the caller's cap.
        let explicit = req_with_max(7);
        assert_eq!(
            keyed_membership_ceiling(&explicit, true),
            7,
            "an explicit max_results is still a caller cap on the keyed path"
        );

        // KEYED + explicit count above the hard guard → clamped to the guard.
        let huge = req_with_max(MAX_RECALL_RESULTS + 100);
        assert_eq!(keyed_membership_ceiling(&huge, true), MAX_RECALL_RESULTS);

        // KEYLESS (fuzzy) path is unchanged: a no-count request keeps the
        // default-50 window — the fuzzy fallback is never widened.
        assert_eq!(
            membership_ceiling(&normalized),
            DEFAULT_RECALL_RESULTS,
            "keyless path must keep the fuzzy default window"
        );
        // And a keyless explicit cap is honoured (clamped to the guard).
        assert_eq!(membership_ceiling(&req_with_max(12)), 12);
    }

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

    // ── Read-path belonging logic: consensus collapse + abstention ──────────
    // These cover the model-free membership-arbitration changes (A1/A4) and the
    // structural abstention gate — all pure, no server, fast.

    // `GroundedOutcome`, `MemoryResult`, `MemoryKindWire`, `RetrieverNameWire`
    // are all in scope via `use super::*`.

    /// Minimal `MemoryResult` for membership-logic tests: only `memory_id` and
    /// the contributing-retriever lanes matter; everything else is benign.
    fn mr(id: u128, lanes: &[RetrieverNameWire]) -> MemoryResult {
        MemoryResult {
            memory_id: id,
            text: String::new(),
            similarity_score: 0.0,
            confidence: 0.0,
            salience: 0.0,
            kind: MemoryKindWire::Episodic,
            space_id: [0u8; 16],
            session_id: 0,
            created_at_unix_nanos: 0,
            last_accessed_at_unix_nanos: 0,
            edges: None,
            graph: None,
            contributing_retrievers: lanes.to_vec(),
            fused_score: 0.0,
            rerank_score: None,
            salience_initial: 0.0,
            access_count: 0,
            lsn: 0,
            flags: 0,
            consolidated_at_unix_nanos: None,
            occurred_at_unix_nanos: None,
            edges_out_count: 0,
            edges_in_count: 0,
        }
    }

    /// Like [`mr`] but with an explicit RRF `fused_score` — for the ordering
    /// tests that exercise the fused tiebreak within a cosine bucket.
    fn mr_fused(id: u128, lanes: &[RetrieverNameWire], fused: f32) -> MemoryResult {
        MemoryResult {
            fused_score: fused,
            ..mr(id, lanes)
        }
    }

    use RetrieverNameWire::{Graph, Lexical, Semantic};

    #[test]
    fn collapse_fires_only_when_unique_consensus_is_also_top() {
        // A is the unique 2-lane consensus AND the top-belonging member → collapse.
        let out = vec![mr(1, &[Semantic, Lexical]), mr(2, &[Semantic])];
        let got = consensus_collapse(out, Some(1));
        assert_eq!(got.len(), 1, "unique consensus that is also top → Single");
        assert_eq!(got[0].memory_id, 1);
    }

    #[test]
    fn no_collapse_when_consensus_is_not_top() {
        // A is the unique 2-lane consensus but B is the top-belonging member.
        // The lane winner is not the score winner → keep the full set so the real
        // answer (B) is never discarded. This is the paraphrase/lexical guard.
        let out = vec![mr(1, &[Semantic, Lexical]), mr(2, &[Semantic])];
        let got = consensus_collapse(out, Some(2));
        assert_eq!(
            got.len(),
            2,
            "consensus≠top must not collapse the answer away"
        );
    }

    #[test]
    fn no_collapse_on_tied_max_lane_count() {
        // Two members share the max lane count (2) → no UNIQUE consensus → keep both.
        let out = vec![mr(1, &[Semantic, Lexical]), mr(2, &[Semantic, Graph])];
        let got = consensus_collapse(out, Some(1));
        assert_eq!(got.len(), 2, "tied consensus → full set preserved");
    }

    #[test]
    fn no_collapse_when_max_is_single_lane() {
        // Every member has one lane → no multi-lane consensus → keep the set.
        let out = vec![mr(1, &[Semantic]), mr(2, &[Lexical])];
        let got = consensus_collapse(out, Some(1));
        assert_eq!(got.len(), 2, "single-lane max never collapses");
    }

    #[test]
    fn collapse_empty_in_empty_out() {
        assert!(consensus_collapse(Vec::new(), None).is_empty());
        // top_member_id None can never equal a real id → never collapses.
        let out = vec![mr(1, &[Semantic, Lexical]), mr(2, &[Semantic])];
        assert_eq!(consensus_collapse(out, None).len(), 2);
    }

    #[test]
    fn anchor_abstention_keeps_set_when_anchor_present() {
        // A resolved anchor is the OTHER gate's domain → this gate is a no-op,
        // regardless of support.
        let members = vec![mr(1, &[Semantic])];
        let kept = apply_anchor_abstention(
            members,
            Some(brain_core::EntityId::new()),
            &GroundedOutcome::NoAnswer,
            false,
        );
        assert_eq!(kept.len(), 1, "a resolved anchor suppresses this gate");
    }

    #[test]
    fn anchor_abstention_abstains_only_on_zero_support() {
        // No anchor, grounded NoAnswer, and NO member has any cross-lane support
        // (max_support == 0) → abstain (FIX C).
        let members = vec![mr(1, &[Semantic]), mr(2, &[Semantic])];
        let kept = apply_anchor_abstention(members, None, &GroundedOutcome::NoAnswer, false);
        assert!(
            kept.is_empty(),
            "nothing belongs (lone passage cosine) → abstain"
        );
    }

    #[test]
    fn anchor_abstention_keeps_supported_set() {
        // Any support (>=1) means the facts ship — FIX C never empties a set that
        // has real support, even without an anchor or a grounded answer.
        let members = vec![mr(1, &[Semantic]), mr(2, &[Semantic])];
        let kept = apply_anchor_abstention(members, None, &GroundedOutcome::NoAnswer, true);
        assert_eq!(
            kept.len(),
            2,
            "supported members are returned, never emptied"
        );
    }

    // ── Kind-presence abstention (adversarial questions) ────────────────────

    #[test]
    fn kind_presence_keeps_set_when_grounded_answered() {
        // The typed graph has the fact → never our gate's business, even at 0.
        let members = vec![mr(1, &[Semantic])];
        let g = GroundedOutcome::Answer(
            GroundedAnswer {
                kind: AnswerKind::Single,
                values: Vec::new(),
            },
            true,
        );
        let kept =
            apply_kind_presence_abstention(members, Some(brain_core::EntityId::new()), &g, false);
        assert_eq!(kept.len(), 1, "grounded answer present → keep");
    }

    #[test]
    fn kind_presence_keeps_set_without_anchor() {
        // No subject resolved → the other gate handles it, not this one.
        let members = vec![mr(1, &[Semantic])];
        let kept = apply_kind_presence_abstention(members, None, &GroundedOutcome::NoAnswer, false);
        assert_eq!(kept.len(), 1, "no anchor → this gate is a no-op");
    }

    #[test]
    fn kind_presence_abstains_on_zero_support() {
        // Subject resolved, grounded NoAnswer, and NO member has any cross-lane
        // support (max_support == 0) → the adversarial case → None. The member's
        // own lanes are irrelevant; the gate keys on the corroboration scalar.
        let members = vec![mr(1, &[Semantic])];
        let kept = apply_kind_presence_abstention(
            members,
            Some(brain_core::EntityId::new()),
            &GroundedOutcome::NoAnswer,
            false,
        );
        assert!(
            kept.is_empty(),
            "subject resolved but nothing supports the cue → None"
        );
    }

    #[test]
    fn kind_presence_keeps_supported_answer() {
        // A supported member (max_support >= 1) is a real answer grounded simply
        // hadn't extracted → keep. FIX C never emits an empty answer over support.
        let members = vec![mr(1, &[Graph, Semantic]), mr(2, &[Semantic])];
        let kept = apply_kind_presence_abstention(
            members,
            Some(brain_core::EntityId::new()),
            &GroundedOutcome::NoAnswer,
            true,
        );
        assert_eq!(kept.len(), 2, "a belonging member → keep the set");
    }

    // ── Slot-projection subject scoping (wrong-subject rejection) ────────────

    #[test]
    fn slot_projection_rejects_wrong_subject_when_anchor_resolved() {
        // "when did Melanie run a charity race" resolves anchor = {Melanie}. The
        // GLOBAL question-bridge probe can surface a Slot::Time question that was
        // generated from CAROLINE's event — that hit must be REJECTED so it can
        // never project Caroline's time as Melanie's answer.
        let melanie = EntityId::new();
        let caroline = EntityId::new();
        let anchors: HashSet<EntityId> = [melanie].into_iter().collect();

        assert!(
            !statement_subject_in_scope(SubjectRef::Entity(caroline), &anchors),
            "a fact about a different named subject must not project"
        );
        assert!(
            statement_subject_in_scope(SubjectRef::Entity(melanie), &anchors),
            "the anchored subject's own fact projects"
        );
        // A memory-subject (temporal Event) or a pending subject can never equal
        // a named anchor, so both are rejected under a non-empty scope.
        assert!(!statement_subject_in_scope(
            SubjectRef::Memory(MemoryId::from_raw(1)),
            &anchors
        ));
    }

    #[test]
    fn slot_projection_global_when_no_named_anchor() {
        // "when was the trip" resolves no named subject (only the always-present
        // self fallback), so the anchor scope is empty and the projection stays
        // global — historical behavior, since there is no named anchor to check
        // a hit against.
        let empty: HashSet<EntityId> = HashSet::new();
        assert!(statement_subject_in_scope(
            SubjectRef::Entity(EntityId::new()),
            &empty
        ));
        assert!(statement_subject_in_scope(
            SubjectRef::Memory(MemoryId::from_raw(9)),
            &empty
        ));
    }

    #[test]
    fn abstention_stays_honest_after_wrong_subject_rejection() {
        // Because the wrong-subject Time hit is rejected (test above),
        // `best_grounded_for_cue` returns NoAnswer instead of a spurious Caroline
        // Answer. With the anchor resolved (Melanie) but grounded NoAnswer, the
        // kind-presence gate abstains when NO surviving member is supported — the
        // sole survivor is an off-cue buried fact with zero cross-lane support
        // (max_support == 0) — restoring honest abstention rather than merely
        // reordering.
        let members = vec![mr(1, &[])];
        let kept = apply_kind_presence_abstention(
            members,
            Some(EntityId::new()),      // Melanie resolved
            &GroundedOutcome::NoAnswer, // no spurious wrong-subject answer
            false,                      // nothing belongs (lone off-cue fact)
        );
        assert!(
            kept.is_empty(),
            "no wrong-subject Answer + zero support → honest abstention"
        );
    }

    // ── Phase 1: answer-relevance is the PRIMARY ordering key ───────────────

    #[test]
    fn answer_relevance_leads_over_higher_cosine_topical_neighbor() {
        // The dominant failure: a topically-adjacent memory (id 1) has the higher
        // passage cosine but does NOT answer the cue, while the answering memory
        // (id 2) has the higher HyPE answer-lead. Answer-relevance is primary, so
        // id 2 must lead. Membership is unchanged — both survive.
        let out = vec![mr(1, &[Semantic]), mr(2, &[Semantic]), mr(3, &[Semantic])];
        let hype: HashMap<u128, f32> = [(1u128, 0.30), (2u128, 0.80), (3u128, 0.25)]
            .into_iter()
            .collect();
        let cos: HashMap<u128, f32> = [(1u128, 0.90), (2u128, 0.50), (3u128, 0.40)]
            .into_iter()
            .collect();
        let got = order_by_answer_relevance(out, &hype, &cos);
        assert_eq!(got.len(), 3, "recall untouched — only order changes");
        assert_eq!(
            got[0].memory_id, 2,
            "the answering memory leads over the higher-cosine topical neighbor"
        );
    }

    #[test]
    fn answer_relevance_flat_hype_falls_back_to_cosine_order() {
        // No HyPE signal (empty map) → no answer-relevance discrimination, so the
        // members order by topical cosine: distinct cosines (gaps well beyond one
        // bucket) decide, so the highest-cosine member leads.
        let out = vec![mr(1, &[Semantic]), mr(2, &[Semantic]), mr(3, &[Semantic])];
        let cos: HashMap<u128, f32> = [(1u128, 0.90), (2u128, 0.70), (3u128, 0.40)]
            .into_iter()
            .collect();
        let got = order_by_answer_relevance(out, &HashMap::new(), &cos);
        assert_eq!(
            got.iter().map(|m| m.memory_id).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "empty HyPE → order by topical cosine when the gaps are real"
        );

        // A present-but-FLAT HyPE (all equal) is equally non-discriminating → the
        // incoming (assembly) order is preserved verbatim (the lexical/paraphrase
        // no-regression guarantee); this is NOT the exact-token regime (the cosine
        // gap is real, so the flat-corpus path does not engage).
        let out = vec![mr(3, &[Semantic]), mr(1, &[Semantic])];
        let flat: HashMap<u128, f32> = [(3u128, 0.5), (1u128, 0.5)].into_iter().collect();
        let got = order_by_answer_relevance(out, &flat, &cos);
        assert_eq!(
            got.iter().map(|m| m.memory_id).collect::<Vec<_>>(),
            vec![3, 1],
            "flat HyPE + real cosine gap → incoming order preserved"
        );
    }

    #[test]
    fn answer_relevance_near_duplicate_cosine_defers_to_fused_lexical() {
        // The exact-token regression at the unit level: three near-duplicate
        // passages whose cosines sit within one bucket (0.727–0.739, BGE-small
        // compression) and whose HyPE is flat. Pure-cosine ordering is query-
        // independent and strands the exact-token match. Because all three tie on
        // both the answer-relevance and topical-cosine buckets, the RRF
        // `fused_score` — which carries the lexical rank-1 exact match — decides.
        // Only id 2 was surfaced by the lexical lane (its nonce), so it leads even
        // though it does NOT have the highest raw cosine.
        let out = vec![
            mr_fused(1, &[Semantic], 0.016),
            mr_fused(2, &[Semantic, Lexical], 0.048),
            mr_fused(3, &[Semantic], 0.015),
        ];
        let flat: HashMap<u128, f32> = [(1u128, 0.31), (2u128, 0.30), (3u128, 0.32)]
            .into_iter()
            .collect();
        let cos: HashMap<u128, f32> = [(1u128, 0.739), (2u128, 0.727), (3u128, 0.733)]
            .into_iter()
            .collect();
        let got = order_by_answer_relevance(out, &flat, &cos);
        assert_eq!(
            got[0].memory_id, 2,
            "within a cosine bucket the exact lexical (fused) match must lead",
        );
    }

    #[test]
    fn answer_relevance_unique_lexical_leads_within_flat_bucket() {
        // The exact-token known-answer case, faithful to the live measurement: a
        // dense near-duplicate corpus where every member sits in one cosine bucket
        // (~0.73) with flat HyPE, and the target's RRF `fused_score` is NOT the
        // highest (the semantic-rank spread buries the small lexical bump). The
        // target is the UNIQUE original-query lexical hit, so it must lead within
        // the bucket even though a Semantic-only neighbour has a higher fused
        // score.
        let out = vec![
            mr_fused(1, &[Semantic], 0.141),
            mr_fused(2, &[Semantic], 0.118),
            mr_fused(3, &[Semantic, Lexical], 0.108), // target, lower fused
            mr_fused(4, &[Semantic], 0.107),
        ];
        let flat: HashMap<u128, f32> = [(1u128, 0.30), (2u128, 0.30), (3u128, 0.30), (4u128, 0.30)]
            .into_iter()
            .collect();
        let cos: HashMap<u128, f32> = [
            (1u128, 0.738),
            (2u128, 0.735),
            (3u128, 0.733),
            (4u128, 0.735),
        ]
        .into_iter()
        .collect();
        let got = order_by_answer_relevance(out, &flat, &cos);
        assert_eq!(
            got[0].memory_id, 3,
            "the unique exact-token lexical match leads within the flat cosine bucket",
        );
    }

    #[test]
    fn answer_relevance_non_unique_lexical_does_not_hijack() {
        // Precision guard: when a low-specificity term matches MANY members (all
        // tagged Lexical), the exact-match key is inert — no unique hit — so
        // ordering falls back to fused_score within the bucket. id 1 (highest
        // fused) leads; the several lexical hits do not collectively hijack.
        let out = vec![
            mr_fused(1, &[Semantic], 0.141),
            mr_fused(2, &[Semantic, Lexical], 0.118),
            mr_fused(3, &[Semantic, Lexical], 0.108),
        ];
        let flat: HashMap<u128, f32> = [(1u128, 0.30), (2u128, 0.30), (3u128, 0.30)]
            .into_iter()
            .collect();
        let cos: HashMap<u128, f32> = [(1u128, 0.738), (2u128, 0.735), (3u128, 0.733)]
            .into_iter()
            .collect();
        let got = order_by_answer_relevance(out, &flat, &cos);
        assert_eq!(
            got[0].memory_id, 1,
            "non-unique lexical coverage must not hijack the lead over the top fused member",
        );
    }

    #[test]
    fn answer_relevance_unique_lexical_yields_to_real_cosine_gap() {
        // The exact-match tiebreak sits BELOW the cosine bucket: a genuine topical
        // gap (a better cosine bucket) still wins over a unique lexical hit in a
        // worse bucket. id 1 (cos 0.80) leads over the unique-lexical id 2 (0.55).
        let out = vec![
            mr_fused(1, &[Semantic], 0.10),
            mr_fused(2, &[Semantic, Lexical], 0.20),
        ];
        let cos: HashMap<u128, f32> = [(1u128, 0.80), (2u128, 0.55)].into_iter().collect();
        let got = order_by_answer_relevance(out, &HashMap::new(), &cos);
        assert_eq!(
            got[0].memory_id, 1,
            "a real cosine gap outranks a unique lexical hit in a worse bucket",
        );
    }

    #[test]
    fn answer_relevance_real_cosine_gap_beats_fused_coverage() {
        // Guard against coverage bias: a genuine topical-cosine gap (larger than
        // one bucket) must NOT be overridden by a lexical coverage hit. id 1 is the
        // clear topical answer (cos 0.80); id 2 is a distractor a common term
        // surfaced lexically (higher fused) but with a much lower cosine (0.55).
        // The cosine bucket separates them, so id 1 leads — the lexical signal only
        // breaks ties, it never overrides a real topical gap.
        let out = vec![
            mr_fused(1, &[Semantic], 0.016),
            mr_fused(2, &[Semantic, Lexical], 0.048),
        ];
        let cos: HashMap<u128, f32> = [(1u128, 0.80), (2u128, 0.55)].into_iter().collect();
        let got = order_by_answer_relevance(out, &HashMap::new(), &cos);
        assert_eq!(
            got[0].memory_id, 1,
            "a real cosine gap wins over higher lexical-coverage fused_score",
        );
    }

    #[test]
    fn answer_relevance_cosine_breaks_hype_ties() {
        // When HyPE varies overall (so ordering runs) but two members tie on the
        // answer-lead, passage cosine is the SECONDARY tiebreak.
        let out = vec![mr(1, &[Semantic]), mr(2, &[Semantic]), mr(3, &[Semantic])];
        let hype: HashMap<u128, f32> = [(1u128, 0.80), (2u128, 0.80), (3u128, 0.40)]
            .into_iter()
            .collect();
        let cos: HashMap<u128, f32> = [(1u128, 0.30), (2u128, 0.60), (3u128, 0.90)]
            .into_iter()
            .collect();
        let got = order_by_answer_relevance(out, &hype, &cos);
        assert_eq!(
            got.iter().map(|m| m.memory_id).collect::<Vec<_>>(),
            vec![2, 1, 3],
            "equal answer-lead → higher cosine first; the low-lead member stays last"
        );
    }

    // ── Phase 2/3: grounded commit ──────────────────────────────────────────

    use crate::grounded::GroundedValue;

    /// One grounded value over a text object, with a chosen source memory and
    /// match score. Confidence / recency are benign — only object, source, and
    /// score drive the commit.
    fn gv(text: &str, src: u128, match_score: f32) -> GroundedValue {
        GroundedValue {
            predicate: "brain:works_at".to_string(),
            object: brain_core::StatementObject::Value(brain_core::StatementValue::Text(
                text.to_string(),
            )),
            confidence: 1.0,
            source_memory: Some(MemoryId::from_raw(src)),
            match_score,
            recency: 0,
        }
    }

    fn outcome(
        kind: AnswerKind,
        values: Vec<GroundedValue>,
        anchor_scoped: bool,
    ) -> GroundedOutcome {
        GroundedOutcome::Answer(GroundedAnswer { kind, values }, anchor_scoped)
    }

    /// A [`support`] lookup for the commit tests: the listed ids are cross-lane
    /// CORROBORATED (support `SUPPORT_CORROBORATED` = grounded + one lane); every
    /// other id is grounded-only (support 1, below the gate).
    fn sup(corroborated: &[u128]) -> impl Fn(u128) -> u8 + '_ {
        move |id| {
            if corroborated.contains(&id) {
                SUPPORT_CORROBORATED
            } else {
                1
            }
        }
    }

    #[test]
    fn grounded_commit_fires_on_corroborated_anchor_scoped_strong_single() {
        // FIX B: anchor-scoped Single clearing the strong floor AND cross-lane
        // corroborated (support >= 2) → commit that one source memory, shape Single.
        let g = outcome(AnswerKind::Single, vec![gv("OpenAI", 7, 0.7)], true);
        let lead = grounded_commit(&g, &sup(&[7])).expect("corroborated strong single commits");
        assert_eq!(lead.ids, vec![7]);
        assert_eq!(lead.shape, AnswerKindWire::Single);
    }

    #[test]
    fn grounded_commit_declines_uncorroborated_single() {
        // FIX B: the SAME strong, anchor-scoped value but grounded-only (support 1,
        // no independent lane) → NO commit. A lone topical-cosine neighbor may not
        // lead; it falls to the answer-relevance-ordered list instead.
        let g = outcome(AnswerKind::Single, vec![gv("OpenAI", 7, 0.7)], true);
        assert!(
            grounded_commit(&g, &sup(&[])).is_none(),
            "an uncorroborated grounded value must not commit"
        );
    }

    #[test]
    fn grounded_commit_declines_unscoped_answer() {
        // NOT anchor-scoped (self / loose match) → no commit even when corroborated.
        // The standing grounded-first tripwire: a wrong-subject match can never
        // hijack the lead.
        let g = outcome(AnswerKind::Single, vec![gv("OpenAI", 7, 0.7)], false);
        assert!(
            grounded_commit(&g, &sup(&[7])).is_none(),
            "an unscoped grounded value must not commit"
        );
    }

    #[test]
    fn grounded_commit_declines_weak_match() {
        // 0.55 clears the loose grounded floor (0.5) but not the strong-commit bar
        // (0.6), even anchor-scoped + corroborated → no commit (floor is necessary).
        let g = outcome(AnswerKind::Single, vec![gv("OpenAI", 7, 0.55)], true);
        assert!(
            grounded_commit(&g, &sup(&[7])).is_none(),
            "a floor-grazing match must not commit"
        );
    }

    #[test]
    fn grounded_commit_declines_no_answer() {
        assert!(grounded_commit(&GroundedOutcome::NoAnswer, &sup(&[])).is_none());
    }

    #[test]
    fn grounded_commit_set_commits_when_a_member_is_corroborated() {
        // A Set whose representative clears 0.6 and at least one member is
        // corroborated → commit the whole SET (both source memories lead), shape
        // Many. "what did X research?" → both facts. The `other`/`single_hop`
        // gains are preserved: a genuinely-answering set the lanes agree on commits.
        let g = outcome(
            AnswerKind::Set,
            vec![gv("topology", 4, 0.7), gv("category theory", 9, 0.7)],
            true,
        );
        let lead = grounded_commit(&g, &sup(&[4])).expect("corroborated strong set commits");
        assert_eq!(lead.ids, vec![4, 9], "both members lead, in grounded order");
        assert_eq!(lead.shape, AnswerKindWire::Many);
    }

    #[test]
    fn grounded_commit_declines_uncorroborated_set() {
        // FIX B: a whole set of grounded-only neighbors (no member confirmed by an
        // independent lane) must NOT lead — it falls to answer-relevance ordering.
        let g = outcome(
            AnswerKind::Set,
            vec![gv("topology", 4, 0.7), gv("category theory", 9, 0.7)],
            true,
        );
        assert!(
            grounded_commit(&g, &sup(&[])).is_none(),
            "an uncorroborated set must not commit"
        );
    }

    #[test]
    fn apply_grounded_commit_leads_and_retains_episodic_below() {
        // The standing guardrail: the committed lead is FIRST, and the rest of the
        // membership is RETAINED below it — never cleared. Here the lead is id 2;
        // the episodic remainder (ids 1, 3) is answer-relevance ordered beneath.
        let out = vec![mr(1, &[Semantic]), mr(2, &[Semantic]), mr(3, &[Semantic])];
        let lead = CommitLead {
            ids: vec![2],
            shape: AnswerKindWire::Single,
        };
        // Among the remainder, id 3 answers better than id 1 → 3 before 1.
        let hype: HashMap<u128, f32> = [(1u128, 0.20), (3u128, 0.70)].into_iter().collect();
        let cos: HashMap<u128, f32> = [(1u128, 0.90), (3u128, 0.40)].into_iter().collect();
        let got = apply_grounded_commit(out, &lead, &hype, &cos);
        assert_eq!(
            got.iter().map(|m| m.memory_id).collect::<Vec<_>>(),
            vec![2, 3, 1],
            "committed lead first, then the answer-relevance-ordered episodic set"
        );
    }

    #[test]
    fn apply_grounded_commit_set_lead_keeps_grounded_order() {
        // A Set commit puts every member at the front in grounded order, episodic
        // retained below.
        let out = vec![
            mr(1, &[Semantic]),
            mr(4, &[Semantic]),
            mr(9, &[Semantic]),
            mr(2, &[Semantic]),
        ];
        let lead = CommitLead {
            ids: vec![9, 4],
            shape: AnswerKindWire::Many,
        };
        let got = apply_grounded_commit(out, &lead, &HashMap::new(), &HashMap::new());
        assert_eq!(
            got.iter().map(|m| m.memory_id).take(2).collect::<Vec<_>>(),
            vec![9, 4],
            "set members lead in grounded order"
        );
        assert_eq!(got.len(), 4, "episodic members 1 and 2 retained below");
    }

    // ── Support: the one unifying corroboration signal ──────────────────────

    #[test]
    fn support_counts_each_independent_lane_once() {
        let none: HashSet<u128> = HashSet::new();
        let no_hype: HashMap<u128, f32> = HashMap::new();
        // No lanes, not a strong semantic match, not grounded, no HyPE → 0.
        assert_eq!(support(1, &[], false, &none, &no_hype), 0);
        // A STRONG semantic match alone → 1.
        assert_eq!(support(1, &[], true, &none, &no_hype), 1);
        // A Semantic fan-out lane WITHOUT a strong cosine no longer counts → 0.
        // This is the abstention fix: a weak (below-strong-bar) semantic hit —
        // the BGE-compression case a nonsense cue produces — corroborates nothing.
        assert_eq!(support(1, &[Semantic], false, &none, &no_hype), 0);
        // Strong semantic; the Semantic lane doesn't double-count → 1.
        assert_eq!(support(1, &[Semantic], true, &none, &no_hype), 1);
        // Lexical + Graph are real lanes; a non-strong Semantic lane adds nothing → 2.
        assert_eq!(
            support(1, &[Semantic, Lexical, Graph], false, &none, &no_hype),
            2
        );
        // With a strong semantic match, all three lanes count → 3.
        assert_eq!(
            support(1, &[Semantic, Lexical, Graph], true, &none, &no_hype),
            3
        );
    }

    #[test]
    fn support_counts_grounded_and_hype_lanes() {
        let grounded: HashSet<u128> = [1u128].into_iter().collect();
        let no_grounded: HashSet<u128> = HashSet::new();
        let at_strong: HashMap<u128, f32> = [(1u128, STRONG_HYPE_SUPPORT)].into_iter().collect();
        // A weak lead below the strong support bar (the old 0.5 ordering floor).
        let weak: HashMap<u128, f32> = [(1u128, STRONG_HYPE_SUPPORT - 0.1)].into_iter().collect();
        let empty: HashMap<u128, f32> = HashMap::new();
        // Grounded source alone → 1.
        assert_eq!(support(1, &[], false, &grounded, &empty), 1);
        // HyPE at/above the STRONG support bar alone → 1. A weak lead (at the
        // looser ordering floor, below the strong bar) corroborates nothing → 0:
        // this is the HyPE half of the abstention fix.
        assert_eq!(support(1, &[], false, &no_grounded, &at_strong), 1);
        assert_eq!(support(1, &[], false, &no_grounded, &weak), 0);
        // Grounded + one independent lane = corroborated. A real Lexical lane
        // corroborates; a non-strong Semantic lane would NOT (it must clear the
        // strong-support bar), so this uses Lexical to express the invariant.
        assert!(support(1, &[Lexical], false, &grounded, &empty) >= SUPPORT_CORROBORATED);
        // Grounded + a STRONG semantic match also corroborates.
        assert!(support(1, &[], true, &grounded, &empty) >= SUPPORT_CORROBORATED);
    }

    #[test]
    fn support_grounded_only_is_not_corroborated() {
        // The FIX B invariant: a grounded-only source (no independent lane) is
        // support 1 — below the corroboration bar, so it cannot commit.
        let grounded: HashSet<u128> = [5u128].into_iter().collect();
        let no_hype: HashMap<u128, f32> = HashMap::new();
        assert_eq!(support(5, &[], false, &grounded, &no_hype), 1);
        assert!(support(5, &[], false, &grounded, &no_hype) < SUPPORT_CORROBORATED);
    }

    // ── FIX A: cue-scoped object set membership ──────────────────────────────

    #[test]
    fn cue_scoped_object_set_folds_cross_predicate_scopes_and_dedups() {
        use brain_core::{
            Entity, EntityType, EvidenceRef, Statement, StatementKind, StatementObject,
            StatementValue, SubjectRef,
        };
        let dir = tempfile::tempdir().unwrap();
        let db = brain_metadata::MetadataDb::open(dir.path().join("m.redb")).unwrap();
        let scope =
            brain_metadata::RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xC1; 16]);
        let x = EntityId::new();
        let y = EntityId::new();
        let wtxn = db.write_txn().unwrap();
        for (id, name) in [(x, "X"), (y, "Y")] {
            brain_metadata::entity::ops::entity_put(
                &wtxn,
                scope,
                brain_core::SessionId::DEFAULT,
                &Entity::new_active(id, EntityType::PERSON_ID, name.into(), name.into(), 1),
            )
            .unwrap();
        }
        let p_plays = brain_metadata::schema::predicate::predicate_intern_or_get(
            &wtxn, "test", "plays", 0, 1,
        )
        .unwrap();
        let p_enjoys = brain_metadata::schema::predicate::predicate_intern_or_get(
            &wtxn, "test", "enjoys", 0, 1,
        )
        .unwrap();
        let mk = |subject: EntityId, pid, obj: &str| {
            Statement::new_root(
                brain_core::StatementId::new(),
                StatementKind::Fact,
                SubjectRef::Entity(subject),
                pid,
                StatementObject::Value(StatementValue::Text(obj.into())),
                0.9,
                EvidenceRef::default(),
                brain_core::ExtractorId::from(0),
                1,
                1,
            )
        };
        let s_soccer = mk(x, p_plays, "soccer"); // on-cue
        let s_tennis = mk(x, p_enjoys, "tennis"); // on-cue, DIFFERENT predicate
        let s_soccer_dup = mk(x, p_plays, "soccer"); // duplicate object
        let s_chess = mk(x, p_plays, "chess"); // off-cue (its hit is below the floor)
        let s_cricket = mk(y, p_plays, "cricket"); // WRONG subject (Y, not the anchor)
        for s in [&s_soccer, &s_tennis, &s_soccer_dup, &s_chess, &s_cricket] {
            brain_metadata::statement::crud::statement_create(
                &wtxn,
                scope,
                brain_core::SessionId::DEFAULT,
                s,
                1,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let anchors: HashSet<EntityId> = [x].into_iter().collect();
        // Hits arrive score-descending. cricket (0.90) is highest but WRONG subject
        // → scoped out; chess (0.40) is below the strong floor → dropped;
        // soccer_dup shares soccer's object → deduped.
        let hits = vec![
            (s_cricket.id, Slot::Object, 0.90),
            (s_soccer.id, Slot::Object, 0.80),
            (s_tennis.id, Slot::Object, 0.72),
            (s_soccer_dup.id, Slot::Object, 0.70),
            (s_chess.id, Slot::Object, 0.40),
        ];
        let values = cue_scoped_object_set(&rtxn, &hits, &anchors, scope).unwrap();
        let objs: Vec<String> = values
            .iter()
            .filter_map(|v| match &v.object {
                StatementObject::Value(StatementValue::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            objs,
            vec!["soccer".to_string(), "tennis".to_string()],
            "cross-predicate on-cue folds in; off-cue (below floor), wrong-subject, and duplicate excluded"
        );
    }

    // ── R1: bounded cue-token surface mining (resolver-invocation DoS) ──────

    #[test]
    fn mine_cue_surfaces_caps_pathological_cue() {
        // A cue of many DISTINCT non-resolving tokens must not fan out into an
        // unbounded number of resolver surfaces — each surface is one heavy
        // `entity_resolve_scored` call on the shard core.
        let cue: String = (0..10_000)
            .map(|i| format!("tok{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let surfaces = mine_cue_surfaces("", &cue);
        assert!(
            surfaces.len() <= MAX_CUE_SURFACES,
            "distinct-token cue mined {} surfaces, cap is {}",
            surfaces.len(),
            MAX_CUE_SURFACES
        );
    }

    #[test]
    fn mine_cue_surfaces_caps_repeated_token_cue() {
        // A cue that is millions of copies of a few distinct tokens fills the
        // dedup set slowly, so the surface cap alone wouldn't stop the scan; the
        // token-scan clamp keeps it bounded regardless.
        let cue = "alpha beta ".repeat(1_000_000);
        let surfaces = mine_cue_surfaces("", &cue);
        assert!(surfaces.len() <= MAX_CUE_SURFACES);
        // Only two distinct tokens exist, so we resolve at most two surfaces.
        assert!(surfaces.contains(&"alpha".to_string()));
        assert!(surfaces.contains(&"beta".to_string()));
        assert_eq!(surfaces.len(), 2);
    }

    #[test]
    fn mine_cue_surfaces_keeps_normal_cue() {
        // A realistic cue still yields its named subject as a surface, unchanged.
        let surfaces = mine_cue_surfaces("", "who does Niraj report to");
        assert!(
            surfaces.contains(&"Niraj".to_string()),
            "normal cue must still surface its named subject: {surfaces:?}"
        );
        // Explicit subject_name bypasses mining entirely.
        assert_eq!(
            mine_cue_surfaces("Niraj Georgian", "irrelevant cue text"),
            vec!["Niraj Georgian".to_string()]
        );
    }

    // ── R2: slot-projection tenant scoping (cross-space answer leak) ─────────

    #[allow(clippy::type_complexity)]
    fn scoped_statement_fixture() -> (
        tempfile::TempDir,
        brain_metadata::MetadataDb,
        brain_metadata::RowScope, // scope A (caller)
        brain_metadata::RowScope, // scope B (foreign)
        brain_core::StatementId,  // A's statement
        brain_core::StatementId,  // B's statement
    ) {
        use brain_core::{
            Entity, EntityType, EvidenceRef, Statement, StatementKind, StatementObject,
            StatementValue, SubjectRef,
        };
        let dir = tempfile::tempdir().unwrap();
        let db = brain_metadata::MetadataDb::open(dir.path().join("m.redb")).unwrap();
        let scope_a =
            brain_metadata::RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xA1; 16]);
        let scope_b =
            brain_metadata::RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xB2; 16]);
        let subj_a = EntityId::new();
        let subj_b = EntityId::new();
        let wtxn = db.write_txn().unwrap();
        for (scope, id, name) in [(scope_a, subj_a, "Alice"), (scope_b, subj_b, "Bob")] {
            brain_metadata::entity::ops::entity_put(
                &wtxn,
                scope,
                brain_core::SessionId::DEFAULT,
                &Entity::new_active(id, EntityType::PERSON_ID, name.into(), name.into(), 1),
            )
            .unwrap();
        }
        let pid = brain_metadata::schema::predicate::predicate_intern_or_get(
            &wtxn, "test", "plays", 0, 1,
        )
        .unwrap();
        let mk = |subject: EntityId, obj: &str| {
            Statement::new_root(
                brain_core::StatementId::new(),
                StatementKind::Fact,
                SubjectRef::Entity(subject),
                pid,
                StatementObject::Value(StatementValue::Text(obj.into())),
                0.9,
                EvidenceRef::default(),
                brain_core::ExtractorId::from(0),
                1,
                1,
            )
        };
        let s_a = mk(subj_a, "soccer");
        let s_b = mk(subj_b, "cricket");
        let (id_a, id_b) = (s_a.id, s_b.id);
        brain_metadata::statement::crud::statement_create(
            &wtxn,
            scope_a,
            brain_core::SessionId::DEFAULT,
            &s_a,
            1,
        )
        .unwrap();
        brain_metadata::statement::crud::statement_create(
            &wtxn,
            scope_b,
            brain_core::SessionId::DEFAULT,
            &s_b,
            1,
        )
        .unwrap();
        wtxn.commit().unwrap();
        (dir, db, scope_a, scope_b, id_a, id_b)
    }

    #[test]
    fn statement_in_caller_scope_drops_foreign_and_missing() {
        let (_dir, db, scope_a, scope_b, id_a, id_b) = scoped_statement_fixture();
        let rtxn = db.read_txn().unwrap();
        assert!(statement_in_caller_scope(&rtxn, id_a, scope_a).unwrap());
        assert!(!statement_in_caller_scope(&rtxn, id_b, scope_a).unwrap());
        assert!(statement_in_caller_scope(&rtxn, id_b, scope_b).unwrap());
        // A missing row is out of scope, never an answer source.
        assert!(
            !statement_in_caller_scope(&rtxn, brain_core::StatementId::new(), scope_a).unwrap()
        );
    }

    #[test]
    fn cue_scoped_object_set_drops_foreign_space_hit_with_empty_anchors() {
        // The regression: with NO resolved subject (anchors empty) the old
        // `slot_hit_projectable` let Object hits from ANY tenant through, so a
        // space-B statement whose object-slot question matched a space-A caller's
        // cue would project (and, upstream, disable abstention). The row-load
        // scope check must drop the foreign hit while keeping the same-space one.
        use brain_core::{StatementObject, StatementValue};
        let (_dir, db, scope_a, _scope_b, id_a, id_b) = scoped_statement_fixture();
        let rtxn = db.read_txn().unwrap();
        let anchors: HashSet<EntityId> = HashSet::new(); // cue resolved no subject
        let hits = vec![
            (id_b, Slot::Object, 0.90), // foreign (space B), strongest
            (id_a, Slot::Object, 0.80), // same space (A)
        ];
        let values = cue_scoped_object_set(&rtxn, &hits, &anchors, scope_a).unwrap();
        let objs: Vec<String> = values
            .iter()
            .filter_map(|v| match &v.object {
                StatementObject::Value(StatementValue::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            objs,
            vec!["soccer".to_string()],
            "foreign-space object must be dropped; same-space object still projects"
        );
    }

    // ── T1: RECALL/QUERY trace cross-tenant leak wall ───────────────────────
    //
    // The answer path re-verifies `(namespace_id, space_id)` on every row, but
    // the opt-in `trace = true` diagnostic surface renders candidate text /
    // labels for the pre-fusion set of EVERY lane — and the lexical / graph /
    // statement-semantic lanes don't push scope down. Without a per-candidate
    // re-check, a caller in space A whose lanes surface space-B rows would read
    // B's memory text, statement subject-predicate-object, entity canonical
    // names, and relation labels inside its own trace. These pin the wall at the
    // exact rendering boundary: a foreign / missing candidate renders as an
    // opaque id with NO content, while same-space candidates render fully.

    #[allow(clippy::type_complexity)]
    fn scoped_trace_fixture() -> (
        tempfile::TempDir,
        brain_metadata::MetadataDb,
        brain_metadata::RowScope, // scope A (caller)
        brain_metadata::RowScope, // scope B (foreign)
        EntityId,                 // A's entity (Alice)
        EntityId,                 // B's entity (Bob)
        brain_core::StatementId,  // A's statement
        brain_core::StatementId,  // B's statement
        brain_core::RelationId,   // A's relation
        brain_core::RelationId,   // B's relation
    ) {
        use brain_core::{
            Entity, EntityType, EvidenceRef, Relation, Statement, StatementKind, StatementObject,
            StatementValue,
        };
        let dir = tempfile::tempdir().unwrap();
        let db = brain_metadata::MetadataDb::open(dir.path().join("m.redb")).unwrap();
        // Distinct in BOTH halves of the scope so the test exercises the
        // namespace wall and the space wall together.
        let scope_a =
            brain_metadata::RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xA1; 16]);
        let scope_b = brain_metadata::RowScope::from_bytes(2, [0xB2; 16]);
        let ent_a = EntityId::new();
        let ent_b = EntityId::new();
        let wtxn = db.write_txn().unwrap();
        for (scope, id, name) in [(scope_a, ent_a, "Alice"), (scope_b, ent_b, "Bob")] {
            brain_metadata::entity::ops::entity_put(
                &wtxn,
                scope,
                brain_core::SessionId::DEFAULT,
                &Entity::new_active(id, EntityType::PERSON_ID, name.into(), name.into(), 1),
            )
            .unwrap();
        }
        let pid = brain_metadata::schema::predicate::predicate_intern_or_get(
            &wtxn, "test", "plays", 0, 1,
        )
        .unwrap();
        let rtid = brain_metadata::relation::types::relation_type_intern_or_get(
            &wtxn, "test", "knows", 0, 1,
        )
        .unwrap();
        let mk_stmt = |subject: EntityId, obj: &str| {
            Statement::new_root(
                brain_core::StatementId::new(),
                StatementKind::Fact,
                brain_core::SubjectRef::Entity(subject),
                pid,
                StatementObject::Value(StatementValue::Text(obj.into())),
                0.9,
                EvidenceRef::default(),
                brain_core::ExtractorId::from(0),
                1,
                1,
            )
        };
        let s_a = mk_stmt(ent_a, "soccer");
        let s_b = mk_stmt(ent_b, "cricket");
        let (sid_a, sid_b) = (s_a.id, s_b.id);
        for (scope, s) in [(scope_a, &s_a), (scope_b, &s_b)] {
            brain_metadata::statement::crud::statement_create(
                &wtxn,
                scope,
                brain_core::SessionId::DEFAULT,
                s,
                1,
            )
            .unwrap();
        }
        let mk_rel = |from: EntityId| {
            Relation::new_root(
                brain_core::RelationId::new(),
                rtid,
                from,
                from, // self-edge keeps the fixture to one entity per scope
                0.9,
                Vec::new(),
                brain_core::ExtractorId::from(0),
                1,
                false,
            )
        };
        let r_a = mk_rel(ent_a);
        let r_b = mk_rel(ent_b);
        let (rid_a, rid_b) = (r_a.id, r_b.id);
        for (scope, r) in [(scope_a, &r_a), (scope_b, &r_b)] {
            brain_metadata::relation::ops::relation_create(
                &wtxn,
                scope,
                brain_core::SessionId::DEFAULT,
                r,
                1,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();
        (
            dir, db, scope_a, scope_b, ent_a, ent_b, sid_a, sid_b, rid_a, rid_b,
        )
    }

    #[test]
    fn entity_in_caller_scope_walls_foreign_and_missing() {
        let (_dir, db, scope_a, scope_b, ent_a, ent_b, ..) = scoped_trace_fixture();
        let rtxn = db.read_txn().unwrap();
        assert!(entity_in_caller_scope(&rtxn, ent_a, scope_a));
        assert!(!entity_in_caller_scope(&rtxn, ent_b, scope_a));
        assert!(entity_in_caller_scope(&rtxn, ent_b, scope_b));
        // A missing entity is out of scope — never renders a name.
        assert!(!entity_in_caller_scope(&rtxn, EntityId::new(), scope_a));
    }

    #[test]
    fn relation_in_caller_scope_walls_foreign_and_missing() {
        let (_dir, db, scope_a, scope_b, .., rid_a, rid_b) = scoped_trace_fixture();
        let rtxn = db.read_txn().unwrap();
        assert!(relation_in_caller_scope(&rtxn, rid_a, scope_a));
        assert!(!relation_in_caller_scope(&rtxn, rid_b, scope_a));
        assert!(relation_in_caller_scope(&rtxn, rid_b, scope_b));
        assert!(!relation_in_caller_scope(
            &rtxn,
            brain_core::RelationId::new(),
            scope_a
        ));
    }

    #[test]
    fn candidate_from_ranked_walls_foreign_typed_items() {
        let (_dir, db, scope_a, _scope_b, ent_a, ent_b, sid_a, sid_b, rid_a, rid_b) =
            scoped_trace_fixture();
        let rtxn = db.read_txn().unwrap();
        let empty_texts: HashMap<MemoryId, String> = HashMap::new();
        let render =
            |id: RankedItemId| candidate_from_ranked(Some(&rtxn), scope_a, &id, 0.5, &empty_texts);

        // Same-space typed items render their full label.
        let c_ea = render(RankedItemId::Entity(ent_a));
        assert_eq!(c_ea.text, "Alice", "same-space entity name must render");
        let c_sa = render(RankedItemId::Statement(sid_a));
        assert_eq!(
            c_sa.text, "Alice test:plays soccer",
            "same-space statement label must render"
        );
        let c_ra = render(RankedItemId::Relation(rid_a));
        assert_eq!(
            c_ra.text, "Alice —test:knows→ Alice",
            "same-space relation label must render"
        );

        // Foreign-space typed items render as opaque ids with NO content: the
        // id is preserved for observability, but B's name / label never leaks.
        let c_eb = render(RankedItemId::Entity(ent_b));
        assert_eq!(c_eb.item_id, u128::from_be_bytes(ent_b.to_bytes()));
        assert!(
            c_eb.text.is_empty(),
            "foreign entity name must be walled, got {:?}",
            c_eb.text
        );
        let c_sb = render(RankedItemId::Statement(sid_b));
        assert_eq!(c_sb.item_id, u128::from_be_bytes(sid_b.to_bytes()));
        assert!(
            c_sb.text.is_empty(),
            "foreign statement label must be walled, got {:?}",
            c_sb.text
        );
        // Specifically: none of B's subject / object appears anywhere.
        assert!(!c_sb.text.contains("Bob") && !c_sb.text.contains("cricket"));
        let c_rb = render(RankedItemId::Relation(rid_b));
        assert_eq!(c_rb.item_id, u128::from_be_bytes(rid_b.to_bytes()));
        assert!(
            c_rb.text.is_empty(),
            "foreign relation label must be walled, got {:?}",
            c_rb.text
        );
    }

    #[test]
    fn candidate_from_ranked_memory_renders_only_walled_map() {
        // The memory scope wall lives in `fetch_candidate_texts` (which only
        // populates the map for in-scope rows). At the render boundary a memory
        // whose id is absent from the map (foreign, missing, or tombstoned since
        // fusion) must therefore render with no text.
        let mid_in = MemoryId::pack(1, 7, 1);
        let mid_out = MemoryId::pack(1, 8, 1);
        let mut texts: HashMap<MemoryId, String> = HashMap::new();
        texts.insert(mid_in, "in-scope body".to_string());
        let scope =
            brain_metadata::RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xA1; 16]);

        let c_in = candidate_from_ranked(None, scope, &RankedItemId::Memory(mid_in), 0.5, &texts);
        assert_eq!(c_in.text, "in-scope body");
        let c_out = candidate_from_ranked(None, scope, &RankedItemId::Memory(mid_out), 0.5, &texts);
        assert_eq!(c_out.item_id, mid_out.raw());
        assert!(
            c_out.text.is_empty(),
            "a memory absent from the walled map renders no text"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fetch_candidate_texts_walls_foreign_space_memory() {
        use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
        use brain_index::{IndexParams, SharedHnsw};
        use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
        use brain_metadata::tables::text::TEXTS_TABLE;
        use brain_metadata::MetadataDb;
        use brain_planner::{ExecutorContext, WriterHandle};
        use std::sync::Arc;

        struct ZeroDispatcher;
        impl brain_embed::Dispatcher for ZeroDispatcher {
            fn embed(
                &self,
                _text: &str,
            ) -> Result<[f32; brain_embed::VECTOR_DIM], brain_embed::EmbedError> {
                Ok([0.0; brain_embed::VECTOR_DIM])
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<[f32; brain_embed::VECTOR_DIM]>, brain_embed::EmbedError> {
                Ok(texts
                    .iter()
                    .map(|_| [0.0; brain_embed::VECTOR_DIM])
                    .collect())
            }
            fn fingerprint(&self) -> [u8; 16] {
                [0xAB; 16]
            }
        }

        let tempdir = tempfile::tempdir().unwrap();
        let metadata: Arc<MetadataDb> =
            Arc::new(MetadataDb::open(tempdir.path().join("metadata.redb")).unwrap());
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer = Arc::new(crate::RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let executor = ExecutorContext::new(
            Arc::new(ZeroDispatcher) as Arc<dyn brain_embed::Dispatcher>,
            shared,
            metadata.clone(),
            writer as Arc<dyn WriterHandle>,
        );
        let ctx = crate::test_support::ops_context_for_tests(executor, tempdir.path());

        // Scope A is the caller; scope B is a foreign space in a different
        // namespace. Seed one memory + text row in each.
        let space_a = SpaceId::from([0xA1; 16]);
        let space_b = SpaceId::from([0xB2; 16]);
        let mid_a = MemoryId::pack(1, 7, 1);
        let mid_b = MemoryId::pack(1, 8, 1);
        let wtxn = metadata.write_txn().unwrap();
        {
            let mut mt = wtxn.open_table(MEMORIES_TABLE).unwrap();
            let mut tt = wtxn.open_table(TEXTS_TABLE).unwrap();
            for (ns, space, mid, body) in [
                (NamespaceId::SYSTEM, space_a, mid_a, "alice private note"),
                (NamespaceId::from(2u32), space_b, mid_b, "bob private note"),
            ] {
                let m = MemoryMetadata::new_active(
                    mid,
                    ns,
                    space,
                    SessionId(0),
                    mid.slot(),
                    1,
                    MemoryKind::Episodic,
                    [0xAB; 16],
                    0.5,
                    body.len() as u32,
                    1_700_000_000_000_000_000,
                );
                mt.insert(&mid.to_be_bytes(), &m).unwrap();
                tt.insert(&mid.to_be_bytes(), body.as_bytes()).unwrap();
            }
        }
        wtxn.commit().unwrap();

        let scope_a = brain_metadata::RowScope::new(NamespaceId::SYSTEM, space_a);
        let ids: HashSet<MemoryId> = [mid_a, mid_b].into_iter().collect();
        let texts =
            fetch_candidate_texts(&ids, scope_a, brain_metadata::ScopeMode::Space, &ctx).unwrap();

        assert_eq!(
            texts.get(&mid_a).map(String::as_str),
            Some("alice private note"),
            "same-space memory text must be present in the trace"
        );
        assert!(
            !texts.contains_key(&mid_b),
            "foreign-space memory must be walled out of the trace text map"
        );
        // Belt-and-suspenders: B's body never appears in any value.
        assert!(texts.values().all(|t| !t.contains("bob")));
    }

    // ── R4: in-txn read-your-writes confidence semantics ────────────────────

    #[test]
    fn pending_confidence_gate_matches_committed_salience_semantics() {
        // Committed path gates memory hits by `salience >= confidence_min`
        // (brain-planner `filter_confidence`). The overlay must agree: filter
        // pending hits by salience, never cosine.
        //
        // High salience, LOW cosine → committed KEEPS it (salience-based), so the
        // overlay must keep it too. A cosine floor would have wrongly dropped it.
        assert!(pending_clears_confidence(0.9, 0.5));
        // Low salience, (any) cosine → committed DROPS it; overlay must drop it.
        assert!(!pending_clears_confidence(0.3, 0.5));
        // Exactly at the floor is kept (>=), matching committed.
        assert!(pending_clears_confidence(0.5, 0.5));
        // Zero threshold admits everything (committed leaves confidence_min unset).
        assert!(pending_clears_confidence(0.0, 0.0));
    }
}
