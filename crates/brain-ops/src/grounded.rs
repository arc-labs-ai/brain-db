//! Grounded answer engine — the precise overlay on the memory-layer read.
//!
//! "Give me the memory for this question" → the caller resolves the subject
//! entity, then this engine matches the question's *relation* against the
//! subject's stored predicates and returns the stored value(s) shaped by the
//! actual rows, or **nothing**.
//!
//! This is a precise overlay over the combined vector+lexical read — never the
//! primary answer, and it **boosts, never replaces**: a grounded hit moves its
//! source memory to the front of the fused results (the rest still come back
//! beneath it), so a mis-match costs ordering, never the real answer.
//!
//! Matching is **purely semantic**: the cue embedding's cosine against each
//! candidate predicate / relation-type embedding (`works_at` is embedded
//! write-time as the phrase "works at"), gated by [`GROUNDED_MATCH_FLOOR`].
//! There is no string tokenization, stop-word list, or stemmer — those are
//! brittle, English-only static-text heuristics Brain deliberately avoids; the
//! embedder is the single source of relation similarity. A subject with no
//! predicate clearing the floor yields `AnswerKind::None`, and the boost is a
//! no-op — the episodic read stands on its own.

use std::collections::HashMap;

use brain_core::{
    EntityId, EvidenceRef, MemoryId, PredicateId, RelationTypeId, Statement, StatementObject,
    StatementValue,
};
use brain_metadata::{
    entity_get, predicate_embedding_get, predicate_get, relation_list_from, relation_list_to,
    relation_type_embedding_get, relation_type_get, statement_list, RelationListFilter,
    StatementListFilter,
};
use redb::ReadTransaction;

/// The shape of a grounded answer, decided by the matching kind's
/// cardinality — not by a caller-supplied count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnswerKind {
    /// No stored memory matched the relation above the threshold.
    None,
    /// A single-valued kind (Attribute / Directive / custom `single`): the
    /// one current value.
    Single,
    /// A set-valued kind (Relation / Preference / Event / Fact): all current
    /// members.
    Set,
}

/// One stored value backing a grounded answer, with provenance.
#[derive(Clone, Debug)]
pub struct GroundedValue {
    /// Canonical predicate qname the relation matched (`namespace:name`).
    pub predicate: String,
    pub object: StatementObject,
    pub confidence: f32,
    /// First evidence memory, when present.
    pub source_memory: Option<MemoryId>,
    /// The relation match score: the cosine between the cue embedding and the
    /// matched predicate / relation-type embedding (>= [`GROUNDED_MATCH_FLOOR`]).
    pub match_score: f32,
    /// When this fact was asserted, in unix nanos: the event time if known,
    /// else the record (extraction) time. Competing current values are ranked
    /// most-recent-first so a question about the present surfaces the latest
    /// assertion ("works at OpenAI" over an older "works at Google") without
    /// any keyword detection — recency is the tiebreaker the stored rows carry.
    pub recency: u64,
}

/// The grounded answer: a shape plus zero-or-more stored values.
#[derive(Clone, Debug)]
pub struct GroundedAnswer {
    pub kind: AnswerKind,
    pub values: Vec<GroundedValue>,
}

impl GroundedAnswer {
    #[must_use]
    pub fn none() -> Self {
        Self {
            kind: AnswerKind::None,
            values: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_none(&self) -> bool {
        matches!(self.kind, AnswerKind::None)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum GroundedError {
    #[error("metadata: {0}")]
    Metadata(String),
}

/// Minimum cosine between the cue embedding and a stored predicate /
/// relation-type embedding for the precise grounded overlay to fire.
///
/// Matching is purely semantic — the question's relation intent against the
/// predicate's own embedding ("works_at" is embedded write-time as the phrase
/// "works at", which sits close to "where does X work now"). There is no
/// string tokenization, stop-word list, or stemmer: those are brittle,
/// English-only, and exactly the static-text heuristics Brain avoids. The
/// embedder is the single source of relation similarity, the same model that
/// drives every other retrieval lane.
///
/// The floor is eval-calibrated. It is deliberately low-stakes: the overlay
/// only BOOSTS the matched memory within the combined vector+lexical results
/// (it never replaces them), so too low a floor merely re-orders and too high
/// a floor merely misses a boost — neither erases the episodic answer.
const GROUNDED_MATCH_FLOOR: f32 = 0.5;

/// Cosine similarity between two equal-length vectors. Predicate / relation
/// embeddings are stored L2-normalized and the cue vector arrives normalized,
/// so this is effectively a dot product — but we divide by the norms
/// defensively. A zero-norm or length-mismatched vector yields `0.0` rather
/// than `NaN`, so it simply fails the floor instead of poisoning the ranking.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Compare two stored objects for value equality. Two single-valued rows
/// that assert the same object are agreement / duplicates, not a conflict;
/// distinct objects under a single-valued predicate ARE a contradiction.
/// `StatementObject` derives `PartialEq`, so this is a direct comparison —
/// named for intent at the call site.
fn same_object(a: &StatementObject, b: &StatementObject) -> bool {
    a == b
}

/// Shape a set of matched, blank-filtered, confidence-sorted candidate
/// values into a `GroundedAnswer`, following the actual stored rows rather
/// than blindly trusting the kind's cardinality.
///
/// The shaping rules (apply to BOTH the statement and relation paths):
/// - 0 rows: `None`.
/// - exactly 1 row: `Single`.
/// - more than 1 row that all share the same object: collapse to `Single`
///   (the rows agree / are duplicates; the cardinality is honored).
/// - more than 1 row with differing objects: a `Set` of all of them,
///   confidence-descending. For a single-valued kind this is deliberate: a
///   normally-single predicate that holds two disagreeing current rows is a
///   contradiction, and Brain surfaces contradictions rather than silently
///   resolving them. There is no wire change. A multi-value `Set` returned
///   for a normally-single predicate IS the surfaced conflict; the caller
///   reads the extra members as the competing claims, ranked most-confident
///   first.
///
/// `values` must already be sorted confidence-descending and free of blank
/// objects. The matched kind's declared cardinality no longer drives the
/// shape — the stored rows do — so it is intentionally not a parameter:
/// even a single-valued kind yields a `Set` when two current rows disagree,
/// which is how a contradiction is surfaced.
fn shape_answer(mut values: Vec<GroundedValue>) -> Option<GroundedAnswer> {
    match values.len() {
        0 => None,
        1 => Some(GroundedAnswer {
            kind: AnswerKind::Single,
            values,
        }),
        _ => {
            let all_agree = values
                .iter()
                .all(|v| same_object(&v.object, &values[0].object));
            if all_agree {
                // Duplicates / agreement: collapse to the most-confident one.
                values.truncate(1);
                Some(GroundedAnswer {
                    kind: AnswerKind::Single,
                    values,
                })
            } else {
                // Differing objects. For a cumulative kind this is the natural
                // member list; for a single-valued kind it is a surfaced
                // contradiction. Either way: a confidence-ranked Set.
                Some(GroundedAnswer {
                    kind: AnswerKind::Set,
                    values,
                })
            }
        }
    }
}

fn first_evidence_memory(ev: &EvidenceRef) -> Option<MemoryId> {
    match ev {
        EvidenceRef::Inline(v) => v.first().map(|e| e.memory_id),
        EvidenceRef::Overflow(_) => None,
    }
}

/// Whether an object carries real content. A blank/whitespace text value
/// (or empty blob) is not a memory — returning it would let an empty stored
/// object masquerade as an answer. Entities, numbers, bools, timestamps,
/// memory/statement refs are always meaningful.
fn is_meaningful_object(o: &StatementObject) -> bool {
    match o {
        StatementObject::Value(brain_core::StatementValue::Text(t)) => !t.trim().is_empty(),
        StatementObject::Value(brain_core::StatementValue::Blob(b)) => !b.is_empty(),
        _ => true,
    }
}

/// Answer a grounded relation question for one resolved subject.
///
/// Returns the matching kind-shaped value(s), or `AnswerKind::None` when no
/// stored predicate / relation type embeds close enough to the cue. Matching
/// is semantic (cosine of `cue_vec` vs the predicate's stored embedding) —
/// no string matching, no threshold-free exactness.
pub fn grounded_answer(
    rtxn: &ReadTransaction,
    subject: EntityId,
    cue_vec: &[f32; brain_embed::VECTOR_DIM],
) -> Result<GroundedAnswer, GroundedError> {
    // The subject's facts live in two stores: attribute/value facts in the
    // statements table (predicate-keyed) and entity↔entity links in the
    // relations table (relation-type-keyed). Match BOTH and return the higher
    // cosine — never statement-first. Statement-first was a bug: a weak
    // statement match (e.g. Niraj's `co_authored`@0.56 against "who does Niraj
    // report to") would preempt a far stronger relation match (`reports_to`
    // ~0.85, which lives in the relations table because Niraj→Meera is an
    // entity link). Comparing by score lets the relation win. On a tie, prefer
    // the statement (an attribute is a more specific answer than a generic edge).
    let stmt = best_statement_answer(rtxn, subject, cue_vec)?;
    let rel = best_relation_answer(rtxn, subject, cue_vec)?;
    let score = |a: &GroundedAnswer| a.values.first().map(|v| v.match_score).unwrap_or(0.0);
    let answer = match (stmt, rel) {
        (Some(s), Some(r)) => {
            if score(&r) > score(&s) {
                r
            } else {
                s
            }
        }
        (Some(s), None) => s,
        (None, Some(r)) => r,
        (None, None) => GroundedAnswer::none(),
    };
    Ok(answer)
}

/// Default depth of the multi-hop grounded walk. 3 covers the chains the
/// typed graph realistically encodes — "X's manager's former employer's city"
/// is already 3 edges from X — while keeping the bounded fan-out cheap. The
/// walk reduces to the 1-hop [`grounded_answer`] when no edge from the anchor
/// embeds close to the cue, so a deeper bound never hurts a single-hop query.
const GROUNDED_WALK_MAX_HOPS: usize = 3;

/// Per-node branching factor of the walk: at each entity we follow only the
/// `GROUNDED_WALK_BEAM` relation edges whose *type* embeds closest to the cue,
/// not every edge. This is what keeps the walk from dumping a hub's whole
/// neighborhood — a question selects the relations it's about (cosine of the
/// cue against each relation-type embedding), and only those are expanded.
const GROUNDED_WALK_BEAM: usize = 4;

/// Per-hop discount applied to a node's match score when selecting the walk
/// winner. A fact reached in fewer hops is a better answer to a bare cue than
/// an equally-strong fact several relations away: "Where does Niraj work?" must
/// return Niraj's OWN employer (1 edge) — not a relative's employer reached by
/// chaining family edges, even though both match the `works_at` relation
/// equally. The discount makes the nearest match win UNLESS a deeper fact scores
/// high enough on its own to overcome it — which is exactly what a genuinely
/// multi-hop cue ("X's sister's occupation") produces, since the intermediate
/// relation words keep the deep node's cosine high. Mild (0.9) so a strong deep
/// answer still beats a weak shallow one.
const GROUNDED_WALK_DEPTH_DISCOUNT: f32 = 0.9;

/// Multi-hop grounded answer: a bounded beam walk over the typed graph from
/// `anchor`, running the 1-hop [`grounded_answer`] at every reachable node and
/// returning the single best-scoring answer found.
///
/// This is the read-side mechanism for multi-hop questions ("Where did Niraj's
/// manager work before?", "What does Niraj's sister do?"). It needs no LLM and
/// no read-time generation: at each hop it scores every incident relation edge
/// by the cosine of the cue against the edge's relation-type embedding, expands
/// the strongest [`GROUNDED_WALK_BEAM`] neighbors, and at each visited node asks
/// the same precise 1-hop matcher whether that node answers the cue. The walk
/// assembles the chain from whatever edges exist at read time — so it follows
/// reports_to → worked_before, or family_of → married_to → occupation, purely
/// from edge/predicate similarity to the cue. Depth is capped at
/// [`GROUNDED_WALK_MAX_HOPS`] and visited nodes are deduped, so the work is
/// bounded by `beam^hops` regardless of graph size.
///
/// The winner is the highest DEPTH-DISCOUNTED match score across all visited
/// nodes (see `GROUNDED_WALK_DEPTH_DISCOUNT`): a deep fact must out-score the
/// per-hop discount to beat a nearer one, so "…work before?" still follows
/// reports_to → prior-employer (the deep predicate scores high), while a bare
/// "where does X work?" keeps X's own 1-hop employer instead of chaining into a
/// relative's. Returns `AnswerKind::None` when nothing on the walk clears the
/// floor — the boost is then a no-op and the episodic read stands alone.
pub fn grounded_answer_walk(
    rtxn: &ReadTransaction,
    anchor: EntityId,
    cue_vec: &[f32; brain_embed::VECTOR_DIM],
) -> Result<GroundedAnswer, GroundedError> {
    use std::collections::{HashMap, HashSet};

    let mut visited: HashSet<EntityId> = HashSet::new();
    visited.insert(anchor);
    let mut frontier: Vec<EntityId> = vec![anchor];
    // Every node whose grounded answer clears the floor, with the hop distance
    // at which we reached it. Depth drives the nearest-wins discount and the
    // path-vs-terminal test below.
    let mut answers: HashMap<EntityId, (GroundedAnswer, usize)> = HashMap::new();
    let anchor_ans = grounded_answer(rtxn, anchor, cue_vec)?;
    if !matches!(anchor_ans.kind, AnswerKind::None) {
        answers.insert(anchor, (anchor_ans, 0));
    }

    for hop in 0..GROUNDED_WALK_MAX_HOPS {
        let mut next: Vec<EntityId> = Vec::new();
        for &node in &frontier {
            // Score every incident edge (both directions) by cue↔relation-type
            // cosine; the surfaced neighbor is always the OTHER endpoint.
            let filter = RelationListFilter {
                relation_type: None,
                current_only: true,
                limit: 0,
            };
            let mut scored: Vec<(EntityId, f32)> = Vec::new();
            let outgoing = relation_list_from(rtxn, node, &filter)
                .map_err(|e| GroundedError::Metadata(format!("{e}")))?;
            let incoming = relation_list_to(rtxn, node, &filter)
                .map_err(|e| GroundedError::Metadata(format!("{e}")))?;
            for r in outgoing.iter().chain(incoming.iter()) {
                let other = if r.from_entity == node {
                    r.to_entity
                } else {
                    r.from_entity
                };
                if visited.contains(&other) {
                    continue;
                }
                let edge_score = relation_type_embedding_get(rtxn, r.relation_type)
                    .map_err(|e| GroundedError::Metadata(format!("{e}")))?
                    .map(|emb| cosine(cue_vec, &emb))
                    .unwrap_or(0.0);
                scored.push((other, edge_score));
            }
            // Strongest-first; expand only the top-beam neighbors.
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored.dedup_by_key(|(id, _)| *id);
            for (other, _) in scored.into_iter().take(GROUNDED_WALK_BEAM) {
                if !visited.insert(other) {
                    continue;
                }
                next.push(other);
                let ans = grounded_answer(rtxn, other, cue_vec)?;
                if !matches!(ans.kind, AnswerKind::None) {
                    answers.insert(other, (ans, hop + 1));
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    Ok(select_walk_winner(&answers, anchor))
}

/// Pick the walk winner from the per-node answers (each tagged with the hop
/// depth at which it was reached), by DEPTH-DISCOUNTED match score. Pure and
/// deterministic, so it is unit-testable without a populated graph.
///
/// The effective score of a node's answer is its relation cosine times the
/// per-hop discount raised to the hop distance, so the nearest fact answering
/// the cue wins unless a deeper fact scores high enough to overcome the discount
/// — which a genuinely multi-hop cue produces, since its intermediate relation
/// words keep the deep node's cosine high.
///
/// A node's answer is a *path*, not a terminal, only when following its edge
/// reaches a destination whose OWN answer is at least as good (effective) — i.e.
/// descending does not lose score ("Niraj's sister" → Priya, whose occupation
/// out-scores the `sibling_of` edge, so we descend). A relation answer whose
/// destination has no answer, or a strictly weaker one, is itself the terminal:
/// this is what stops "Where does Niraj work?" from skipping his own
/// `works_at→NeuraCorp` (a strong 1-hop answer) just because NeuraCorp happens
/// to carry unrelated facts, and then descending to a far, weaker employer.
/// Reading graph SHAPE, not the question's words, keeps this domain-agnostic.
///
/// The chosen answer's `match_score` is scaled by its depth discount before
/// return, so the caller's cross-anchor selection (`best_grounded_for_cue`, one
/// walk result per resolved subject) also prefers the nearest answer.
/// Deterministic order: effective score desc, then shallower depth, then id.
fn select_walk_winner(
    answers: &HashMap<EntityId, (GroundedAnswer, usize)>,
    anchor: EntityId,
) -> GroundedAnswer {
    let score_of = |a: &GroundedAnswer| a.values.first().map(|v| v.match_score).unwrap_or(0.0);
    let eff = |raw: f32, depth: usize| raw * GROUNDED_WALK_DEPTH_DISCOUNT.powi(depth as i32);
    let is_path = |id: EntityId, ans: &GroundedAnswer, depth: usize| -> bool {
        let Some(dest) = ans.values.first().and_then(|v| v.object.as_entity()) else {
            return false;
        };
        if dest == anchor || dest == id {
            return false;
        }
        match answers.get(&dest) {
            Some((dest_ans, dest_depth)) => {
                eff(score_of(dest_ans), *dest_depth) >= eff(score_of(ans), depth)
            }
            None => false,
        }
    };

    let mut ranked: Vec<(EntityId, &GroundedAnswer, usize, f32)> = answers
        .iter()
        .map(|(id, (ans, depth))| (*id, ans, *depth, eff(score_of(ans), *depth)))
        .collect();
    ranked.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.2.cmp(&b.2))
            .then(a.0.to_bytes().cmp(&b.0.to_bytes()))
    });

    let chosen = ranked
        .iter()
        .find(|(id, ans, depth, _)| !is_path(*id, ans, *depth))
        .or_else(|| ranked.first());

    match chosen {
        Some((_, ans, depth, _)) => {
            let mut ans = (*ans).clone();
            let factor = GROUNDED_WALK_DEPTH_DISCOUNT.powi(*depth as i32);
            for v in &mut ans.values {
                v.match_score *= factor;
            }
            ans
        }
        None => GroundedAnswer::none(),
    }
}

/// Best statement-backed answer for the subject, or `None` when no current
/// statement predicate embeds at/above [`GROUNDED_MATCH_FLOOR`] against the cue
/// (or every match has a blank object).
fn best_statement_answer(
    rtxn: &ReadTransaction,
    subject: EntityId,
    cue_vec: &[f32; brain_embed::VECTOR_DIM],
) -> Result<Option<GroundedAnswer>, GroundedError> {
    let stmts = statement_list(
        rtxn,
        &StatementListFilter {
            subject: Some(subject),
            predicate: None,
            kind: None,
            current_only: true,
            min_confidence: None,
            limit: 0,
        },
    )
    .map_err(|e| GroundedError::Metadata(format!("{e}")))?;
    if stmts.is_empty() {
        return Ok(None);
    }

    // Group the subject's current statements by predicate.
    let mut by_pred: HashMap<PredicateId, Vec<Statement>> = HashMap::new();
    for s in stmts {
        by_pred.entry(s.predicate).or_default().push(s);
    }

    // Match each distinct predicate by EMBEDDING cosine against the cue; keep
    // the single best-scoring predicate that clears the floor. The matched
    // predicate's declared cardinality no longer decides the answer shape —
    // the actual stored rows do (see `shape_answer`), so a single-valued
    // predicate with two disagreeing current rows surfaces both as a
    // contradiction. A predicate with no stored embedding (older rows, or
    // written when the embedder was absent) can't match semantically and is
    // skipped — never a panic.
    let mut best: Option<(PredicateId, f32)> = None;
    for pid in by_pred.keys() {
        let Some(emb) = predicate_embedding_get(rtxn, *pid)
            .map_err(|e| GroundedError::Metadata(format!("{e}")))?
        else {
            continue;
        };
        let score = cosine(cue_vec, &emb);
        if score >= GROUNDED_MATCH_FLOOR && best.as_ref().is_none_or(|b| score > b.1) {
            best = Some((*pid, score));
        }
    }
    let Some((pid, match_score)) = best else {
        return Ok(None);
    };

    let qname = predicate_get(rtxn, pid)
        .ok()
        .flatten()
        .map(|p| p.canonical())
        .unwrap_or_default();

    let mut group = by_pred.remove(&pid).unwrap_or_default();
    // Drop statements whose object is blank — a stored empty value is not a
    // real memory and must never surface as a (fake) answer. If the matched
    // predicate has only blank objects, there is no memory → None.
    group.retain(|s| is_meaningful_object(&s.object));
    // Most-RECENT first (event time if known, else record time), confidence
    // breaking ties. When two current rows disagree (e.g. an older
    // "works_at Google" and a newer "works_at OpenAI"), the present-tense
    // answer is the latest assertion; `shape_answer` keeps this order both to
    // rank a surfaced contradiction and to pick the survivor when rows agree.
    let recency = |s: &Statement| s.event_at_unix_nanos.unwrap_or(s.extracted_at_unix_nanos);
    group.sort_by(|a, b| {
        recency(b).cmp(&recency(a)).then(
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    let values: Vec<GroundedValue> = group
        .into_iter()
        .map(|s| GroundedValue {
            predicate: qname.clone(),
            source_memory: first_evidence_memory(&s.evidence),
            confidence: s.confidence,
            match_score,
            recency: recency(&s),
            object: s.object,
        })
        .collect();

    Ok(shape_answer(values))
}

/// Best relation-backed answer for the subject, or `None` when no current
/// outgoing relation's type name is named exactly in the question.
///
/// Entity↔entity links live in the relations table keyed by relation type,
/// not in the statements table. We match the question against each distinct
/// relation-type *name* by exact membership, same as the predicate path.
///
/// A winning relation type yields its current edges shaped by the actual
/// rows (see `shape_answer`): each edge's object is the `to_entity`'s
/// canonical name. Relation cardinality governs supersession at write time
/// (stale edges are already non-current), so the read surfaces every current
/// member — and when two current edges resolve to the same target name they
/// collapse to a single value, while distinct targets form the natural Set.
fn best_relation_answer(
    rtxn: &ReadTransaction,
    subject: EntityId,
    cue_vec: &[f32; brain_embed::VECTOR_DIM],
) -> Result<Option<GroundedAnswer>, GroundedError> {
    // A relation is directional, but a question can ask from EITHER end. The
    // surfaced answer is always the OTHER endpoint from the subject:
    //   "what did Arjun found"  → Arjun's OUTGOING `founded` edge → object = to-entity
    //   "who founded NeuraCorp" → NeuraCorp's INCOMING `founded` edge → object = from-entity
    // Querying only outgoing made every reverse question unanswerable (the
    // `founded` / `acquired` edge lives on the other node). Collect both; the
    // relation-type embedding is direction-agnostic, so the same cosine match
    // applies. (other_entity, confidence, source_memory, recency) per edge.
    let filter = RelationListFilter {
        relation_type: None,
        current_only: true,
        limit: 0,
    };
    let outgoing = relation_list_from(rtxn, subject, &filter)
        .map_err(|e| GroundedError::Metadata(format!("{e}")))?;
    let incoming = relation_list_to(rtxn, subject, &filter)
        .map_err(|e| GroundedError::Metadata(format!("{e}")))?;
    if outgoing.is_empty() && incoming.is_empty() {
        return Ok(None);
    }

    type EdgeVal = (EntityId, f32, Option<MemoryId>, u64); // (other, conf, src, recency)
    let mut by_type: HashMap<RelationTypeId, Vec<EdgeVal>> = HashMap::new();
    for r in outgoing {
        by_type.entry(r.relation_type).or_default().push((
            r.to_entity,
            r.confidence,
            r.evidence.first().copied(),
            r.extracted_at_unix_nanos,
        ));
    }
    for r in incoming {
        by_type.entry(r.relation_type).or_default().push((
            r.from_entity,
            r.confidence,
            r.evidence.first().copied(),
            r.extracted_at_unix_nanos,
        ));
    }

    // Match each distinct relation type by EMBEDDING cosine against the cue,
    // same as the predicate path; keep the best that clears the floor. A
    // relation type with no stored embedding is skipped (never a panic).
    let mut best: Option<(RelationTypeId, f32)> = None;
    for &rtid in by_type.keys() {
        let Some(emb) = relation_type_embedding_get(rtxn, rtid)
            .map_err(|e| GroundedError::Metadata(format!("{e}")))?
        else {
            continue;
        };
        let score = cosine(cue_vec, &emb);
        if score >= GROUNDED_MATCH_FLOOR && best.as_ref().is_none_or(|b| score > b.1) {
            best = Some((rtid, score));
        }
    }
    let Some((rtid, match_score)) = best else {
        return Ok(None);
    };

    let qname = relation_type_get(rtxn, rtid)
        .ok()
        .flatten()
        .map(|rt| rt.canonical())
        .unwrap_or_default();

    let group = by_type.remove(&rtid).unwrap_or_default();
    // Map each edge to a value: object text = the OTHER endpoint's canonical
    // name. An edge whose other entity is missing or unnamed carries no
    // surfaceable object and is dropped, mirroring the blank-object guard on
    // statements.
    let mut values = Vec::new();
    for (other_entity, confidence, source_memory, recency) in group {
        let object_name = entity_get(rtxn, other_entity)
            .map_err(|e| GroundedError::Metadata(format!("{e}")))?
            .map(|e| e.canonical_name)
            .unwrap_or_default();
        if object_name.trim().is_empty() {
            continue;
        }
        values.push(GroundedValue {
            predicate: qname.clone(),
            object: StatementObject::Value(StatementValue::Text(object_name)),
            confidence,
            source_memory,
            match_score,
            // Edges carry only a record time (no separate event time).
            recency,
        });
    }
    // Most-RECENT first (confidence breaks ties), matching the statement
    // path: a present-tense question surfaces the latest edge, and this is the
    // order `shape_answer` ranks a Set in / picks the survivor from when edges
    // agree on a target.
    values.sort_by(|a, b| {
        b.recency.cmp(&a.recency).then(
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    // Entity links accumulate (one subject may work_at / know many targets),
    // so distinct targets form a Set; two current edges to the same-named
    // target collapse to a single value — the data, not the cardinality,
    // decides (see `shape_answer`).
    Ok(shape_answer(values))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meaningful_object_rejects_blank_values() {
        use brain_core::{StatementObject, StatementValue};
        // Blank / whitespace text is NOT a memory.
        assert!(!is_meaningful_object(&StatementObject::Value(
            StatementValue::Text(String::new())
        )));
        assert!(!is_meaningful_object(&StatementObject::Value(
            StatementValue::Text("   ".into())
        )));
        assert!(!is_meaningful_object(&StatementObject::Value(
            StatementValue::Blob(Vec::new())
        )));
        // Real content is meaningful.
        assert!(is_meaningful_object(&StatementObject::Value(
            StatementValue::Text("Berlin".into())
        )));
        assert!(is_meaningful_object(&StatementObject::Value(
            StatementValue::Integer(0)
        )));
        assert!(is_meaningful_object(&StatementObject::Entity(
            brain_core::EntityId::new()
        )));
    }

    fn eid(b: u8) -> EntityId {
        EntityId::from([b; 16])
    }
    fn ans_entity(score: f32, dest: EntityId) -> GroundedAnswer {
        GroundedAnswer {
            kind: AnswerKind::Single,
            values: vec![GroundedValue {
                predicate: "brain:x".into(),
                object: StatementObject::Entity(dest),
                confidence: 1.0,
                source_memory: None,
                match_score: score,
                recency: 0,
            }],
        }
    }
    fn ans_value(score: f32, text: &str) -> GroundedAnswer {
        GroundedAnswer {
            kind: AnswerKind::Single,
            values: vec![GroundedValue {
                predicate: "brain:x".into(),
                object: StatementObject::Value(StatementValue::Text(text.into())),
                confidence: 1.0,
                source_memory: None,
                match_score: score,
                recency: 0,
            }],
        }
    }

    #[test]
    fn walk_winner_keeps_near_answer_edge_over_far_leaf() {
        // "Where does Niraj work?": the anchor's OWN works_at→NeuraCorp (1-hop)
        // must win over a relative's works_at→AirIndia reached 3 hops away, even
        // though both edges match `works_at` equally. NeuraCorp carrying an
        // unrelated, weaker fact (headquarters) must NOT demote the anchor's
        // answer to a skippable "path".
        let anchor = eid(1);
        let neura = eid(2);
        let air_india = eid(9);
        let mut answers: HashMap<EntityId, (GroundedAnswer, usize)> = HashMap::new();
        answers.insert(anchor, (ans_entity(0.78, neura), 0)); // works_at→NeuraCorp
        answers.insert(neura, (ans_value(0.55, "Pune"), 1)); // headquarters (weaker)
        answers.insert(air_india, (ans_value(0.78, "Air India"), 3)); // far works_at leaf
        let w = select_walk_winner(&answers, anchor);
        assert_eq!(
            w.values.first().and_then(|v| v.object.as_entity()),
            Some(neura),
            "must return the anchor's own employer, not the far leaf"
        );
    }

    #[test]
    fn walk_winner_descends_when_deeper_scores_higher() {
        // "What does Niraj's sister do?": the `sibling_of` edge (anchor, 0.70)
        // is a path because the destination's occupation (0.82, 1 hop) out-scores
        // it even after the depth discount, so the deeper attribute wins.
        let anchor = eid(1);
        let priya = eid(3);
        let mut answers: HashMap<EntityId, (GroundedAnswer, usize)> = HashMap::new();
        answers.insert(anchor, (ans_entity(0.70, priya), 0)); // sibling_of→Priya
        answers.insert(priya, (ans_value(0.82, "cardiologist"), 1)); // occupation
        let w = select_walk_winner(&answers, anchor);
        assert!(
            matches!(
                w.values.first().map(|v| &v.object),
                Some(StatementObject::Value(StatementValue::Text(t))) if t == "cardiologist"
            ),
            "deeper, higher-scoring attribute must win: {:?}",
            w.values.first().map(|v| &v.object)
        );
    }

    #[test]
    fn walk_winner_is_deterministic_on_ties() {
        // Equal effective score at equal depth: the winner must be stable across
        // calls (tie-break by entity id), never HashMap-iteration-order-dependent.
        let anchor = eid(1);
        let mut answers: HashMap<EntityId, (GroundedAnswer, usize)> = HashMap::new();
        answers.insert(anchor, (ans_value(0.40, "anchor-weak"), 0));
        answers.insert(eid(7), (ans_value(0.80, "seven"), 2));
        answers.insert(eid(4), (ans_value(0.80, "four"), 2));
        answers.insert(eid(9), (ans_value(0.80, "nine"), 2));
        let first = select_walk_winner(&answers, anchor);
        for _ in 0..20 {
            let again = select_walk_winner(&answers, anchor);
            assert_eq!(
                first.values.first().map(|v| &v.object),
                again.values.first().map(|v| &v.object),
                "winner must be deterministic across calls"
            );
        }
        // Smallest id among the tied top scorers wins (eid(4)).
        assert!(matches!(
            first.values.first().map(|v| &v.object),
            Some(StatementObject::Value(StatementValue::Text(t))) if t == "four"
        ));
    }

    #[test]
    fn cosine_is_defensive_on_degenerate_input() {
        // Equal vectors → 1.0; orthogonal → 0.0; zero-norm → 0.0 (never NaN);
        // length mismatch → 0.0. These are the guards that keep a missing or
        // malformed embedding from poisoning the ranking.
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0, 2.0, 3.0], &[1.0, 2.0]), 0.0);
    }
}
