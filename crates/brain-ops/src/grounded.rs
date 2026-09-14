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
//! write-time as the phrase "works at"), gated by `GROUNDED_MATCH_FLOOR`.
//! There is no string tokenization, stop-word list, or stemmer — those are
//! brittle, English-only static-text heuristics Brain deliberately avoids; the
//! embedder is the single source of relation similarity. A subject with no
//! predicate clearing the floor yields `AnswerKind::None`, and the boost is a
//! no-op — the episodic read stands on its own.

use std::collections::HashMap;

use brain_core::{
    EntityId, EvidenceRef, KindBehavior, KindCardinality, MemoryId, PredicateId, RelationTypeId,
    Slot, Statement, StatementKind, StatementObject, StatementValue, SubjectRef, TemporalModel,
};
use brain_metadata::{
    entity_get, kind_behavior, predicate_embedding_get, predicate_get, relation_list_from,
    relation_list_to, relation_type_embedding_get, relation_type_get, statement_list,
    RelationListFilter, RowScope, StatementListFilter,
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
    /// matched predicate / relation-type embedding (>= `GROUNDED_MATCH_FLOOR`).
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
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32 {
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

/// Collapse a recency-ranked value list to its distinct MEMBERS, order-
/// preserving (so the head stays "most current"). Two values are the same
/// member when they assert the same object — EXCEPT under a polarity kind
/// (Preference), where the member key also includes the predicate so a
/// "likes X" and a "dislikes X" (same object, opposite polarity carried by
/// the predicate) are never merged into one. The polarity distinction comes
/// from the KIND's `KindBehavior`, never from parsing the predicate string.
fn dedup_members(values: Vec<GroundedValue>, polarity: bool) -> Vec<GroundedValue> {
    let mut out: Vec<GroundedValue> = Vec::with_capacity(values.len());
    for v in values {
        let dup = out
            .iter()
            .any(|e| e.object == v.object && (!polarity || e.predicate == v.predicate));
        if !dup {
            out.push(v);
        }
    }
    out
}

/// Shape matched, blank-filtered, recency-sorted candidate values into a
/// `GroundedAnswer` driven by the matched fact's KIND — its `KindBehavior`,
/// not the raw row count, is the primary signal. `values` must already be
/// recency-ranked (head = current) and free of blank objects.
///
/// Cardinality is the driver:
/// - **Single** (Attribute / Directive): one current value. Supersession has
///   already left the latest current row at the head, so the head is "now".
///   The stored-row contradiction refinement is kept: two DISTINCT current
///   values under a single-valued kind is a contradiction Brain surfaces (a
///   recency-ranked `Set` of the competing claims) rather than silently
///   resolving.
/// - **Set** (Relation / Preference / Event / Fact): enumerate ALL current
///   members so a downstream "how many" is simply `|members|`. True
///   duplicates (same member key) collapse; a lone member is a `Single`, two
///   or more a `Set`. A polarity kind keys members on `(predicate, object)`
///   so likes and dislikes both survive (see [`dedup_members`]).
///
/// Returns `None` for an empty input (no memory).
fn shape_answer_for_kind(
    values: Vec<GroundedValue>,
    behavior: KindBehavior,
) -> Option<GroundedAnswer> {
    if values.is_empty() {
        return None;
    }
    let members = dedup_members(values, behavior.polarity);
    let kind = match behavior.cardinality {
        // Single: one value unless the graph disagrees with itself (contradiction → Set).
        KindCardinality::Single if members.len() > 1 => AnswerKind::Set,
        KindCardinality::Single => AnswerKind::Single,
        // Set: a lone member is a Single; two or more distinct members are the Set.
        KindCardinality::Set if members.len() > 1 => AnswerKind::Set,
        KindCardinality::Set => AnswerKind::Single,
    };
    Some(GroundedAnswer {
        kind,
        values: members,
    })
}

/// The `KindBehavior` for entity↔entity links (the relations table). Every
/// row there is a `Relation` kind by construction — a set-valued, stateful,
/// non-polar link — so the read shapes them with that behavior directly
/// rather than resolving a per-row kind. A built-in kind always has a
/// behavior, so the fallback is unreachable but kept non-panicking.
fn relation_behavior() -> KindBehavior {
    StatementKind::Relation
        .builtin_behavior()
        .unwrap_or(KindBehavior::new(
            KindCardinality::Set,
            TemporalModel::State,
            false,
        ))
}

fn first_evidence_memory(ev: &EvidenceRef) -> Option<MemoryId> {
    match ev {
        EvidenceRef::Inline(v) => v.first().map(|e| e.memory_id),
        EvidenceRef::Overflow(_) => None,
    }
}

/// A memory's own time anchor: its client-supplied `occurred_at`, else its
/// record `created_at`. This mirrors the write path's anchor (the date a
/// same-day event's resolved date is compared against), so an Event with no
/// distinct `event_at` answers "when" with the same instant the write treated
/// as the message time. Returns `None` only when the memory row is absent.
fn memory_time_anchor(rtxn: &ReadTransaction, mid: MemoryId) -> Result<Option<u64>, GroundedError> {
    use brain_metadata::tables::memory::MEMORIES_TABLE;
    let table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| GroundedError::Metadata(format!("{e}")))?;
    let row = table
        .get(&mid.to_be_bytes())
        .map_err(|e| GroundedError::Metadata(format!("{e}")))?;
    Ok(row.map(|g| {
        let m = g.value();
        m.occurred_at_unix_nanos.unwrap_or(m.created_at_unix_nanos)
    }))
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

/// Minimum statement-question cosine for the reified slot-projection overlay to
/// fire. Deliberately above the loose `GROUNDED_MATCH_FLOOR` (0.5): projecting
/// a specific slot's value AS the grounded answer is a stronger claim than
/// boosting on a predicate-name cosine, so it demands a strong, unambiguous
/// question match.
///
/// Raised from 0.6 to 0.66 as the grounded half of the honest-abstention fix. A
/// grounded `Answer` makes the read bypass BOTH abstention gates (the typed
/// graph is presumed to hold the fact), so a *spurious* slot-projection answer
/// is not merely a wrong result — it suppresses `None` entirely. Measured: an
/// off-topic cue ("asdfghjkl qwerty") slot-matched at 0.626 and cleared the old
/// 0.6 bar, shipping the whole band as `Many`. BGE-small's compressed geometry
/// puts even gibberish slot-matches in the low 0.6s, so the projection bar must
/// sit above that noise floor. Genuine slot answers score well clear of it (the
/// passage-level gap between real and off-topic cues is ~2×). Calibrated against
/// the read fixtures; the robust long-term fix is a cross-encoder verifier
/// (rerank is off by default here), tracked as follow-up.
pub const SLOT_PROJECTION_STRONG_FLOOR: f32 = 0.66;

/// Project the matched [`Slot`] of a reified statement into a [`GroundedValue`].
///
/// A bridge question is generated by omitting exactly one slot, so a
/// question-index hit carries the slot the cue asked for — and "return the
/// object" is just one case of "return the requested slot":
///   - [`Slot::Object`] → the statement's stored object (as the object path
///     does today). A blank object is not a memory → `None`.
///   - [`Slot::Time`] → the Event fact's `event_at_unix_nanos`, rendered as a
///     `UnixNanos` value. When an Event carries no distinct `event_at` (a same-
///     day event whose resolved date equalled the memory anchor, so the write
///     left it unstamped), fall back to the evidence memory's own time anchor —
///     the honest message-time estimate for "when". Only an Event exposes a Time
///     role; a State/Atemporal fact → `None` (never fabricate a record
///     timestamp), and an Event with no memory date behind it → `None`.
///   - [`Slot::Subject`] → the statement's subject entity, resolved to its
///     canonical name. A pending/unnamed subject → `None`.
///
/// `match_score` is the statement-question cosine; `source_memory` is the
/// statement's first evidence memory. Pure projection over one loaded
/// statement, so the slot-selection logic is unit-testable without a populated
/// question index.
pub fn project_statement_slot(
    rtxn: &ReadTransaction,
    s: &Statement,
    slot: Slot,
    match_score: f32,
) -> Result<Option<GroundedValue>, GroundedError> {
    let object = match slot {
        Slot::Object => {
            if !is_meaningful_object(&s.object) {
                return Ok(None);
            }
            s.object.clone()
        }
        Slot::Time => {
            // The Time role exists ONLY for an Event kind. A State/Atemporal fact
            // has no time role, so a "when …" cue must not be answered by
            // fabricating a record timestamp — the gate is kind-driven (Event ⇒
            // TemporalModel::Event), never a guess from whether `event_at` happens
            // to be populated on a non-Event row.
            let behavior =
                kind_behavior(rtxn, s.kind).map_err(|e| GroundedError::Metadata(format!("{e}")))?;
            if !behavior.temporal.is_event() {
                return Ok(None);
            }
            // Explicit temporal keys: prefer the Event fact's own resolved event
            // time. When it carries none — a same-day event whose resolved date
            // equalled the memory anchor, so the write deliberately left no
            // distinct `event_at` — fall back to the evidence memory's own time
            // anchor (its `occurred_at`, else `created_at`): the honest message-
            // time estimate for "when". Never invent a time with no memory behind
            // it (absent memory row → `None`).
            //
            // This fallback is now REACHABLE for real "when" reads: it goes live
            // the moment the write side stops downgrading a dateless action from
            // Event to Fact, so an Event with no distinct `event_at` reaches this
            // branch (a Fact would have failed the `is_event()` gate above and
            // returned `None`). The logic below is deliberately unchanged.
            let t = match s.event_at_unix_nanos {
                Some(t) => t,
                None => {
                    let Some(mid) = first_evidence_memory(&s.evidence) else {
                        return Ok(None);
                    };
                    match memory_time_anchor(rtxn, mid)? {
                        Some(t) => t,
                        None => return Ok(None),
                    }
                }
            };
            StatementObject::Value(StatementValue::UnixNanos(t))
        }
        Slot::Subject => {
            let SubjectRef::Entity(subject_id) = s.subject else {
                return Ok(None);
            };
            let Some(name) = entity_get(rtxn, subject_id)
                .map_err(|e| GroundedError::Metadata(format!("{e}")))?
                .map(|e| e.canonical_name)
            else {
                return Ok(None);
            };
            if name.trim().is_empty() {
                return Ok(None);
            }
            StatementObject::Value(StatementValue::Text(name))
        }
    };

    let predicate = predicate_get(rtxn, s.predicate)
        .ok()
        .flatten()
        .map(|p| p.canonical())
        .unwrap_or_default();
    Ok(Some(GroundedValue {
        predicate,
        object,
        confidence: s.confidence,
        source_memory: first_evidence_memory(&s.evidence),
        match_score,
        recency: s.event_at_unix_nanos.unwrap_or(s.extracted_at_unix_nanos),
    }))
}

/// Answer a grounded relation question for one resolved subject.
///
/// Returns the matching kind-shaped value(s), or `AnswerKind::None` when no
/// stored predicate / relation type embeds close enough to the cue. Matching
/// is semantic (cosine of `cue_vec` vs the predicate's stored embedding) —
/// no string matching, no threshold-free exactness.
pub fn grounded_answer(
    rtxn: &ReadTransaction,
    scope: RowScope,
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
    let stmt = best_statement_answer(rtxn, scope, subject, cue_vec)?;
    let rel = best_relation_answer(rtxn, scope, subject, cue_vec)?;
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

/// The winning answer together with the hop DEPTH at which it was found (0 = the
/// anchor itself). The caller uses the depth to tell a genuine multi-hop chain
/// (`depth >= 1`, reached by descending an edge) from a shallow single-hop answer
/// sitting on the anchor — so a real multi-hop walk answer can win over the
/// single-hop slot projection without any interrogative-word heuristic.
pub type WalkAnswer = (GroundedAnswer, usize);

/// Multi-hop grounded answer: a bounded beam walk over the typed graph from
/// `anchor`, running the 1-hop [`grounded_answer`] at every reachable node and
/// returning the single best-scoring answer found.
///
/// This is the read-side mechanism for multi-hop questions ("Where did Niraj's
/// manager work before?", "What does Niraj's sister do?"). It needs no LLM and
/// no read-time generation: at each hop it scores every incident relation edge
/// by the cosine of the cue against the edge's relation-type embedding, expands
/// the strongest `GROUNDED_WALK_BEAM` neighbors, and at each visited node asks
/// the same precise 1-hop matcher whether that node answers the cue. The walk
/// assembles the chain from whatever edges exist at read time — so it follows
/// reports_to → worked_before, or family_of → married_to → occupation, purely
/// from edge/predicate similarity to the cue. Depth is capped at
/// `GROUNDED_WALK_MAX_HOPS` and visited nodes are deduped, so the work is
/// bounded by `beam^hops` regardless of graph size.
///
/// The winner is the highest DEPTH-DISCOUNTED match score across all visited
/// nodes (see `GROUNDED_WALK_DEPTH_DISCOUNT`): a deep fact must out-score the
/// per-hop discount to beat a nearer one, so "…work before?" still follows
/// reports_to → prior-employer (the deep predicate scores high), while a bare
/// "where does X work?" keeps X's own 1-hop employer instead of chaining into a
/// relative's. Returns `AnswerKind::None` (depth 0) when nothing on the walk
/// clears the floor — the boost is then a no-op and the episodic read stands
/// alone. The returned depth (see [`WalkAnswer`]) is the hop distance of the
/// chosen answer, so the caller can distinguish a genuine multi-hop chain from a
/// shallow single-hop answer on the anchor.
pub fn grounded_answer_walk(
    rtxn: &ReadTransaction,
    scope: RowScope,
    anchor: EntityId,
    cue_vec: &[f32; brain_embed::VECTOR_DIM],
) -> Result<WalkAnswer, GroundedError> {
    use std::collections::{HashMap, HashSet};

    let mut visited: HashSet<EntityId> = HashSet::new();
    visited.insert(anchor);
    let mut frontier: Vec<EntityId> = vec![anchor];
    // Every node whose grounded answer clears the floor, with the hop distance
    // at which we reached it. Depth drives the nearest-wins discount and the
    // path-vs-terminal test below.
    let mut answers: HashMap<EntityId, (GroundedAnswer, usize)> = HashMap::new();
    let anchor_ans = grounded_answer(rtxn, scope, anchor, cue_vec)?;
    if !matches!(anchor_ans.kind, AnswerKind::None) {
        answers.insert(anchor, (anchor_ans, 0));
    }

    for hop in 0..GROUNDED_WALK_MAX_HOPS {
        let mut next: Vec<EntityId> = Vec::new();
        for &node in &frontier {
            // Score every incident edge by cue↔edge cosine and expand only the
            // top-beam neighbors; the surfaced neighbor is always the OTHER
            // endpoint. Two edge KINDS are unified here, because a hop in the
            // typed graph is encoded either as a relations-table row (an
            // entity↔entity link, scored by its relation-type embedding) OR as a
            // statement whose object is an entity (a Fact/Event about the node
            // pointing at another entity, scored by its predicate embedding). A
            // chain that mixes the two — "X --friend(statement)--> Y
            // --works_at(relation)--> Z" — stays connected only when BOTH kinds
            // are walkable; scoring an entity-object statement by its predicate
            // embedding is exactly the statement-side analogue of the relation
            // path, so both feed one beam under the same cue↔edge cosine. Without
            // this, any hop encoded as a statement silently broke the chain.
            //
            // When a neighbor is reachable by more than one edge we keep its BEST
            // edge score, so a strong link isn't crowded out of the beam by a
            // weaker parallel one.
            let mut best_edge: HashMap<EntityId, f32> = HashMap::new();
            let mut consider = |other: EntityId, score: f32| {
                if other == node || visited.contains(&other) {
                    return;
                }
                best_edge
                    .entry(other)
                    .and_modify(|e| {
                        if score > *e {
                            *e = score;
                        }
                    })
                    .or_insert(score);
            };

            // Relation-table edges (both directions), scored by the relation type.
            let rel_filter = RelationListFilter {
                relation_type: None,
                current_only: true,
                limit: 0,
            };
            let outgoing = relation_list_from(rtxn, scope, node, &rel_filter)
                .map_err(|e| GroundedError::Metadata(format!("{e}")))?;
            let incoming = relation_list_to(rtxn, scope, node, &rel_filter)
                .map_err(|e| GroundedError::Metadata(format!("{e}")))?;
            for r in outgoing.iter().chain(incoming.iter()) {
                let other = if r.from_entity == node {
                    r.to_entity
                } else {
                    r.from_entity
                };
                let edge_score = relation_type_embedding_get(rtxn, r.relation_type)
                    .map_err(|e| GroundedError::Metadata(format!("{e}")))?
                    .map(|emb| cosine(cue_vec, &emb))
                    .unwrap_or(0.0);
                consider(other, edge_score);
            }

            // Statement edges: the node's current statements whose OBJECT is an
            // entity are hops too, scored by the statement's predicate embedding
            // (the same signal the relation path reads off the relation type).
            let stmts = statement_list(
                rtxn,
                scope,
                &StatementListFilter {
                    subject: Some(node),
                    current_only: true,
                    limit: 0,
                    ..Default::default()
                },
            )
            .map_err(|e| GroundedError::Metadata(format!("{e}")))?;
            for s in &stmts {
                let Some(other) = s.object.as_entity() else {
                    continue;
                };
                let edge_score = predicate_embedding_get(rtxn, s.predicate)
                    .map_err(|e| GroundedError::Metadata(format!("{e}")))?
                    .map(|emb| cosine(cue_vec, &emb))
                    .unwrap_or(0.0);
                consider(other, edge_score);
            }

            // Strongest-first, id tie-break for deterministic beam selection.
            let mut scored: Vec<(EntityId, f32)> = best_edge.into_iter().collect();
            scored.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.0.to_bytes().cmp(&b.0.to_bytes()))
            });
            for (other, _) in scored.into_iter().take(GROUNDED_WALK_BEAM) {
                if !visited.insert(other) {
                    continue;
                }
                next.push(other);
                let ans = grounded_answer(rtxn, scope, other, cue_vec)?;
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
) -> WalkAnswer {
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
            (ans, *depth)
        }
        None => (GroundedAnswer::none(), 0),
    }
}

/// Best statement-backed answer for the subject, or `None` when no current
/// statement predicate embeds at/above [`GROUNDED_MATCH_FLOOR`] against the cue
/// (or every match has a blank object).
fn best_statement_answer(
    rtxn: &ReadTransaction,
    scope: RowScope,
    subject: EntityId,
    cue_vec: &[f32; brain_embed::VECTOR_DIM],
) -> Result<Option<GroundedAnswer>, GroundedError> {
    let stmts = statement_list(
        rtxn,
        scope,
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
    // fact's KIND drives the answer shape (`shape_answer_for_kind`): a
    // single-valued kind with two disagreeing current rows surfaces both as a
    // contradiction, a set-valued kind enumerates its members. A predicate with
    // no stored embedding (older rows, or
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
    // answer is the latest assertion; the kind-aware shaper keeps this order
    // both to rank a surfaced contradiction and to pick the survivor.
    let recency = |s: &Statement| s.event_at_unix_nanos.unwrap_or(s.extracted_at_unix_nanos);
    group.sort_by(|a, b| {
        recency(b).cmp(&recency(a)).then(
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    // The answer's SHAPE derives from the matched fact's KIND, not the raw row
    // count. All rows in this group share the predicate, so they share the kind
    // (statements of one predicate carry one kind); the head after the recency
    // sort is the representative. `Custom` kinds resolve their behavior via the
    // metadata kind registry the read txn reaches; a missing declaration degrades
    // to Set/Atemporal (never a panic).
    let kind = group.first().map(|s| s.kind).unwrap_or(StatementKind::Fact);
    let behavior =
        kind_behavior(rtxn, kind).map_err(|e| GroundedError::Metadata(format!("{e}")))?;

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

    Ok(shape_answer_for_kind(values, behavior))
}

/// Best relation-backed answer for the subject, or `None` when no current
/// outgoing relation's type name is named exactly in the question.
///
/// Entity↔entity links live in the relations table keyed by relation type,
/// not in the statements table. We match the question against each distinct
/// relation-type *name* by exact membership, same as the predicate path.
///
/// A winning relation type yields its current edges shaped by the Relation
/// kind (`shape_answer_for_kind`, set-valued): each edge's object is the
/// `to_entity`'s canonical name. Relation cardinality governs supersession at
/// write time
/// (stale edges are already non-current), so the read surfaces every current
/// member — and when two current edges resolve to the same target name they
/// collapse to a single value, while distinct targets form the natural Set.
fn best_relation_answer(
    rtxn: &ReadTransaction,
    scope: RowScope,
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
    let outgoing = relation_list_from(rtxn, scope, subject, &filter)
        .map_err(|e| GroundedError::Metadata(format!("{e}")))?;
    let incoming = relation_list_to(rtxn, scope, subject, &filter)
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
    // order `shape_answer_for_kind` ranks a Set in / picks the survivor from
    // when edges agree on a target.
    values.sort_by(|a, b| {
        b.recency.cmp(&a.recency).then(
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    // Entity links are the Relation kind — set-valued by construction: one
    // subject may work_at / know many targets, so distinct targets form a Set,
    // while two current edges to the same-named target collapse to one member.
    Ok(shape_answer_for_kind(values, relation_behavior()))
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
        let (w, _depth) = select_walk_winner(&answers, anchor);
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
        let (w, depth) = select_walk_winner(&answers, anchor);
        assert_eq!(depth, 1, "the deeper attribute is reached at hop 1");
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
        let (first, _) = select_walk_winner(&answers, anchor);
        for _ in 0..20 {
            let (again, _) = select_walk_winner(&answers, anchor);
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

    /// Build a temp metadata db with one subject entity + one predicate, and a
    /// statement builder over them. Returns `(dir, db, scope, subject, build)`.
    #[allow(clippy::type_complexity)]
    fn slot_projection_fixture() -> (
        tempfile::TempDir,
        brain_metadata::MetadataDb,
        RowScope,
        EntityId,
        PredicateId,
    ) {
        use brain_core::{Entity, EntityType};
        let dir = tempfile::tempdir().unwrap();
        let db = brain_metadata::MetadataDb::open(dir.path().join("m.redb")).unwrap();
        let scope = RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xB2; 16]);
        let subject = EntityId::new();
        let wtxn = db.write_txn().unwrap();
        brain_metadata::entity::ops::entity_put(
            &wtxn,
            scope,
            brain_core::SessionId::DEFAULT,
            &Entity::new_active(
                subject,
                EntityType::PERSON_ID,
                "Melanie".into(),
                "melanie".into(),
                1,
            ),
        )
        .unwrap();
        let pid =
            brain_metadata::schema::predicate::predicate_intern_or_get(&wtxn, "test", "ran", 0, 1)
                .unwrap();
        wtxn.commit().unwrap();
        (dir, db, scope, subject, pid)
    }

    fn statement_with(
        subject: EntityId,
        pid: PredicateId,
        kind: brain_core::StatementKind,
        object: StatementObject,
        event_at: Option<u64>,
    ) -> Statement {
        let mut s = Statement::new_root(
            brain_core::StatementId::new(),
            kind,
            SubjectRef::Entity(subject),
            pid,
            object,
            0.9,
            EvidenceRef::default(),
            brain_core::ExtractorId::from(0),
            1,
            1,
        );
        s.event_at_unix_nanos = event_at;
        s
    }

    #[test]
    fn slot_projection_object_returns_object() {
        let (_dir, db, _scope, subject, pid) = slot_projection_fixture();
        let rtxn = db.read_txn().unwrap();
        let s = statement_with(
            subject,
            pid,
            brain_core::StatementKind::Fact,
            StatementObject::Value(StatementValue::Text("charity race".into())),
            None,
        );
        let v = project_statement_slot(&rtxn, &s, Slot::Object, 0.8)
            .unwrap()
            .expect("object slot projects");
        assert_eq!(
            v.object,
            StatementObject::Value(StatementValue::Text("charity race".into()))
        );
        assert!((v.match_score - 0.8).abs() < 1e-6);
    }

    #[test]
    fn slot_projection_object_skips_blank() {
        let (_dir, db, _scope, subject, pid) = slot_projection_fixture();
        let rtxn = db.read_txn().unwrap();
        let s = statement_with(
            subject,
            pid,
            brain_core::StatementKind::Fact,
            StatementObject::Value(StatementValue::Text("   ".into())),
            None,
        );
        assert!(project_statement_slot(&rtxn, &s, Slot::Object, 0.8)
            .unwrap()
            .is_none());
    }

    #[test]
    fn slot_projection_time_returns_event_at() {
        let (_dir, db, _scope, subject, pid) = slot_projection_fixture();
        let rtxn = db.read_txn().unwrap();
        const T: u64 = 1_577_836_800_000_000_000;
        let s = statement_with(
            subject,
            pid,
            brain_core::StatementKind::Event,
            StatementObject::Value(StatementValue::Text("charity race".into())),
            Some(T),
        );
        let v = project_statement_slot(&rtxn, &s, Slot::Time, 0.7)
            .unwrap()
            .expect("time slot projects when event_at set");
        assert_eq!(
            v.object,
            StatementObject::Value(StatementValue::UnixNanos(T))
        );
    }

    #[test]
    fn slot_projection_time_none_without_event_at() {
        let (_dir, db, _scope, subject, pid) = slot_projection_fixture();
        let rtxn = db.read_txn().unwrap();
        let s = statement_with(
            subject,
            pid,
            brain_core::StatementKind::Fact,
            StatementObject::Value(StatementValue::Text("charity race".into())),
            None,
        );
        assert!(
            project_statement_slot(&rtxn, &s, Slot::Time, 0.7)
                .unwrap()
                .is_none(),
            "a fact with no event time cannot answer a when-question"
        );
    }

    /// Insert a memory row carrying an explicit event time, so a same-day
    /// Event with no distinct `event_at` can fall back to it for "when".
    fn put_memory(
        db: &brain_metadata::MetadataDb,
        id: MemoryId,
        occurred_at: Option<u64>,
        created_at: u64,
    ) {
        use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
        let row = MemoryMetadata::new_active(
            id,
            brain_core::NamespaceId::SYSTEM,
            brain_core::SpaceId::new(),
            brain_core::SessionId::from(0),
            id.slot(),
            id.version(),
            brain_core::MemoryKind::Episodic,
            [0u8; 16],
            0.5,
            0,
            created_at,
        )
        .with_occurred_at(occurred_at);
        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&id.to_be_bytes(), &row).unwrap();
        }
        wtxn.commit().unwrap();
    }

    fn statement_with_evidence(
        subject: EntityId,
        pid: PredicateId,
        kind: brain_core::StatementKind,
        object: StatementObject,
        event_at: Option<u64>,
        evidence: EvidenceRef,
    ) -> Statement {
        let mut s = Statement::new_root(
            brain_core::StatementId::new(),
            kind,
            SubjectRef::Entity(subject),
            pid,
            object,
            0.9,
            evidence,
            brain_core::ExtractorId::from(0),
            1,
            1,
        );
        s.event_at_unix_nanos = event_at;
        s
    }

    #[test]
    fn slot_projection_time_event_falls_back_to_memory_occurred_at() {
        // A same-day Event carries no distinct event_at (its resolved date
        // equalled the memory anchor). The Time role still answers "when" with
        // the evidence memory's own occurred_at — the honest message-time.
        let (_dir, db, _scope, subject, pid) = slot_projection_fixture();
        const OCCURRED: u64 = 1_684_972_800_000_000_000; // 2023-05-25.
        let mid = MemoryId::pack(0, 1, 0);
        put_memory(&db, mid, Some(OCCURRED), 1);
        let ev = EvidenceRef::inline_from_slice(&[brain_core::EvidenceEntry::from_parts(
            mid,
            0.9,
            0,
            brain_core::ExtractorId::from(0),
        )]);
        let rtxn = db.read_txn().unwrap();
        let s = statement_with_evidence(
            subject,
            pid,
            brain_core::StatementKind::Event,
            StatementObject::Value(StatementValue::Text("charity race".into())),
            None,
            ev,
        );
        let v = project_statement_slot(&rtxn, &s, Slot::Time, 0.7)
            .unwrap()
            .expect("event with no event_at falls back to memory occurred_at");
        assert_eq!(
            v.object,
            StatementObject::Value(StatementValue::UnixNanos(OCCURRED))
        );
    }

    #[test]
    fn slot_projection_time_event_no_evidence_is_none() {
        // An Event with no event_at AND no evidence memory has no time to give.
        let (_dir, db, _scope, subject, pid) = slot_projection_fixture();
        let rtxn = db.read_txn().unwrap();
        let s = statement_with_evidence(
            subject,
            pid,
            brain_core::StatementKind::Event,
            StatementObject::Value(StatementValue::Text("charity race".into())),
            None,
            EvidenceRef::default(),
        );
        assert!(
            project_statement_slot(&rtxn, &s, Slot::Time, 0.7)
                .unwrap()
                .is_none(),
            "no event_at and no evidence memory → no when"
        );
    }

    #[test]
    fn slot_projection_subject_returns_entity_name() {
        let (_dir, db, _scope, subject, pid) = slot_projection_fixture();
        let rtxn = db.read_txn().unwrap();
        let s = statement_with(
            subject,
            pid,
            brain_core::StatementKind::Fact,
            StatementObject::Value(StatementValue::Text("charity race".into())),
            None,
        );
        let v = project_statement_slot(&rtxn, &s, Slot::Subject, 0.9)
            .unwrap()
            .expect("subject slot projects to the subject entity name");
        assert_eq!(
            v.object,
            StatementObject::Value(StatementValue::Text("Melanie".into()))
        );
    }

    // ── Kind-anchored answer shaping ────────────────────────────────────────

    fn val(predicate: &str, object: &str) -> GroundedValue {
        GroundedValue {
            predicate: predicate.into(),
            object: StatementObject::Value(StatementValue::Text(object.into())),
            confidence: 1.0,
            source_memory: None,
            match_score: 0.7,
            recency: 0,
        }
    }

    fn behavior(card: KindCardinality, temporal: TemporalModel, polarity: bool) -> KindBehavior {
        KindBehavior::new(card, temporal, polarity)
    }

    #[test]
    fn attribute_single_returns_one_current_value() {
        // Attribute is Single/State: one current value even with agreeing
        // duplicate rows (supersession left the head as "now").
        let b = StatementKind::Attribute.builtin_behavior().unwrap();
        let a = shape_answer_for_kind(vec![val("brain:city", "Berlin")], b).unwrap();
        assert_eq!(a.kind, AnswerKind::Single);
        assert_eq!(a.values.len(), 1);
    }

    #[test]
    fn attribute_single_surfaces_contradiction_as_set() {
        // Two DISTINCT current values under a single-valued kind → contradiction,
        // surfaced as a Set of the competing claims.
        let b = StatementKind::Attribute.builtin_behavior().unwrap();
        let a = shape_answer_for_kind(
            vec![val("brain:city", "Berlin"), val("brain:city", "Paris")],
            b,
        )
        .unwrap();
        assert_eq!(a.kind, AnswerKind::Set);
        assert_eq!(a.values.len(), 2);
    }

    #[test]
    fn relation_set_enumerates_all_members() {
        // Relation is Set: distinct targets all survive so a downstream count works.
        let b = StatementKind::Relation.builtin_behavior().unwrap();
        let a = shape_answer_for_kind(
            vec![
                val("brain:knows", "Alice"),
                val("brain:knows", "Bob"),
                val("brain:knows", "Carol"),
            ],
            b,
        )
        .unwrap();
        assert_eq!(a.kind, AnswerKind::Set);
        assert_eq!(a.values.len(), 3, "all current members enumerated");
    }

    #[test]
    fn fact_set_dedups_true_duplicates() {
        // Same object twice = one member → Single (count 1); recency head kept.
        let b = StatementKind::Fact.builtin_behavior().unwrap();
        let a =
            shape_answer_for_kind(vec![val("brain:p", "same"), val("brain:p", "same")], b).unwrap();
        assert_eq!(a.kind, AnswerKind::Single);
        assert_eq!(a.values.len(), 1);
    }

    #[test]
    fn preference_polarity_splits_like_and_dislike() {
        // Preference carries polarity: "likes X" and "dislikes X" share the
        // object but differ by predicate → two members, never merged. A non-
        // polar kind with the same rows WOULD collapse them.
        let pref = StatementKind::Preference.builtin_behavior().unwrap();
        assert!(pref.polarity);
        let split = shape_answer_for_kind(
            vec![
                val("brain:likes", "coffee"),
                val("brain:dislikes", "coffee"),
            ],
            pref,
        )
        .unwrap();
        assert_eq!(split.kind, AnswerKind::Set);
        assert_eq!(
            split.values.len(),
            2,
            "polarity keeps like/dislike distinct"
        );

        // Same two rows under a non-polar Set kind collapse on object → one member.
        let nonpolar = behavior(KindCardinality::Set, TemporalModel::State, false);
        let merged = shape_answer_for_kind(
            vec![
                val("brain:likes", "coffee"),
                val("brain:dislikes", "coffee"),
            ],
            nonpolar,
        )
        .unwrap();
        assert_eq!(merged.values.len(), 1, "non-polar kind merges on object");
    }

    #[test]
    fn empty_values_is_none() {
        let b = StatementKind::Fact.builtin_behavior().unwrap();
        assert!(shape_answer_for_kind(Vec::new(), b).is_none());
    }

    #[test]
    fn slot_projection_time_refused_on_non_event_even_with_event_at() {
        // A non-Event kind must NEVER expose a Time role, even if the row happens
        // to carry an event_at — the gate is kind-driven, not populated-field-driven.
        let (_dir, db, _scope, subject, pid) = slot_projection_fixture();
        let rtxn = db.read_txn().unwrap();
        const T: u64 = 1_577_836_800_000_000_000;
        let s = statement_with(
            subject,
            pid,
            brain_core::StatementKind::Attribute,
            StatementObject::Value(StatementValue::Text("charity race".into())),
            Some(T),
        );
        assert!(
            project_statement_slot(&rtxn, &s, Slot::Time, 0.7)
                .unwrap()
                .is_none(),
            "a State/Attribute fact has no time role even with event_at set"
        );
    }

    /// A `VECTOR_DIM` unit vector with `1.0` at index `i`, `0.0` elsewhere — a
    /// controllable basis so cue↔embedding cosines are exact (equal index → 1.0,
    /// different index → 0.0).
    fn unit_at(i: usize) -> [f32; brain_embed::VECTOR_DIM] {
        let mut v = [0.0f32; brain_embed::VECTOR_DIM];
        v[i] = 1.0;
        v
    }

    #[test]
    fn walk_traverses_entity_object_statement_to_a_deeper_answer() {
        // A hop encoded as a STATEMENT (not a relations-table edge) must be
        // walkable: "what does X's friend do" needs X --friend(statement)--> Y,
        // then Y's occupation. The friend link is a Fact whose object is the
        // entity Y, so before this fix Y was unreachable and the walk returned
        // nothing. The anchor X itself has NO cue-matching predicate, so the ONLY
        // way to answer is by traversing the statement edge to Y and matching
        // Y's occupation there (depth 1).
        use brain_core::{Entity, EntityType};
        let dir = tempfile::tempdir().unwrap();
        let db = brain_metadata::MetadataDb::open(dir.path().join("m.redb")).unwrap();
        let scope = RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xB3; 16]);
        let x = EntityId::new();
        let y = EntityId::new();

        let cue = unit_at(0);
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
        // The friend predicate is ORTHOGONAL to the cue (cosine 0): it is a
        // walkable edge but never itself an answer, so the anchor produces none.
        let p_friend = brain_metadata::schema::predicate::predicate_intern_or_get(
            &wtxn, "test", "friend", 0, 1,
        )
        .unwrap();
        brain_metadata::schema::predicate::predicate_embedding_put(&wtxn, p_friend, &unit_at(1))
            .unwrap();
        // The occupation predicate EQUALS the cue (cosine 1.0): a strong answer
        // at Y that only the walk can reach.
        let p_occ = brain_metadata::schema::predicate::predicate_intern_or_get(
            &wtxn,
            "test",
            "occupation",
            0,
            1,
        )
        .unwrap();
        brain_metadata::schema::predicate::predicate_embedding_put(&wtxn, p_occ, &cue).unwrap();

        let s_friend = statement_with(
            x,
            p_friend,
            brain_core::StatementKind::Fact,
            StatementObject::Entity(y),
            None,
        );
        brain_metadata::statement::crud::statement_create(
            &wtxn,
            scope,
            brain_core::SessionId::DEFAULT,
            &s_friend,
            1,
        )
        .unwrap();
        let s_occ = statement_with(
            y,
            p_occ,
            brain_core::StatementKind::Fact,
            StatementObject::Value(StatementValue::Text("doctor".into())),
            None,
        );
        brain_metadata::statement::crud::statement_create(
            &wtxn,
            scope,
            brain_core::SessionId::DEFAULT,
            &s_occ,
            1,
        )
        .unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let (answer, depth) = grounded_answer_walk(&rtxn, scope, x, &cue).unwrap();
        assert_eq!(
            depth, 1,
            "the answer is found one statement-hop away from the anchor"
        );
        assert!(
            matches!(
                answer.values.first().map(|v| &v.object),
                Some(StatementObject::Value(StatementValue::Text(t))) if t == "doctor"
            ),
            "walking the entity-object statement reaches Y's occupation: {:?}",
            answer.values.first().map(|v| &v.object)
        );
    }

    #[test]
    fn walk_single_hop_answer_stays_at_the_anchor() {
        // A single-hop cue whose answer sits on the anchor must return depth 0
        // even now that statement edges are walkable — the deeper traversal must
        // never hijack a genuine single-hop answer.
        use brain_core::{Entity, EntityType};
        let dir = tempfile::tempdir().unwrap();
        let db = brain_metadata::MetadataDb::open(dir.path().join("m.redb")).unwrap();
        let scope = RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xB4; 16]);
        let x = EntityId::new();
        let cue = unit_at(0);
        let wtxn = db.write_txn().unwrap();
        brain_metadata::entity::ops::entity_put(
            &wtxn,
            scope,
            brain_core::SessionId::DEFAULT,
            &Entity::new_active(x, EntityType::PERSON_ID, "X".into(), "x".into(), 1),
        )
        .unwrap();
        let p_city =
            brain_metadata::schema::predicate::predicate_intern_or_get(&wtxn, "test", "city", 0, 1)
                .unwrap();
        brain_metadata::schema::predicate::predicate_embedding_put(&wtxn, p_city, &cue).unwrap();
        let s_city = statement_with(
            x,
            p_city,
            brain_core::StatementKind::Fact,
            StatementObject::Value(StatementValue::Text("Berlin".into())),
            None,
        );
        brain_metadata::statement::crud::statement_create(
            &wtxn,
            scope,
            brain_core::SessionId::DEFAULT,
            &s_city,
            1,
        )
        .unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let (answer, depth) = grounded_answer_walk(&rtxn, scope, x, &cue).unwrap();
        assert_eq!(depth, 0, "a single-hop answer is found at the anchor");
        assert!(matches!(
            answer.values.first().map(|v| &v.object),
            Some(StatementObject::Value(StatementValue::Text(t))) if t == "Berlin"
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
