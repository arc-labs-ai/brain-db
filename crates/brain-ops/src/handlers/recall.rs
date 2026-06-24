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

use brain_core::{ContextId, EntityId, MemoryId};
use brain_index::RankedItemId;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_planner::retrieval::executor::{
    execute as retrieval_execute, ExecutionError, QueryResult, RetrievalExecutorContext,
};
use brain_planner::retrieval::planner::{plan as retrieval_plan, PlanError};
use brain_planner::retrieval::router::{
    QueryRequest as PlannerQueryRequest, Retriever, RetrieverSelection,
};
use brain_protocol::envelope::request::{MemoryKindWire, RecallRequest};
use brain_protocol::envelope::response::{AnswerKindWire, MemoryResult, RecallResponseFrame};
use brain_protocol::RetrieverNameWire;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::grounded::{grounded_answer_walk, AnswerKind, GroundedAnswer};
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

/// Upper bound on the entry count of any one recall filter list
/// (`agent_filter`, `context_filter`, `kind_filter`). These are only
/// bounded by the 16 MiB payload cap otherwise; an explicit cap turns a
/// crafted oversized filter into a clear `InvalidRequest` instead of
/// silently building a large `HashSet` for the post-filter pass. The
/// bound is generous — far above any legitimate scoping need.
pub const MAX_RECALL_FILTER_ENTRIES: usize = 1024;

/// Upper bound on the number of subject candidates resolved from a single
/// cue. Each candidate costs a few redb point lookups plus a grounded
/// scan; capping bounds the per-call work so a cue packed with proper-noun
/// surfaces can't fan out unboundedly. Generous — real cues name a handful
/// of subjects at most.
pub const MAX_SUBJECT_CANDIDATES: usize = 8;

pub async fn handle_recall(
    mut req: RecallRequest,
    ctx: &OpsContext,
) -> Result<RecallResponseFrame, OpError> {
    // Did the caller ask for a specific result count? `0` means "no count, use
    // the server default"; any non-zero value is an explicit caller cap. We
    // capture this BEFORE normalising `max_results` below, because the keyed
    // (exact-anchor) path must not clip the intrinsic belonging set to the fuzzy
    // default window when the caller never asked for a count — and the
    // normalisation overwrites `0` with the default, erasing the distinction.
    let client_requested_count = req.max_results != 0;

    // Normalise the safety cap. `0` means "server default"; anything
    // above the hard ceiling is clamped (not rejected) — the cap is a
    // bound, never the caller's intent. The answer's shape comes from
    // the data, so there is no "zero results" request to honour here.
    if req.max_results == 0 {
        req.max_results = DEFAULT_RECALL_RESULTS;
    }
    if req.max_results > MAX_RECALL_RESULTS {
        req.max_results = MAX_RECALL_RESULTS;
    }
    // A memory without its text is useless to the caller — recall always
    // returns the remembered text. `include_text` is not a knob anyone
    // wants set to false; force it on regardless of what the client sent.
    // (The wire field is retained for now; a later lockstep pass drops it.)
    req.include_text = true;
    if req.agent_filter.len() > MAX_RECALL_FILTER_ENTRIES {
        return Err(OpError::InvalidRequest(format!(
            "recall: agent_filter must have <= {MAX_RECALL_FILTER_ENTRIES} entries"
        )));
    }
    if let Some(ref ctxs) = req.context_filter {
        if ctxs.len() > MAX_RECALL_FILTER_ENTRIES {
            return Err(OpError::InvalidRequest(format!(
                "recall: context_filter must have <= {MAX_RECALL_FILTER_ENTRIES} entries"
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

    // Embed the cue once for the grounding overlay. A failed embed degrades to
    // the plain fan-out — grounding is an overlay, never a reason to fail the
    // read.
    let cue_vec = ctx.executor.embedder.embed(&req.cue_text).ok();

    // Statement lanes are always searched inside `retrieve_memories` (cue-driven,
    // never gated). The entity-graph traversal lane is ALSO always lit: we anchor
    // it on the cue's resolved subject and `retrieve_memories` cue-conditions the
    // graph candidates (scaling each by its cosine to the query), so the
    // structural walk can't re-introduce the subject-dump flood. No flags: every
    // read traverses everything the write built, ranked by relevance to the cue.
    let anchor = resolve_graph_anchor(&req, ctx);
    let memories = retrieve_memories(&req, ctx, anchor, cue_vec.as_ref()).await?;

    let Some(cue_vec) = cue_vec else {
        return Ok(recall_frame(memories));
    };

    let grounded = best_grounded_for_cue(&req, ctx, &cue_vec)?;

    // MEMBERSHIP MODEL — recall is not a top-k pile, it is the SET of memories
    // that belong to this cue. Two signals decide belonging and are UNIONed:
    //   S_struct — the precise typed-graph answer: source memories of the
    //              grounded values (Single → one, Set → its members).
    //   S_sem    — the associative belonging set: the fan-out cut at its natural
    //              score cliff (adaptive gap), never a fixed count.
    // A memory in BOTH is the most-confirmed (both lanes agree) and ranks first.
    // The answer's SHAPE follows the set's cardinality: 0 → None, 1 → Single,
    // N → Many. There is no caller-supplied count anywhere in this path.
    let membership = build_membership(
        memories,
        &grounded,
        &req,
        ctx,
        &cue_vec,
        anchor,
        client_requested_count,
    );
    // Structural abstention (write-time anchors): if the cue resolves to no
    // subject entity present in the store, the grounded layer found nothing, and
    // no surviving member is confirmed by a non-semantic lane, the cue has no
    // anchor here — return None rather than topical semantic noise.
    //
    // In-txn reads are exempt: they are read-your-writes, and a pending write
    // the caller just made in THIS transaction is not topical noise — it carries
    // no retrieval-lane confirmation only because it isn't committed/indexed yet.
    // Abstaining it away would silently break the read-your-writes guarantee.
    let membership = if req.txn_id.is_some() {
        membership
    } else {
        apply_anchor_abstention(membership, anchor, &grounded)
    };

    Ok(recall_frame(membership))
}

/// Honest abstention by structural anchor — unconditional, no flag/knob. Drops to
/// an empty (None) answer only when ALL hold: no resolved subject entity, no
/// grounded answer, and no surviving member confirmed by a non-semantic
/// (lexical/graph) lane. Substrate-safe by construction: a stored subject's name
/// appears lexically in its own memory, so the lexical lane confirms it and this
/// never fires; it triggers only when the cue's subject is nowhere in the store.
fn apply_anchor_abstention(
    members: Vec<MemoryResult>,
    anchor: Option<EntityId>,
    grounded: &GroundedOutcome,
) -> Vec<MemoryResult> {
    if anchor.is_some() || matches!(grounded, GroundedOutcome::Answer(_)) {
        return members;
    }
    let structurally_confirmed = members.iter().any(|m| {
        m.contributing_retrievers.len() >= 2
            || m.contributing_retrievers
                .iter()
                .any(|r| !matches!(r, RetrieverNameWire::Semantic))
    });
    if structurally_confirmed {
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

/// Build the membership set for a cue: `S_struct ∪ S_sem`, ranked by
/// confirmation strength (in both lanes first, then structured-only, then
/// associative-only), deduped by memory id. The associative side (`S_sem`) is
/// the fan-out cut at its adaptive score cliff — belonging is decided by the
/// score distribution, not a fixed count. Structured sources the fan-out missed
/// are hydrated so the typed graph adds recall the vector lane couldn't reach.
fn build_membership(
    ranked: Vec<MemoryResult>,
    grounded: &GroundedOutcome,
    req: &RecallRequest,
    ctx: &OpsContext,
    cue_vec: &[f32; brain_embed::VECTOR_DIM],
    anchor: Option<EntityId>,
    client_requested_count: bool,
) -> Vec<MemoryResult> {
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

    // The highest-belonging member (used below so the lane-consensus collapse only
    // fires when the consensus and the score agree — never collapses the answer
    // away to a lexical term-matcher that merely hit more lanes).
    let top_member_id: Option<u128> = scored.first().map(|(m, _)| m.memory_id);

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
    if let GroundedOutcome::Answer(answer) = grounded {
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

    // Cross-lane consensus collapse (model-free; see `consensus_collapse`).
    let mut out = consensus_collapse(out, top_member_id);

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
    // agent/kind/context/salience/age/tombstone filters as every other path, so
    // this never leaks a memory the caller couldn't otherwise see.
    //
    // Strictly additive and bounded: only ids NOT already placed are appended,
    // and the pull is capped at the per-read result scale before the safety
    // ceiling below trims the whole set. It can only ADD recall, never reorder
    // or drop an existing member.
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
    let keyed = anchor.is_some() || matches!(grounded, GroundedOutcome::Answer(_));
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
        "recall: membership set"
    );

    out
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
    if let Ok(stmts) = statement_list(
        &rtxn,
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

/// Build the response frame from the router's chosen memories, deriving the
/// answer cardinality from the count: none / one / many.
fn recall_frame(memories: Vec<MemoryResult>) -> RecallResponseFrame {
    let answer_kind = match memories.len() {
        0 => AnswerKindWire::None,
        1 => AnswerKindWire::Single,
        _ => AnswerKindWire::Many,
    };
    let cumulative_count = u32::try_from(memories.len()).unwrap_or(u32::MAX);
    RecallResponseFrame {
        answer_kind,
        memories,
        is_final: true,
        cumulative_count,
        estimated_remaining: None,
    }
}

/// Result of the grounded attempt: either a confident precise answer, or
/// none — in which case the unified read path degrades to episodic.
enum GroundedOutcome {
    /// A confident precise answer (`Single`/`Set`).
    Answer(GroundedAnswer),
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
/// first candidate that clears the floor. First-match-wins was a bug: the agent
/// self-entity is always candidate[0], and a loose self-predicate (e.g. the
/// agent's `usually_reviews` against "who does Niraj report to") could clear the
/// floor and short-circuit before the actually-named subject's exact predicate
/// (`reports_to`, a far higher cosine) was ever tried. Comparing all candidates
/// by cosine lets the strong, specific match win. On a near-tie we prefer a
/// named (non-self) subject, since a cue that names someone is asking about
/// them, not the writer.
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

    let candidates = subject_candidates_from_cue(&rtxn, req, ctx)?;
    if candidates.is_empty() {
        return Ok(GroundedOutcome::NoAnswer);
    }
    let self_id = EntityId::from(ctx.executor.caller_agent.0.into_bytes());

    // Score is the matched predicate's cosine (shared by all values of an
    // answer). Near-tie band within which we prefer a named subject over self.
    const TIE_EPS: f32 = 0.02;
    let mut best: Option<(GroundedAnswer, f32, bool)> = None; // (answer, score, is_self)
    for subject in candidates {
        // Multi-hop: a bounded depth-discounted beam walk from this candidate
        // over the typed graph, running the 1-hop matcher at every reachable node
        // (nearest answer wins unless a deeper one out-scores the per-hop
        // discount). Reduces to the 1-hop answer when no edge embeds close to the
        // cue, so single-hop questions are unaffected — no read LLM.
        let answer = grounded_answer_walk(&rtxn, subject, cue_vec)
            .map_err(|e| OpError::Internal(format!("recall grounded walk: {e}")))?;
        if matches!(answer.kind, AnswerKind::None) {
            continue;
        }
        let score = answer.values.first().map(|v| v.match_score).unwrap_or(0.0);
        let is_self = subject == self_id;
        let take = match &best {
            None => true,
            Some((_, best_score, best_is_self)) => {
                score > *best_score + TIE_EPS
                    || ((score - *best_score).abs() <= TIE_EPS && *best_is_self && !is_self)
            }
        };
        if take {
            best = Some((answer, score, is_self));
        }
    }
    match best {
        Some((answer, _, _)) => Ok(GroundedOutcome::Answer(answer)),
        None => Ok(GroundedOutcome::NoAnswer),
    }
}

/// Hydrate `MemoryResult`s straight from `MEMORIES_TABLE` (+ `TEXTS_TABLE`
/// when `include_text`) for a set of memory ids — the structured branch's
/// projector, which answers from stored ids rather than a retrieval result.
/// Applies the same post-filters as the fan-out projector (agent scope, kind,
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

    // Agent scope, same precedence as `build_planner_request`: explicit
    // filter → that set; include_other_agents → no restriction; else the
    // caller alone.
    let agent_scope: Option<HashSet<[u8; 16]>> = if !req.agent_filter.is_empty() {
        Some(req.agent_filter.iter().copied().collect())
    } else if req.include_other_agents {
        None
    } else {
        Some(
            [<[u8; 16]>::from(ctx.executor.caller_agent)]
                .into_iter()
                .collect(),
        )
    };
    let kind_filter: Option<HashSet<MemoryKindWire>> = req
        .kind_filter
        .as_ref()
        .map(|v| v.iter().copied().collect());
    let context_filter: Option<HashSet<u64>> = req
        .context_filter
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
        if let Some(ref scope) = agent_scope {
            if !scope.contains(&row.agent_id_bytes) {
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
            agent_id: row.agent_id_bytes,
            context_id: ContextId(row.context_id).into(),
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
///   1. The caller's agent self-entity — covers every first-person
///      "what are my X" query with zero pronoun parsing.
///   2. An explicit `subject_name`, when the client did pass one.
///   3. Surfaces mined from the cue: capitalized multi-word runs (Latin
///      proper nouns) and individual whitespace tokens of length ≥ 2
///      (catches CJK single-token names and lowercase entity names).
///
/// Deduped and capped at `MAX_SUBJECT_CANDIDATES` to bound the per-call
/// grounded work (each candidate is a few redb point lookups).
/// The cue's subject entity, used as the always-on graph-lane anchor. We take
/// the strongest NON-self candidate — the agent self-entity is too broad to
/// anchor a walk (everything the agent ever said connects to it). `None` when
/// nothing resolves, in which case the graph lane simply has no seed.
fn resolve_graph_anchor(req: &RecallRequest, ctx: &OpsContext) -> Option<EntityId> {
    let rtxn = ctx.executor.metadata.read_txn().ok()?;
    let self_id = EntityId::from(ctx.executor.caller_agent.0.into_bytes());
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

    // 1. Agent self-entity (same derivation as MATERIALIZE_PROCEDURAL).
    push(
        EntityId::from(ctx.executor.caller_agent.0.into_bytes()),
        &mut out,
        &mut seen,
    );

    // The surfaces to resolve against the canonical-name index.
    let mut surfaces: Vec<String> = Vec::new();
    let subject = req.subject_name.trim();
    if subject.is_empty() {
        // Mine surfaces from the cue when the client gave no subject.
        surfaces.extend(capitalized_runs(&req.cue_text));
        surfaces.extend(
            req.cue_text
                .split_whitespace()
                .filter(|t| t.chars().count() >= 2)
                .map(str::to_string),
        );
    } else {
        // 2. Explicit subject still works.
        surfaces.push(subject.to_string());
    }

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
        let ids = brain_metadata::entity_resolve_scored(rtxn, &surface, 5)
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
    for raw in cue.split_whitespace() {
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
) -> Result<Vec<MemoryResult>, OpError> {
    let planner_req = build_planner_request(req, ctx.executor.caller_agent, entity_anchor);

    let plan = retrieval_plan(&planner_req).map_err(map_plan_error)?;
    let exec_ctx = RetrievalExecutorContext {
        semantic: ctx.semantic_retriever.clone(),
        lexical: ctx.lexical_retriever.clone(),
        graph: ctx.graph_retriever.clone(),
        metadata: ctx.executor.metadata.clone(),
        cross_encoder: ctx.cross_encoder.as_arc().cloned(),
    };
    // The statement corpus (statement HNSW + statements.tantivy) is ALWAYS
    // searched — it is a cue-driven lane like memory semantic/lexical (it
    // matches statement text against the query), so there is no reason to
    // ever gate it off. RECALL stays memory-centric: a statement hit surfaces
    // its SOURCE memory (the projector maps `Statement` items back through
    // evidence). The flooding risk was only ever the subject-anchored graph
    // walk, never these cue-conditioned statement lanes.
    let mut result = retrieval_execute(&plan, &planner_req, true, &exec_ctx)
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

    Ok(memory_results)
}

/// Env gate for autocut (`BRAIN_AUTOCUT`). Default OFF.
fn autocut_enabled() -> bool {
    matches!(
        std::env::var("BRAIN_AUTOCUT").ok().as_deref(),
        Some("1" | "true" | "TRUE" | "on" | "ON")
    )
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
        merged.truncate(req.max_results as usize);
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
    merged.truncate(req.max_results as usize);
    Ok(merged)
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
        agent_id: p.agent_id.into(),
        context_id: p.context_id.into(),
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
fn fetch_enrichment_for(
    memory_ids: &[MemoryId],
    rtxn: &redb::ReadTransaction,
) -> Result<Vec<brain_protocol::envelope::response::GraphEnrichment>, OpError> {
    use brain_core::{EdgeKindRef, NodeRef};
    use brain_core::{EntityId, StatementId, SubjectRef};
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
    let evidence_table = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).map_err(|e| {
        OpError::Internal(format!(
            "include_graph: open STATEMENTS_BY_EVIDENCE_TABLE: {e}"
        ))
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
            let lo = (mid, [0u8; 16]);
            let hi = (mid, [0xFFu8; 16]);
            let mut stmts: Vec<brain_core::Statement> = Vec::new();
            for entry in evidence_table
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
    caller_agent: brain_core::AgentId,
    entity_anchor: Option<EntityId>,
) -> PlannerQueryRequest {
    // Agent-scope resolution. Recall isolates to the calling agent by
    // default so one tenant never sees another's memories without
    // asking. Three cases, in precedence:
    //   1. explicit `agent_filter` → scope to exactly that set.
    //   2. `include_other_agents` → no agent filter (across-agents).
    //   3. neither → implicit `[caller_agent]` isolation.
    let agent_filter: Vec<brain_core::AgentId> = if !req.agent_filter.is_empty() {
        req.agent_filter
            .iter()
            .map(|bytes| brain_core::AgentId::from(*bytes))
            .collect()
    } else if req.include_other_agents {
        Vec::new()
    } else {
        vec![caller_agent]
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
        context_filter: req.context_filter.as_ref().cloned().unwrap_or_default(),
        agent_filter,
        confidence_min: if req.confidence_threshold > 0.0 {
            Some(req.confidence_threshold)
        } else {
            None
        },
        include_tombstoned: false,
        include_superseded: false,
        as_of_record_time_unix_nanos: req.as_of_record_time_unix_nanos,
        limit: req.max_results,
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
        let enriched = fetch_enrichment_for(&ids, &rtxn)?;
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
            agent_id: row.agent_id_bytes,
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

        if out.len() == req.max_results as usize {
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

    /// Minimal `RecallRequest` for ceiling-logic tests. Only `max_results`
    /// matters here; everything else is a benign zero/empty value.
    fn req_with_max(max_results: u32) -> RecallRequest {
        RecallRequest {
            cue_text: String::new(),
            subject_name: String::new(),
            max_results,
            confidence_threshold: 0.0,
            context_filter: None,
            age_bound_unix_nanos: None,
            as_of_record_time_unix_nanos: None,
            kind_filter: None,
            salience_floor: 0.0,
            include_edges: false,
            include_graph: false,
            include_text: true,
            request_id: None,
            txn_id: None,
            agent_filter: Vec::new(),
            include_other_agents: false,
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
            agent_id: [0u8; 16],
            context_id: 0,
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
    fn abstention_keeps_set_when_anchor_present() {
        let members = vec![mr(1, &[Semantic])];
        let kept = apply_anchor_abstention(
            members,
            Some(brain_core::EntityId::new()),
            &GroundedOutcome::NoAnswer,
        );
        assert_eq!(kept.len(), 1, "a resolved anchor suppresses abstention");
    }

    #[test]
    fn abstention_drops_unanchored_semantic_only_noise() {
        // No anchor, no grounded answer, and every member is semantic-only (no
        // cross-lane confirmation) → topical noise → abstain (empty).
        let members = vec![mr(1, &[Semantic]), mr(2, &[Semantic])];
        let kept = apply_anchor_abstention(members, None, &GroundedOutcome::NoAnswer);
        assert!(kept.is_empty(), "unanchored semantic-only set must abstain");
    }

    #[test]
    fn abstention_keeps_lane_confirmed_member_without_anchor() {
        // No anchor, but one member is confirmed by a non-semantic lane (lexical):
        // a real keyword/paraphrase hit, not topical noise → keep the set.
        let members = vec![mr(1, &[Semantic, Lexical]), mr(2, &[Semantic])];
        let kept = apply_anchor_abstention(members, None, &GroundedOutcome::NoAnswer);
        assert_eq!(kept.len(), 2, "a lane-confirmed hit suppresses abstention");
    }
}
