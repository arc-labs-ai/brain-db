//! Entity resolver — the resolution algorithm plus the traits it
//! operates against.
//!
//! ## Dependency inversion
//!
//! brain-core can't depend on brain-metadata (cycle) or brain-embed or
//! brain-index. The resolver therefore takes three traits — provided
//! by the caller — and operates against them generically:
//!
//! - [`ResolverStorage`] — exact / alias / trigram lookups + type
//!   metadata. Implemented by brain-metadata for `MetadataDb`.
//! - [`ResolverEmbedder`] — text → 384-dim BGE vector. Implemented by
//!   brain-embed.
//! - [`ResolverIndex`] — entity HNSW top-k. Implemented by brain-index
//!   for `EntityHnswIndex`.
//!
//! The concrete trait impls are provided by the extractor layer that
//! invokes the resolver.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::ids::{AuditId, EntityId, EntityTypeId};
use crate::resolution::trigrams;

/// Dimension of the entity-embedding vector. 384 = BGE-small-en-v1.5
/// (matches the substrate). Hardcoded so trait signatures stay
/// concrete; no const-generic on `resolve_entity`.
pub const VECTOR_DIM: usize = 384;

/// Ambiguity-detection delta: when the top two candidates' scores are
/// within `δ` of each other (and both above threshold), the resolver
/// returns `Ambiguous` rather than picking arbitrarily. Tuned via a
/// future `ResolverConfig` knob.
const AMBIGUITY_DELTA: f32 = 0.05;

// ---------------------------------------------------------------------------
// ResolverTier.
// ---------------------------------------------------------------------------

/// Which tier of the resolver pipeline produced an outcome.
/// `Created` is a side-effect, not a tier in the strict
/// sense — included for completeness so audit records carry a
/// single enum.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(u8)]
pub enum ResolverTier {
    Exact = 0,
    Fuzzy = 1,
    Embedding = 2,
    Llm = 3,
    Created = 4,
}

impl ResolverTier {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Exact,
            1 => Self::Fuzzy,
            2 => Self::Embedding,
            3 => Self::Llm,
            4 => Self::Created,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// TypeConstraint.
// ---------------------------------------------------------------------------

/// How strictly the resolver honors the caller's `entity_type_hint`
///
/// - `Strict` — candidates must match the hint; cross-type matches
///   are not considered.
/// - `Hint` — prefer the hinted type; fall back across types if no
///   in-type match.
/// - `None` — ignore the hint entirely.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
pub enum TypeConstraint {
    Strict,
    /// Default per spec.
    #[default]
    Hint,
    None,
}

// ---------------------------------------------------------------------------
// ResolutionOutcome.
// ---------------------------------------------------------------------------

/// The three possible outcomes of a resolution call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ResolutionOutcome {
    /// Single high-confidence candidate found.
    Resolved {
        entity: EntityId,
        confidence: f32,
        tier: ResolverTier,
    },
    /// Multiple plausible candidates; resolution deferred for human
    /// or async-worker review. An audit record is written before
    /// returning this variant.
    Ambiguous {
        audit_id: AuditId,
        candidates: Vec<(EntityId, f32)>,
    },
    /// No match above threshold; a new entity was created.
    Created { entity: EntityId },
}

impl ResolutionOutcome {
    /// `true` for `Resolved` outcomes; `false` for `Ambiguous` and
    /// `Created`.
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        matches!(self, Self::Resolved { .. })
    }

    /// `true` for `Created` outcomes only.
    #[must_use]
    pub fn is_created(&self) -> bool {
        matches!(self, Self::Created { .. })
    }
}

// ---------------------------------------------------------------------------
// ResolverConfig.
// ---------------------------------------------------------------------------

/// Resolver configuration. Per-extractor overrides come from the
/// extractor layer.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolverConfig {
    pub enable_exact: bool,
    pub enable_fuzzy: bool,
    pub fuzzy_threshold: f32,
    pub enable_embedding: bool,
    pub embedding_threshold: f32,
    pub embedding_top_k: usize,
    pub enable_llm: bool,
    pub llm_threshold: f32,
    pub create_confidence: f32,
    pub type_constraint: TypeConstraint,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            enable_exact: true,
            enable_fuzzy: true,
            fuzzy_threshold: 0.85,
            enable_embedding: true,
            embedding_threshold: 0.78,
            embedding_top_k: 5,
            enable_llm: false,
            llm_threshold: 0.85,
            create_confidence: 0.6,
            type_constraint: TypeConstraint::Hint,
        }
    }
}

// ===========================================================================
// 16.5: ResolverError, traits, algorithm, mock-impl tests.
// ===========================================================================

/// Errors surfaced by the resolver pipeline.
///
/// Concrete impls in brain-metadata / brain-embed / brain-index
/// convert their native errors via `.to_string()`. Imperfect type
/// erasure, but keeps brain-core free of cross-crate dependencies.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum ResolverError {
    #[error("storage: {0}")]
    Storage(String),
    #[error("embedder: {0}")]
    Embedder(String),
    #[error("index: {0}")]
    Index(String),
    /// Tier-4 LLM disambiguation failure: transport error, timeout,
    /// schema-invalid response, etc. The impl crate (brain-extractors)
    /// converts brain-llm errors and parse failures into this string.
    #[error("llm: {0}")]
    Llm(String),
}

// ---------------------------------------------------------------------------
// Traits.
// ---------------------------------------------------------------------------

/// Read-side access to the entity registry. The resolver uses it for
/// tier-1 (exact + alias), tier-2 (trigram candidates + per-entity
/// trigrams), and the type-constraint filter on tier 3.
pub trait ResolverStorage {
    /// Tier-1 exact: `(type, normalized(candidate))` → at most one
    /// EntityId (the `entity_by_canonical_name` index is single-value).
    fn lookup_exact_canonical_name(
        &self,
        type_id: EntityTypeId,
        candidate: &str,
    ) -> Result<Option<EntityId>, ResolverError>;

    /// Tier-1 alias: `(type, normalized(candidate))` → zero or more
    /// EntityIds (alias index is multi-value).
    fn lookup_exact_aliases(
        &self,
        type_id: EntityTypeId,
        candidate: &str,
    ) -> Result<Vec<EntityId>, ResolverError>;

    /// Tier-2 candidate union: every EntityId whose trigram set
    /// shares ≥1 trigram with `query_normalized`'s trigrams.
    fn trigram_candidates(
        &self,
        type_id: EntityTypeId,
        query_normalized: &str,
    ) -> Result<HashSet<EntityId>, ResolverError>;

    /// Tier-2 per-candidate trigram set, for Jaccard scoring.
    fn trigrams_of(&self, id: EntityId) -> Result<HashSet<[u8; 3]>, ResolverError>;

    /// Tier-3 type-constraint filter: the entity's declared type.
    /// `Ok(None)` means "no such entity" — tombstoned or never
    /// existed.
    fn entity_type_of(&self, id: EntityId) -> Result<Option<EntityTypeId>, ResolverError>;
}

/// Tier-3 text → vector. Produces a 384-dim BGE-small L2-normalised
/// vector. Implemented by brain-embed.
pub trait ResolverEmbedder {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], ResolverError>;
}

/// Tier-3 HNSW top-k. Returns `(EntityId, similarity)` descending by
/// similarity. Tombstoned entries pre-filtered by the impl.
pub trait ResolverIndex {
    fn search(
        &self,
        query: &[f32; VECTOR_DIM],
        top_k: usize,
    ) -> Result<Vec<(EntityId, f32)>, ResolverError>;
}

/// Tier-4 LLM disambiguator. Given the original candidate string,
/// surrounding context, and a list of candidate (EntityId, score)
/// pairs from tiers 2-3, returns a decision.
///
/// Implementation lives outside brain-core (in brain-extractors)
/// because LLM transport is async; the impl handles the async-to-sync
/// boundary so the resolver itself stays sync.
pub trait ResolverLlm {
    fn disambiguate(
        &self,
        candidate: &str,
        context: &str,
        candidates: &[(EntityId, f32)],
    ) -> Result<ResolverLlmDecision, ResolverError>;
}

/// Outcome of a tier-4 LLM disambiguation call.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolverLlmDecision {
    /// Pick one of the provided candidates; the LLM's confidence in
    /// the choice in [0, 1].
    Pick { entity: EntityId, confidence: f32 },
    /// None of the candidates match — mint a new entity.
    None,
    /// LLM declines to pick — fall through to the ambiguity check
    /// (which surfaces the candidates to the caller).
    Ambiguous,
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Normalize for the resolver: trim + Unicode-aware lowercase +
/// whitespace collapse. Identical semantics to
/// `brain_metadata::normalize_name` but duplicated here so brain-core
/// stays dependency-free; both must produce the same output for a
/// given input (testing via concrete impl).
fn normalize_for_resolver(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Truncate `context` at the largest byte index ≤ `max_chars` that
/// falls on a Unicode codepoint boundary. Avoids slicing mid-codepoint
/// when constructing the tier-3 embedding input.
fn truncate_context_chars(context: &str, max_chars: usize) -> &str {
    if context.chars().count() <= max_chars {
        return context;
    }
    match context.char_indices().nth(max_chars) {
        Some((idx, _)) => &context[..idx],
        None => context,
    }
}

/// Apply `TypeConstraint` to a candidate. Returns `Ok(true)` if the
/// candidate passes the filter for the given hint + constraint.
fn passes_type_constraint<S>(
    storage: &S,
    id: EntityId,
    hint: Option<EntityTypeId>,
    constraint: TypeConstraint,
) -> Result<bool, ResolverError>
where
    S: ResolverStorage + ?Sized,
{
    match (constraint, hint) {
        (TypeConstraint::None, _) | (_, None) => Ok(true),
        (TypeConstraint::Hint, Some(_)) => {
            // Hint mode: prefer matching type but allow fallback.
            // For tier-3 filtering we don't reject; downstream
            // scoring may bias.
            Ok(true)
        }
        (TypeConstraint::Strict, Some(want)) => {
            match storage.entity_type_of(id)? {
                Some(got) => Ok(got == want),
                None => Ok(false), // Strict rejects missing-type
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Algorithm.
// ---------------------------------------------------------------------------

/// Resolve `candidate` to a [`ResolutionOutcome`].
///
///
/// 1. **Tier 1** (exact): exact canonical_name match → Resolved.
///    Alias hits: 1 → Resolved; multiple → carry forward as
///    candidates.
/// 2. **Tier 2** (fuzzy/trigram): Jaccard-scored candidates above
///    `fuzzy_threshold`. Single hit → Resolved. Multiple → carry.
/// 3. **Tier 3** (embedding): HNSW top-k filtered by type
///    constraint; single hit above `embedding_threshold` → Resolved.
/// 4. **Tier 4** (LLM): when `config.enable_llm` is set and an
///    `llm` impl is provided, the merged tier-2 / tier-3 candidate
///    pool (top 5 by score) is sent to the LLM disambiguator.
///    `Pick` above `llm_threshold` → `Resolved { tier: Llm }`;
///    `None` → `Created`; `Pick` below threshold or `Ambiguous` →
///    fall through to the ambiguity check.
/// 5. **Ambiguity check**: if tier-2 / tier-3 produced ≥2
///    candidates with top-two scores within `AMBIGUITY_DELTA` of
///    each other, return `Ambiguous`. Audit_id minted but NOT
///    persisted — caller writes the audit row if it wants one.
/// 6. **Tier 5** (Created): mint a fresh `EntityId` and return
///    `Created`. The caller persists via `entity_put`.
///
/// The `llm` argument is `None` when the caller hasn't wired the
/// LLM disambiguator (e.g., schemaless mode, or `enable_llm = false`
/// at config time). Passing `None` while `enable_llm = true` emits a
/// warn log and falls through (no panic, no error).
#[allow(clippy::too_many_arguments)]
pub fn resolve_entity<S, E, I>(
    storage: &S,
    embedder: &E,
    index: &I,
    candidate: &str,
    context: &str,
    entity_type_hint: Option<EntityTypeId>,
    config: &ResolverConfig,
    llm: Option<&dyn ResolverLlm>,
) -> Result<ResolutionOutcome, ResolverError>
where
    S: ResolverStorage + ?Sized,
    E: ResolverEmbedder + ?Sized,
    I: ResolverIndex + ?Sized,
{
    let normalized = normalize_for_resolver(candidate);

    // -------------------- Tier 1: Exact match ---------------------------
    let mut tier1_alias_pool: Vec<EntityId> = Vec::new();
    if config.enable_exact {
        if let Some(hint) = entity_type_hint {
            // 1a. Exact canonical_name.
            if let Some(id) = storage.lookup_exact_canonical_name(hint, &normalized)? {
                return Ok(ResolutionOutcome::Resolved {
                    entity: id,
                    confidence: 1.0,
                    tier: ResolverTier::Exact,
                });
            }
            // 1b. Aliases.
            let alias_hits = storage.lookup_exact_aliases(hint, &normalized)?;
            match alias_hits.len() {
                0 => {} // proceed to tier 2
                1 => {
                    return Ok(ResolutionOutcome::Resolved {
                        entity: alias_hits[0],
                        confidence: 1.0,
                        tier: ResolverTier::Exact,
                    });
                }
                _ => tier1_alias_pool = alias_hits,
            }
        }
        // If no type hint: skip tier 1 entirely. The exact-name index
        // is keyed by type; without a hint we can't query it
        // deterministically.
    }

    // -------------------- Tier 2: Fuzzy (trigram) -----------------------
    let mut tier2_scored: Vec<(EntityId, f32)> = Vec::new();
    if config.enable_fuzzy {
        let q_trigrams = trigrams::extract_trigrams(&normalized);
        if let (false, Some(hint)) = (q_trigrams.is_empty(), entity_type_hint) {
            let mut cands = storage.trigram_candidates(hint, &normalized)?;
            // Include the tier-1 alias pool — they're real matches we
            // want scored too.
            cands.extend(tier1_alias_pool.iter().copied());
            for cid in cands {
                if !passes_type_constraint(storage, cid, entity_type_hint, config.type_constraint)?
                {
                    continue;
                }
                let cid_trigrams = storage.trigrams_of(cid)?;
                let score = trigrams::jaccard(&q_trigrams, &cid_trigrams);
                if score > 0.0 {
                    tier2_scored.push((cid, score));
                }
            }
            tier2_scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            // "match fuzzy_hits.len { 1 if score ≥
            // threshold => Resolved, _ => keep for tier 3 }".
            // Interpret: at least one above threshold AND clear of
            // the runner-up by AMBIGUITY_DELTA → Resolved at Fuzzy
            // tier. Otherwise fall through.
            let above: Vec<&(EntityId, f32)> = tier2_scored
                .iter()
                .filter(|(_, s)| *s >= config.fuzzy_threshold)
                .collect();
            if above.len() == 1 {
                return Ok(ResolutionOutcome::Resolved {
                    entity: above[0].0,
                    confidence: above[0].1,
                    tier: ResolverTier::Fuzzy,
                });
            }
            if above.len() >= 2 {
                let top = above[0].1;
                let second = above[1].1;
                if (top - second) >= AMBIGUITY_DELTA {
                    return Ok(ResolutionOutcome::Resolved {
                        entity: above[0].0,
                        confidence: top,
                        tier: ResolverTier::Fuzzy,
                    });
                }
                // Otherwise top-2 are close — carry to tier-3 +
                // ambiguity check.
            }
        }
    }

    // -------------------- Tier 3: Embedding -----------------------------
    let mut tier3_scored: Vec<(EntityId, f32)> = Vec::new();
    if config.enable_embedding {
        let ctx = truncate_context_chars(context, 100);
        let text = if ctx.is_empty() {
            candidate.to_owned()
        } else {
            format!("{candidate} {ctx}")
        };
        let q_vec = embedder.embed(&text)?;
        let hits = index.search(&q_vec, config.embedding_top_k)?;
        for (cid, sim) in hits {
            if !passes_type_constraint(storage, cid, entity_type_hint, config.type_constraint)? {
                continue;
            }
            tier3_scored.push((cid, sim));
        }
        tier3_scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let above: Vec<&(EntityId, f32)> = tier3_scored
            .iter()
            .filter(|(_, s)| *s >= config.embedding_threshold)
            .collect();
        if above.len() == 1 {
            return Ok(ResolutionOutcome::Resolved {
                entity: above[0].0,
                confidence: above[0].1,
                tier: ResolverTier::Embedding,
            });
        }
        if above.len() >= 2 {
            let top = above[0].1;
            let second = above[1].1;
            if (top - second) >= AMBIGUITY_DELTA {
                return Ok(ResolutionOutcome::Resolved {
                    entity: above[0].0,
                    confidence: top,
                    tier: ResolverTier::Embedding,
                });
            }
            // top-2 close — carry to ambiguity check.
        }
    }

    // -------------------- Tier 4: LLM -----------------------------------
    if config.enable_llm {
        if let Some(llm) = llm {
            // Build the candidate pool from tier-2 + tier-3 above their
            // respective thresholds. Only invoke the LLM if 2+ candidates
            // are competitive (otherwise the lower tiers already
            // resolved).
            let mut llm_pool: Vec<(EntityId, f32)> = Vec::new();
            for (id, score) in tier2_scored
                .iter()
                .filter(|(_, s)| *s >= config.fuzzy_threshold)
            {
                llm_pool.push((*id, *score));
            }
            for (id, score) in tier3_scored
                .iter()
                .filter(|(_, s)| *s >= config.embedding_threshold)
            {
                // Dedupe — keep the higher score per id.
                if let Some(existing) = llm_pool.iter_mut().find(|(eid, _)| *eid == *id) {
                    if *score > existing.1 {
                        existing.1 = *score;
                    }
                } else {
                    llm_pool.push((*id, *score));
                }
            }
            llm_pool.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            // Cap at top 5 — context-window economy.
            const LLM_TOP_N: usize = 5;
            if llm_pool.len() > LLM_TOP_N {
                llm_pool.truncate(LLM_TOP_N);
            }

            if llm_pool.len() >= 2 {
                match llm.disambiguate(candidate, context, &llm_pool)? {
                    ResolverLlmDecision::Pick { entity, confidence }
                        if confidence >= config.llm_threshold =>
                    {
                        return Ok(ResolutionOutcome::Resolved {
                            entity,
                            confidence,
                            tier: ResolverTier::Llm,
                        });
                    }
                    ResolverLlmDecision::Pick { .. } => {
                        // Confidence below threshold — fall through to
                        // ambiguity check.
                    }
                    ResolverLlmDecision::None => {
                        // LLM declined every candidate; skip ambiguity
                        // and go straight to Create.
                        return Ok(ResolutionOutcome::Created {
                            entity: EntityId::new(),
                        });
                    }
                    ResolverLlmDecision::Ambiguous => {
                        // LLM explicitly said ambiguous; fall through.
                    }
                }
            }
        } else {
            tracing::warn!(
                "config.enable_llm = true but no ResolverLlm impl provided to resolve_entity"
            );
        }
    }

    // -------------------- Ambiguity check -------------------------------
    // Merge tier-2 + tier-3 candidates above their respective
    // thresholds; dedupe by EntityId keeping the higher score.
    let mut by_id: HashMap<EntityId, f32> = HashMap::new();
    for (cid, score) in tier2_scored
        .iter()
        .filter(|(_, s)| *s >= config.fuzzy_threshold)
    {
        let e = by_id.entry(*cid).or_insert(0.0);
        if *score > *e {
            *e = *score;
        }
    }
    for (cid, score) in tier3_scored
        .iter()
        .filter(|(_, s)| *s >= config.embedding_threshold)
    {
        let e = by_id.entry(*cid).or_insert(0.0);
        if *score > *e {
            *e = *score;
        }
    }
    let mut merged: Vec<(EntityId, f32)> = by_id.into_iter().collect();
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    if merged.len() >= 2 && (merged[0].1 - merged[1].1).abs() < AMBIGUITY_DELTA {
        return Ok(ResolutionOutcome::Ambiguous {
            audit_id: AuditId::new(),
            candidates: merged,
        });
    }

    // -------------------- Tier 5: Create --------------------------------
    Ok(ResolutionOutcome::Created {
        entity: EntityId::new(),
    })
}

// ---------------------------------------------------------------------------
// Tests with mock impls.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod algorithm_tests {
    use super::*;
    use std::cell::RefCell;

    /// In-memory mock for the three traits. Configured per test.
    #[derive(Default)]
    struct MockBackend {
        // Tier 1.
        canonical: RefCell<HashMap<(EntityTypeId, String), EntityId>>,
        aliases: RefCell<HashMap<(EntityTypeId, String), Vec<EntityId>>>,
        // Tier 2.
        trigram_candidates: RefCell<HashMap<(EntityTypeId, String), HashSet<EntityId>>>,
        trigrams: RefCell<HashMap<EntityId, HashSet<[u8; 3]>>>,
        // Tier 3.
        embeddings: RefCell<HashMap<String, [f32; VECTOR_DIM]>>,
        index_results: RefCell<Vec<(EntityId, f32)>>,
        // Type-constraint filter.
        types: RefCell<HashMap<EntityId, EntityTypeId>>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self::default()
        }
        fn set_canonical(&self, type_id: EntityTypeId, name: &str, id: EntityId) {
            self.canonical
                .borrow_mut()
                .insert((type_id, normalize_for_resolver(name)), id);
        }
        fn set_aliases(&self, type_id: EntityTypeId, alias: &str, ids: Vec<EntityId>) {
            self.aliases
                .borrow_mut()
                .insert((type_id, normalize_for_resolver(alias)), ids);
        }
        fn set_trigram_candidates(
            &self,
            type_id: EntityTypeId,
            query: &str,
            ids: HashSet<EntityId>,
        ) {
            self.trigram_candidates
                .borrow_mut()
                .insert((type_id, normalize_for_resolver(query)), ids);
        }
        fn set_trigrams_for(&self, id: EntityId, name: &str) {
            let tg = trigrams::extract_trigrams(&normalize_for_resolver(name));
            self.trigrams.borrow_mut().insert(id, tg);
        }
        fn set_embedding(&self, text: &str, v: [f32; VECTOR_DIM]) {
            self.embeddings.borrow_mut().insert(text.to_owned(), v);
        }
        fn set_index_results(&self, results: Vec<(EntityId, f32)>) {
            *self.index_results.borrow_mut() = results;
        }
        fn set_type(&self, id: EntityId, type_id: EntityTypeId) {
            self.types.borrow_mut().insert(id, type_id);
        }
    }

    impl ResolverStorage for MockBackend {
        fn lookup_exact_canonical_name(
            &self,
            type_id: EntityTypeId,
            candidate: &str,
        ) -> Result<Option<EntityId>, ResolverError> {
            Ok(self
                .canonical
                .borrow()
                .get(&(type_id, normalize_for_resolver(candidate)))
                .copied())
        }
        fn lookup_exact_aliases(
            &self,
            type_id: EntityTypeId,
            candidate: &str,
        ) -> Result<Vec<EntityId>, ResolverError> {
            Ok(self
                .aliases
                .borrow()
                .get(&(type_id, normalize_for_resolver(candidate)))
                .cloned()
                .unwrap_or_default())
        }
        fn trigram_candidates(
            &self,
            type_id: EntityTypeId,
            query_normalized: &str,
        ) -> Result<HashSet<EntityId>, ResolverError> {
            Ok(self
                .trigram_candidates
                .borrow()
                .get(&(type_id, query_normalized.to_owned()))
                .cloned()
                .unwrap_or_default())
        }
        fn trigrams_of(&self, id: EntityId) -> Result<HashSet<[u8; 3]>, ResolverError> {
            Ok(self.trigrams.borrow().get(&id).cloned().unwrap_or_default())
        }
        fn entity_type_of(&self, id: EntityId) -> Result<Option<EntityTypeId>, ResolverError> {
            Ok(self.types.borrow().get(&id).copied())
        }
    }

    impl ResolverEmbedder for MockBackend {
        fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], ResolverError> {
            self.embeddings
                .borrow()
                .get(text)
                .copied()
                .ok_or_else(|| ResolverError::Embedder(format!("no fixture for {text:?}")))
        }
    }

    impl ResolverIndex for MockBackend {
        fn search(
            &self,
            _query: &[f32; VECTOR_DIM],
            _top_k: usize,
        ) -> Result<Vec<(EntityId, f32)>, ResolverError> {
            Ok(self.index_results.borrow().clone())
        }
    }

    fn person() -> EntityTypeId {
        crate::nodes::entity::EntityType::PERSON_ID
    }

    // --- Tier 1 --------------------------------------------------------

    #[test]
    fn tier1_canonical_hit_resolves() {
        let m = MockBackend::new();
        let id = EntityId::new();
        m.set_canonical(person(), "Priya Patel", id);
        let out = resolve_entity(
            &m,
            &m,
            &m,
            "Priya Patel",
            "",
            Some(person()),
            &ResolverConfig::default(),
            None,
        )
        .unwrap();
        assert!(matches!(
            out,
            ResolutionOutcome::Resolved { entity, tier: ResolverTier::Exact, confidence }
            if entity == id && (confidence - 1.0).abs() < f32::EPSILON
        ));
    }

    #[test]
    fn tier1_single_alias_hit_resolves() {
        let m = MockBackend::new();
        let id = EntityId::new();
        m.set_aliases(person(), "priya", vec![id]);
        let out = resolve_entity(
            &m,
            &m,
            &m,
            "Priya",
            "",
            Some(person()),
            &ResolverConfig::default(),
            None,
        )
        .unwrap();
        assert!(matches!(
            out,
            ResolutionOutcome::Resolved { entity, tier: ResolverTier::Exact, .. }
            if entity == id
        ));
    }

    #[test]
    fn tier1_disabled_falls_through_to_tier5_when_nothing_else_set() {
        let m = MockBackend::new();
        let id = EntityId::new();
        m.set_canonical(person(), "Priya", id);
        let cfg = ResolverConfig {
            enable_exact: false,
            enable_fuzzy: false,
            enable_embedding: false,
            ..Default::default()
        };
        let out = resolve_entity(&m, &m, &m, "Priya", "", Some(person()), &cfg, None).unwrap();
        assert!(out.is_created(), "all tiers disabled → Created");
    }

    // --- Tier 2 --------------------------------------------------------

    #[test]
    fn tier2_single_high_jaccard_resolves() {
        // Jaccard for short names is brittle around the 0.85 default.
        // Use identical normalized text — the test demonstrates tier-2
        // resolves on a high-score single hit; the score-shape itself
        // is exercised by trigrams.rs's unit tests.
        let m = MockBackend::new();
        let id = EntityId::new();
        let q = normalize_for_resolver("Priya Patel");
        let cands: HashSet<EntityId> = [id].into_iter().collect();
        m.set_trigram_candidates(person(), &q, cands);
        m.set_trigrams_for(id, "Priya Patel");
        let out = resolve_entity(
            &m,
            &m,
            &m,
            "Priya Patel",
            "",
            Some(person()),
            &ResolverConfig::default(),
            None,
        )
        .unwrap();
        assert!(matches!(
            out,
            ResolutionOutcome::Resolved { tier: ResolverTier::Fuzzy, entity, .. } if entity == id
        ));
    }

    #[test]
    fn tier2_below_threshold_falls_through() {
        let m = MockBackend::new();
        let id = EntityId::new();
        let q = normalize_for_resolver("Priya Patel");
        m.set_trigram_candidates(person(), &q, [id].into_iter().collect());
        m.set_trigrams_for(id, "Totally Different"); // low Jaccard
                                                     // Disable tier 3 + tier 1 to isolate.
        let cfg = ResolverConfig {
            enable_embedding: false,
            ..Default::default()
        };
        let out =
            resolve_entity(&m, &m, &m, "Priya Patel", "", Some(person()), &cfg, None).unwrap();
        assert!(
            out.is_created(),
            "below-threshold fuzzy with no tier-3 → Created; got {out:?}"
        );
    }

    // --- Tier 3 --------------------------------------------------------

    #[test]
    fn tier3_single_above_threshold_resolves() {
        let m = MockBackend::new();
        let id = EntityId::new();
        m.set_embedding("Priya ctx", [0.5; VECTOR_DIM]);
        m.set_index_results(vec![(id, 0.95)]);
        m.set_type(id, person());

        let out = resolve_entity(
            &m,
            &m,
            &m,
            "Priya",
            "ctx",
            Some(person()),
            &ResolverConfig::default(),
            None,
        )
        .unwrap();
        assert!(matches!(
            out,
            ResolutionOutcome::Resolved { tier: ResolverTier::Embedding, entity, .. }
            if entity == id
        ));
    }

    #[test]
    fn tier3_below_threshold_falls_through() {
        let m = MockBackend::new();
        let id = EntityId::new();
        m.set_embedding("Priya ctx", [0.5; VECTOR_DIM]);
        m.set_index_results(vec![(id, 0.5)]); // below default 0.78
        m.set_type(id, person());

        let out = resolve_entity(
            &m,
            &m,
            &m,
            "Priya",
            "ctx",
            Some(person()),
            &ResolverConfig::default(),
            None,
        )
        .unwrap();
        assert!(out.is_created());
    }

    #[test]
    fn tier3_strict_type_constraint_filters_out_wrong_type() {
        let m = MockBackend::new();
        let cross = EntityId::new();
        m.set_embedding("Priya ctx", [0.5; VECTOR_DIM]);
        m.set_index_results(vec![(cross, 0.99)]);
        m.set_type(cross, EntityTypeId(7)); // not Person

        let cfg = ResolverConfig {
            type_constraint: TypeConstraint::Strict,
            ..Default::default()
        };
        let out = resolve_entity(&m, &m, &m, "Priya", "ctx", Some(person()), &cfg, None).unwrap();
        assert!(
            out.is_created(),
            "Strict + wrong type → no tier-3 match → Created; got {out:?}"
        );
    }

    // --- Ambiguity ----------------------------------------------------

    #[test]
    fn tier3_top_two_close_returns_ambiguous() {
        let m = MockBackend::new();
        let a = EntityId::new();
        let b = EntityId::new();
        m.set_embedding("Priya ctx", [0.5; VECTOR_DIM]);
        m.set_index_results(vec![(a, 0.90), (b, 0.89)]); // within 0.05 of each other
        m.set_type(a, person());
        m.set_type(b, person());

        let out = resolve_entity(
            &m,
            &m,
            &m,
            "Priya",
            "ctx",
            Some(person()),
            &ResolverConfig::default(),
            None,
        )
        .unwrap();
        match out {
            ResolutionOutcome::Ambiguous { candidates, .. } => {
                let ids: HashSet<EntityId> = candidates.iter().map(|(id, _)| *id).collect();
                assert!(ids.contains(&a));
                assert!(ids.contains(&b));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    // --- Tier 5 (create) ----------------------------------------------

    #[test]
    fn all_tiers_empty_creates_fresh_entity() {
        // When context is empty, the algorithm embeds just `candidate`
        // (no trailing space — see resolve_entity tier-3 branch).
        let m = MockBackend::new();
        m.set_embedding("X", [0.5; VECTOR_DIM]);
        m.set_index_results(vec![]);
        let out = resolve_entity(
            &m,
            &m,
            &m,
            "X",
            "",
            Some(person()),
            &ResolverConfig::default(),
            None,
        )
        .unwrap();
        match out {
            ResolutionOutcome::Created { entity } => {
                // Fresh UUIDv7 should not be the null EntityId.
                assert_ne!(entity, EntityId::from([0u8; 16]));
            }
            other => panic!("expected Created, got {other:?}"),
        }
    }

    // --- Tier 4 (LLM) -------------------------------------------------

    /// One recorded LLM-disambiguator invocation. Fields are
    /// inspectable for assertions; `dead_code` is fine because the
    /// recorder exists primarily for the call-count check.
    #[allow(dead_code)]
    #[derive(Debug, Clone)]
    struct MockLlmCall {
        candidate: String,
        context: String,
        candidates: Vec<(EntityId, f32)>,
    }

    /// In-memory disambiguator that records every call and returns
    /// a configured decision.
    struct MockLlm {
        decision: ResolverLlmDecision,
        calls: RefCell<Vec<MockLlmCall>>,
    }

    impl MockLlm {
        fn new(decision: ResolverLlmDecision) -> Self {
            Self {
                decision,
                calls: RefCell::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl ResolverLlm for MockLlm {
        fn disambiguate(
            &self,
            candidate: &str,
            context: &str,
            candidates: &[(EntityId, f32)],
        ) -> Result<ResolverLlmDecision, ResolverError> {
            self.calls.borrow_mut().push(MockLlmCall {
                candidate: candidate.to_owned(),
                context: context.to_owned(),
                candidates: candidates.to_vec(),
            });
            Ok(self.decision.clone())
        }
    }

    /// Helper: build a resolver scenario where tier-3 finds two close
    /// candidates (within `AMBIGUITY_DELTA`), so tier-4 + ambiguity
    /// have something to chew on.
    fn close_pair_scenario() -> (MockBackend, EntityId, EntityId) {
        let m = MockBackend::new();
        let a = EntityId::new();
        let b = EntityId::new();
        m.set_embedding("Priya ctx", [0.5; VECTOR_DIM]);
        m.set_index_results(vec![(a, 0.90), (b, 0.89)]);
        m.set_type(a, person());
        m.set_type(b, person());
        (m, a, b)
    }

    #[test]
    fn tier4_llm_pick_above_threshold_resolves() {
        let (m, a, _b) = close_pair_scenario();
        let llm = MockLlm::new(ResolverLlmDecision::Pick {
            entity: a,
            confidence: 0.92,
        });
        let cfg = ResolverConfig {
            enable_llm: true,
            ..Default::default()
        };

        let out =
            resolve_entity(&m, &m, &m, "Priya", "ctx", Some(person()), &cfg, Some(&llm)).unwrap();

        assert_eq!(llm.call_count(), 1, "LLM should be invoked exactly once");
        match out {
            ResolutionOutcome::Resolved {
                entity,
                tier,
                confidence,
            } => {
                assert_eq!(entity, a);
                assert_eq!(tier, ResolverTier::Llm);
                assert!((confidence - 0.92).abs() < f32::EPSILON);
            }
            other => panic!("expected Resolved at tier Llm, got {other:?}"),
        }
    }

    #[test]
    fn tier4_llm_pick_below_threshold_falls_through_to_ambiguity() {
        let (m, a, _b) = close_pair_scenario();
        let llm = MockLlm::new(ResolverLlmDecision::Pick {
            entity: a,
            confidence: 0.5, // below default 0.85 threshold
        });
        let cfg = ResolverConfig {
            enable_llm: true,
            ..Default::default()
        };

        let out =
            resolve_entity(&m, &m, &m, "Priya", "ctx", Some(person()), &cfg, Some(&llm)).unwrap();

        assert_eq!(llm.call_count(), 1);
        // Falls through to the ambiguity check; tier-3 candidates were
        // close so we get Ambiguous.
        assert!(
            matches!(out, ResolutionOutcome::Ambiguous { .. }),
            "expected Ambiguous, got {out:?}"
        );
    }

    #[test]
    fn tier4_llm_none_creates_new_entity() {
        let (m, _a, _b) = close_pair_scenario();
        let llm = MockLlm::new(ResolverLlmDecision::None);
        let cfg = ResolverConfig {
            enable_llm: true,
            ..Default::default()
        };

        let out =
            resolve_entity(&m, &m, &m, "Priya", "ctx", Some(person()), &cfg, Some(&llm)).unwrap();

        assert_eq!(llm.call_count(), 1);
        assert!(
            matches!(out, ResolutionOutcome::Created { .. }),
            "expected Created, got {out:?}"
        );
    }

    #[test]
    fn tier4_llm_ambiguous_returns_ambiguity() {
        let (m, _a, _b) = close_pair_scenario();
        let llm = MockLlm::new(ResolverLlmDecision::Ambiguous);
        let cfg = ResolverConfig {
            enable_llm: true,
            ..Default::default()
        };

        let out =
            resolve_entity(&m, &m, &m, "Priya", "ctx", Some(person()), &cfg, Some(&llm)).unwrap();

        assert_eq!(llm.call_count(), 1);
        // Falls through to ambiguity check, which fires because the
        // tier-3 scores were within AMBIGUITY_DELTA.
        assert!(
            matches!(out, ResolutionOutcome::Ambiguous { .. }),
            "expected Ambiguous, got {out:?}"
        );
    }

    #[test]
    fn tier4_disabled_skips_llm_call() {
        let (m, _a, _b) = close_pair_scenario();
        let llm = MockLlm::new(ResolverLlmDecision::Pick {
            entity: EntityId::new(),
            confidence: 0.99,
        });
        // enable_llm = false by default.
        let cfg = ResolverConfig::default();

        let _ =
            resolve_entity(&m, &m, &m, "Priya", "ctx", Some(person()), &cfg, Some(&llm)).unwrap();

        assert_eq!(llm.call_count(), 0, "LLM must not be invoked when disabled");
    }

    #[test]
    fn tier4_no_llm_provided_skips_with_warn() {
        let (m, _a, _b) = close_pair_scenario();
        let cfg = ResolverConfig {
            enable_llm: true,
            ..Default::default()
        };
        // Pass None despite enable_llm = true; must not panic.
        let out = resolve_entity(&m, &m, &m, "Priya", "ctx", Some(person()), &cfg, None).unwrap();
        // Falls through to ambiguity check; tier-3 was close → Ambiguous.
        assert!(
            matches!(out, ResolutionOutcome::Ambiguous { .. }),
            "expected Ambiguous fallthrough, got {out:?}"
        );
    }

    #[test]
    fn tier4_single_candidate_pool_skips_llm() {
        // Only one above-threshold candidate — tier-3 already
        // resolved before reaching tier-4. Sanity check that we never
        // pay LLM cost when there's nothing to disambiguate.
        let m = MockBackend::new();
        let id = EntityId::new();
        m.set_embedding("Priya ctx", [0.5; VECTOR_DIM]);
        m.set_index_results(vec![(id, 0.95)]);
        m.set_type(id, person());

        let llm = MockLlm::new(ResolverLlmDecision::Pick {
            entity: id,
            confidence: 0.99,
        });
        let cfg = ResolverConfig {
            enable_llm: true,
            ..Default::default()
        };
        let _ =
            resolve_entity(&m, &m, &m, "Priya", "ctx", Some(person()), &cfg, Some(&llm)).unwrap();
        assert_eq!(
            llm.call_count(),
            0,
            "Single-candidate path resolves at tier-3, LLM untouched"
        );
    }

    // --- Helpers ------------------------------------------------------

    #[test]
    fn normalize_for_resolver_matches_metadata_layer() {
        // Spot-check Unicode + whitespace collapse parity. brain-metadata's
        // normalize_name has the same semantics.
        assert_eq!(normalize_for_resolver("  Priya   Patel  "), "priya patel");
        assert_eq!(normalize_for_resolver("Straße"), "straße");
    }

    #[test]
    fn truncate_context_respects_unicode_boundaries() {
        let s = "αβγδεζηθικλμ"; // multi-byte each
        let truncated = truncate_context_chars(s, 5);
        assert!(truncated.chars().count() <= 5);
        // Must be a valid str slice (no codepoint splitting).
        assert_eq!(truncated.chars().count(), 5);
    }

    // ---------------------------------------------------------------------
    // Adversarial / fuzz-style input cases.
    //
    // Fuzz the resolver with adversarial inputs (Unicode, very long
    // strings, empty strings). These are hand-curated unit-level cases
    // that exercise the cleanup paths tier 1+2 take; the broader
    // cargo-fuzz integration lives in the protocol-fuzz suite.
    // ---------------------------------------------------------------------

    fn adv_vec_zeros() -> [f32; VECTOR_DIM] {
        [0.0; VECTOR_DIM]
    }

    /// Adversarial tests focus on tier 1 + 2 robustness. Tier 3
    /// (embedding) is configured-off so the mock embedder doesn't
    /// need fixtures for every weird input.
    fn adv_config() -> ResolverConfig {
        ResolverConfig {
            enable_embedding: false,
            ..ResolverConfig::default()
        }
    }

    #[test]
    fn empty_candidate_does_not_resolve_no_matches() {
        // Empty input — resolver returns Created (or NotFound when
        // create disabled). Either way: no panic, no OOB, single
        // outcome.
        let m = MockBackend::new();
        m.set_embedding("", adv_vec_zeros());
        let out = resolve_entity(&m, &m, &m, "", "", Some(person()), &adv_config(), None).unwrap();
        // Tier 1 yields no canonical / alias hit; tier 2 has no
        // trigrams (the empty-input split produces nothing); tier 3
        // is configured-out by default in this test path. Tier 5
        // (create) fires.
        assert!(matches!(out, ResolutionOutcome::Created { .. }));
    }

    #[test]
    fn whitespace_only_candidate_does_not_resolve_no_matches() {
        let m = MockBackend::new();
        m.set_embedding("", adv_vec_zeros());
        let out = resolve_entity(
            &m,
            &m,
            &m,
            "   \t  \n  ",
            "",
            Some(person()),
            &adv_config(),
            None,
        )
        .unwrap();
        assert!(matches!(out, ResolutionOutcome::Created { .. }));
    }

    #[test]
    fn very_long_candidate_does_not_panic() {
        // 64 KiB of identical chars — well past any practical name.
        // Resolver must process without OOM / panic.
        let huge = "a".repeat(64 * 1024);
        let m = MockBackend::new();
        m.set_embedding(huge.as_str(), adv_vec_zeros());
        let out =
            resolve_entity(&m, &m, &m, &huge, "", Some(person()), &adv_config(), None).unwrap();
        // No fixtures match; tier 5 fires.
        assert!(matches!(out, ResolutionOutcome::Created { .. }));
    }

    #[test]
    fn unicode_multibyte_candidate_normalises_correctly() {
        // Multi-byte CJK + Latin mix. Resolver normalisation is byte-
        // level pg_trgm; multi-byte windows may slice mid-codepoint
        // but the tier-1 path uses the whole normalized string so
        // round-trips correctly.
        let m = MockBackend::new();
        let id = EntityId::new();
        m.set_canonical(person(), "山田 太郎", id);
        m.set_embedding("山田 太郎", adv_vec_zeros());

        let out = resolve_entity(
            &m,
            &m,
            &m,
            "山田 太郎",
            "",
            Some(person()),
            &adv_config(),
            None,
        )
        .unwrap();
        match out {
            ResolutionOutcome::Resolved { entity, tier, .. } => {
                assert_eq!(entity, id);
                assert_eq!(tier, ResolverTier::Exact);
            }
            other => panic!("expected Resolved at tier 1, got {other:?}"),
        }
    }

    #[test]
    fn unicode_combining_marks_treated_byte_wise() {
        // "café" can be NFC (4 chars) or NFD ("cafe" + combining
        // acute = 5 chars). The resolver normalisation is byte-level
        // lowercase + whitespace-collapse only — does NOT apply NFKC.
        // So NFC and NFD forms are *different* entities for tier-1
        // purposes.
        //
        // Verifies we don't accidentally normalise unicode here; if
        // NFKC is added later, this test flips.
        let m = MockBackend::new();
        let nfc = "café"; // NFC: 4 chars / 5 bytes
        let nfd = "cafe\u{0301}"; // NFD: 5 chars / 6 bytes
        let id = EntityId::new();
        m.set_canonical(person(), nfc, id);
        m.set_embedding(nfd, adv_vec_zeros());

        let out = resolve_entity(&m, &m, &m, nfd, "", Some(person()), &adv_config(), None).unwrap();
        // NFD candidate doesn't tier-1 match the NFC stored entity;
        // tier 2 has no trigram fixture; tier 3 LLM disabled by
        // default; tier 5 creates a new entity.
        assert!(matches!(out, ResolutionOutcome::Created { .. }));
    }

    #[test]
    fn emoji_in_candidate_does_not_panic() {
        let m = MockBackend::new();
        m.set_embedding("🚀 rocket", adv_vec_zeros());
        let out = resolve_entity(
            &m,
            &m,
            &m,
            "🚀 rocket",
            "",
            Some(person()),
            &adv_config(),
            None,
        )
        .unwrap();
        // Emoji is a 4-byte codepoint; trigram windows slice mid-
        // codepoint. We just want "no panic" + a sane outcome.
        assert!(matches!(out, ResolutionOutcome::Created { .. }));
    }

    #[test]
    fn pathological_repeated_chars_clamps_trigram_set() {
        // "aaaaaaaaaa..." has very few unique trigrams; tier-2
        // candidates is empty unless something was stored with the
        // same pattern.
        let huge_a = "a".repeat(10_000);
        let m = MockBackend::new();
        m.set_embedding(huge_a.as_str(), adv_vec_zeros());
        let out =
            resolve_entity(&m, &m, &m, &huge_a, "", Some(person()), &adv_config(), None).unwrap();
        assert!(matches!(out, ResolutionOutcome::Created { .. }));
    }

    #[test]
    fn mixed_case_and_whitespace_normalised_for_tier1() {
        let m = MockBackend::new();
        let id = EntityId::new();
        m.set_canonical(person(), "Priya Patel", id);
        m.set_embedding("  PrIyA   PaTeL  ", adv_vec_zeros());

        let out = resolve_entity(
            &m,
            &m,
            &m,
            "  PrIyA   PaTeL  ",
            "",
            Some(person()),
            &adv_config(),
            None,
        )
        .unwrap();
        // Normalised form ("priya patel") matches tier-1 canonical.
        match out {
            ResolutionOutcome::Resolved { entity, tier, .. } => {
                assert_eq!(entity, id);
                assert_eq!(tier, ResolverTier::Exact);
            }
            other => panic!("expected Resolved at tier 1, got {other:?}"),
        }
    }

    #[test]
    fn tabs_and_newlines_normalised() {
        let m = MockBackend::new();
        let id = EntityId::new();
        m.set_canonical(person(), "Foo Bar", id);
        m.set_embedding("Foo\t\n  Bar", adv_vec_zeros());

        let out = resolve_entity(
            &m,
            &m,
            &m,
            "Foo\t\n  Bar",
            "",
            Some(person()),
            &adv_config(),
            None,
        )
        .unwrap();
        assert!(matches!(out, ResolutionOutcome::Resolved { .. }));
    }
}
