//! Entity resolver used by the extractor pipeline worker.
//!
//! The extractor framework emits `EntityMention { entity_type_qname,
//! text, ... }` records before any persistence happens. The resolver
//! turns each surface form into a stable `EntityId` by walking a
//! gauntlet of lookup tiers:
//!
//! 1. **Exact** — normalize the surface form and look it up in the
//!    `(entity_type_id, normalized_name)` canonical-name index.
//! 2. **Alias** — look it up in the alias index keyed by the same
//!    normalized form.
//! 3. **Fuzzy (trigram + Jaccard)** — fetch trigram-overlap candidates
//!    from `entity_trigrams`, score them by Jaccard similarity over
//!    trigrams. If the best candidate's score exceeds
//!    [`DEFAULT_FUZZY_THRESHOLD`], add the surface form as an alias
//!    and return that EntityId.
//! 4. **Embedding (HNSW + cosine)** — when the caller wires an entity
//!    HNSW and an embedder, embed the surface form and ask the HNSW
//!    for the top-K nearest entities (`type_id`-filtered). If the top
//!    score is at or above [`EMBED_RESOLVE_THRESHOLD`], add the
//!    surface form as an alias and return that EntityId. This catches
//!    paraphrases trigrams miss (e.g. "Stripe Inc." vs
//!    "Stripe Payments").
//! 5. **Create** — mint a fresh UUIDv7 EntityId, intern the type if
//!    needed, embed the canonical name (when an HNSW is wired), write
//!    the entity row + the durable vector row, and STAGE the HNSW
//!    insert in a [`StagedEntityVectors`] the caller flushes after its
//!    write txn commits. Staged vectors are visible to the embedding
//!    tier for the rest of the pass, so subsequent resolves still
//!    short-circuit immediately; a worker-driven HNSW backfill isn't
//!    required because entity creation is rare relative to statement
//!    creation.
//!
//! Determinism comes from the lookup contract: given the same DB
//! state + same surface form, the resolver always returns the same
//! EntityId. Tier-5 creates use UUIDv7 (time + random), so two
//! independent resolves of the same brand-new surface form against
//! the same DB produce different IDs only if both observe a
//! tier-1/2/3/4 miss — which is the intended split-brain semantics
//! for two simultaneous extractions.
//!
//! The embedding threshold defaults to 0.78 cosine, carried on
//! [`EmbeddingDeps::embed_threshold`]; the shard ferries
//! `[extractors.resolver] embed_threshold` there. Callers that have no
//! HNSW or no embedder pass `None` for either and the tier silently
//! skips — the gauntlet still flows through tier-1/2/3/5 unchanged.

use std::collections::HashSet;
use std::sync::Arc;

use brain_core::resolution::trigrams;
use brain_core::MergeId;
use brain_core::{Entity, EntityId, EntityTypeId};
use brain_embed::Dispatcher;
use brain_index::entity_hnsw::EntityHnswIndex;
use brain_index::VECTOR_DIM;
use brain_llm::LlmClient;
use brain_metadata::entity::ops::{
    entity_add_alias, entity_put, entity_resolve_canonical_all_types_wtxn, entity_vector_put,
    normalize_name, EntityOpError,
};
use brain_metadata::entity::review::{enqueue_merge_proposal, MergeReviewError};
use brain_metadata::entity::trigram::TrigramOpError;
use brain_metadata::entity::types::{
    entity_type_intern, entity_type_lookup_by_name, EntityTypeOpError,
};
use brain_metadata::tables::entity::{
    EntityMetadata, ENTITIES_TABLE, ENTITY_ALIASES_TABLE, ENTITY_BY_CANONICAL_NAME_TABLE,
    ENTITY_TRIGRAMS_TABLE,
};
use brain_metadata::tables::merge_review_queue::proposal_tier;
use brain_metadata::RowScope;
use parking_lot::RwLock;
use redb::{ReadableTable, WriteTransaction};

/// Read-side projection of one candidate entity, snapshotted from
/// storage and handed to the disambiguator so it can build a useful
/// prompt. The disambiguator matches on `entity_id`.
#[derive(Debug, Clone)]
pub struct LlmCandidateView {
    pub entity_id: EntityId,
    pub canonical_name: String,
    pub aliases: Vec<String>,
    pub entity_type_name: String,
}

/// Jaccard floor for tier-3 fuzzy matching. Below this, the resolver
/// treats the candidate as a near-miss and skips it. Tuned conservatively
/// per the plan's "0.92" sketch — we use a lower 0.75 because trigram
/// Jaccard is a stricter signal than HNSW cosine for short names
/// (3-byte windows on a 5-character name yield only 3 trigrams; one
/// transposition halves Jaccard).
pub const DEFAULT_FUZZY_THRESHOLD: f32 = 0.75;

/// Default cosine floor for tier-3 embedding lookups. A surface form
/// whose top embedding-HNSW neighbour scores at or above this is
/// accepted as an alias of that entity. The 0.78 default tracks the
/// spec's "Tier 3 — embedding HNSW" guidance.
pub const EMBED_RESOLVE_THRESHOLD: f32 = 0.78;

/// Floor for the confidence-banded merge-review queue. A surface form
/// whose top embedding-HNSW neighbour scores in
/// `[PARTIAL_MATCH_FLOOR, EMBED_RESOLVE_THRESHOLD)` is treated as a
/// "close but not confident" near-miss — the resolver creates a fresh
/// entity for the new surface form and enqueues a `Pending`
/// `MergeReviewProposal` so the ambiguity-resolver worker can re-check
/// the pair as the entity HNSW grows.
///
/// Lower than the auto-alias threshold (0.78) and higher than the
/// floor below which the candidate is not even considered (0.7).
/// — "0.7 to 0.95 goes to review".
pub const PARTIAL_MATCH_FLOOR: f32 = 0.7;

/// Top-K asked of the entity HNSW during a tier-3 embedding probe.
/// 8 balances "enough candidates to break a near-tie" against the
/// cost of `entity_get`-ing each one for the type-filter pass.
const EMBED_RESOLVE_TOP_K: usize = 8;

/// Minimum length (in characters) of a single-token Person surface for
/// the diminutive / prefix coref tier to treat it as a nickname. Below
/// this ("Al", "Jo", "Ed") the shared prefix is too weak a signal — many
/// distinct names share a 2-character stem — so the resolver declines and
/// mints rather than risk merging distinct people.
const MIN_DIMINUTIVE_LEN: usize = 3;

/// Small closed set of leading greeting / vocative interjections stripped
/// from a Person surface before resolution. These are domain-general
/// English discourse markers that essentially never begin a real person
/// name — NOT a per-name list. Kept deliberately tight: words that could
/// plausibly begin a name ("well", "so", "man") are excluded.
const GREETING_TOKENS: &[&str] = &[
    "hey", "hi", "hiya", "heya", "hello", "hullo", "yo", "thanks", "thx", "thankyou", "yeah",
    "yea", "yep", "yup", "yes", "wow", "oh", "ohh", "ooh", "hmm", "hm", "uh", "um", "er", "ah",
    "please", "dear",
];

/// Timeout for the LLM disambiguation confirm call. The call is now
/// `.await`-ed off the shard reactor (in the worker's plan step, before
/// any write txn opens), so this bounds only one memory's own extraction
/// latency, not a shard-wide freeze — a natural value is fine. A hung
/// provider degrades to the merge/create fallback. See `ask_if_same_entity`.
const DISAMBIGUATOR_LLM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Outcome of one resolve attempt. The worker uses the tier to bump
/// per-tier counters on the pipeline audit row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionTier {
    Exact,
    Alias,
    Fuzzy,
    Embedding,
    /// A second-opinion check confirmed an ambiguous-band candidate as
    /// the same entity — the surface form was aliased onto the existing
    /// entity instead of minting a new one. The check happens after the
    /// embedding probe lands in the partial-match band; the backend is
    /// pluggable (LLM today; heuristics or classifier later) but the
    /// outcome shape is the same.
    Disambiguated,
    Created,
}

/// Successful resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub entity_id: EntityId,
    pub tier: ResolutionTier,
}

/// Errors the resolver can surface to the worker. Most are storage-level;
/// `EmptyNormalizedName` is the only logical one — extractors that emit
/// pure whitespace are dropped at the worker layer, not stored.
#[derive(thiserror::Error, Debug)]
pub enum ResolverError {
    #[error("surface form normalises to empty string")]
    EmptyNormalizedName,

    #[error("entity op: {0}")]
    EntityOp(#[from] EntityOpError),

    #[error("entity_type op: {0}")]
    EntityTypeOp(#[from] EntityTypeOpError),

    #[error("trigram op: {0}")]
    TrigramOp(#[from] TrigramOpError),

    #[error("merge-review queue: {0}")]
    MergeReview(#[from] MergeReviewError),

    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
}

/// Map `"brain:Person"` style qnames to a bare type name. Returns the
/// whole input if the colon is absent.
fn qname_to_type_name(qname: &str) -> &str {
    qname.split_once(':').map(|(_, n)| n).unwrap_or(qname)
}

/// True when the entity-type qname denotes a Person. Person is the only
/// type the greeting-strip and diminutive-coref guards touch: both are
/// person-name phenomena, and widening them risks mangling legitimate
/// org / product names ("Hello Fresh" the company, "Goog" → "Google").
fn surface_type_is_person(entity_type_qname: &str) -> bool {
    qname_to_type_name(entity_type_qname).eq_ignore_ascii_case("person")
}

/// Strip leading greeting / vocative tokens from `surface`, returning the
/// remainder when at least one was removed and something non-empty is
/// left. Returns `None` when the surface carries no leading greeting — so a
/// bare "Hey" (no following name) or a plain "Mel" is never altered, and
/// the caller keeps the original surface.
///
/// Extractors routinely tag conversational vocatives ("Hey Mel", "Thanks
/// Mel", "Yeah Mel") as standalone Person surfaces; each would otherwise
/// mint a distinct entity and fragment Mel's facts. Case-insensitive;
/// trailing punctuation on the greeting token ("Hey,") is tolerated; the
/// surviving name tokens keep their original casing.
///
/// ```
/// use brain_extractors::resolver::strip_leading_vocative;
/// assert_eq!(strip_leading_vocative("Hey Mel").as_deref(), Some("Mel"));
/// assert_eq!(strip_leading_vocative("Thanks Mel").as_deref(), Some("Mel"));
/// assert_eq!(strip_leading_vocative("Mel"), None);
/// ```
#[must_use]
pub fn strip_leading_vocative(surface: &str) -> Option<String> {
    let mut tokens: Vec<&str> = surface.split_whitespace().collect();
    let mut stripped = false;
    // Never reduce below one token: a greeting with no trailing name is not
    // a name at all, so leave it for the ordinary (empty-name) rejection.
    while tokens.len() >= 2 {
        let head_clean: String = tokens[0]
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        // Two-word "thank you <name>".
        if head_clean == "thank"
            && tokens.len() >= 3
            && tokens[1]
                .trim_matches(|c: char| !c.is_alphanumeric())
                .eq_ignore_ascii_case("you")
        {
            tokens.drain(0..2);
            stripped = true;
            continue;
        }
        if GREETING_TOKENS.contains(&head_clean.as_str()) {
            tokens.remove(0);
            stripped = true;
            continue;
        }
        break;
    }
    if stripped && !tokens.is_empty() {
        Some(tokens.join(" "))
    } else {
        None
    }
}

/// True when `surface` is, in its entirety, a date or relative-time phrase
/// — "Last Friday", "Last Fri", "yesterday", "next week", "3 days ago",
/// "in 2 weeks", "January 2026", "January 5, 2026", "2026", an ISO date —
/// and therefore names no entity. The extractor tiers call this to drop
/// temporal spans the LLM / classifier occasionally tag as Person / entity
/// surfaces; such a span belongs in the graph as an Event statement, never
/// as a node of its own.
///
/// Conservative on the axis that matters: it never rejects a BARE weekday
/// or month name ("Friday", "Sun", "May"), any of which can legitimately be
/// a person name. It rejects only forms that are unambiguously temporal — a
/// deictic, a relative lead ("last / this / next / …") before a weekday or
/// period, an explicit offset ("N units ago", "in N units"), or a calendar
/// date that pins a year.
///
/// ```
/// use brain_extractors::resolver::is_temporal_expression_surface;
/// assert!(is_temporal_expression_surface("Last Friday"));
/// assert!(is_temporal_expression_surface("yesterday"));
/// assert!(is_temporal_expression_surface("January 2026"));
/// assert!(!is_temporal_expression_surface("Melanie"));
/// assert!(!is_temporal_expression_surface("Friday")); // could be a name
/// assert!(!is_temporal_expression_surface("January")); // could be a name
/// ```
#[must_use]
pub fn is_temporal_expression_surface(surface: &str) -> bool {
    let lowered = surface.trim().to_lowercase();
    if lowered.is_empty() {
        return false;
    }
    const DEICTIC: &[&str] = &["yesterday", "today", "tomorrow", "tonight", "tonite"];
    const WEEKDAY: &[&str] = &[
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
        "mon",
        "tue",
        "tues",
        "wed",
        "weds",
        "thu",
        "thur",
        "thurs",
        "fri",
        "sat",
        "sun",
    ];
    const PERIOD: &[&str] = &["week", "weekend", "month", "year", "quarter"];
    const RELATIVE_LEAD: &[&str] = &[
        "last", "this", "next", "past", "coming", "previous", "upcoming",
    ];
    // Full calendar dates — "January 2026", "January 5, 2026", "8 May 2023",
    // "2020-01-15", a bare "2026". Delegated to the pattern tier's date
    // recognizers, the one authority on what parses as a date here, rather
    // than re-deriving the forms token-wise (the comma in "January 5, 2026"
    // alone defeats whitespace splitting). Whole-surface anchored there, so a
    // year embedded in a name ("Room 2026") still passes.
    if crate::pattern::temporal::is_full_date_surface(&lowered) {
        return true;
    }
    // Match ergonomics on `&[&str]` bind `one`/`unit`/… as `&&str`. `contains`
    // takes `&&str` directly; the `is_*` helpers take `&str` and get it via the
    // `&&str` -> `&str` deref coercion at the argument position.
    let tokens: Vec<&str> = lowered.split_whitespace().collect();
    match tokens.as_slice() {
        [one] => DEICTIC.contains(one) || is_iso_date_token(one),
        [lead, rest] => {
            RELATIVE_LEAD.contains(lead) && (WEEKDAY.contains(rest) || PERIOD.contains(rest))
        }
        [count, unit, "ago"] => count.chars().all(|c| c.is_ascii_digit()) && is_time_unit(unit),
        ["in", count, unit] => count.chars().all(|c| c.is_ascii_digit()) && is_time_unit(unit),
        _ => false,
    }
}

/// A single date/time counting unit, plural tolerated ("day", "weeks").
fn is_time_unit(tok: &str) -> bool {
    matches!(
        tok.strip_suffix('s').unwrap_or(tok),
        "day" | "week" | "weekend" | "month" | "year" | "hour" | "minute" | "quarter" | "decade"
    )
}

/// Whole-token ISO date test (`YYYY-MM-DD`), digits-and-hyphens only.
fn is_iso_date_token(tok: &str) -> bool {
    let b = tok.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

/// Look up (or auto-intern) the [`EntityTypeId`] for `qname`. New
/// types get an empty schema blob — they're flagged as `ImplicitFromWrite`
/// from the registry's standpoint (the bootstrap seeds Person at id=1
/// so the common case never enters intern).
fn resolve_entity_type(
    wtxn: &WriteTransaction,
    qname: &str,
    now_unix_nanos: u64,
) -> Result<EntityTypeId, ResolverError> {
    let name = qname_to_type_name(qname);
    if let Some(def) = entity_type_lookup_by_name(wtxn, name)? {
        return Ok(def.id());
    }
    Ok(entity_type_intern(wtxn, name, Vec::new(), now_unix_nanos)?)
}

/// Fetch the trigram set for `entity_id`'s canonical_name + aliases.
/// Returns an empty set when the entity has no primary row (caller
/// can then skip the candidate without aborting).
fn trigram_set_for_entity(
    wtxn: &WriteTransaction,
    entity_id: EntityId,
) -> Result<HashSet<[u8; 3]>, ResolverError> {
    let t = wtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&entity_id.to_bytes())?.map(|g| g.value());
    let Some(row) = row else {
        return Ok(HashSet::new());
    };
    let mut out = trigrams::extract_trigrams(&normalize_name(&row.canonical_name));
    for alias in &row.aliases {
        out.extend(trigrams::extract_trigrams(&normalize_name(alias)));
    }
    Ok(out)
}

/// Wtxn-friendly mirror of `entity_lookup_by_canonical_name`. The
/// public op takes a `ReadTransaction`; we resolve inside the caller's
/// write txn so the resolve + downstream writes commit atomically.
fn lookup_canonical_wtxn(
    wtxn: &WriteTransaction,
    scope: RowScope,
    type_id: EntityTypeId,
    normalized: &str,
) -> Result<Option<EntityId>, ResolverError> {
    let t = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
    let bytes: Option<[u8; 16]> = t
        .get(&(
            scope.namespace_id,
            scope.space_id_bytes,
            type_id.raw(),
            normalized,
        ))?
        .map(|g| g.value());
    Ok(bytes.map(EntityId::from))
}

/// Wtxn-friendly mirror of `entity_lookup_by_alias`.
fn lookup_alias_wtxn(
    wtxn: &WriteTransaction,
    scope: RowScope,
    type_id: EntityTypeId,
    normalized: &str,
) -> Result<Vec<EntityId>, ResolverError> {
    let t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
    let lo = (
        scope.namespace_id,
        scope.space_id_bytes,
        type_id.raw(),
        normalized,
        [0u8; 16],
    );
    let hi = (
        scope.namespace_id,
        scope.space_id_bytes,
        type_id.raw(),
        normalized,
        [0xFFu8; 16],
    );
    let mut out = Vec::new();
    for entry in t.range(lo..=hi)? {
        let (k, _) = entry?;
        let (k_ns, k_space, k_type, k_alias, k_id) = k.value();
        if k_ns == scope.namespace_id
            && k_space == scope.space_id_bytes
            && k_type == type_id.raw()
            && k_alias == normalized
        {
            out.push(EntityId::from(k_id));
        }
    }
    Ok(out)
}

/// Wtxn-friendly mirror of `candidates_for_query`.
fn trigram_candidates_wtxn(
    wtxn: &WriteTransaction,
    scope: RowScope,
    type_id: EntityTypeId,
    normalized: &str,
) -> Result<HashSet<EntityId>, ResolverError> {
    let qg = trigrams::extract_trigrams(normalized);
    let mut out = HashSet::new();
    if qg.is_empty() {
        return Ok(out);
    }
    let t = wtxn.open_table(ENTITY_TRIGRAMS_TABLE)?;
    for tg in qg {
        let lo = (
            scope.namespace_id,
            scope.space_id_bytes,
            type_id.raw(),
            tg,
            [0u8; 16],
        );
        let hi = (
            scope.namespace_id,
            scope.space_id_bytes,
            type_id.raw(),
            tg,
            [0xFFu8; 16],
        );
        for entry in t.range(lo..=hi)? {
            let (k, _) = entry?;
            let (k_ns, k_space, k_type, k_tg, k_id) = k.value();
            if k_ns == scope.namespace_id
                && k_space == scope.space_id_bytes
                && k_type == type_id.raw()
                && k_tg == tg
            {
                out.insert(EntityId::from(k_id));
            }
        }
    }
    Ok(out)
}

/// Embedding-tier handles bundled together. Callers wire both or
/// neither: an HNSW without an embedder can't be queried, and an
/// embedder without an HNSW has nowhere to send the vector. `None`
/// (the caller's choice) makes the resolver skip tier-3b cleanly
/// and the gauntlet runs as the 1/2/3a/4 flow.
#[derive(Clone)]
pub struct EmbeddingDeps {
    pub hnsw: Arc<RwLock<EntityHnswIndex>>,
    pub embedder: Arc<dyn Dispatcher>,
    /// Cosine floor for tier-3 auto-aliasing. Surface forms whose top
    /// neighbour scores at or above this are aliased onto that entity;
    /// scores in `[PARTIAL_MATCH_FLOOR, embed_threshold)` enter the
    /// merge-review queue. Defaults to [`EMBED_RESOLVE_THRESHOLD`] —
    /// the shard ferries `[extractors.resolver] embed_threshold` here.
    pub embed_threshold: f32,
}

impl std::fmt::Debug for EmbeddingDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingDeps").finish_non_exhaustive()
    }
}

/// Entity-HNSW insertions produced by one resolver pass, held back until
/// that pass's write transaction commits.
///
/// The entity HNSW is in-RAM and NOT transactional, and `hnsw_rs` has no
/// point removal — an insert made while a write txn is open can never be
/// taken back if that txn rolls back, leaving a vector that points at an
/// entity id redb never kept. Rollback is a live path, not a theoretical
/// one: the extractor's two-phase disambiguation discards its plan pass
/// whenever an ambiguous candidate needs an LLM verdict, and any error
/// out of an apply body drops the txn too. So tier-4 stages here and the
/// caller flushes with [`flush_into_hnsw`](Self::flush_into_hnsw) after
/// `commit()` returns; a rolled-back pass just drops the staging area.
///
/// Staging is per-pass and must never be shared between passes — that is
/// exactly what makes "drop it" a correct rollback.
///
/// Within a pass the staged vectors stay visible to the embedding tier
/// (see [`tier_embedding`]), so two paraphrases inside one memory still
/// collapse onto a single entity, as they did when the insert was inline.
///
/// ```no_run
/// # use brain_extractors::resolver::{
/// #     resolve_or_create_with_deps, Disambiguation, EmbeddingDeps, StagedEntityVectors,
/// # };
/// # use brain_metadata::RowScope;
/// # fn demo(wtxn: redb::WriteTransaction, scope: RowScope, deps: &EmbeddingDeps, now: u64) {
/// let mut staged = StagedEntityVectors::new();
/// let res = resolve_or_create_with_deps(
///     &wtxn,
///     scope,
///     "Stripe Payments",
///     "brain:Organization",
///     0.9,
///     now,
///     Some(deps),
///     &mut staged,
///     &mut Disambiguation::Off,
/// );
/// if wtxn.commit().is_ok() {
///     // Durable now — safe to publish the vectors to the in-RAM index.
///     staged.flush_into_hnsw(deps);
/// }
/// # let _ = res;
/// # }
/// ```
#[derive(Debug, Default)]
pub struct StagedEntityVectors(Vec<(EntityId, [f32; VECTOR_DIM])>);

impl StagedEntityVectors {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Publish every staged vector into the entity HNSW. Call ONLY once
    /// the transaction that wrote the corresponding entity rows has
    /// committed. Returns the number of points actually inserted.
    ///
    /// Consumes `self` so a staging area cannot be flushed twice, and
    /// skips ids the index already carries, so an entity can never end up
    /// with two points (the `(id, vector)` pair is also idempotent — the
    /// same entity staged twice inserts once).
    pub fn flush_into_hnsw(self, deps: &EmbeddingDeps) -> usize {
        if self.0.is_empty() {
            return 0;
        }
        let mut hnsw = deps.hnsw.write();
        let mut inserted = 0usize;
        for (entity_id, vector) in self.0 {
            if hnsw.contains(entity_id) {
                continue;
            }
            match hnsw.insert(entity_id, &vector) {
                Ok(()) => inserted += 1,
                Err(e) => tracing::warn!(
                    target: "brain_extractors::resolver",
                    ?entity_id,
                    error = %e,
                    "entity-HNSW insert failed; entity is durable but unreachable via tier-3b until a rebuild",
                ),
            }
        }
        inserted
    }

    fn stage(&mut self, entity_id: EntityId, vector: [f32; VECTOR_DIM]) {
        self.0.push((entity_id, vector));
    }

    /// Cosine-score every staged vector against `query` and return the
    /// best `k`, descending. Brute force is right here: a staging area
    /// holds the handful of entities one memory minted, and an exact scan
    /// avoids the approximate index entirely for them.
    fn probe(&self, query: &[f32; VECTOR_DIM], k: usize) -> Vec<(EntityId, f32)> {
        let mut scored: Vec<(EntityId, f32)> = self
            .0
            .iter()
            .map(|(id, v)| (*id, cosine(query, v)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }
}

/// Cosine similarity of two equal-length vectors. The embedder returns
/// L2-normalised output so this is usually just the dot product, but the
/// normalisation is cheap and keeps the score honest for any dispatcher.
fn cosine(a: &[f32; VECTOR_DIM], b: &[f32; VECTOR_DIM]) -> f32 {
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for i in 0..VECTOR_DIM {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Per-shard handle to a disambiguator capable of distinguishing
/// near-duplicate entities at resolution time.
///
/// When the embedding tier returns a candidate in the ambiguous band
/// (`[PARTIAL_MATCH_FLOOR, EMBED_RESOLVE_THRESHOLD)`), the resolver
/// asks the disambiguator whether the surface form is the same entity,
/// a different one, or genuinely unclear. The verdict decides whether
/// to alias onto the candidate, mint a fresh entity, or fall back to
/// the existing merge-proposal flow.
///
/// The current backend is LLM-driven: the disambiguator owns an
/// [`LlmClient`] + model identifier and issues a single yes/no/uncertain
/// prompt per ambiguous partial match. The prompt grammar is narrower
/// than the multi-candidate one [`crate::BrainLlmDisambiguator`] uses because
/// the resolver's question is binary. Swapping in a heuristic or
/// classifier backend later is a localised change to
/// `confirm_partial_match`.
pub struct EntityDisambiguator {
    client: Arc<dyn LlmClient>,
    model: String,
    /// Confidence floor for accepting a [`MatchVerdict::Confirmed`].
    /// Below this, the resolver treats the verdict as
    /// [`MatchVerdict::Uncertain`] and falls through to Create.
    pub min_confidence: f32,
}

/// Default floor for accepting a confirmed match. Mirrors the
/// brain-core resolver-config `llm_threshold` default so an operator
/// who tightens one expects the other to follow.
pub const DEFAULT_DISAMBIGUATOR_MIN_CONFIDENCE: f32 = 0.85;

impl EntityDisambiguator {
    /// Construct from an LLM client + model identifier. The default
    /// [`min_confidence`](Self::min_confidence) is
    /// [`DEFAULT_DISAMBIGUATOR_MIN_CONFIDENCE`]; use
    /// [`with_min_confidence`](Self::with_min_confidence) to tighten or
    /// loosen.
    #[must_use]
    pub fn new(client: Arc<dyn LlmClient>, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
            min_confidence: DEFAULT_DISAMBIGUATOR_MIN_CONFIDENCE,
        }
    }

    /// Override the confidence floor. Returns `self` for builder-style
    /// configuration at construction time.
    #[must_use]
    pub fn with_min_confidence(mut self, min_confidence: f32) -> Self {
        self.min_confidence = min_confidence;
        self
    }

    /// Ask the LLM whether `surface_form` refers to the same real-world
    /// entity as `view`. This is `.await`-ed OFF the shard reactor by the
    /// extractor worker — in the disambiguation *plan* step, before any
    /// write transaction is open — so, unlike the old in-txn `block_on`,
    /// it never parks the shard core. The resulting [`MatchVerdict`] is
    /// stashed in a [`PrecomputedVerdicts`] map the apply step consults
    /// synchronously. Any soft failure (transport error, unparseable
    /// reply) degrades to [`MatchVerdict::Skipped`], which the resolver
    /// treats as "fall through to the no-disambiguator path".
    pub async fn confirm(&self, view: &LlmCandidateView, surface_form: &str) -> MatchVerdict {
        let candidate = view.entity_id;
        match ask_if_same_entity(self, view, surface_form).await {
            Ok(SameEntityReply::Yes(confidence)) => {
                if confidence >= self.min_confidence {
                    MatchVerdict::Confirmed {
                        entity: candidate,
                        confidence,
                    }
                } else {
                    MatchVerdict::Uncertain
                }
            }
            Ok(SameEntityReply::No) => MatchVerdict::Rejected,
            Ok(SameEntityReply::Uncertain) => MatchVerdict::Uncertain,
            Err(reason) => MatchVerdict::Skipped { reason },
        }
    }
}

impl std::fmt::Debug for EntityDisambiguator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntityDisambiguator")
            .field("model", &self.model)
            .field("min_confidence", &self.min_confidence)
            .finish_non_exhaustive()
    }
}

/// What the disambiguator concluded about a single ambiguous-band
/// candidate. The resolver's question is a binary one — "is this
/// candidate the same entity as the surface form?" — and the four
/// variants spell out how the resolver should act on the answer.
#[derive(Debug, Clone, PartialEq)]
pub enum MatchVerdict {
    /// The surface form refers to the same entity as the candidate.
    /// The resolver aliases the surface form onto `entity` and skips
    /// the merge-proposal enqueue (no ambiguity left to review).
    Confirmed { entity: EntityId, confidence: f32 },
    /// The candidate is a different entity. The resolver mints a fresh
    /// entity and skips the merge-proposal enqueue — the two are
    /// confirmed distinct, no review needed.
    Rejected,
    /// The disambiguator declined to commit either way. The resolver
    /// proceeds with its existing fallback: mint a fresh entity and
    /// enqueue a Pending merge proposal so the ambiguity-resolver
    /// worker can re-check the pair later.
    Uncertain,
    /// The disambiguator was not invoked — either no backend was wired
    /// or a soft failure occurred while preparing the candidate view.
    /// Treated identically to [`Uncertain`](Self::Uncertain); carries a
    /// short reason for log correlation.
    Skipped { reason: String },
}

/// One ambiguous-band candidate discovered during the disambiguation
/// *plan* pass. The worker awaits an LLM verdict for each of these
/// off the reactor, then re-runs resolution with the verdicts in hand.
#[derive(Debug, Clone)]
pub struct PendingVerdict {
    /// Normalized surface form — the map key the apply pass looks up.
    pub norm_surface: String,
    /// Raw surface form — fed to the LLM prompt verbatim.
    pub raw_surface: String,
    /// The candidate entity the embedding tier proposed.
    pub candidate: EntityId,
    /// Snapshot of the candidate for the LLM prompt.
    pub view: LlmCandidateView,
}

/// Disambiguation verdicts computed off-reactor and consulted
/// synchronously by the apply pass. Keyed by `(normalized surface,
/// candidate)` — the exact pair the resolver's embedding tier lands on.
#[derive(Debug, Default)]
pub struct PrecomputedVerdicts(std::collections::HashMap<(String, EntityId), MatchVerdict>);

impl PrecomputedVerdicts {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the verdict for one `(normalized surface, candidate)` pair.
    pub fn insert(&mut self, norm_surface: String, candidate: EntityId, verdict: MatchVerdict) {
        self.0.insert((norm_surface, candidate), verdict);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Look up the verdict for a pair. A miss means the apply pass landed
    /// a candidate the plan pass did not — the registry shifted between
    /// snapshots — so degrade to the no-disambiguator path rather than
    /// ever blocking on the LLM inside the write txn.
    fn get(&self, norm_surface: &str, candidate: EntityId) -> MatchVerdict {
        self.0
            .get(&(norm_surface.to_string(), candidate))
            .cloned()
            .unwrap_or_else(|| MatchVerdict::Skipped {
                reason: "no precomputed verdict (plan/apply divergence)".to_string(),
            })
    }
}

/// How `resolve_or_create_with_deps` obtains a verdict when the embedding
/// tier lands an ambiguous-band candidate. The LLM is never called from
/// inside the write txn; it is called off-reactor between the two passes.
pub enum Disambiguation<'a> {
    /// No disambiguator wired — behave as the pre-disambiguator resolver
    /// (the cosine threshold is the sole arbiter).
    Off,
    /// Plan pass: record each ambiguous candidate for later async
    /// resolution and take the no-disambiguator path for now.
    Collect(&'a mut Vec<PendingVerdict>),
    /// Apply pass: consult the precomputed verdicts; a miss degrades to
    /// the no-disambiguator path.
    Replay(&'a PrecomputedVerdicts),
}

/// Resolve the disambiguation verdict for an ambiguous-band candidate
/// WITHOUT ever calling the LLM inline: `Off` skips, `Collect` records
/// the candidate (reading its view from the txn) and skips, `Replay`
/// looks up the precomputed verdict.
fn resolve_verdict(
    mode: &mut Disambiguation<'_>,
    candidate: EntityId,
    surface_form: &str,
    wtxn: &WriteTransaction,
    entity_type_qname: &str,
) -> MatchVerdict {
    match mode {
        Disambiguation::Off => MatchVerdict::Skipped {
            reason: "no disambiguator wired".to_string(),
        },
        Disambiguation::Replay(verdicts) => verdicts.get(&normalize_name(surface_form), candidate),
        Disambiguation::Collect(out) => {
            match read_candidate_view(wtxn, candidate, entity_type_qname) {
                Ok(Some(view)) => {
                    out.push(PendingVerdict {
                        norm_surface: normalize_name(surface_form),
                        raw_surface: surface_form.to_string(),
                        candidate,
                        view,
                    });
                    MatchVerdict::Skipped {
                        reason: "deferred to async disambiguation".to_string(),
                    }
                }
                Ok(None) => MatchVerdict::Skipped {
                    reason: format!("candidate entity {candidate:?} not found"),
                },
                Err(e) => MatchVerdict::Skipped {
                    reason: format!("read candidate view: {e}"),
                },
            }
        }
    }
}

/// Resolve `surface_form` against the entity registry without the
/// embedding tier or the disambiguator. Equivalent to
/// [`resolve_or_create_with_deps`] called with both dep slots `None`.
pub fn resolve_or_create(
    wtxn: &WriteTransaction,
    scope: RowScope,
    surface_form: &str,
    entity_type_qname: &str,
    confidence: f32,
    now_unix_nanos: u64,
) -> Result<Resolution, ResolverError> {
    // No embedding deps means tier-4 has nothing to embed, so this
    // staging area always comes back empty and there is nothing to flush.
    let mut staged = StagedEntityVectors::new();
    let res = resolve_or_create_with_deps(
        wtxn,
        scope,
        surface_form,
        entity_type_qname,
        confidence,
        now_unix_nanos,
        None,
        &mut staged,
        &mut Disambiguation::Off,
    );
    debug_assert!(staged.is_empty(), "invariant: no embed deps, no staging");
    res
}

/// Resolve `surface_form` against the entity registry, creating a new
/// entity if no tier matched. The caller drives the txn; all reads +
/// writes happen inside it so the resolver's outcome is atomic with
/// downstream writes (mention edges, statement creation).
///
/// When `embed_deps` is `Some`, the resolver consults the entity
/// HNSW between the trigram-fuzzy tier and the create tier, and stages
/// the canonical-name embedding of every newly-minted entity in
/// `staged` so the next resolve of a paraphrase can short-circuit at the
/// embedding tier. The caller MUST call
/// [`StagedEntityVectors::flush_into_hnsw`] once — and only once — the
/// txn has committed; a rolled-back pass drops its staging area instead
/// (the HNSW cannot un-insert). Failures inside the embedding path
/// (embedder errors, HNSW lock contention) degrade gracefully: the
/// resolver logs at `warn` and falls through to the next tier, never
/// aborts the txn.
///
/// When the embedding probe lands a candidate in the ambiguous band, the
/// `disambiguation` mode decides the verdict WITHOUT calling the LLM
/// inline: [`Disambiguation::Off`] takes the cosine-only path;
/// [`Disambiguation::Collect`] records the candidate for the worker to
/// resolve off-reactor, then takes the cosine-only path; and
/// [`Disambiguation::Replay`] consults a [`PrecomputedVerdicts`] map. A
/// [`MatchVerdict::Confirmed`] aliases onto the existing entity; a
/// [`MatchVerdict::Rejected`] mints a fresh entity with no merge
/// proposal (the two are confirmed distinct); the other verdicts fall
/// through to the existing Create + enqueue-merge-proposal flow.
// Each argument is a distinct, non-bundleable concern (txn, tenant scope,
// surface form, type, confidence, clock, embedding deps, disambiguation mode);
// folding them into a params struct would obscure the call sites, not clarify.
#[allow(clippy::too_many_arguments)]
pub fn resolve_or_create_with_deps(
    wtxn: &WriteTransaction,
    scope: RowScope,
    surface_form: &str,
    entity_type_qname: &str,
    _confidence: f32,
    now_unix_nanos: u64,
    embed_deps: Option<&EmbeddingDeps>,
    staged: &mut StagedEntityVectors,
    disambiguation: &mut Disambiguation<'_>,
) -> Result<Resolution, ResolverError> {
    // Greeting / vocative phantom guard (Person only). "Hey Mel", "Thanks
    // Mel", "Yeah Mel" arrive as standalone Person surfaces; each would mint
    // a distinct entity that fragments Mel's facts. Strip the leading
    // greeting so they resolve to the bare name and its existing entity.
    // Person-gated because stripping a leading word from a non-person name is
    // unsafe (the company "Hello Fresh" must stay intact). The cleaned form
    // then flows through the whole gauntlet, so a freshly-minted entity also
    // gets the clean canonical name ("Mel", not "Hey Mel").
    let stripped = if surface_type_is_person(entity_type_qname) {
        strip_leading_vocative(surface_form)
    } else {
        None
    };
    let surface_form: &str = stripped.as_deref().unwrap_or(surface_form);

    let normalized = normalize_name(surface_form);
    if normalized.is_empty() {
        return Err(ResolverError::EmptyNormalizedName);
    }
    let type_id = resolve_entity_type(wtxn, entity_type_qname, now_unix_nanos)?;

    // Tier 1 — exact canonical-name lookup.
    if let Some(id) = lookup_canonical_wtxn(wtxn, scope, type_id, &normalized)? {
        return Ok(Resolution {
            entity_id: id,
            tier: ResolutionTier::Exact,
        });
    }

    // Tier 2 — alias lookup. The alias index is multi-valued; if more
    // than one entity shares this alias we pick the first (smallest
    // EntityId, which is a deterministic byte order) so the result
    // stays stable across re-runs. A future ambiguity-aware resolver
    // could surface the conflict; the worker drops mentions with
    // ambiguous aliases at the cost of one extra resolve.
    let alias_hits = lookup_alias_wtxn(wtxn, scope, type_id, &normalized)?;
    if let Some(id) = alias_hits.into_iter().min() {
        return Ok(Resolution {
            entity_id: id,
            tier: ResolutionTier::Alias,
        });
    }

    // Tier 1b — cross-type exact canonical. Every tier above is scoped to the
    // hinted `type_id`, but the pattern and LLM extractor tiers routinely assign
    // DIFFERENT type ids to the SAME referent (e.g. "Atlas" under one type and
    // "The Atlas" — normalized to the same `atlas` key — under another). Without
    // a cross-type check the referent fragments into two nodes and its relations
    // split across them, which silently breaks multi-hop traversal even though
    // every individual fact was extracted. An EXACT normalized-name match under a
    // different type is the same referent at very high precision, so reuse it.
    // Guard: only when EXACTLY ONE cross-type entity matches — a normalized name
    // shared by two different-typed entities is a genuine homograph ("Apple" the
    // company vs the fruit), so fall through to mint rather than conflate them.
    {
        let cross = entity_resolve_canonical_all_types_wtxn(wtxn, scope, surface_form)?;
        if cross.len() == 1 {
            let id = cross[0];
            entity_add_alias(wtxn, id, surface_form.to_string(), now_unix_nanos)?;
            return Ok(Resolution {
                entity_id: id,
                tier: ResolutionTier::Exact,
            });
        }
    }

    // Tier 3a — trigram fuzzy lookup. Candidates whose Jaccard against
    // the query is above `DEFAULT_FUZZY_THRESHOLD` get the surface
    // form added as an alias and are returned as the match.
    let candidate_ids = trigram_candidates_wtxn(wtxn, scope, type_id, &normalized)?;
    if !candidate_ids.is_empty() {
        let query_tgs = trigrams::extract_trigrams(&normalized);
        if !query_tgs.is_empty() {
            let mut best: Option<(EntityId, f32)> = None;
            for &cid in &candidate_ids {
                let cid_tgs = trigram_set_for_entity(wtxn, cid)?;
                if cid_tgs.is_empty() {
                    continue;
                }
                let score = trigrams::jaccard(&query_tgs, &cid_tgs);
                if score < DEFAULT_FUZZY_THRESHOLD {
                    continue;
                }
                match best {
                    Some((_, bs)) if bs >= score => {}
                    _ => best = Some((cid, score)),
                }
            }
            if let Some((cid, _)) = best {
                // The surface form is now associated with this entity;
                // re-runs of the same string hit tier 2 directly.
                entity_add_alias(wtxn, cid, surface_form.to_string(), now_unix_nanos)?;
                return Ok(Resolution {
                    entity_id: cid,
                    tier: ResolutionTier::Alias,
                });
            }
        }

        // Tier 3a' — partial-name (token-subset) coref. A short reference
        // ("Niraj") whose normalized tokens are a STRICT subset of an
        // existing same-type entity's canonical name ("Niraj Georgian") is
        // the same entity. Jaccard misses this — the longer name dilutes the
        // trigram overlap below the fuzzy floor — yet in a personal-knowledge
        // graph first-name / short references are the dominant coref case, and
        // missing them mints duplicate person nodes that fragment every
        // relation across name variants (Niraj's reports_to lands on one node,
        // his family_of on another), which is what breaks multi-hop reads.
        // Token containment is far stricter than cosine (no "Tokyo"≈"Japan"
        // risk), so it's safe where the embedding tier is deliberately
        // conservative. Merge ONLY when EXACTLY ONE candidate contains the
        // surface tokens — "John" with both "John Smith" and "John Doe" is
        // genuinely ambiguous, so fall through to create rather than guess.
        let q_tokens: Vec<&str> = normalized.split_whitespace().collect();
        if !q_tokens.is_empty() {
            let mut containment: Option<EntityId> = None;
            let mut ambiguous = false;
            for &cid in &candidate_ids {
                let Some(cn) = read_entity_canonical(wtxn, cid)? else {
                    continue;
                };
                let c_norm = normalize_name(&cn);
                let c_tokens: Vec<&str> = c_norm.split_whitespace().collect();
                // Strict superset: candidate carries every query token and at
                // least one more (so "Niraj" → "Niraj Georgian", never the
                // reverse, which would alias a full name onto a bare first).
                if c_tokens.len() > q_tokens.len()
                    && q_tokens.iter().all(|qt| c_tokens.contains(qt))
                {
                    if containment.is_some() {
                        ambiguous = true;
                        break;
                    }
                    containment = Some(cid);
                }
            }
            if !ambiguous {
                if let Some(cid) = containment {
                    entity_add_alias(wtxn, cid, surface_form.to_string(), now_unix_nanos)?;
                    return Ok(Resolution {
                        entity_id: cid,
                        tier: ResolutionTier::Alias,
                    });
                }
            }
        }

        // Tier 3a'' — diminutive / prefix coref (Person only). A nickname
        // such as "Mel" → "Melanie" or "Caro" → "Caroline" is a strict PREFIX
        // of a longer first name, not a token subset, so tier-3a' misses it,
        // and the trigram Jaccard of a 3-char query against a 7-char name sits
        // far below the fuzzy floor. In a personal-knowledge graph nicknames
        // are a dominant coref case; missing them fragments a person's facts
        // across "Mel" / "Melanie" nodes so a query for one can't see the
        // other's memories.
        //
        // Deliberately narrow so it can never merge two DISTINCT people:
        //   * Person type only — a diminutive is a person-name phenomenon; org
        //     / product prefixes ("Goog" → "Google") stay separate.
        //   * single-token query of >= MIN_DIMINUTIVE_LEN chars — a bare
        //     first-name reference, never a phrase.
        //   * strict prefix of the FIRST token of EXACTLY ONE same-(scope,type)
        //     candidate. Two matches ("Sam" → {Samuel, Samantha}) are
        //     ambiguous, so the resolver abstains and mints rather than guess.
        //     The candidate set is already scope- and type-filtered, so this
        //     only ever unifies within one tenant's own graph.
        //
        // No embedding floor is applied: short-nickname embeddings are
        // unreliable, and the exactly-one-in-scope prefix is a stricter
        // identity signal than cosine here — the same reasoning tier-3a'
        // already relies on for token containment.
        if surface_type_is_person(entity_type_qname)
            && q_tokens.len() == 1
            && normalized.chars().count() >= MIN_DIMINUTIVE_LEN
        {
            let query = q_tokens[0];
            let mut prefix_of: Option<EntityId> = None;
            let mut ambiguous = false;
            for &cid in &candidate_ids {
                let Some(cn) = read_entity_canonical(wtxn, cid)? else {
                    continue;
                };
                let c_norm = normalize_name(&cn);
                let Some(c_first) = c_norm.split_whitespace().next() else {
                    continue;
                };
                // Strict prefix: the candidate's first name carries the query
                // as a leading substring AND is strictly longer ("mel" →
                // "melanie", never "mel" → "mel", which tiers 1 / 3a' handle).
                if c_first != query && c_first.starts_with(query) {
                    if prefix_of.is_some() {
                        ambiguous = true;
                        break;
                    }
                    prefix_of = Some(cid);
                }
            }
            if !ambiguous {
                if let Some(cid) = prefix_of {
                    entity_add_alias(wtxn, cid, surface_form.to_string(), now_unix_nanos)?;
                    return Ok(Resolution {
                        entity_id: cid,
                        tier: ResolutionTier::Alias,
                    });
                }
            }
        }

        // Tier 3a''' — retroactive nickname absorption (Person only). The
        // mirror of tier 3a''. Tier 3a'' merges a NEW nickname onto an
        // EXISTING full name; it can only fire when the full name arrived
        // first. When the ORDER is reversed — "Mel" mentioned before
        // "Melanie" — "Mel" mints its own node and the later "Melanie" node
        // never absorbs it, so the person fragments across two entities.
        // Here, when a fuller name arrives, we look for the pre-existing
        // single-token nickname it extends and alias the fuller surface onto
        // THAT node, collapsing both to one entity. Same `entity_add_alias`
        // mechanism as every other coref tier — no new storage path.
        //
        // Deliberately narrow so it can never merge two DISTINCT people:
        //   * Person type only.
        //   * the nickname candidate is a SINGLE token, >= MIN_DIMINUTIVE_LEN
        //     chars, and a STRICT prefix of the new name's FIRST token
        //     ("mel" -> "melanie", never equal).
        //   * EXACTLY ONE such nickname candidate — two ("mel" AND "mela"
        //     both prefixing "melanie") is ambiguous, so abstain and mint.
        //   * the nickname node must not already be BOUND to a different
        //     fuller form. Once "Mel" has absorbed "Melanie", a later
        //     "Melissa" must NOT also fold in — that would put two distinct
        //     people behind one nickname. The first fuller to arrive claims
        //     the nickname; distinct later fullers stay separate. (This is
        //     the residual over-merge boundary: the FIRST fuller always
        //     wins the bare nickname even if a later, equally-plausible
        //     fuller would have been just as valid — an unavoidable cost of
        //     resolving the nickname the moment its first fuller appears.)
        if surface_type_is_person(entity_type_qname) {
            if let Some(&q_first) = q_tokens.first() {
                let mut nickname: Option<(EntityId, String)> = None;
                let mut ambiguous = false;
                for &cid in &candidate_ids {
                    let Some(cn) = read_entity_canonical(wtxn, cid)? else {
                        continue;
                    };
                    let c_norm = normalize_name(&cn);
                    let mut c_toks = c_norm.split_whitespace();
                    // Single-token candidate only: a phrase ("Mel Gibson") is
                    // a full name, not a bare nickname, so it never absorbs.
                    let (Some(c_only), None) = (c_toks.next(), c_toks.next()) else {
                        continue;
                    };
                    if c_only.chars().count() < MIN_DIMINUTIVE_LEN {
                        continue;
                    }
                    // Strict prefix of the new name's first token.
                    if c_only != q_first && q_first.starts_with(c_only) {
                        if nickname.is_some() {
                            ambiguous = true;
                            break;
                        }
                        nickname = Some((cid, c_only.to_string()));
                    }
                }
                if !ambiguous {
                    if let Some((cid, nick_tok)) = nickname {
                        if nickname_entity_free_for_fuller(wtxn, cid, &nick_tok, q_first)? {
                            entity_add_alias(wtxn, cid, surface_form.to_string(), now_unix_nanos)?;
                            return Ok(Resolution {
                                entity_id: cid,
                                tier: ResolutionTier::Alias,
                            });
                        }
                    }
                }
            }
        }
    }

    // Tier 3b — embedding HNSW. The trigram tier above misses
    // paraphrases ("Stripe Inc." vs "Stripe Payments"); a semantic
    // similarity probe catches those without growing the alias index
    // pre-emptively.
    //
    // The probe also surfaces "close but not confident" candidates in
    // the `[PARTIAL_MATCH_FLOOR, EMBED_RESOLVE_THRESHOLD)` band. Those
    // do NOT auto-alias — they're queued for the ambiguity-resolver
    // worker, which re-checks them as the HNSW grows.
    let mut partial_match: Option<(EntityId, f32)> = None;
    if let Some(deps) = embed_deps {
        match tier_embedding(deps, staged, scope, type_id, surface_form, wtxn) {
            Ok(EmbeddingProbe::AutoAlias { entity_id, .. }) => {
                // A high cosine alone is not proof of identity: two
                // distinct same-type entities ("Japan" vs "Tokyo", both
                // Places) can sit above the auto-alias threshold and the
                // tier is type-scoped, so the type filter never separates
                // them. When a disambiguator is wired, get a second
                // opinion before merging; only an explicit rejection
                // blocks the alias, so genuine paraphrases ("Stripe Inc."
                // vs "Stripe Payments") — confirmed or merely uncertain —
                // still merge as before. With no disambiguator, the
                // cosine threshold remains the sole arbiter.
                match resolve_verdict(
                    disambiguation,
                    entity_id,
                    surface_form,
                    wtxn,
                    entity_type_qname,
                ) {
                    MatchVerdict::Rejected => {
                        // Confirmed distinct despite the high cosine.
                        // Fall through to Create without enqueuing a
                        // merge proposal — there is nothing to review.
                        tracing::info!(
                            target: "brain_extractors::resolver",
                            ?entity_id,
                            surface_form,
                            "high-cosine auto-alias rejected by disambiguator; minting a distinct entity",
                        );
                    }
                    MatchVerdict::Confirmed { entity, confidence } => {
                        entity_add_alias(wtxn, entity, surface_form.to_string(), now_unix_nanos)?;
                        tracing::info!(
                            target: "brain_extractors::resolver",
                            ?entity,
                            confidence,
                            "high-cosine auto-alias confirmed by disambiguator",
                        );
                        return Ok(Resolution {
                            entity_id: entity,
                            tier: ResolutionTier::Disambiguated,
                        });
                    }
                    // No disambiguator, or it declined to commit either
                    // way (Uncertain / Skipped). The cosine already
                    // cleared the auto-alias threshold, so preserve the
                    // pre-disambiguator behaviour and merge.
                    MatchVerdict::Uncertain | MatchVerdict::Skipped { .. } => {
                        entity_add_alias(
                            wtxn,
                            entity_id,
                            surface_form.to_string(),
                            now_unix_nanos,
                        )?;
                        return Ok(Resolution {
                            entity_id,
                            tier: ResolutionTier::Embedding,
                        });
                    }
                }
            }
            Ok(EmbeddingProbe::PartialMatch { entity_id, score }) => {
                partial_match = Some((entity_id, score));
            }
            Ok(EmbeddingProbe::None) => {}
            Err(reason) => {
                tracing::warn!(
                    target: "brain_extractors::resolver",
                    surface_form,
                    reason,
                    "tier-3 embedding probe failed; falling through to create",
                );
            }
        }
    }

    // Disambiguation step — second opinion on the partial match.
    //
    // The embedding tier just landed a candidate in the ambiguous
    // band. Ask the disambiguator whether it's actually the same
    // entity: a confirmed match aliases and returns; an explicit
    // rejection lets us skip the (now-unnecessary) merge proposal;
    // uncertainty falls through to the existing Create + enqueue path.
    if let Some((candidate, _score)) = partial_match {
        match resolve_verdict(
            disambiguation,
            candidate,
            surface_form,
            wtxn,
            entity_type_qname,
        ) {
            MatchVerdict::Confirmed { entity, confidence } => {
                entity_add_alias(wtxn, entity, surface_form.to_string(), now_unix_nanos)?;
                tracing::info!(
                    target: "brain_extractors::resolver",
                    ?entity,
                    confidence,
                    "partial match confirmed by disambiguator",
                );
                return Ok(Resolution {
                    entity_id: entity,
                    tier: ResolutionTier::Disambiguated,
                });
            }
            MatchVerdict::Rejected => {
                // Confirmed distinct: drop the partial match so the
                // Create branch below doesn't enqueue a merge proposal
                // for a pair the disambiguator already ruled apart.
                partial_match = None;
            }
            MatchVerdict::Uncertain => {
                // Existing behaviour: Create + enqueue merge proposal.
            }
            MatchVerdict::Skipped { reason } => {
                tracing::warn!(
                    target: "brain_extractors::resolver",
                    surface_form,
                    %reason,
                    "disambiguator skipped; falling through to create",
                );
            }
        }
    }

    // Tier 4 — create. UUIDv7 makes the new id roughly time-ordered;
    // re-running this branch with the same surface form produces a
    // different id because the previous one is still around for
    // tiers 1/2 to short-circuit.
    //
    // Reaching create means tiers 1/2/3a (and 3b when wired) all
    // missed. Without the embedding tier the gauntlet cannot catch
    // paraphrases the trigram tier misses ("Stripe Inc." vs "Stripe
    // Payments"), so a missing embedding tier here is the prime cause
    // of one real-world entity splitting into many duplicate nodes.
    // Warn so an operator can correlate a creeping entity-cardinality
    // blowup with an absent embedding tier instead of chasing it blind.
    if embed_deps.is_none() {
        tracing::warn!(
            target: "brain_extractors::resolver",
            surface_form,
            "resolver fell through to create with no embedding tier wired; paraphrases the trigram tier misses will over-split into duplicate entities",
        );
    }
    let new_id = EntityId::new();
    let mut entity = Entity::new_active(
        new_id,
        type_id,
        surface_form.to_string(),
        normalized,
        now_unix_nanos,
    );
    entity.mention_count = 1;
    entity_put(wtxn, scope, &entity)?;

    // Minting a new node is normal much of the time, but it is also the
    // exact event that grows entity cardinality. Record it at debug so a
    // suspected over-split can be reconstructed from logs — which surface
    // forms produced fresh entities, and how many collapse onto the same
    // real-world thing once aliased.
    tracing::debug!(
        target: "brain_extractors::resolver",
        surface_form,
        entity_id = ?new_id,
        "all resolver tiers missed; created a new entity",
    );

    // Feed the entity to tier-3b so the next paraphrase can match it:
    // the durable vector row goes into `wtxn` (committing or rolling back
    // with the entity row it describes), the in-RAM HNSW insert is staged
    // for the caller to flush after commit. Failures here are non-fatal:
    // the entity row is durable; the worst case is a near-miss future
    // resolve until the next boot rebuild.
    //
    // Staged in EVERY disambiguation mode. The HNSW cannot un-insert, so
    // an inline insert would outlive any rollback — the two-phase
    // disambiguation's discarded plan pass, or an error out of the apply
    // body — and point at an entity id no committed txn ever wrote.
    if let Some(deps) = embed_deps {
        if let Err(reason) = stage_entity_vector(wtxn, deps, staged, new_id, surface_form) {
            tracing::warn!(
                target: "brain_extractors::resolver",
                entity_id = ?new_id,
                reason,
                "tier-4 entity-vector staging failed; entity is durable but unreachable via tier-3b until a rebuild",
            );
        }
    }

    // Tier 3b near-miss: the embedding probe spotted a candidate in
    // the partial-match band. Enqueue a `Pending` merge proposal so
    // the ambiguity-resolver worker can re-check the pair after the
    // HNSW absorbs more aliases / paraphrases. The new entity has
    // already been written above — the worker will merge the new
    // entity into the candidate if the recomputed cosine clears the
    // auto-apply threshold.
    if let Some((candidate, score)) = partial_match {
        let proposal_id = MergeId::new();
        enqueue_merge_proposal(
            wtxn,
            proposal_id,
            new_id,
            candidate,
            score,
            proposal_tier::EMBEDDING,
            now_unix_nanos,
        )?;
    }

    Ok(Resolution {
        entity_id: new_id,
        tier: ResolutionTier::Created,
    })
}

/// Outcome of one tier-3b embedding probe.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EmbeddingProbe {
    /// Top neighbour cleared [`EMBED_RESOLVE_THRESHOLD`]; the resolver
    /// auto-aliases the surface form to this entity.
    AutoAlias { entity_id: EntityId, score: f32 },
    /// Top neighbour scored in
    /// `[PARTIAL_MATCH_FLOOR, EMBED_RESOLVE_THRESHOLD)`; the resolver
    /// mints a fresh entity AND enqueues a `Pending` proposal.
    PartialMatch { entity_id: EntityId, score: f32 },
    /// No neighbour above the floor — the probe contributes nothing.
    None,
}

/// Tier-3b worker: embed the surface form, ask the HNSW (plus the
/// pass-local staging area) for the top-K nearest entities, type-filter,
/// classify the top score against the auto-alias / partial-match / drop
/// thresholds.
///
/// - `score >= EMBED_RESOLVE_THRESHOLD` → [`EmbeddingProbe::AutoAlias`].
/// - `PARTIAL_MATCH_FLOOR <= score < EMBED_RESOLVE_THRESHOLD`
///   → [`EmbeddingProbe::PartialMatch`].
/// - `score < PARTIAL_MATCH_FLOOR` or no candidate → [`EmbeddingProbe::None`].
/// - `Err(reason)` for transient backend failures (embedder, HNSW lock).
fn tier_embedding(
    deps: &EmbeddingDeps,
    staged: &StagedEntityVectors,
    scope: RowScope,
    type_id: EntityTypeId,
    surface_form: &str,
    wtxn: &WriteTransaction,
) -> Result<EmbeddingProbe, String> {
    let threshold = deps.embed_threshold;
    let vector = deps
        .embedder
        .embed(surface_form)
        .map_err(|e| format!("embedder failed: {e}"))?;
    let mut hits = {
        let hnsw = deps.hnsw.read();
        if hnsw.is_empty() {
            Vec::new()
        } else {
            hnsw.search(&vector, EMBED_RESOLVE_TOP_K)
                .map_err(|e| format!("hnsw search failed: {e}"))?
        }
    };
    // Entities minted earlier in THIS pass aren't in the index yet — their
    // insert is held back until the txn commits — but their rows are in
    // `wtxn`, so they're legitimate candidates for a later surface in the
    // same memory. Scan them alongside the index hits and re-sort.
    if !staged.is_empty() {
        hits.extend(staged.probe(&vector, EMBED_RESOLVE_TOP_K));
        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut seen = HashSet::with_capacity(hits.len());
        hits.retain(|(id, _)| seen.insert(*id));
        hits.truncate(EMBED_RESOLVE_TOP_K);
    }
    if hits.is_empty() {
        return Ok(EmbeddingProbe::None);
    }
    // Filter by (scope, entity_type). The entity HNSW is a single
    // per-shard index shared across every tenant + space + type, so a
    // probe can surface a neighbour belonging to a foreign
    // `(namespace, space)` or a different type. The tenant wall is
    // unconditional: a candidate from another scope is dropped before
    // the threshold check so `acme/chatbot`'s "John" can never resolve
    // onto `globex`'s or `acme/research`'s "John". The type filter is
    // the same honesty guard as before (a Person lookup must not alias
    // onto an Organization neighbour).
    let typed_hits: Vec<(EntityId, f32)> = hits
        .into_iter()
        .filter_map(|(eid, score)| match read_entity_type_and_scope(wtxn, eid) {
            Ok(Some((t, s))) if t == type_id && s == scope => Some((eid, score)),
            _ => None,
        })
        .collect();
    if typed_hits.is_empty() {
        return Ok(EmbeddingProbe::None);
    }
    let (best_id, best_score) = typed_hits[0];
    if best_score >= threshold {
        return Ok(EmbeddingProbe::AutoAlias {
            entity_id: best_id,
            score: best_score,
        });
    }
    if best_score >= PARTIAL_MATCH_FLOOR {
        return Ok(EmbeddingProbe::PartialMatch {
            entity_id: best_id,
            score: best_score,
        });
    }
    Ok(EmbeddingProbe::None)
}

/// Read the `entity_type_id` + `(namespace, space)` scope for `id`
/// inside an existing write txn. Lighter than `entity_get_inside_wtxn`
/// (no aliases, no blob decoding) — tier-3b only needs the type +
/// scope filter to drop foreign-type and foreign-tenant HNSW hits.
fn read_entity_type_and_scope(
    wtxn: &WriteTransaction,
    id: EntityId,
) -> Result<Option<(EntityTypeId, RowScope)>, ResolverError> {
    let t = wtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    Ok(row.map(|m| {
        (
            EntityTypeId::from(m.entity_type_id),
            RowScope::from_bytes(m.namespace_id, m.space_id_bytes),
        )
    }))
}

/// Read just the `canonical_name` for `id` inside an existing write txn.
/// Used by the partial-name coref tier to compare token sets.
fn read_entity_canonical(
    wtxn: &WriteTransaction,
    id: EntityId,
) -> Result<Option<String>, ResolverError> {
    let t = wtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    Ok(row.map(|m| m.canonical_name))
}

/// True when the single-token nickname entity `cid` is still FREE to bind
/// to the fuller first name `new_first` — i.e. it does not already carry a
/// DIFFERENT fuller form (in its canonical name or aliases).
///
/// Used by the retroactive nickname tier (3a''') to stop a nickname that has
/// already absorbed one fuller name from absorbing a second, distinct one:
/// once "Mel" holds the alias "Melanie", a later "Melissa" must not fold in,
/// or two different people end up behind one nickname. A stored form equal to
/// `new_first` (an idempotent re-merge of the same fuller) does not count as a
/// conflicting bind, so re-resolving "Melanie" onto "Mel" stays a no-op merge.
fn nickname_entity_free_for_fuller(
    wtxn: &WriteTransaction,
    cid: EntityId,
    nick_tok: &str,
    new_first: &str,
) -> Result<bool, ResolverError> {
    let t = wtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&cid.to_bytes())?.map(|g| g.value());
    let Some(row) = row else {
        return Ok(true);
    };
    let mut names = Vec::with_capacity(1 + row.aliases.len());
    names.push(row.canonical_name);
    names.extend(row.aliases);
    for name in names {
        let norm = normalize_name(&name);
        let Some(first) = norm.split_whitespace().next() else {
            continue;
        };
        // A fuller form of the nickname whose first token strictly extends the
        // nickname and differs from the incoming one — the nickname is taken.
        if first != nick_tok
            && first != new_first
            && first.starts_with(nick_tok)
            && first.chars().count() > nick_tok.chars().count()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Embed the entity's canonical name, persist the vector in `wtxn`, and
/// stage the HNSW insert for post-commit publication. Best-effort:
/// returns `Err(reason)` so the caller can decide whether to log or
/// proceed. The resolver logs at `warn` and proceeds (the entity row
/// still lands; tier-3b is merely unreachable for paraphrases of this
/// entity until a future HNSW rebuild).
fn stage_entity_vector(
    wtxn: &redb::WriteTransaction,
    deps: &EmbeddingDeps,
    staged: &mut StagedEntityVectors,
    entity_id: EntityId,
    canonical_name: &str,
) -> Result<(), String> {
    let vector: [f32; VECTOR_DIM] = deps
        .embedder
        .embed(canonical_name)
        .map_err(|e| format!("embedder failed: {e}"))?;
    // The stored vector is the durability hook for restart: on next boot
    // the entity HNSW rebuilds from these rows without re-embedding. It
    // is written INSIDE `wtxn`, so it lands exactly when the entity row
    // does. A failure here is non-fatal — log + still stage, so the
    // entity is resolvable for the rest of this process; on restart the
    // absent row drops back to the re-embed fallback.
    if let Err(e) = entity_vector_put(wtxn, entity_id, &vector) {
        tracing::warn!(
            target: "brain_extractors::resolver",
            entity_id = ?entity_id,
            error = %e,
            "entity_vector_put failed; HNSW will reseed via re-embed on next restart",
        );
    }
    staged.stage(entity_id, vector);
    Ok(())
}

/// Three-way reply to the "is this the same entity?" question. Mirrors
/// [`MatchVerdict`] minus the [`Skipped`](MatchVerdict::Skipped) case,
/// which represents preparation failure rather than a backend opinion.
#[derive(Debug, Clone, PartialEq)]
enum SameEntityReply {
    Yes(f32),
    No,
    Uncertain,
}

/// Send the candidate view + surface form to the LLM backend and parse
/// the reply. `.await`-ed off the shard reactor by the worker's
/// disambiguation *plan* step, before any write txn is open — so it
/// never parks the shard core. Any soft failure (transport error,
/// unparseable reply) surfaces as `Err`, which the caller maps to
/// [`MatchVerdict::Skipped`] (the no-disambiguator fallback).
async fn ask_if_same_entity(
    disambiguator: &EntityDisambiguator,
    view: &LlmCandidateView,
    surface_form: &str,
) -> Result<SameEntityReply, String> {
    use brain_llm::{LlmMessage, LlmRequest, LlmRole};

    let system = build_confirm_system_prompt();
    let user = build_confirm_user_prompt(view, surface_form);
    let req = LlmRequest {
        model: disambiguator.model.clone(),
        system_blocks: vec![brain_llm::types::SystemBlock::cached(system)],
        messages: vec![LlmMessage {
            role: LlmRole::User,
            content: user,
        }],
        response_schema: None,
        temperature: 0.0,
        max_tokens: 64,
        // Off the reactor now, so this bounds only this memory's own
        // extraction latency (not a shard-wide freeze); a natural value
        // is fine. A slow/hung provider still degrades to the
        // merge/create fallback via the `Err` path.
        timeout: DISAMBIGUATOR_LLM_TIMEOUT,
    };
    let resp = disambiguator
        .client
        .complete(req)
        .await
        .map_err(|e| format!("llm transport: {e}"))?;
    parse_confirm_reply(&resp.content)
        .ok_or_else(|| format!("unparseable disambiguator reply: {:?}", resp.content))
}

fn build_confirm_system_prompt() -> String {
    // Cacheable: stable across calls in a session. Anthropic prompt
    // caching keys on byte-identical blocks — any drift wipes the
    // cache, so the wording is fixed.
    "You decide whether a candidate surface name refers to the same \
real-world entity as a known entity record. Reply with EXACTLY ONE of: \
`YES <confidence>` where confidence is a decimal in [0.0, 1.0] when the \
candidate is the same entity; `NO` when the candidate is a different \
entity; or `UNCERTAIN` when you cannot tell from the given information. \
Do not explain. Do not add any other text. Reply on a single line."
        .to_owned()
}

fn build_confirm_user_prompt(view: &LlmCandidateView, surface_form: &str) -> String {
    let mut out = String::new();
    out.push_str("Surface form: ");
    out.push_str(surface_form);
    out.push_str("\nKnown entity:\n  Canonical name: ");
    out.push_str(&view.canonical_name);
    out.push_str("\n  Type: ");
    out.push_str(&view.entity_type_name);
    if !view.aliases.is_empty() {
        out.push_str("\n  Aliases: ");
        out.push_str(&view.aliases.join(", "));
    }
    out.push_str("\n\nReply with one of: YES <confidence>, NO, UNCERTAIN.\n");
    out
}

fn parse_confirm_reply(content: &str) -> Option<SameEntityReply> {
    let line = content.trim().lines().next()?.trim();
    if line.eq_ignore_ascii_case("NO") {
        return Some(SameEntityReply::No);
    }
    if line.eq_ignore_ascii_case("UNCERTAIN") {
        return Some(SameEntityReply::Uncertain);
    }
    let mut parts = line.split_whitespace();
    if !parts.next()?.eq_ignore_ascii_case("YES") {
        return None;
    }
    let conf: f32 = parts.next()?.parse().ok()?;
    if (0.0..=1.0).contains(&conf) {
        Some(SameEntityReply::Yes(conf))
    } else {
        None
    }
}

/// Snapshot the canonical name + aliases for `id` from the live write
/// transaction. Returns `Ok(None)` when the row doesn't exist — the
/// caller turns that into [`MatchVerdict::Skipped`].
fn read_candidate_view(
    wtxn: &WriteTransaction,
    id: EntityId,
    entity_type_qname: &str,
) -> Result<Option<LlmCandidateView>, ResolverError> {
    let t = wtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    Ok(row.map(|m| LlmCandidateView {
        entity_id: id,
        canonical_name: m.canonical_name,
        aliases: m.aliases,
        entity_type_name: qname_to_type_name(entity_type_qname).to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_core::EntityType;
    use brain_metadata::entity::ops::entity_get;
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    const NOW: u64 = 1_700_000_000_000_000_000;

    /// Fixed `(namespace, space)` scope for resolver unit tests. The
    /// system namespace + a stable space are enough to exercise the
    /// scoped gauntlet; cross-scope distinctness is proven at the
    /// brain-ops handler layer (`typed_graph_namespace_isolation.rs`).
    fn test_scope() -> RowScope {
        RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xA1; 16])
    }

    fn db(dir: &TempDir) -> MetadataDb {
        MetadataDb::open(dir.path().join("metadata.redb")).expect("open")
    }

    #[test]
    fn tier_exact_returns_existing_entity_by_canonical_name() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let existing = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Priya Patel".into(),
            normalize_name("Priya Patel"),
            NOW,
        );
        let existing_id = existing.id;
        {
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), &existing).unwrap();
            wtxn.commit().unwrap();
        }
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, test_scope(), "Priya Patel", "brain:Person", 0.9, NOW)
            .unwrap();
        assert_eq!(res.entity_id, existing_id);
        assert_eq!(res.tier, ResolutionTier::Exact);
        wtxn.commit().unwrap();
    }

    #[test]
    fn tier_alias_returns_existing_entity_via_alias() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let mut existing = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Priya Patel".into(),
            normalize_name("Priya Patel"),
            NOW,
        );
        existing.aliases.push("Priya".into());
        let id = existing.id;
        {
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), &existing).unwrap();
            wtxn.commit().unwrap();
        }
        let wtxn = d.write_txn().unwrap();
        let res =
            resolve_or_create(&wtxn, test_scope(), "priya", "brain:Person", 0.7, NOW).unwrap();
        assert_eq!(res.entity_id, id);
        assert_eq!(res.tier, ResolutionTier::Alias);
        wtxn.commit().unwrap();
    }

    #[test]
    fn tier_partial_name_subset_resolves_to_full_entity() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let full = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Niraj Georgian".into(),
            normalize_name("Niraj Georgian"),
            NOW,
        );
        let full_id = full.id;
        {
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), &full).unwrap();
            wtxn.commit().unwrap();
        }
        // A bare first-name reference must coref onto the full-name entity
        // (token subset) instead of minting a duplicate person node.
        let wtxn = d.write_txn().unwrap();
        let res =
            resolve_or_create(&wtxn, test_scope(), "Niraj", "brain:Person", 0.8, NOW + 1).unwrap();
        assert_eq!(
            res.entity_id, full_id,
            "Niraj should coref to Niraj Georgian"
        );
        assert_eq!(res.tier, ResolutionTier::Alias);
        wtxn.commit().unwrap();
    }

    #[test]
    fn tier_partial_name_ambiguous_does_not_merge() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        for name in ["Niraj Georgian", "Niraj Patel"] {
            let e = Entity::new_active(
                EntityId::new(),
                EntityType::PERSON_ID,
                name.into(),
                normalize_name(name),
                NOW,
            );
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), &e).unwrap();
            wtxn.commit().unwrap();
        }
        // "Niraj" is a subset of TWO distinct people → ambiguous → must not
        // guess; mint a fresh entity instead of mis-merging.
        let wtxn = d.write_txn().unwrap();
        let res =
            resolve_or_create(&wtxn, test_scope(), "Niraj", "brain:Person", 0.8, NOW + 1).unwrap();
        assert_eq!(
            res.tier,
            ResolutionTier::Created,
            "ambiguous partial name must not merge"
        );
        wtxn.commit().unwrap();
    }

    #[test]
    fn tier_diminutive_prefix_resolves_to_full_first_name() {
        // A nickname ("Mel") that is a strict prefix of exactly one same-scope
        // Person's first name ("Melanie") must coref onto that entity — not
        // mint a duplicate that would hide Mel's facts from a Melanie query.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let full = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Melanie".into(),
            normalize_name("Melanie"),
            NOW,
        );
        let full_id = full.id;
        {
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), &full).unwrap();
            wtxn.commit().unwrap();
        }
        let wtxn = d.write_txn().unwrap();
        let res =
            resolve_or_create(&wtxn, test_scope(), "Mel", "brain:Person", 0.8, NOW + 1).unwrap();
        assert_eq!(res.entity_id, full_id, "Mel should coref to Melanie");
        assert_eq!(res.tier, ResolutionTier::Alias);
        wtxn.commit().unwrap();
    }

    #[test]
    fn tier_diminutive_prefix_ambiguous_does_not_merge() {
        // "Mel" is a prefix of TWO distinct people ("Melanie", "Melissa") →
        // ambiguous → must mint rather than merge onto the wrong person. This
        // is the guard that keeps the nickname tier from over-merging.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        for name in ["Melanie", "Melissa"] {
            let e = Entity::new_active(
                EntityId::new(),
                EntityType::PERSON_ID,
                name.into(),
                normalize_name(name),
                NOW,
            );
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), &e).unwrap();
            wtxn.commit().unwrap();
        }
        let wtxn = d.write_txn().unwrap();
        let res =
            resolve_or_create(&wtxn, test_scope(), "Mel", "brain:Person", 0.8, NOW + 1).unwrap();
        assert_eq!(
            res.tier,
            ResolutionTier::Created,
            "ambiguous diminutive prefix must not merge"
        );
        wtxn.commit().unwrap();
    }

    #[test]
    fn tier_diminutive_prefix_never_merges_distinct_full_names() {
        // Two distinct FULL names sharing no prefix must never collapse: "Mel"
        // resolving must not touch "Caroline". Belt-and-suspenders regression
        // against the nickname tier reaching beyond a genuine prefix.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let caroline = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Caroline".into(),
            normalize_name("Caroline"),
            NOW,
        );
        let caroline_id = caroline.id;
        {
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), &caroline).unwrap();
            wtxn.commit().unwrap();
        }
        let wtxn = d.write_txn().unwrap();
        let res =
            resolve_or_create(&wtxn, test_scope(), "Mel", "brain:Person", 0.8, NOW + 1).unwrap();
        assert_ne!(
            res.entity_id, caroline_id,
            "Mel must not merge onto Caroline"
        );
        assert_eq!(res.tier, ResolutionTier::Created);
        wtxn.commit().unwrap();
    }

    #[test]
    fn retroactive_nickname_then_full_name_unifies() {
        // Ordering mirror of the diminutive tier: the NICKNAME is minted first
        // ("Mel"), then the FULLER name arrives ("Melanie"). Tier 3a'' can't
        // fire (no full name existed when "Mel" was created); the retroactive
        // tier must fold "Melanie" onto the pre-existing "Mel" node so the two
        // don't fragment.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let mel_id = {
            let wtxn = d.write_txn().unwrap();
            let res =
                resolve_or_create(&wtxn, test_scope(), "Mel", "brain:Person", 0.8, NOW).unwrap();
            assert_eq!(res.tier, ResolutionTier::Created);
            wtxn.commit().unwrap();
            res.entity_id
        };
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, test_scope(), "Melanie", "brain:Person", 0.8, NOW + 1)
            .unwrap();
        assert_eq!(
            res.entity_id, mel_id,
            "Melanie should retroactively absorb the earlier Mel node"
        );
        assert_eq!(res.tier, ResolutionTier::Alias);
        wtxn.commit().unwrap();
        // "Melanie" is now an alias of the unified node → re-resolve hits tier 2.
        let rtxn = d.read_txn().unwrap();
        let got = entity_get(&rtxn, mel_id).unwrap().unwrap();
        assert!(
            got.aliases.iter().any(|a| a == "Melanie"),
            "retroactive merge should alias the fuller surface; got {:?}",
            got.aliases
        );
    }

    #[test]
    fn retroactive_nickname_ambiguous_second_fuller_not_merged() {
        // "Mel" created, then "Melanie" folds onto it. A LATER, equally-valid
        // fuller "Melissa" must NOT also fold in — that would put two distinct
        // people behind one nickname. The freeze guard keeps them apart: the
        // first fuller claims the nickname; distinct later fullers stay their
        // own node.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let mel_id = {
            let wtxn = d.write_txn().unwrap();
            let res =
                resolve_or_create(&wtxn, test_scope(), "Mel", "brain:Person", 0.8, NOW).unwrap();
            wtxn.commit().unwrap();
            res.entity_id
        };
        let melanie_id = {
            let wtxn = d.write_txn().unwrap();
            let res =
                resolve_or_create(&wtxn, test_scope(), "Melanie", "brain:Person", 0.8, NOW + 1)
                    .unwrap();
            assert_eq!(res.entity_id, mel_id, "Melanie claims the Mel nickname");
            wtxn.commit().unwrap();
            res.entity_id
        };
        let wtxn = d.write_txn().unwrap();
        let melissa =
            resolve_or_create(&wtxn, test_scope(), "Melissa", "brain:Person", 0.8, NOW + 2)
                .unwrap();
        assert_eq!(
            melissa.tier,
            ResolutionTier::Created,
            "second fuller must not fold into an already-claimed nickname"
        );
        assert_ne!(
            melissa.entity_id, melanie_id,
            "Melissa and Melanie are distinct people, must not share a node"
        );
        wtxn.commit().unwrap();
    }

    #[test]
    fn retroactive_two_prefix_nicknames_do_not_merge() {
        // Two single-token nicknames both prefix the new name ("mel" AND
        // "mela" both prefix "melanie"). Which one owns "Melanie" is
        // ambiguous, so the retroactive tier abstains and mints.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        // Seed the two nicknames directly: resolving "Mela" after "Mel" would
        // itself fold them together, so plant them as distinct nodes to set up
        // the two-candidate ambiguity the tier must decline.
        for nick in ["Mel", "Mela"] {
            let e = Entity::new_active(
                EntityId::new(),
                EntityType::PERSON_ID,
                nick.into(),
                normalize_name(nick),
                NOW,
            );
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), &e).unwrap();
            wtxn.commit().unwrap();
        }
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, test_scope(), "Melanie", "brain:Person", 0.8, NOW + 1)
            .unwrap();
        assert_eq!(
            res.tier,
            ResolutionTier::Created,
            "two candidate nicknames is ambiguous; must not guess"
        );
        wtxn.commit().unwrap();
    }

    #[test]
    fn retroactive_never_merges_multi_token_full_name() {
        // A pre-existing MULTI-token full name ("Mel Gibson") shares its first
        // token with the newcomer "Melanie" but is a real full name, not a
        // bare nickname. The single-token guard keeps them distinct — the
        // retroactive tier only absorbs bare nicknames.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let gibson_id = {
            let wtxn = d.write_txn().unwrap();
            let res =
                resolve_or_create(&wtxn, test_scope(), "Mel Gibson", "brain:Person", 0.8, NOW)
                    .unwrap();
            wtxn.commit().unwrap();
            res.entity_id
        };
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, test_scope(), "Melanie", "brain:Person", 0.8, NOW + 1)
            .unwrap();
        assert_ne!(
            res.entity_id, gibson_id,
            "Melanie must not absorb the multi-token full name Mel Gibson"
        );
        assert_eq!(res.tier, ResolutionTier::Created);
        wtxn.commit().unwrap();
    }

    #[test]
    fn retroactive_never_merges_two_distinct_full_first_names() {
        // Two distinct single-token full names that merely share a stem
        // ("Melanie" and "Melissa", neither a prefix of the other) must never
        // collapse via the retroactive tier.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let melanie_id = {
            let wtxn = d.write_txn().unwrap();
            let res = resolve_or_create(&wtxn, test_scope(), "Melanie", "brain:Person", 0.8, NOW)
                .unwrap();
            wtxn.commit().unwrap();
            res.entity_id
        };
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(&wtxn, test_scope(), "Melissa", "brain:Person", 0.8, NOW + 1)
            .unwrap();
        assert_ne!(
            res.entity_id, melanie_id,
            "Melissa and Melanie share a stem but neither is a prefix of the other"
        );
        assert_eq!(res.tier, ResolutionTier::Created);
        wtxn.commit().unwrap();
    }

    #[test]
    fn greeting_prefixed_surface_resolves_to_existing_person() {
        // "Hey Mel" must strip the vocative and resolve to the existing "Mel"
        // entity (tier-1 exact after the strip), not mint a "Hey Mel" node.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let mel = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Mel".into(),
            normalize_name("Mel"),
            NOW,
        );
        let mel_id = mel.id;
        {
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), &mel).unwrap();
            wtxn.commit().unwrap();
        }
        for surface in ["Hey Mel", "Thanks Mel", "Yeah Mel", "Wow Mel"] {
            let wtxn = d.write_txn().unwrap();
            let res = resolve_or_create(&wtxn, test_scope(), surface, "brain:Person", 0.8, NOW + 1)
                .unwrap();
            assert_eq!(res.entity_id, mel_id, "{surface} should resolve to Mel");
            assert_eq!(res.tier, ResolutionTier::Exact);
            wtxn.commit().unwrap();
        }
    }

    #[test]
    fn greeting_strip_is_person_gated() {
        // A non-Person surface keeps its leading word: the company "Hello
        // Fresh" must not be mangled into "Fresh". Resolve under a non-Person
        // type and confirm the created canonical name is intact.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(
            &wtxn,
            test_scope(),
            "Hello Fresh",
            "brain:Organization",
            0.8,
            NOW,
        )
        .unwrap();
        assert_eq!(res.tier, ResolutionTier::Created);
        wtxn.commit().unwrap();
        let rtxn = d.read_txn().unwrap();
        let got = entity_get(&rtxn, res.entity_id).unwrap().unwrap();
        assert_eq!(got.canonical_name, "Hello Fresh");
    }

    // ----- Pure-logic surface normalization -------------------------------

    #[test]
    fn strip_leading_vocative_removes_greetings() {
        assert_eq!(strip_leading_vocative("Hey Mel").as_deref(), Some("Mel"));
        assert_eq!(strip_leading_vocative("Thanks Mel").as_deref(), Some("Mel"));
        assert_eq!(strip_leading_vocative("Yeah Mel").as_deref(), Some("Mel"));
        assert_eq!(strip_leading_vocative("Wow Mel").as_deref(), Some("Mel"));
        assert_eq!(strip_leading_vocative("Hey, Mel").as_deref(), Some("Mel"));
        assert_eq!(strip_leading_vocative("oh hey Mel").as_deref(), Some("Mel"));
        assert_eq!(
            strip_leading_vocative("thank you Mel").as_deref(),
            Some("Mel"),
        );
        // Multi-token names survive the strip whole.
        assert_eq!(
            strip_leading_vocative("Hey Mary Jane").as_deref(),
            Some("Mary Jane"),
        );
    }

    #[test]
    fn strip_leading_vocative_leaves_plain_names_untouched() {
        // No leading greeting → None (caller keeps the original surface).
        assert_eq!(strip_leading_vocative("Mel"), None);
        assert_eq!(strip_leading_vocative("Melanie Cross"), None);
        // A bare greeting with no trailing name is not reduced to empty.
        assert_eq!(strip_leading_vocative("Hey"), None);
        assert_eq!(strip_leading_vocative("thank you"), None);
    }

    #[test]
    fn is_temporal_expression_surface_rejects_relative_dates() {
        for t in [
            "Last Friday",
            "last fri",
            "Next Monday",
            "this weekend",
            "next week",
            "yesterday",
            "Today",
            "tomorrow",
            "3 days ago",
            "in 2 weeks",
            "2020-01-15",
            // Full calendar dates: the forms that used to slip through and
            // become orphan Event-typed entity nodes.
            "January 2026",
            "january 2026",
            "Jan 2026",
            "January 5, 2026",
            "January 5 2026",
            "5 January 2026",
            "8 May, 2023",
            "2026",
            "  January 2026  ",
        ] {
            assert!(
                is_temporal_expression_surface(t),
                "{t} should be a temporal expression"
            );
        }
    }

    #[test]
    fn is_temporal_expression_surface_keeps_names() {
        // Bare weekday / month names can be people — never reject them; and
        // real names are obviously not temporal.
        for t in [
            "Friday",
            "Sun",
            "May",
            "June",
            // A bare month name stays eligible: it can be a person or a
            // product. Only a month paired with a year is a date.
            "January",
            "Jan",
            "Melanie",
            "Mel",
            "Last Name",
            "next door",
        ] {
            assert!(
                !is_temporal_expression_surface(t),
                "{t} must not be treated as temporal"
            );
        }
    }

    #[test]
    fn is_temporal_expression_surface_does_not_over_match_names_containing_dates() {
        // The date recognizers are whole-surface anchored, so a year or month
        // merely embedded in a longer name is not a date — these are genuine
        // entities and must survive the guard.
        for t in [
            "Room 2026",
            "Project 2026",
            "Apollo 1969",
            "2026 Roadmap",
            "Q1 2026",
            "Diego",
            "billing team",
            "Stripe",
            "May Fourth Movement",
            "January Jones",
        ] {
            assert!(
                !is_temporal_expression_surface(t),
                "{t} must not be treated as temporal"
            );
        }
    }

    #[test]
    fn tier_fuzzy_matches_close_surface_form_and_adds_alias() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        // Two entities to make the candidate set non-trivial.
        let target = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Priya Patel".into(),
            normalize_name("Priya Patel"),
            NOW,
        );
        let other = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Aleksandar Kovacevic".into(),
            normalize_name("Aleksandar Kovacevic"),
            NOW,
        );
        let target_id = target.id;
        {
            let wtxn = d.write_txn().unwrap();
            entity_put(&wtxn, test_scope(), &target).unwrap();
            entity_put(&wtxn, test_scope(), &other).unwrap();
            wtxn.commit().unwrap();
        }
        // Tier-3 fuzzy: typo'd surface form should resolve to target.
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(
            &wtxn,
            test_scope(),
            "Priya  Patel",
            "brain:Person",
            0.8,
            NOW + 1,
        )
        .unwrap();
        // "priya  patel" normalises to "priya patel" → tier-1 hit.
        assert_eq!(res.entity_id, target_id);
        assert_eq!(res.tier, ResolutionTier::Exact);
        wtxn.commit().unwrap();

        // Now a true fuzzy match — a partial name share.
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(
            &wtxn,
            test_scope(),
            "Priya Patell",
            "brain:Person",
            0.8,
            NOW + 2,
        )
        .unwrap();
        assert_eq!(res.entity_id, target_id);
        // First fuzzy hit promotes via alias index. Re-resolve picks
        // tier-2 next time.
        assert_eq!(res.tier, ResolutionTier::Alias);
        wtxn.commit().unwrap();

        // Verify the alias was actually written.
        let rtxn = d.read_txn().unwrap();
        let got = entity_get(&rtxn, target_id).unwrap().unwrap();
        assert!(
            got.aliases.iter().any(|a| a == "Priya Patell"),
            "tier-3 should add the surface form as an alias; got {:?}",
            got.aliases
        );
    }

    #[test]
    fn tier_create_mints_new_entity_when_no_match() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(
            &wtxn,
            test_scope(),
            "Brand New Name",
            "brain:Person",
            0.5,
            NOW,
        )
        .unwrap();
        assert_eq!(res.tier, ResolutionTier::Created);
        wtxn.commit().unwrap();
        let rtxn = d.read_txn().unwrap();
        let got = entity_get(&rtxn, res.entity_id).unwrap().unwrap();
        assert_eq!(got.canonical_name, "Brand New Name");
        assert_eq!(got.entity_type, EntityType::PERSON_ID);
        // A second resolve on the same surface form should hit tier 1
        // (deterministic re-resolve).
        let wtxn = d.write_txn().unwrap();
        let res2 = resolve_or_create(
            &wtxn,
            test_scope(),
            "Brand New Name",
            "brain:Person",
            0.5,
            NOW + 1,
        )
        .unwrap();
        assert_eq!(res2.entity_id, res.entity_id);
        assert_eq!(res2.tier, ResolutionTier::Exact);
        wtxn.commit().unwrap();
    }

    #[test]
    fn cross_type_exact_reuses_existing_entity_under_different_type() {
        // The pattern and LLM extractor tiers routinely assign DIFFERENT type
        // ids to the same referent: "The Atlas" (determiner-stripped to the
        // normalized key "atlas") under one type and "Atlas" under another.
        // Without cross-type reuse these fragment into two nodes and split the
        // entity's relations across them, silently breaking multi-hop traversal.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        // First mention mints under Concept; determiner-strip → key "atlas".
        let wtxn = d.write_txn().unwrap();
        let first =
            resolve_or_create(&wtxn, test_scope(), "The Atlas", "brain:Concept", 0.9, NOW).unwrap();
        assert_eq!(first.tier, ResolutionTier::Created);
        wtxn.commit().unwrap();
        // Second mention, DIFFERENT type hint, bare form → must reuse, not mint.
        let wtxn = d.write_txn().unwrap();
        let second =
            resolve_or_create(&wtxn, test_scope(), "Atlas", "brain:Person", 0.9, NOW + 1).unwrap();
        assert_eq!(
            second.entity_id, first.entity_id,
            "cross-type exact name match must reuse the existing node, not fragment"
        );
        assert_eq!(second.tier, ResolutionTier::Exact);
        wtxn.commit().unwrap();
    }

    #[test]
    fn cross_type_exact_declines_on_homograph_ambiguity() {
        // A normalized name shared by two DIFFERENT-typed entities is a genuine
        // homograph ("Apple" the company vs the fruit). Cross-type reuse must
        // NOT pick one arbitrarily — with >1 match it falls through to mint a
        // distinct entity.
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        // Mint "Apple" as an Organization (registers the Organization type).
        let wtxn = d.write_txn().unwrap();
        let org = resolve_or_create(&wtxn, test_scope(), "Apple", "brain:Organization", 0.9, NOW)
            .unwrap();
        wtxn.commit().unwrap();
        // Seed a second "apple" under the (already-registered) Person type.
        let person_apple = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Apple".into(),
            normalize_name("Apple"),
            NOW,
        );
        let person_id = person_apple.id;
        let wtxn = d.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), &person_apple).unwrap();
        wtxn.commit().unwrap();
        // Now resolve "Apple" under a THIRD type: two cross-type matches exist
        // → ambiguous → mint a fresh entity rather than conflate them.
        let wtxn = d.write_txn().unwrap();
        let res =
            resolve_or_create(&wtxn, test_scope(), "Apple", "brain:Event", 0.9, NOW + 1).unwrap();
        assert!(
            res.entity_id != org.entity_id && res.entity_id != person_id,
            "ambiguous cross-type homograph must not be reused"
        );
        assert_eq!(res.tier, ResolutionTier::Created);
        wtxn.commit().unwrap();
    }

    #[test]
    fn empty_surface_form_is_rejected() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let wtxn = d.write_txn().unwrap();
        let err = resolve_or_create(&wtxn, test_scope(), "   ", "brain:Person", 0.5, NOW)
            .expect_err("empty");
        assert!(matches!(err, ResolverError::EmptyNormalizedName));
    }

    #[test]
    fn unknown_entity_type_qname_is_interned_on_demand() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let wtxn = d.write_txn().unwrap();
        let res = resolve_or_create(
            &wtxn,
            test_scope(),
            "Acme Corp",
            "brain:Organization",
            0.7,
            NOW,
        )
        .unwrap();
        assert_eq!(res.tier, ResolutionTier::Created);
        wtxn.commit().unwrap();
        // The new type lives in the registry now.
        let d = d;
        let wtxn = d.write_txn().unwrap();
        let def = entity_type_lookup_by_name(&wtxn, "Organization").unwrap();
        assert!(def.is_some());
        wtxn.commit().unwrap();
    }

    // ----- Tier 3 embedding -----------------------------------------------

    use brain_embed::EmbedError;
    use brain_index::entity_hnsw::EntityHnswParams;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    /// Deterministic embedder driven by a `name → vector` table.
    /// Surface forms not in the table return a unit-axis vector that
    /// is far from every fixture (axis chosen via blake3 hash) so the
    /// "below threshold" branch is reproducible.
    struct ScriptedEmbedder {
        table: StdMutex<HashMap<String, [f32; VECTOR_DIM]>>,
    }

    impl ScriptedEmbedder {
        fn new() -> Self {
            Self {
                table: StdMutex::new(HashMap::new()),
            }
        }

        fn set(&self, key: &str, v: [f32; VECTOR_DIM]) {
            self.table.lock().unwrap().insert(key.to_string(), v);
        }
    }

    impl Dispatcher for ScriptedEmbedder {
        fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            if let Some(v) = self.table.lock().unwrap().get(text).copied() {
                return Ok(v);
            }
            // Fallback: deterministic far vector keyed off the text
            // hash. Distinct keys land on distinct axes so they're
            // orthogonal (cosine = 0) to the fixture vectors.
            let h = blake3::hash(text.as_bytes());
            let axis = (u32::from_le_bytes([
                h.as_bytes()[0],
                h.as_bytes()[1],
                h.as_bytes()[2],
                h.as_bytes()[3],
            ]) as usize
                % (VECTOR_DIM - 32))
                + 32;
            let mut v = [0.0_f32; VECTOR_DIM];
            v[axis] = 1.0;
            Ok(v)
        }

        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            texts.iter().map(|t| self.embed(t)).collect()
        }

        fn fingerprint(&self) -> [u8; 16] {
            [0x77; 16]
        }
    }

    /// Build a sparse unit vector that is mostly along axis `peak` plus
    /// a small share at `co`. Lets tests stage two surface forms with
    /// a chosen cosine between them.
    fn shared_axis(peak: usize, co: usize, peak_w: f32, co_w: f32) -> [f32; VECTOR_DIM] {
        let mut v = [0.0_f32; VECTOR_DIM];
        v[peak] = peak_w;
        v[co] = co_w;
        // L2-normalise so cosine ≈ dot.
        let norm = (peak_w * peak_w + co_w * co_w).sqrt();
        if norm > 0.0 {
            v[peak] /= norm;
            v[co] /= norm;
        }
        v
    }

    fn fresh_hnsw() -> Arc<RwLock<EntityHnswIndex>> {
        Arc::new(RwLock::new(
            EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap(),
        ))
    }

    fn deps(embedder: Arc<ScriptedEmbedder>, hnsw: Arc<RwLock<EntityHnswIndex>>) -> EmbeddingDeps {
        EmbeddingDeps {
            hnsw,
            embedder: embedder as Arc<dyn Dispatcher>,
            embed_threshold: EMBED_RESOLVE_THRESHOLD,
        }
    }

    /// Resolve with the embedding tier wired and publish whatever tier-4
    /// staged, mirroring the production `commit()`-then-flush sequence
    /// (these tests commit on the next line and never roll back, so the
    /// flush is ordered with the commit either way).
    fn resolve_and_publish(
        wtxn: &WriteTransaction,
        scope: RowScope,
        surface_form: &str,
        entity_type_qname: &str,
        confidence: f32,
        now_unix_nanos: u64,
        embed_deps: Option<&EmbeddingDeps>,
    ) -> Result<Resolution, ResolverError> {
        let mut staged = StagedEntityVectors::new();
        let res = resolve_or_create_with_deps(
            wtxn,
            scope,
            surface_form,
            entity_type_qname,
            confidence,
            now_unix_nanos,
            embed_deps,
            &mut staged,
            &mut Disambiguation::Off,
        );
        if let Some(deps) = embed_deps {
            staged.flush_into_hnsw(deps);
        }
        res
    }

    /// Stage an entity in redb + the HNSW with a chosen embedding.
    fn seed_entity(
        d: &mut MetadataDb,
        hnsw: &Arc<RwLock<EntityHnswIndex>>,
        type_id: EntityTypeId,
        canonical: &str,
        vector: [f32; VECTOR_DIM],
    ) -> EntityId {
        let id = EntityId::new();
        let ent = Entity::new_active(
            id,
            type_id,
            canonical.into(),
            normalize_name(canonical),
            NOW,
        );
        let wtxn = d.write_txn().unwrap();
        entity_put(&wtxn, test_scope(), &ent).unwrap();
        wtxn.commit().unwrap();
        hnsw.write().insert(id, &vector).unwrap();
        id
    }

    #[test]
    fn tier_embedding_resolves_near_paraphrase() {
        // "Stripe Inc." sits at a dominant peak; "Stripe Payments"
        // shares ~85 % of that peak with a sliver elsewhere — cosine
        // ≈ 0.85, well above the 0.78 default threshold.
        let stripe_inc_v = shared_axis(10, 11, 1.0, 0.0);
        let stripe_payments_v = shared_axis(10, 11, 0.95, 0.31);

        let embedder = Arc::new(ScriptedEmbedder::new());
        embedder.set("Stripe Payments", stripe_payments_v);

        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let hnsw = fresh_hnsw();
        let target_id = seed_entity(
            &mut d,
            &hnsw,
            brain_core::EntityType::PERSON_ID,
            "Stripe Inc.",
            stripe_inc_v,
        );

        let deps = deps(embedder, hnsw);
        let wtxn = d.write_txn().unwrap();
        let res = resolve_and_publish(
            &wtxn,
            test_scope(),
            "Stripe Payments",
            "brain:Person",
            0.9,
            NOW + 1,
            Some(&deps),
        )
        .unwrap();
        wtxn.commit().unwrap();

        assert_eq!(res.entity_id, target_id);
        assert_eq!(res.tier, ResolutionTier::Embedding);

        // Alias was added — next resolve hits tier-2 directly.
        let rtxn = d.read_txn().unwrap();
        let got = entity_get(&rtxn, target_id).unwrap().unwrap();
        assert!(
            got.aliases.iter().any(|a| a == "Stripe Payments"),
            "tier-3 should add the surface form as an alias; got {:?}",
            got.aliases,
        );
    }

    #[test]
    fn tier_embedding_below_threshold_falls_through() {
        // Two unrelated vectors → cosine ≈ 0, well below 0.78. The
        // resolver must create a fresh entity instead of returning
        // the seed.
        let stripe_v = shared_axis(10, 11, 1.0, 0.0);
        let bitcoin_v = shared_axis(200, 201, 1.0, 0.0);

        let embedder = Arc::new(ScriptedEmbedder::new());
        embedder.set("Bitcoin", bitcoin_v);

        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let hnsw = fresh_hnsw();
        let seed_id = seed_entity(
            &mut d,
            &hnsw,
            brain_core::EntityType::PERSON_ID,
            "Stripe Inc.",
            stripe_v,
        );

        let deps = deps(embedder, hnsw.clone());
        let wtxn = d.write_txn().unwrap();
        let res = resolve_and_publish(
            &wtxn,
            test_scope(),
            "Bitcoin",
            "brain:Person",
            0.9,
            NOW + 1,
            Some(&deps),
        )
        .unwrap();
        wtxn.commit().unwrap();

        assert_eq!(res.tier, ResolutionTier::Created);
        assert_ne!(res.entity_id, seed_id);

        // Tier-4 also populates the HNSW so the next paraphrase of
        // "Bitcoin" can resolve via tier-3b.
        assert!(hnsw.read().contains(res.entity_id));
    }

    #[test]
    fn tier_embedding_respects_entity_type() {
        // A Person and an Organization share the SAME embedding peak, so
        // the HNSW returns both at the top. The embedding tier's type
        // filter must drop the Organization candidate before the
        // threshold check, resolving to the Person.
        //
        // The query is a paraphrase that surface-matches neither seeded
        // name (shares only the leading token, like "Stripe Payments" vs
        // "Stripe Inc." in `tier_embedding_resolves_near_paraphrase`), so
        // the exact / alias / partial-name tiers all miss and tier-3
        // (embedding) is the one under test. ("Wong" alone would be a
        // partial-name subset of both seeded names and resolve earlier.)
        let shared_v = shared_axis(42, 43, 1.0, 0.0);
        // Slightly off-axis for the Org so the HNSW orders the Person
        // higher when the query vector matches the Person exactly — but
        // this test is really about the type filter rejecting the
        // wrong-type top hit.
        let org_v = shared_axis(42, 43, 0.999, 0.045);

        let embedder = Arc::new(ScriptedEmbedder::new());
        embedder.set("Wong Group", shared_v);

        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let hnsw = fresh_hnsw();

        // Intern an Organization type so we can seed a cross-type entity.
        let org_type_id = {
            let wtxn = d.write_txn().unwrap();
            let id = entity_type_intern(&wtxn, "Organization", Vec::new(), NOW).unwrap();
            wtxn.commit().unwrap();
            id
        };

        let wong_person_id = seed_entity(
            &mut d,
            &hnsw,
            brain_core::EntityType::PERSON_ID,
            "Wong Industries",
            shared_v,
        );
        let _org_id = seed_entity(&mut d, &hnsw, org_type_id, "Wong Holdings", org_v);

        let deps = deps(embedder, hnsw);
        let wtxn = d.write_txn().unwrap();
        let res = resolve_and_publish(
            &wtxn,
            test_scope(),
            "Wong Group",
            "brain:Person",
            0.9,
            NOW + 1,
            Some(&deps),
        )
        .unwrap();
        wtxn.commit().unwrap();

        assert_eq!(res.entity_id, wong_person_id);
        assert_eq!(res.tier, ResolutionTier::Embedding);
    }

    #[test]
    fn tier_create_populates_entity_hnsw() {
        // When tier-4 fires with embed_deps, the resolver embeds the
        // canonical_name and inserts into the HNSW so the next
        // paraphrase resolve can hit tier-3b instead of minting again.
        let canonical_v = shared_axis(77, 78, 1.0, 0.0);
        let paraphrase_v = shared_axis(77, 78, 0.95, 0.31);

        let embedder = Arc::new(ScriptedEmbedder::new());
        embedder.set("Brand New Co", canonical_v);
        embedder.set("Brand New Company", paraphrase_v);

        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let hnsw = fresh_hnsw();

        let deps = deps(embedder, hnsw.clone());
        let wtxn = d.write_txn().unwrap();
        let r1 = resolve_and_publish(
            &wtxn,
            test_scope(),
            "Brand New Co",
            "brain:Person",
            0.9,
            NOW,
            Some(&deps),
        )
        .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(r1.tier, ResolutionTier::Created);
        assert!(hnsw.read().contains(r1.entity_id));

        // Second resolve with a paraphrase hits tier-3b.
        let wtxn = d.write_txn().unwrap();
        let r2 = resolve_and_publish(
            &wtxn,
            test_scope(),
            "Brand New Company",
            "brain:Person",
            0.9,
            NOW + 1,
            Some(&deps),
        )
        .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(r2.entity_id, r1.entity_id);
        assert_eq!(r2.tier, ResolutionTier::Embedding);
    }

    #[test]
    fn tier_partial_match_enqueues_proposal() {
        // Cosine in the [0.7, 0.78) band: NOT auto-aliased; the
        // resolver creates a fresh entity AND enqueues a Pending
        // proposal flagging the close-but-not-confident candidate.
        let acme_v = shared_axis(50, 51, 1.0, 0.0);
        // Cosine with acme_v ≈ peak^2 = 0.75^2 + 0.661^2 * (0/0) … use a
        // sparse construction that gives a known cosine.
        // shared_axis L2-normalises (peak/sqrt(p^2+co^2), co/sqrt(...)),
        // so the cosine of two such vectors that share peak axis 50 and
        // differ in their co axis is the dot product = peak1*peak2 +
        // co1*co2 where each pair lies on the unit circle. With weights
        // (1.0, 0.0) and (0.75, 0.661) we get cosine ≈ 0.75.
        let acme_holdings_v = shared_axis(50, 51, 0.75, 0.661);

        let embedder = Arc::new(ScriptedEmbedder::new());
        embedder.set("Acme Holdings", acme_holdings_v);
        // Also stage the canonical name embedding for tier-4 self-insert.
        embedder.set("Acme Holdings", acme_holdings_v);

        let dir = TempDir::new().unwrap();
        let mut d = db(&dir);
        let hnsw = fresh_hnsw();
        let acme_id = seed_entity(
            &mut d,
            &hnsw,
            brain_core::EntityType::PERSON_ID,
            "Acme",
            acme_v,
        );

        let deps_holder = deps(embedder, hnsw.clone());
        let wtxn = d.write_txn().unwrap();
        let res = resolve_and_publish(
            &wtxn,
            test_scope(),
            "Acme Holdings",
            "brain:Person",
            0.9,
            NOW + 1,
            Some(&deps_holder),
        )
        .unwrap();
        wtxn.commit().unwrap();

        // Resolver MUST NOT have merged or aliased — fresh entity.
        assert_eq!(res.tier, ResolutionTier::Created);
        assert_ne!(res.entity_id, acme_id);

        // A Pending merge proposal points from the new entity to Acme.
        let rtxn = d.read_txn().unwrap();
        let pending = brain_metadata::entity::review::list_proposals_by_status(
            &rtxn,
            brain_metadata::tables::merge_review_queue::proposal_status::PENDING,
            16,
        )
        .unwrap();
        assert_eq!(pending.len(), 1, "exactly one Pending proposal");
        let proposal = &pending[0];
        assert_eq!(proposal.source_entity, res.entity_id.to_bytes());
        assert_eq!(proposal.candidate_entity, acme_id.to_bytes());
        assert!(
            proposal.confidence >= PARTIAL_MATCH_FLOOR
                && proposal.confidence < EMBED_RESOLVE_THRESHOLD,
            "confidence {} not in partial-match band",
            proposal.confidence,
        );
        assert_eq!(
            proposal.tier_that_proposed,
            brain_metadata::tables::merge_review_queue::proposal_tier::EMBEDDING,
        );
    }

    #[test]
    fn tier_embedding_skipped_when_deps_absent() {
        // Resolver compatibility: with `None` deps it never consults
        // the HNSW. Exercised by the existing `resolve_or_create`
        // entrypoint already, but pinned here as a regression guard
        // because the worker can momentarily run without deps wired
        // (test fixtures, substrate-only deployments).
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let wtxn = d.write_txn().unwrap();
        let res = resolve_and_publish(&wtxn, test_scope(), "Solo", "brain:Person", 0.9, NOW, None)
            .unwrap();
        assert_eq!(res.tier, ResolutionTier::Created);
        wtxn.commit().unwrap();
    }

    // ----- Disambiguator helpers (pure-logic) -----------------------------

    #[test]
    fn parse_confirm_reply_accepts_canonical_forms() {
        assert_eq!(
            parse_confirm_reply("YES 0.92\n"),
            Some(SameEntityReply::Yes(0.92)),
        );
        assert_eq!(
            parse_confirm_reply("yes 0.5"),
            Some(SameEntityReply::Yes(0.5)),
        );
        assert_eq!(parse_confirm_reply("NO"), Some(SameEntityReply::No));
        assert_eq!(parse_confirm_reply("no\n"), Some(SameEntityReply::No));
        assert_eq!(
            parse_confirm_reply("UNCERTAIN"),
            Some(SameEntityReply::Uncertain),
        );
        assert_eq!(
            parse_confirm_reply("uncertain"),
            Some(SameEntityReply::Uncertain),
        );
    }

    #[test]
    fn parse_confirm_reply_rejects_out_of_range_confidence() {
        assert!(parse_confirm_reply("YES -0.1").is_none());
        assert!(parse_confirm_reply("YES 1.5").is_none());
    }

    #[test]
    fn parse_confirm_reply_rejects_garbage() {
        assert!(parse_confirm_reply("").is_none());
        assert!(parse_confirm_reply("MAYBE").is_none());
        assert!(parse_confirm_reply("YES").is_none());
        assert!(parse_confirm_reply("YES not-a-number").is_none());
    }

    #[test]
    fn build_confirm_user_prompt_renders_surface_form_and_aliases() {
        let view = LlmCandidateView {
            entity_id: EntityId::new(),
            canonical_name: "Priya Patel".into(),
            aliases: vec!["Priya".into()],
            entity_type_name: "Person".into(),
        };
        let out = build_confirm_user_prompt(&view, "Priya P.");
        assert!(out.contains("Surface form: Priya P."));
        assert!(out.contains("Canonical name: Priya Patel"));
        assert!(out.contains("Type: Person"));
        assert!(out.contains("Aliases: Priya"));
        assert!(out.contains("YES <confidence>"));
    }

    #[test]
    fn build_confirm_user_prompt_omits_alias_line_when_empty() {
        let view = LlmCandidateView {
            entity_id: EntityId::new(),
            canonical_name: "Solo".into(),
            aliases: vec![],
            entity_type_name: "Person".into(),
        };
        let out = build_confirm_user_prompt(&view, "solo");
        assert!(!out.contains("Aliases:"));
        assert!(out.contains("Canonical name: Solo"));
    }
}
