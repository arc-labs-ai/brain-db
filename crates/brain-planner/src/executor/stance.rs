//! Stance classification and derived claims for the REASON executor.
//!
//! Two jobs, both read-only:
//!
//! 1. **Stance.** A `ByText` observation resolves its base set by ANN
//!    similarity, and similarity is not agreement: "the OAuth blocker is
//!    fixed" sits right next to "linear-sandbox is blocked by the OAuth
//!    login flow" in embedding space while saying the opposite. Every
//!    base hit used to count as supporting evidence, which pinned the
//!    aggregate confidence at `1.0` and listed the refutation as support.
//!    [`classify`] sorts each hit into supports / contradicts / unrelated
//!    from (a) topical overlap with the observation and (b) the state
//!    polarity each text asserts (a blocked/failing state vs. a
//!    fixed/ready state). Recency then decides which side is stale:
//!    evidence older than the newest opposing evidence is damped by
//!    [`STALE_FACTOR`].
//!
//! 2. **Derived claims.** REASON used to echo the observation back as
//!    its only "inference". [`derive_claims`] draws claims outward from
//!    the typed graph instead: the statements the stance-bearing memories
//!    are evidence for, plus the current statements about those
//!    statements' subject entities. Each claim carries its own evidence,
//!    its own contradicting evidence (read-side Fact contradictions via
//!    `brain_metadata::statement::statements_contradicting`, newer
//!    opposite-polarity memories, closed `valid_to` / superseded rows) and
//!    its own confidence.
//!
//! The polarity lexicon is deliberately small and conservative: an
//! observation without a detectable state polarity falls back to plain
//! topical support, exactly as before, minus the unrelated hits.

use std::collections::{HashMap, HashSet};

use brain_core::{MemoryId, Statement, StatementId, StatementObject, SubjectRef};
use brain_metadata::schema::predicate::predicate_get;
use brain_metadata::statement::{
    read_evidence_ids, statement_get, statement_list, statements_contradicting, StatementListFilter,
};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::statement::STATEMENTS_BY_EVIDENCE_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_metadata::RowScope;

use super::analogical::{render_object, render_subject};

/// Multiplier applied to evidence that is older than the newest evidence
/// asserting the opposite state (or whose statement has been closed /
/// superseded). Halving rather than dropping keeps the item visible in
/// the result so a caller can still see what the old record said.
pub const STALE_FACTOR: f32 = 0.5;

/// Multiplier for a derived claim whose only contradictions are OLDER
/// than its own evidence — the claim is the newer record, so it wins, but
/// the disagreement still costs it a little certainty.
pub const OLDER_CONTRADICTION_FACTOR: f32 = 0.8;

/// ANN cosine at or above which one shared salient token is enough to
/// call a hit topical (paraphrases share few surface tokens).
const HIGH_SIM_ONE_TOKEN: f32 = 0.80;
/// ANN cosine at or above which a hit is topical with no token overlap.
const HIGH_SIM_NO_TOKEN: f32 = 0.90;

/// Per-subject cap on the outward statement fan-out.
const OUTWARD_STATEMENTS_PER_SUBJECT: usize = 64;
/// Cap on distinct subject entities the outward fan-out visits.
const OUTWARD_MAX_SUBJECTS: usize = 16;

// ---------------------------------------------------------------------------
// Text analysis.
// ---------------------------------------------------------------------------

/// The state a text asserts about its topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    /// No detectable blocked/fixed state.
    Neutral,
    /// A blocked / failing / not-ready state.
    Negative,
    /// A fixed / resolved / ready state.
    Resolved,
}

impl Polarity {
    fn opposes(self, other: Polarity) -> bool {
        matches!(
            (self, other),
            (Polarity::Negative, Polarity::Resolved) | (Polarity::Resolved, Polarity::Negative)
        )
    }
}

const RESOLUTION_WORDS: &[&str] = &[
    "fixed",
    "resolved",
    "unblocked",
    "solved",
    "restored",
    "ready",
    "completed",
    "finished",
    "shipped",
];

const NEGATIVE_WORDS: &[&str] = &[
    "blocked", "blocker", "blockers", "blocking", "broken", "failing", "fails", "failed", "stuck",
    "cannot", "can't", "cant", "unable",
];

/// Words that follow "still" to assert an unfinished / failing state.
const STILL_NEGATIVE: &[&str] = &[
    "building", "broken", "blocked", "failing", "pending", "waiting", "stuck",
];

const NEGATORS: &[&str] = &[
    "not", "never", "isn't", "wasn't", "hasn't", "aren't", "weren't", "haven't", "no",
];

/// Lowercased word tokens, apostrophes kept (so "can't" survives).
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\'').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// State polarity of `text`. When a text carries both kinds of cue the
/// LAST one wins: "the OAuth blocker is fixed and it is now ready" is a
/// resolution, "we fixed the reward; still blocked by OAuth" is not.
/// A negated cue flips ("not ready", "no longer blocked").
#[must_use]
pub fn text_polarity(text: &str) -> Polarity {
    let toks = words(text);
    let mut last = Polarity::Neutral;
    for (i, tok) in toks.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| toks[j].as_str());
        let negated = prev.is_some_and(|p| NEGATORS.contains(&p))
            || (prev == Some("longer") && i >= 2 && toks[i - 2] == "no")
            || (prev == Some("yet") && i >= 2 && NEGATORS.contains(&toks[i - 2].as_str()));
        if RESOLUTION_WORDS.contains(&tok.as_str()) {
            last = if negated {
                Polarity::Negative
            } else {
                Polarity::Resolved
            };
        } else if NEGATIVE_WORDS.contains(&tok.as_str()) {
            last = if negated {
                Polarity::Resolved
            } else {
                Polarity::Negative
            };
        } else if tok == "still"
            && toks
                .get(i + 1)
                .is_some_and(|n| STILL_NEGATIVE.contains(&n.as_str()))
        {
            last = Polarity::Negative;
        }
    }
    last
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "over", "was", "were", "are",
    "has", "have", "had", "its", "it's", "our", "you", "your", "they", "them", "their", "then",
    "than", "but", "not", "now", "still", "yet", "which", "what", "when", "where", "who", "will",
    "would", "should", "could", "can", "about", "also", "just", "very", "more", "much", "here",
    "there", "been", "being", "does", "did", "doing", "done", "get", "got", "one", "all", "any",
    "some", "only", "each", "per", "via", "out", "off", "again", "good", "news", "update",
    "status", "actually", "instead", "called", "named", "want", "need", "don't", "we're", "i'm",
    "longer", "never", "isn't", "wasn't", "hasn't", "aren't",
];

/// Topic-bearing tokens: split on every non-alphanumeric character (so
/// `linear-sandbox-8001` contributes `linear`, `sandbox`, `8001`),
/// lowercased, length ≥ 3, minus stopwords and polarity cue words (the
/// cue is the stance, not the topic).
#[must_use]
pub fn salient_tokens(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 3)
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .filter(|w| {
            !RESOLUTION_WORDS.contains(&w.as_str()) && !NEGATIVE_WORDS.contains(&w.as_str())
        })
        .collect()
}

/// A pre-analysed observation (or claim) the candidates are compared to.
#[derive(Clone, Debug)]
pub struct Profile {
    pub polarity: Polarity,
    pub tokens: HashSet<String>,
}

impl Profile {
    #[must_use]
    pub fn of(text: &str) -> Self {
        Self {
            polarity: text_polarity(text),
            tokens: salient_tokens(text),
        }
    }

    fn overlap(&self, other: &HashSet<String>) -> usize {
        self.tokens.intersection(other).count()
    }

    /// Whether a candidate with `tokens` (and ANN cosine `sim`, `0.0`
    /// when unknown) is about the same topic.
    fn topical(&self, tokens: &HashSet<String>, sim: f32) -> bool {
        if self.tokens.is_empty() {
            return false;
        }
        let overlap = self.overlap(tokens);
        let need = self.tokens.len().min(2);
        overlap >= need || (overlap >= 1 && sim >= HIGH_SIM_ONE_TOKEN) || sim >= HIGH_SIM_NO_TOKEN
    }
}

/// How a candidate memory relates to the observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stance {
    Supports,
    Contradicts,
    /// Off-topic, or on-topic without asserting the observation's state.
    Unrelated,
}

/// Classify one candidate text against the observation profile. `sim` is
/// the candidate's ANN cosine to the observation (`0.0` when unknown).
#[must_use]
pub fn classify(observation: &Profile, candidate_text: &str, sim: f32) -> Stance {
    let cand = Profile::of(candidate_text);
    if !observation.topical(&cand.tokens, sim) {
        return Stance::Unrelated;
    }
    match observation.polarity {
        // No state asserted: topical similarity is the support signal, as
        // before this module existed.
        Polarity::Neutral => Stance::Supports,
        obs if cand.polarity == obs => Stance::Supports,
        obs if obs.opposes(cand.polarity) => Stance::Contradicts,
        // On topic but silent about the asserted state ("Priya wants a
        // clone called linear-sandbox") — context, not support.
        _ => Stance::Unrelated,
    }
}

// ---------------------------------------------------------------------------
// Store helpers (all read through the caller's one read txn).
// ---------------------------------------------------------------------------

pub(crate) fn memory_text(rtxn: &redb::ReadTransaction, id: MemoryId) -> Option<String> {
    let table = rtxn.open_table(TEXTS_TABLE).ok()?;
    let guard = table.get(id.to_be_bytes()).ok()??;
    let s = std::str::from_utf8(guard.value()).ok()?;
    (!s.is_empty()).then(|| s.to_owned())
}

/// When the memory's event happened (`occurred_at`, falling back to the
/// record time) — the recency axis for stale-vs-current. `None` when the
/// row is missing or outside `scope`.
pub(crate) fn memory_time(
    rtxn: &redb::ReadTransaction,
    scope: RowScope,
    id: MemoryId,
) -> Option<u64> {
    let table = rtxn.open_table(MEMORIES_TABLE).ok()?;
    let guard = table.get(id.to_be_bytes()).ok()??;
    let row = guard.value();
    if row.namespace_id != scope.namespace_id || row.space_id_bytes != scope.space_id_bytes {
        return None;
    }
    Some(
        row.occurred_at_unix_nanos
            .filter(|t| *t > 0)
            .unwrap_or(row.created_at_unix_nanos),
    )
}

/// Live statements for which `memory` is evidence, via the scoped
/// `STATEMENTS_BY_EVIDENCE_TABLE` reverse index.
pub(crate) fn statements_for_memory(
    rtxn: &redb::ReadTransaction,
    scope: RowScope,
    memory: MemoryId,
) -> Vec<Statement> {
    let Ok(table) = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE) else {
        return Vec::new();
    };
    let mid = memory.to_be_bytes();
    let lo = (scope.namespace_id, scope.space_id_bytes, mid, [0u8; 16]);
    let hi = (scope.namespace_id, scope.space_id_bytes, mid, [0xFFu8; 16]);
    let Ok(range) = table.range(lo..=hi) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in range {
        let Ok((k, _)) = entry else { continue };
        let (_, _, _, sid) = k.value();
        if let Ok(Some(stmt)) = statement_get(rtxn, StatementId::from_bytes(sid)) {
            if !stmt.tombstoned {
                out.push(stmt);
            }
        }
    }
    out
}

/// In-scope evidence memories of `stmt`.
fn evidence_memories(
    rtxn: &redb::ReadTransaction,
    scope: RowScope,
    stmt: &Statement,
) -> Vec<MemoryId> {
    read_evidence_ids(rtxn, &stmt.evidence)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| memory_time(rtxn, scope, *m).is_some())
        .collect()
}

/// The substrate no longer believes `stmt` as current truth: it was
/// superseded, or its validity window has closed.
fn statement_is_closed(stmt: &Statement, now: u64) -> bool {
    stmt.superseded_by.is_some() || stmt.valid_to_unix_nanos.is_some_and(|t| t > 0 && t <= now)
}

/// Memories evidencing a live Fact that disagrees with `stmt` on the same
/// `(subject, predicate)` — the read-side contradiction check
/// (`statements_contradicting`). Empty for non-entity subjects.
fn conflicting_fact_memories(
    rtxn: &redb::ReadTransaction,
    scope: RowScope,
    stmt: &Statement,
) -> Vec<MemoryId> {
    let SubjectRef::Entity(subject) = stmt.subject else {
        return Vec::new();
    };
    let Ok(rivals) = statements_contradicting(rtxn, scope, subject, stmt.predicate) else {
        return Vec::new();
    };
    rivals
        .iter()
        .filter(|r| r.id != stmt.id && r.object != stmt.object)
        .flat_map(|r| evidence_memories(rtxn, scope, r))
        .collect()
}

// ---------------------------------------------------------------------------
// Base-set stance pass.
// ---------------------------------------------------------------------------

/// One base hit after stance classification.
#[derive(Clone, Debug)]
pub struct StancedItem {
    pub memory_id: MemoryId,
    pub score: f32,
    pub stance: Stance,
    pub time: u64,
    pub polarity: Polarity,
    pub tokens: HashSet<String>,
}

/// Classify every base hit against `observation`, then apply recency and
/// statement-supersession damping. Returns the supporting and
/// contradicting items (scores already damped); unrelated hits are
/// dropped. `now` is the wall clock used for `valid_to` checks.
#[must_use]
pub fn stance_pass(
    rtxn: &redb::ReadTransaction,
    scope: RowScope,
    observation: &Profile,
    base: &[(MemoryId, f32)],
    now: u64,
) -> (Vec<StancedItem>, Vec<StancedItem>) {
    let mut items: Vec<StancedItem> = Vec::with_capacity(base.len());
    for &(id, sim) in base {
        let Some(text) = memory_text(rtxn, id) else {
            continue;
        };
        let Some(time) = memory_time(rtxn, scope, id) else {
            continue;
        };
        let profile = Profile::of(&text);
        let stance = classify(observation, &text, sim);
        items.push(StancedItem {
            memory_id: id,
            score: sim.clamp(0.0, 1.0),
            stance,
            time,
            polarity: profile.polarity,
            tokens: profile.tokens,
        });
    }

    // Statement-level contradiction: a supporting memory whose statement
    // was superseded / closed, or whose Fact has a live rival with a
    // different object evidenced by a NEWER memory, is stale; the newer
    // rival memory is contradicting evidence even when ANN missed it.
    let mut extra_contra: Vec<StancedItem> = Vec::new();
    let known: HashSet<MemoryId> = items.iter().map(|i| i.memory_id).collect();
    for item in items.iter_mut().filter(|i| i.stance == Stance::Supports) {
        let mut stale = false;
        for stmt in statements_for_memory(rtxn, scope, item.memory_id) {
            if statement_is_closed(&stmt, now) {
                stale = true;
            }
            for rival in conflicting_fact_memories(rtxn, scope, &stmt) {
                let Some(t) = memory_time(rtxn, scope, rival) else {
                    continue;
                };
                if t <= item.time {
                    continue;
                }
                stale = true;
                if !known.contains(&rival) && !extra_contra.iter().any(|c| c.memory_id == rival) {
                    let tokens = memory_text(rtxn, rival)
                        .map(|t| salient_tokens(&t))
                        .unwrap_or_default();
                    extra_contra.push(StancedItem {
                        memory_id: rival,
                        score: item.score * stmt.confidence.clamp(0.0, 1.0),
                        stance: Stance::Contradicts,
                        time: t,
                        polarity: Polarity::Neutral,
                        tokens,
                    });
                }
            }
        }
        if stale {
            item.score *= STALE_FACTOR;
        }
    }

    let mut supporting: Vec<StancedItem> = Vec::new();
    let mut contradicting: Vec<StancedItem> = extra_contra;
    for item in items {
        match item.stance {
            Stance::Supports => supporting.push(item),
            Stance::Contradicts => contradicting.push(item),
            Stance::Unrelated => {}
        }
    }

    // Recency: whichever side is older than the newest opposing record is
    // the stale side. "Blocked" at t1 and "fixed" at t2 > t1 means the
    // block is history; the reverse order means the fix regressed.
    let newest_support = supporting.iter().map(|i| i.time).max();
    let newest_contra = contradicting.iter().map(|i| i.time).max();
    if let Some(nc) = newest_contra {
        for s in supporting.iter_mut().filter(|s| s.time < nc) {
            s.score *= STALE_FACTOR;
        }
    }
    if let Some(ns) = newest_support {
        for c in contradicting.iter_mut().filter(|c| c.time < ns) {
            c.score *= STALE_FACTOR;
        }
    }
    (supporting, contradicting)
}

// ---------------------------------------------------------------------------
// Derived claims.
// ---------------------------------------------------------------------------

/// A claim drawn outward from the typed graph.
#[derive(Clone, Debug)]
pub struct DerivedClaim {
    pub claim: String,
    /// `(memory, weight)` — evidence memories of the claim's statement(s).
    pub supporting: Vec<(MemoryId, f32)>,
    /// `(memory, weight)` — memories asserting a rival object or the
    /// opposite state more recently.
    pub contradicting: Vec<(MemoryId, f32)>,
    pub confidence: f32,
    /// Salient-token overlap with the observation (ranking key).
    relevance: usize,
}

/// Human-readable predicate: `blocked_by` → `blocked by`.
fn predicate_words(rtxn: &redb::ReadTransaction, stmt: &Statement) -> Option<String> {
    let p = predicate_get(rtxn, stmt.predicate).ok().flatten()?;
    Some(p.name.replace(['_', '-'], " "))
}

fn render_claim(rtxn: &redb::ReadTransaction, stmt: &Statement) -> Option<String> {
    // Memory / statement objects render as opaque ids — not a claim a
    // reader can use.
    if matches!(
        stmt.object,
        StatementObject::Memory(_) | StatementObject::Statement(_)
    ) {
        return None;
    }
    let subject = render_subject(rtxn, &stmt.subject)?;
    let predicate = predicate_words(rtxn, stmt)?;
    let object = render_object(rtxn, &stmt.object)?;
    Some(format!("{subject} {predicate} {object}"))
}

/// Inputs for [`derive_claims`].
pub struct ClaimSources<'a> {
    /// The observation the claims are drawn around.
    pub observation: &'a Profile,
    /// Memories whose statements seed the claims (the stance-bearing
    /// evidence: supports + contradicts).
    pub seed_memories: &'a [MemoryId],
    /// Every stance-classified memory with its time and polarity — used
    /// to find newer opposite-state records for a derived claim.
    pub stanced: &'a [StancedItem],
    pub now: u64,
    pub max_claims: usize,
}

/// Draw up to `max_claims` claims outward from the statements of
/// `seed_memories` and the current statements about their subjects.
/// Claims that merely restate the observation are skipped; the rest are
/// ranked by topical relevance to the observation, then confidence.
#[must_use]
pub fn derive_claims(
    rtxn: &redb::ReadTransaction,
    scope: RowScope,
    src: &ClaimSources<'_>,
) -> Vec<DerivedClaim> {
    if src.max_claims == 0 || src.seed_memories.is_empty() {
        return Vec::new();
    }

    // 1. Seed statements + their subject entities.
    let mut statements: Vec<Statement> = Vec::new();
    let mut seen: HashSet<StatementId> = HashSet::new();
    let mut subjects: Vec<brain_core::EntityId> = Vec::new();
    for &m in src.seed_memories {
        for stmt in statements_for_memory(rtxn, scope, m) {
            if let SubjectRef::Entity(e) = stmt.subject {
                if !subjects.contains(&e) {
                    subjects.push(e);
                }
            }
            if seen.insert(stmt.id) {
                statements.push(stmt);
            }
        }
    }
    // 2. Outward: the current statements about those subjects.
    for subject in subjects.into_iter().take(OUTWARD_MAX_SUBJECTS) {
        let filter = StatementListFilter {
            subject: Some(subject),
            current_only: true,
            limit: OUTWARD_STATEMENTS_PER_SUBJECT,
            ..StatementListFilter::default()
        };
        for stmt in statement_list(rtxn, scope, &filter).unwrap_or_default() {
            if !stmt.tombstoned && seen.insert(stmt.id) {
                statements.push(stmt);
            }
        }
    }

    // 3. Render, score, merge duplicates.
    let mut by_claim: HashMap<String, DerivedClaim> = HashMap::new();
    for stmt in &statements {
        let Some(claim) = render_claim(rtxn, stmt) else {
            continue;
        };
        let claim_profile = Profile::of(&claim);
        // Restating the observation is not an inference.
        if !claim_profile.tokens.is_empty() && claim_profile.tokens == src.observation.tokens {
            continue;
        }
        let relevance = src.observation.overlap(&claim_profile.tokens);
        if relevance == 0 {
            continue;
        }
        let evidence = evidence_memories(rtxn, scope, stmt);
        if evidence.is_empty() {
            continue;
        }
        let newest_evidence = evidence
            .iter()
            .filter_map(|m| memory_time(rtxn, scope, *m))
            .max()
            .unwrap_or(0);

        let mut contradicting: Vec<(MemoryId, f32)> = Vec::new();
        let mut newer_contra = false;
        let mut older_contra = false;
        for rival in conflicting_fact_memories(rtxn, scope, stmt) {
            let t = memory_time(rtxn, scope, rival).unwrap_or(0);
            if t > newest_evidence {
                newer_contra = true;
            } else {
                older_contra = true;
            }
            contradicting.push((rival, stmt.confidence));
        }
        // Opposite-state memories about the same topic.
        if claim_profile.polarity != Polarity::Neutral {
            for item in src.stanced {
                if evidence.contains(&item.memory_id)
                    || !claim_profile.polarity.opposes(item.polarity)
                    || !claim_profile.topical(&item.tokens, 0.0)
                {
                    continue;
                }
                if item.time > newest_evidence {
                    newer_contra = true;
                } else {
                    older_contra = true;
                }
                contradicting.push((item.memory_id, item.score));
            }
        }

        let mut confidence = stmt.confidence.clamp(0.0, 1.0);
        if statement_is_closed(stmt, src.now) || newer_contra {
            confidence *= STALE_FACTOR;
        } else if older_contra {
            confidence *= OLDER_CONTRADICTION_FACTOR;
        }

        let entry = by_claim
            .entry(claim.to_lowercase())
            .or_insert_with(|| DerivedClaim {
                claim: claim.clone(),
                supporting: Vec::new(),
                contradicting: Vec::new(),
                confidence,
                relevance,
            });
        entry.confidence = entry.confidence.max(confidence);
        for m in evidence {
            if !entry.supporting.iter().any(|(id, _)| *id == m) {
                entry.supporting.push((m, stmt.confidence));
            }
        }
        for (m, w) in contradicting {
            if !entry.contradicting.iter().any(|(id, _)| *id == m)
                && !entry.supporting.iter().any(|(id, _)| *id == m)
            {
                entry.contradicting.push((m, w));
            }
        }
    }

    let mut claims: Vec<DerivedClaim> = by_claim.into_values().collect();
    claims.sort_by(|a, b| {
        b.relevance
            .cmp(&a.relevance)
            .then(
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then_with(|| a.claim.cmp(&b.claim))
    });
    claims.truncate(src.max_claims);
    claims
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBS: &str = "linear-sandbox is blocked by the OAuth login flow";
    const SUPPORT: &str = "Status update: linear-sandbox is still building; it's blocked by the \
                           OAuth login flow, which the clone can't reproduce yet.";
    const REFUTE: &str = "Good news: the OAuth blocker is fixed and linear-sandbox is now ready.";
    const CONTEXT: &str = "Actually, switch linear-sandbox over to Playwright instead of \
                           Gymnasium; the browser-native actions matter more than we thought.";

    #[test]
    fn polarity_detects_blocked_and_resolved_states() {
        assert_eq!(text_polarity(OBS), Polarity::Negative);
        assert_eq!(text_polarity(SUPPORT), Polarity::Negative);
        // "blocker" appears, but the last cue ("ready") is a resolution.
        assert_eq!(text_polarity(REFUTE), Polarity::Resolved);
        assert_eq!(text_polarity(CONTEXT), Polarity::Neutral);
    }

    #[test]
    fn polarity_honours_negation_and_last_cue() {
        assert_eq!(text_polarity("the build is not ready"), Polarity::Negative);
        assert_eq!(text_polarity("it is no longer blocked"), Polarity::Resolved);
        assert_eq!(
            text_polarity("we fixed the reward bug; still blocked by OAuth"),
            Polarity::Negative
        );
    }

    #[test]
    fn salient_tokens_split_hyphenated_names_and_drop_cues() {
        let t = salient_tokens(OBS);
        for w in ["linear", "sandbox", "oauth", "login", "flow"] {
            assert!(t.contains(w), "{w} missing from {t:?}");
        }
        assert!(!t.contains("blocked"), "cue words are stance, not topic");
        assert!(!t.contains("the"));
    }

    #[test]
    fn classify_separates_support_refutation_and_context() {
        let obs = Profile::of(OBS);
        assert_eq!(classify(&obs, SUPPORT, 0.8), Stance::Supports);
        assert_eq!(classify(&obs, REFUTE, 0.8), Stance::Contradicts);
        // On topic (linear-sandbox) but silent on the blocked state.
        assert_eq!(classify(&obs, CONTEXT, 0.8), Stance::Unrelated);
        assert_eq!(
            classify(
                &obs,
                "The billing team holds a retro every second Friday.",
                0.3
            ),
            Stance::Unrelated
        );
    }

    #[test]
    fn neutral_observation_keeps_topical_support() {
        let obs = Profile::of("linear-sandbox uses Playwright");
        assert_eq!(classify(&obs, CONTEXT, 0.7), Stance::Supports);
    }
}
