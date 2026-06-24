//! `LlmExtractor` — Tier 3 of the extractor pipeline.
//!
//! Wraps an [`LlmClient`] (Anthropic / OpenAI) behind the
//! [`Extractor`] trait. Adds a per-shard response cache, a per-call
//! cost budget, JSON-schema validation with one retry on schema
//! failure, and output projection to `EntityMention` /
//! `StatementMention` / `RelationMention` per the extractor's
//! [`ExtractorTarget`].
//!
//! ## Degraded mode
//!
//! Mirrors [`crate::classifier::ClassifierExtractor`]: when no
//! `LlmClient` is configured (env keys unset, unknown model prefix,
//! etc.) the materializer constructs a degraded extractor that
//! returns `SkippedDisabled(reason)` on every dispatch. The captured
//! reason still surfaces in the audit row so operators can see why
//! no LLM ran.

use std::sync::Arc;
use std::time::Duration;

use brain_core::{ExtractorId, Memory};
use brain_core::{ExtractorKind, Statement, StatementObject, StatementValue, SubjectRef};
use brain_llm::types::SystemBlock;
use brain_llm::{LlmClient, LlmError, LlmMessage, LlmRequest, LlmRole};
use brain_metadata::entity::ops::entity_get;
use brain_metadata::schema::predicate::predicate_get;
use brain_metadata::statement::{JudgeError, JudgeFuture, JudgeVerdict, StatementJudge};
use brain_metadata::LlmCacheDb;
use brain_protocol::schema::ast::StatementKindAst;
use brain_protocol::schema::ExtractorTarget;
use jsonschema::JSONSchema;
use parking_lot::Mutex;
use redb::ReadTransaction;
use serde_json::Value;

use super::cache::{cache_get, cache_put};
use super::pricing::{estimate_cost, CostBudget, Pricing};
use super::validation::validate_against;
use crate::framework::extractor::{
    ExtractionContext, ExtractionFailureClass, ExtractionFuture, ExtractionResult,
    ExtractionStatus, Extractor, ExtractorContext, NeighborMemory,
};
use crate::framework::item::{EntityMention, ExtractedItem, RelationMention, StatementMention};
use crate::idempotency::hash_memory_text;

const DEFAULT_CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60; // 7 days.

/// Hard cap on per-call input tokens for the LLM extractor. Above this
/// the prompt builder trims sections — first the rolling summary, then
/// the lowest-similarity neighbors — until the request fits. The cap
/// is conservative on purpose: Haiku-class models stay sub-second
/// under ~4k input tokens; pushing past that trades latency for
/// signal that the bounded-context worker already filtered for.
pub(super) const LLM_INPUT_TOKEN_BUDGET: u64 = 4_000;

/// Per-neighbor character cap in the rendered prompt section. The
/// fetch helper truncates at the storage layer (200 chars); this is
/// the same number for self-documentation and a defensive cap in
/// case the helper's cap changes.
const NEIGHBOR_TEXT_CHAR_CAP_IN_PROMPT: usize = 200;

/// Hard cap on the rolling summary's chars in the prompt — the
/// summarizer worker (when wired) is expected to honour this, but
/// we enforce it here too so a misbehaving summarizer can't blow the
/// prompt budget.
const SUMMARY_CHAR_CAP: usize = 500;

/// LLM-tier extractor. Constructed by the materializer
/// (`materialize_llm_extractor`) from an `ExtractorDefinition` row.
pub struct LlmExtractor {
    id: ExtractorId,
    name: String,
    target: ExtractorTarget,
    extractor_version: u32,
    confidence_threshold: f32,
    cost_budget: Option<CostBudget>,
    cache_ttl: Duration,
    pub(super) inner: Option<Arc<LlmExtractorInner>>,
    degraded_reason: Option<String>,
}

/// Fully-wired inner state. Held behind `Option` so degraded
/// extractors can carry just `degraded_reason`.
pub struct LlmExtractorInner {
    pub client: Arc<dyn LlmClient>,
    pub cache: Option<Arc<Mutex<LlmCacheDb>>>,
    pub prompt: String,
    pub examples: Option<Value>,
    pub response_schema: Option<Value>,
    /// Compiled draft-7 validator. `None` when `response_schema`
    /// is absent (free-form response mode).
    pub schema_compiled: Option<JSONSchema>,
    pub pricing: Pricing,
    pub max_tokens: u32,
    pub temperature: f32,
    pub timeout: Duration,
}

impl LlmExtractor {
    /// Fully-wired extractor.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ExtractorId,
        name: String,
        target: ExtractorTarget,
        extractor_version: u32,
        confidence_threshold: f32,
        cost_budget: Option<CostBudget>,
        cache_ttl: Duration,
        inner: LlmExtractorInner,
    ) -> Self {
        Self {
            id,
            name,
            target,
            extractor_version,
            confidence_threshold,
            cost_budget,
            cache_ttl,
            inner: Some(Arc::new(inner)),
            degraded_reason: None,
        }
    }

    /// Flat constructor used by `materialize_llm_extractor`. All
    /// of `LlmExtractorInner`'s fields are passed in directly so
    /// tests can build instances without round-tripping through
    /// the AST.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        id: ExtractorId,
        name: String,
        target: ExtractorTarget,
        extractor_version: u32,
        client: Arc<dyn LlmClient>,
        cache: Option<Arc<Mutex<LlmCacheDb>>>,
        prompt: String,
        examples: Option<Value>,
        response_schema: Option<Value>,
        schema_compiled: Option<JSONSchema>,
        confidence_threshold: f32,
        cost_budget: Option<CostBudget>,
        cache_ttl: Duration,
    ) -> Self {
        let pricing = Pricing::for_model(client.model());
        Self::new(
            id,
            name,
            target,
            extractor_version,
            confidence_threshold,
            cost_budget,
            cache_ttl,
            LlmExtractorInner {
                client,
                cache,
                prompt,
                examples,
                response_schema,
                schema_compiled,
                pricing,
                max_tokens: 1024,
                temperature: 0.0,
                timeout: Duration::from_secs(30),
            },
        )
    }

    /// Degraded extractor — no LLM client wired. Every dispatch
    /// returns `SkippedDisabled(reason)` with the captured cause.
    /// The tier never attempted work, so its status must not feed
    /// the pipeline's partial-failure accounting.
    pub fn degraded(
        id: ExtractorId,
        name: String,
        target: ExtractorTarget,
        extractor_version: u32,
        confidence_threshold: f32,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id,
            name,
            target,
            extractor_version,
            confidence_threshold,
            cost_budget: None,
            cache_ttl: Duration::from_secs(DEFAULT_CACHE_TTL_SECS),
            inner: None,
            degraded_reason: Some(reason.into()),
        }
    }

    /// True iff a real LLM client is wired in.
    #[must_use]
    pub fn is_wired(&self) -> bool {
        self.inner.is_some()
    }

    /// Compile the operator-declared JSON schema. Returns `None`
    /// when `schema` is `None`; otherwise either the compiled
    /// validator or `Err` describing why compilation failed.
    pub fn compile_schema(schema: Option<&Value>) -> Result<Option<JSONSchema>, String> {
        match schema {
            None => Ok(None),
            Some(v) => match JSONSchema::options().compile(v) {
                Ok(c) => Ok(Some(c)),
                Err(e) => Err(format!("schema compile failed: {e}")),
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_request(
        &self,
        inner: &LlmExtractorInner,
        memory_id: brain_core::MemoryId,
        memory_text: &str,
        prior_entities: &[&EntityMention],
        extractor_context: Option<&ExtractorContext>,
        declared_predicates: Option<&str>,
        declared_kinds: Option<&str>,
        now_unix_nanos: u64,
    ) -> (LlmRequest, BuildRequestStats) {
        // Fill the active-schema predicate block once, before the
        // per-memory render. Closed-vocab extraction tracks the active
        // schema (system + user uploads) without baking the predicate
        // list into the template. A template without the placeholder is
        // left unchanged; an unfilled placeholder renders empty.
        let prompt = inner
            .prompt
            .replace("{DECLARED_PREDICATES}", declared_predicates.unwrap_or(""))
            .replace("{DECLARED_KINDS}", declared_kinds.unwrap_or(""));
        let (user_body, stats) = render_prompt_with_context(
            &prompt,
            memory_text,
            prior_entities,
            extractor_context,
            now_unix_nanos,
            memory_id,
        );
        // Split the system context into two cacheable blocks: the role
        // block (stable across every call) and the schema block (stable
        // per schema version). Both are eligible for Anthropic's
        // ephemeral prompt cache. The per-call body lives in the user
        // message and is never cached — that's the surface area that
        // carries the bounded inferential context.
        let request = LlmRequest {
            model: inner.client.model().to_string(),
            system_blocks: build_extractor_system_blocks(inner.examples.as_ref()),
            messages: vec![brain_llm::LlmMessage {
                role: brain_llm::LlmRole::User,
                content: user_body,
            }],
            response_schema: inner.response_schema.clone(),
            temperature: inner.temperature,
            max_tokens: inner.max_tokens,
            timeout: inner.timeout,
        };
        (request, stats)
    }

    fn project_value(&self, parsed: &Value) -> Vec<ExtractedItem> {
        // Three response shapes are supported:
        //   1. `[item, item, ...]` — flat array. Each item is projected
        //      through the extractor's declared target.
        //   2. `{ "statements": [...], "relations": [...] }` — the
        //      schema prompt's structured shape. Each section is
        //      projected through the matching projector regardless of
        //      the extractor's declared target so a single LLM call can
        //      cover both kinds.
        //   3. A single item — wrapped in a one-element vec, same as
        //      shape 1.
        let mut out = Vec::new();
        match parsed {
            Value::Object(obj)
                if obj.contains_key("statements") || obj.contains_key("relations") =>
            {
                if let Some(Value::Array(arr)) = obj.get("statements") {
                    for v in arr {
                        if let Some(item) =
                            project_statement_open(v, self.id.raw(), self.extractor_version)
                        {
                            if item.confidence() >= self.confidence_threshold {
                                out.push(item);
                            }
                        }
                    }
                }
                if let Some(Value::Array(arr)) = obj.get("relations") {
                    for v in arr {
                        if let Some(item) =
                            project_relation_open(v, self.id.raw(), self.extractor_version)
                        {
                            if item.confidence() >= self.confidence_threshold {
                                out.push(item);
                            }
                        }
                    }
                }
            }
            Value::Array(arr) => {
                for v in arr {
                    if let Some(item) = self.project_one(v) {
                        if item.confidence() >= self.confidence_threshold {
                            out.push(item);
                        }
                    }
                }
            }
            v => {
                if let Some(item) = self.project_one(v) {
                    if item.confidence() >= self.confidence_threshold {
                        out.push(item);
                    }
                }
            }
        }
        out
    }

    fn project_one(&self, v: &Value) -> Option<ExtractedItem> {
        match &self.target {
            ExtractorTarget::Entity { entity_type } => {
                project_entity(v, entity_type, self.id.raw(), self.extractor_version)
            }
            ExtractorTarget::Statement { kind } => project_statement(
                v,
                kind_to_byte(*kind),
                self.id.raw(),
                self.extractor_version,
            ),
            ExtractorTarget::Relation { relation_type } => {
                project_relation(v, relation_type, self.id.raw(), self.extractor_version)
            }
            ExtractorTarget::EntityOrStatement => {
                if v.get("predicate").is_some() {
                    project_statement(v, 1, self.id.raw(), self.extractor_version)
                } else if v.is_string() || v.get("name").is_some() {
                    project_entity(v, "brain:Entity", self.id.raw(), self.extractor_version)
                } else {
                    None
                }
            }
        }
    }

    /// Run the supersession judge over a pair of statements. Tier 2 of
    /// the [`brain_metadata::statement::TieredSupersedeDecider`] ladder
    /// calls this when a candidate's cosine sits in the ambiguity band
    /// (typically `[0.82, 0.92)`). Returns `Supersedes` /
    /// `Contradicts` / `Coexists` per the prompt below.
    ///
    /// The judge reuses the LLM extractor's wired client + cache + cost
    /// budget so deployments don't need a second transport — the
    /// per-call budget guards against runaway costs, and the role
    /// block ships through the same Anthropic prompt-cache breakpoint
    /// as the extractor's role block so the steady-state input-token
    /// bill collapses on repeated judge invocations.
    pub async fn judge_supersedes_call(
        &self,
        new: &Statement,
        existing: &Statement,
        rtxn: &ReadTransaction,
    ) -> Result<JudgeVerdict, JudgeError> {
        let Some(inner) = self.inner.as_ref() else {
            return Err(JudgeError::Transport(
                "judge unavailable: LLM extractor is degraded (no client wired)".into(),
            ));
        };

        let new_human = render_statement_human_readable(new, rtxn)
            .map_err(|e| JudgeError::Transport(format!("render new statement: {e}")))?;
        let old_human = render_statement_human_readable(existing, rtxn)
            .map_err(|e| JudgeError::Transport(format!("render existing statement: {e}")))?;

        let user_body = format!(
            "Two extracted statements share the same subject. Decide their relationship:\n\
             - SUPERSEDES if the new statement replaces the old (same attribute, new value).\n\
             - CONTRADICTS if both can't be true at once.\n\
             - COEXISTS if both can be true simultaneously (e.g. multiple skills, sequential roles).\n\n\
             Existing: {old_human}\n\
             New: {new_human}\n\n\
             Respond with exactly one word: SUPERSEDES, CONTRADICTS, or COEXISTS."
        );

        let request = LlmRequest {
            model: inner.client.model().to_string(),
            system_blocks: vec![SystemBlock::cached(JUDGE_ROLE_BLOCK)],
            messages: vec![LlmMessage {
                role: LlmRole::User,
                content: user_body,
            }],
            response_schema: None,
            temperature: 0.0,
            // Single-token verdict; cap budget tightly so a runaway
            // model can't blow the per-call ceiling.
            max_tokens: 16,
            timeout: inner.timeout,
        };

        if let Some(budget) = self.cost_budget {
            let est = estimate_cost(&request, &inner.pricing);
            if est > budget.per_call_micro_usd {
                return Err(JudgeError::Budget(format!(
                    "estimated {} µ$ exceeds per-call budget {} µ$",
                    est, budget.per_call_micro_usd
                )));
            }
        }

        let resp = inner
            .client
            .complete(request)
            .await
            .map_err(|e| JudgeError::Transport(llm_error_reason(&e)))?;
        parse_verdict(&resp.content)
    }
}

/// Stable role block sent on every judge call. The prompt cache reads
/// it on repeated calls so the steady-state input cost is bounded by
/// the per-call statement render only.
const JUDGE_ROLE_BLOCK: &str = "You are a knowledge-graph supersession judge for Brain. \
     Given two extracted statements about the same subject, decide whether the new statement \
     supersedes the old (same attribute, new value), contradicts it (both cannot be true at \
     once), or coexists with it (both can be true simultaneously — multiple skills, sequential \
     roles, parallel facts). Be conservative: if uncertain, prefer COEXISTS. Respond with one \
     word.";

/// Parse the judge's reply. Tolerant to surrounding punctuation,
/// trailing periods, and case — but rejects anything that doesn't
/// match one of the three verdicts so a model that drifted into
/// free-form explanation surfaces as a Parse error rather than
/// silently picking a default.
pub(super) fn parse_verdict(s: &str) -> Result<JudgeVerdict, JudgeError> {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    match cleaned.as_str() {
        x if x.starts_with("SUPERSEDES") => Ok(JudgeVerdict::Supersedes),
        x if x.starts_with("CONTRADICTS") => Ok(JudgeVerdict::Contradicts),
        x if x.starts_with("COEXISTS") => Ok(JudgeVerdict::Coexists),
        _ => Err(JudgeError::Parse(format!("unrecognized verdict: {s:?}"))),
    }
}

/// Render a [`Statement`] into a one-line human-readable phrase using
/// the subject's canonical name and the predicate's qname. Used by
/// the judge to give the LLM a stable, terse rendering of the pair.
fn render_statement_human_readable(
    s: &Statement,
    rtxn: &ReadTransaction,
) -> Result<String, String> {
    let subject = match s.subject {
        SubjectRef::Entity(eid) => match entity_get(rtxn, eid) {
            Ok(Some(e)) => e.canonical_name,
            Ok(None) => format!("entity:{eid:?}"),
            Err(e) => return Err(format!("entity_get: {e}")),
        },
        SubjectRef::Memory(mid) => format!("memory:{mid:?}"),
        SubjectRef::Pending(audit) => format!("pending:{audit:?}"),
    };
    let predicate = match predicate_get(rtxn, s.predicate) {
        Ok(Some(p)) => p.canonical(),
        Ok(None) => format!("predicate:{}", s.predicate.raw()),
        Err(e) => return Err(format!("predicate_get: {e}")),
    };
    let object = match &s.object {
        StatementObject::Entity(eid) => match entity_get(rtxn, *eid) {
            Ok(Some(e)) => e.canonical_name,
            Ok(None) => format!("entity:{eid:?}"),
            Err(e) => return Err(format!("entity_get(object): {e}")),
        },
        StatementObject::Value(v) => render_value(v),
        StatementObject::Memory(mid) => format!("memory:{mid:?}"),
        StatementObject::Statement(sid) => format!("statement:{sid:?}"),
    };
    Ok(format!("{subject} {predicate} {object}"))
}

fn render_value(v: &StatementValue) -> String {
    match v {
        StatementValue::Text(s) => s.clone(),
        StatementValue::Integer(i) => i.to_string(),
        StatementValue::Float(f) => f.to_string(),
        StatementValue::Bool(b) => b.to_string(),
        StatementValue::UnixNanos(n) => format!("unix_ns:{n}"),
        StatementValue::Blob(b) => format!("<blob:{}>", b.len()),
    }
}

impl StatementJudge for LlmExtractor {
    fn judge_supersedes<'a>(
        &'a self,
        new_stmt: &'a Statement,
        existing_stmt: &'a Statement,
        rtxn: &'a ReadTransaction,
    ) -> JudgeFuture<'a> {
        Box::pin(async move {
            self.judge_supersedes_call(new_stmt, existing_stmt, rtxn)
                .await
        })
    }
}

/// Pull `EntityMention`s for `memory_id` out of `ctx.prior_tier_items`.
/// Statement / relation mentions are filtered out — the LLM tier owns
/// predicate and relation extraction, and forwarding lower-tier guesses
/// would muddy its prompt. Returns an empty slice when no prior tier
/// ran (e.g. first tier in the pipeline) or no entities were extracted
/// for this memory.
pub(super) fn collect_prior_entities<'a>(
    ctx: &'a ExtractionContext<'a>,
    memory_id: brain_core::MemoryId,
) -> Vec<&'a EntityMention> {
    let Some(map) = ctx.prior_tier_items else {
        return Vec::new();
    };
    let Some(items) = map.get(&memory_id) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            ExtractedItem::EntityMention(em) => Some(em),
            _ => None,
        })
        .collect()
}

/// Constant role block sent on every LLM extractor call. Stable text
/// — Anthropic's ephemeral prompt cache will serve it as a cache hit
/// after the first call within the 5-minute window, slashing the
/// input-token cost of high-volume extraction.
const ROLE_BLOCK: &str = "You are an entity, statement, and relation extractor for Brain. \
Read the user's text and emit a JSON response that conforms to the response schema. \
Be conservative: only emit items you are confident about, and include a confidence score in [0.0, 1.0] on each. \
Do not invent entities that are not present in the text.";

/// Build the system blocks for an extractor call. Returns up to two
/// blocks, both marked `cache: true` so Anthropic's prompt cache
/// amortises the role + schema bytes across repeated calls. When the
/// operator did not declare examples we emit just the role block.
fn build_extractor_system_blocks(examples: Option<&Value>) -> Vec<SystemBlock> {
    let mut blocks = Vec::with_capacity(2);
    blocks.push(SystemBlock::cached(ROLE_BLOCK));
    if let Some(ex) = examples {
        let schema_text = format!(
            "Schema examples (the active schema declares these as canonical shapes):\n{}",
            serde_json::to_string_pretty(ex).unwrap_or_default(),
        );
        blocks.push(SystemBlock::cached(schema_text));
    }
    blocks
}

/// Side-channel stats returned alongside the rendered prompt so the
/// worker can publish `llm_neighbors_included` + `llm_tokens_per_query`
/// metrics without re-parsing the prompt body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BuildRequestStats {
    /// Number of neighbor entries that survived the per-call token
    /// budget and made it into the rendered prompt. Distinct from
    /// the fetch's `top_m` because the budget may have trimmed some.
    pub neighbors_included: usize,
    /// Whether the rolling summary made it into the prompt. False
    /// when the summary was absent, when the worker chose not to
    /// pass one, or when the budget gate dropped it.
    pub summary_included: bool,
    /// Approximate input-token count of the final request, computed
    /// via `LlmRequest::approx_input_tokens()` semantics
    /// (`chars / 4`). The worker observes this on the
    /// `llm_tokens_per_query` histogram.
    pub approx_input_tokens: u64,
}

/// Render the complete user-message body: the operator's template
/// (with `{TEXT}` + `{PRIOR_ENTITIES}` substituted) plus the
/// bounded-context sections (`Recent context`, optional
/// `Rolling summary`). The context sections precede the memory text
/// so a reader (and the LLM) sees the history first, then the
/// current memory.
fn render_full_prompt(
    template: &str,
    memory_text: &str,
    prior_entities: &[&EntityMention],
    neighbors: &[&NeighborMemory],
    summary: Option<&str>,
    now_unix_nanos: u64,
    memory_id: brain_core::MemoryId,
) -> String {
    let base = render_prompt(template, memory_text, prior_entities, memory_id);
    let mut bounded = String::new();
    if !neighbors.is_empty() {
        bounded.push_str(&format_recent_context_section(neighbors, now_unix_nanos));
    }
    if let Some(s) = summary {
        if !s.is_empty() {
            bounded.push_str(&format_rolling_summary_section(s));
        }
    }
    if bounded.is_empty() {
        return base;
    }
    // Prepend bounded-context sections so the LLM reads "what came
    // before" → "what's now" in source order.
    let mut out = String::with_capacity(bounded.len() + base.len() + 4);
    out.push_str(&bounded);
    out.push_str("\n\n");
    out.push_str(&base);
    out
}

/// Bullet-list of neighbor memories ranked by similarity, with a
/// truncated text body and a "T-Nh" relative-time hint so the LLM
/// can tell recent context from stale priors.
fn format_recent_context_section(neighbors: &[&NeighborMemory], now_unix_nanos: u64) -> String {
    let mut s = String::from("## Recent context\n\n");
    s.push_str(
        "The following are the most similar prior memories from the same \
         context, ranked by similarity. Treat them as background — only \
         emit statements / relations that follow from the current memory \
         text below.\n\n",
    );
    for (i, n) in neighbors.iter().enumerate() {
        let rel = relative_time_hint(now_unix_nanos, n.created_at_unix_nanos);
        let body = truncate_chars(&n.text, NEIGHBOR_TEXT_CHAR_CAP_IN_PROMPT);
        s.push_str(&format!(
            "{}. (similarity={:.2}, {}) \"{}\"\n",
            i + 1,
            n.similarity_score,
            rel,
            body
        ));
    }
    s.push('\n');
    s
}

/// Optional rolling-summary block — single paragraph, capped at
/// [`SUMMARY_CHAR_CAP`] chars so a misbehaving summarizer can't blow
/// the prompt budget single-handedly.
fn format_rolling_summary_section(summary: &str) -> String {
    let mut s = String::from("## Rolling summary\n\n");
    s.push_str(&truncate_chars(summary, SUMMARY_CHAR_CAP));
    s.push_str("\n\n");
    s
}

/// Render a "T-Nh" / "T-Nm" / "T-Ns" relative-time hint between two
/// monotonic timestamps. Both arguments are unix nanos; an `older >
/// now` case (clock skew) collapses to "T+0".
pub(super) fn relative_time_hint(now_unix_nanos: u64, older_unix_nanos: u64) -> String {
    let Some(delta_ns) = now_unix_nanos.checked_sub(older_unix_nanos) else {
        return "T+0".into();
    };
    let delta_secs = delta_ns / 1_000_000_000;
    if delta_secs == 0 {
        return "T-0s".into();
    }
    let days = delta_secs / 86_400;
    if days > 0 {
        return format!("T-{days}d");
    }
    let hours = delta_secs / 3_600;
    if hours > 0 {
        return format!("T-{hours}h");
    }
    let minutes = delta_secs / 60;
    if minutes > 0 {
        return format!("T-{minutes}m");
    }
    format!("T-{delta_secs}s")
}

/// UTF-8-safe char-count truncation with an ellipsis suffix on
/// overflow. Mirrors the storage-layer helper of the same name in
/// `brain_ops::apply::encode_helpers`.
pub(super) fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out = String::with_capacity(max_chars + 1);
    for ch in s.chars().take(max_chars) {
        out.push(ch);
    }
    out.push('…');
    out
}

/// Render the operator-declared prompt template with bounded LLM
/// context. Builds the standard prompt body then
/// inserts `Recent context` + `Rolling summary` sections before the
/// memory text. Enforces the per-call token budget: if the rendered
/// request exceeds [`LLM_INPUT_TOKEN_BUDGET`] the function drops the
/// summary first, then trims neighbors from the lowest-similarity
/// end until the request fits.
pub(super) fn render_prompt_with_context(
    template: &str,
    memory_text: &str,
    prior_entities: &[&EntityMention],
    extractor_context: Option<&ExtractorContext>,
    now_unix_nanos: u64,
    memory_id: brain_core::MemoryId,
) -> (String, BuildRequestStats) {
    // No context wired (or empty) → fall back to the no-context render
    // and skip budget enforcement (the no-context path is already
    // inside the budget).
    let Some(ec) = extractor_context.filter(|c| !c.is_empty()) else {
        let body = render_prompt(template, memory_text, prior_entities, memory_id);
        let approx_input_tokens = approx_tokens_of(&body);
        return (
            body,
            BuildRequestStats {
                neighbors_included: 0,
                summary_included: false,
                approx_input_tokens,
            },
        );
    };

    // Order neighbors by descending similarity so trimming from the
    // tail drops the lowest-signal ones first. The fetch helper
    // returns retriever-order which is the same thing in practice,
    // but we re-sort defensively in case a future helper changes
    // ordering.
    let mut neighbors: Vec<&NeighborMemory> = ec.neighbors.iter().collect();
    neighbors.sort_by(|a, b| {
        b.similarity_score
            .partial_cmp(&a.similarity_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut include_summary = ec.summary.is_some();
    if ec.summary.is_none() {
        tracing::debug!(
            target: "brain_extractors::llm",
            memory_id = ?memory_id,
            "no rolling summary available; passing None to LLM extractor",
        );
    }

    // Render-loop: start with all neighbors and (when present) the
    // summary; if we're over budget, drop the summary; if still over,
    // peel neighbors off the tail until we fit. The memory text +
    // prior-entity block is the inviolable floor — neighbors[0..0]
    // and no summary still leaves the prompt the LLM needs to do its
    // primary job.
    let mut included = neighbors.len();
    loop {
        let body = render_full_prompt(
            template,
            memory_text,
            prior_entities,
            &neighbors[..included],
            if include_summary {
                ec.summary.as_deref()
            } else {
                None
            },
            now_unix_nanos,
            memory_id,
        );
        let tokens = approx_tokens_of(&body);
        if tokens <= LLM_INPUT_TOKEN_BUDGET {
            return (
                body,
                BuildRequestStats {
                    neighbors_included: included,
                    summary_included: include_summary,
                    approx_input_tokens: tokens,
                },
            );
        }
        if include_summary {
            include_summary = false;
            continue;
        }
        if included == 0 {
            // Floor hit. Return what we have — the prompt is still
            // the canonical memory + prior-entity block, the budget
            // just couldn't be met. Operators see this via the
            // `llm_tokens_per_query` p99.
            return (
                body,
                BuildRequestStats {
                    neighbors_included: 0,
                    summary_included: false,
                    approx_input_tokens: tokens,
                },
            );
        }
        included -= 1;
    }
}

/// Approximate input tokens for `s` using the same `chars / 4`
/// heuristic [`LlmRequest::approx_input_tokens`] uses. The system
/// blocks contribute a fixed cost across every call (cached after
/// the first); the per-call budget is dominated by the user message
/// so estimating off the user body alone is conservative enough for
/// gating.
fn approx_tokens_of(s: &str) -> u64 {
    (s.chars().count() as u64) / 4
}

/// Render the operator-declared prompt template against the runtime
/// values for this invocation. Two placeholders are supported:
///
/// - `{TEXT}` — the memory's text.
/// - `{PRIOR_ENTITIES}` — bullet list of entity mentions produced by
///   earlier tiers (pattern + classifier) for this memory. Empty
///   when no prior entities exist.
///
/// Templates that don't include `{TEXT}` get the standard input
/// section appended so prompts authored before the templating layer
/// landed still see the memory text. `{PRIOR_ENTITIES}` is silently
/// no-op when absent — when an operator's prompt doesn't anchor on
/// prior tier output, the LLM falls back to its own extraction.
pub(super) fn render_prompt(
    template: &str,
    memory_text: &str,
    prior_entities: &[&EntityMention],
    memory_id: brain_core::MemoryId,
) -> String {
    let prior_section = if prior_entities.is_empty() {
        String::new()
    } else {
        format_prior_entities_section(prior_entities)
    };
    let mut out = template.to_string();
    let has_text_placeholder = out.contains("{TEXT}");
    out = out.replace("{TEXT}", memory_text);
    out = out.replace("{PRIOR_ENTITIES}", prior_section.trim_end());
    // Detect placeholders the runtime didn't fill. We log+strip rather
    // than panic — a stuck `{FOO}` in a prompt is a configuration bug
    // an operator can fix without losing the in-flight extraction.
    if let Some(unfilled) = find_unfilled_placeholder(&out) {
        tracing::warn!(
            target: "brain_extractors::llm",
            memory_id = ?memory_id,
            placeholder = %unfilled,
            "llm prompt template contains an unfilled placeholder; leaving literal text in place",
        );
    }
    if !has_text_placeholder {
        // Fallback: append the standard text section so prompts that
        // predate the templating layer still receive the memory body.
        out.push_str("\n\nInput text:\n```\n");
        out.push_str(memory_text);
        out.push_str("\n```");
    }
    out
}

fn format_prior_entities_section(prior_entities: &[&EntityMention]) -> String {
    let mut s = String::from("\n\nPreviously extracted entities for this text:\n");
    for em in prior_entities {
        s.push_str(&format!(
            "- \"{}\" -> {} (confidence {:.2})\n",
            em.text, em.entity_type_qname, em.confidence
        ));
    }
    s.push_str(
        "\nUse these entities verbatim as subjects/objects in your output. \
         Do not re-extract them as new entities; only emit statements \
         and relations that involve them.\n",
    );
    s
}

/// Return the first `{IDENT}` placeholder in `s` whose ident is
/// uppercase + underscores only — that's the convention the templating
/// layer reserves. Lowercase / mixed-case curly fragments are left
/// alone so JSON examples in the prompt body don't get flagged.
pub(super) fn find_unfilled_placeholder(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'}' {
                j += 1;
            }
            if j < bytes.len() && j > start {
                let ident = &s[start..j];
                let looks_reserved = !ident.is_empty()
                    && ident
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit());
                if looks_reserved {
                    return Some(ident.to_string());
                }
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    None
}

fn kind_to_byte(k: StatementKindAst) -> u8 {
    // Wire convention is the `brain_core` kind byte + 1.
    match k {
        StatementKindAst::Fact => 1,
        StatementKindAst::Preference => 2,
        StatementKindAst::Event => 3,
        StatementKindAst::Attribute => 4,
        StatementKindAst::Relation => 5,
        StatementKindAst::Directive => 6,
        // `Any` only appears in query AST; extractors targeting
        // `Any` default to Fact (1).
        StatementKindAst::Any => 1,
    }
}

/// Map a kind NAME emitted by the LLM (case-insensitive, trimmed) to the
/// `StatementMention` wire byte (`brain_core` kind byte + 1). Any unknown
/// name falls back to `Fact` (1), the catch-all kind.
fn kind_from_name(s: &str) -> u8 {
    use brain_core::StatementKind;
    let core = match s.trim().to_ascii_lowercase().as_str() {
        "attribute" => StatementKind::Attribute,
        "relation" => StatementKind::Relation,
        "preference" => StatementKind::Preference,
        "event" => StatementKind::Event,
        "directive" => StatementKind::Directive,
        "fact" => StatementKind::Fact,
        _ => StatementKind::Fact,
    };
    core.as_u8() + 1
}

fn read_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

fn read_conf(v: &Value) -> f32 {
    v.get("confidence")
        .and_then(Value::as_f64)
        .map(|f| f as f32)
        .unwrap_or(1.0)
}

/// Whether the LLM marked the statement's object as a referenced entity (vs a
/// literal value). Default false → the apply pass keeps it a text value unless
/// it was already surfaced as an entity or the predicate constrains it.
fn read_object_is_entity(v: &Value) -> bool {
    v.get("object_is_entity")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether the LLM marked this statement's subject as the writing agent
/// itself (first person, in any language). The model judges this from
/// meaning — there is no pronoun list anywhere — so it is language-neutral.
fn read_subject_is_self(v: &Value) -> bool {
    v.get("subject_is_self")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether the LLM marked this statement as a RETRACTION of a prior fact
/// ("not at Google anymore"). The model judges negation from meaning — no
/// keyword list — and the apply pass tombstones the matching current
/// statement(s) instead of creating a new row. Default false = a normal
/// positive assertion.
fn read_retract(v: &Value) -> bool {
    v.get("retract").and_then(Value::as_bool).unwrap_or(false)
}

/// The event time the LLM emits as an ISO date (`"event_at": "2020-01-01"`)
/// for Event-kind statements — resolved to unix-nanos here (models are
/// reliable at dates, not raw epoch-nanos). `None` when absent/unparseable.
fn read_event_at(v: &Value) -> Option<u64> {
    read_str(v, "event_at").and_then(|s| crate::pattern::temporal::parse_event_date(&s))
}

fn project_entity(
    v: &Value,
    entity_type: &str,
    extractor_id: u32,
    extractor_version: u32,
) -> Option<ExtractedItem> {
    let text = if let Some(s) = v.as_str() {
        s.to_string()
    } else {
        read_str(v, "name").or_else(|| read_str(v, "text"))?
    };
    Some(ExtractedItem::EntityMention(EntityMention {
        entity_type_qname: entity_type.to_string(),
        text,
        start: 0,
        end: 0,
        confidence: read_conf(v),
        extractor_id,
        extractor_version,
    }))
}

fn project_statement(
    v: &Value,
    kind: u8,
    extractor_id: u32,
    extractor_version: u32,
) -> Option<ExtractedItem> {
    let predicate = read_str(v, "predicate")?;
    // LLM may emit `"is_stateful": true` to mark a per-extraction
    // statefulness signal. Default false: most LLM-coined facts are
    // cumulative observations, not stateful settings.
    let is_stateful = v
        .get("is_stateful")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(ExtractedItem::StatementMention(StatementMention {
        kind,
        subject_text: read_str(v, "subject"),
        predicate_qname: predicate,
        object_text: read_str(v, "object"),
        confidence: read_conf(v),
        extractor_id,
        extractor_version,
        is_stateful,
        subject_is_memory: false,
        object_is_entity: read_object_is_entity(v),
        event_at_unix_nanos: read_event_at(v),
        subject_is_self: read_subject_is_self(v),
        retract: read_retract(v),
    }))
}

/// Project a statement entry from the structured-output shape
/// `{ "statements": [...] }`. Reads the predicate qname directly off
/// the LLM's emission so a single call can produce statements across
/// any declared predicate; the schema-gating in the worker takes care
/// of routing unknown predicates to `brain:fact`.
fn project_statement_open(
    v: &Value,
    extractor_id: u32,
    extractor_version: u32,
) -> Option<ExtractedItem> {
    let predicate = read_str(v, "predicate")?;
    let is_stateful = v
        .get("is_stateful")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // The LLM may classify the statement via an optional `"kind"` name;
    // absent or unknown falls back to Fact (the catch-all).
    let kind = read_str(v, "kind")
        .map(|s| kind_from_name(&s))
        .unwrap_or_else(|| brain_core::StatementKind::Fact.as_u8() + 1);
    Some(ExtractedItem::StatementMention(StatementMention {
        kind,
        subject_text: read_str(v, "subject"),
        predicate_qname: predicate,
        object_text: read_str(v, "object"),
        confidence: read_conf(v),
        extractor_id,
        extractor_version,
        is_stateful,
        subject_is_memory: false,
        object_is_entity: read_object_is_entity(v),
        event_at_unix_nanos: read_event_at(v),
        subject_is_self: read_subject_is_self(v),
        retract: read_retract(v),
    }))
}

/// Project a relation entry from the structured-output shape
/// `{ "relations": [...] }`. Reads the relation_type qname directly
/// off the LLM's emission.
fn project_relation_open(
    v: &Value,
    extractor_id: u32,
    extractor_version: u32,
) -> Option<ExtractedItem> {
    let relation_type = read_str(v, "relation_type").or_else(|| read_str(v, "type"))?;
    let subject = read_str(v, "from").or_else(|| read_str(v, "subject"))?;
    let object = read_str(v, "to").or_else(|| read_str(v, "object"))?;
    Some(ExtractedItem::RelationMention(RelationMention {
        relation_type_qname: relation_type,
        subject_text: subject,
        object_text: object,
        confidence: read_conf(v),
        extractor_id,
        extractor_version,
    }))
}

fn project_relation(
    v: &Value,
    relation_type: &str,
    extractor_id: u32,
    extractor_version: u32,
) -> Option<ExtractedItem> {
    let subject = read_str(v, "from").or_else(|| read_str(v, "subject"))?;
    let object = read_str(v, "to").or_else(|| read_str(v, "object"))?;
    Some(ExtractedItem::RelationMention(RelationMention {
        relation_type_qname: relation_type.to_string(),
        subject_text: subject,
        object_text: object,
        confidence: read_conf(v),
        extractor_id,
        extractor_version,
    }))
}
impl Extractor for LlmExtractor {
    fn id(&self) -> ExtractorId {
        self.id
    }

    fn kind(&self) -> ExtractorKind {
        ExtractorKind::Llm
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn extractor_version(&self) -> u32 {
        self.extractor_version
    }

    fn is_wired(&self) -> bool {
        self.inner.is_some()
    }

    /// Run the whole micro-batch's LLM calls CONCURRENTLY rather than the
    /// default serial loop. Each memory's `run` is an independent network
    /// round-trip (build prompt → `client.complete().await` → cache →
    /// project), so overlapping them on the executor lane turns an
    /// N×round-trip drain into ~1×. Output stays input-aligned
    /// (`join_all` preserves order). Cost-budget overshoot is bounded by
    /// the micro-batch size (all calls in a batch launch before any
    /// completes); the cross-cycle budget gate still stops the next batch.
    fn run_batch<'a>(
        &'a self,
        ctx: &'a ExtractionContext<'a>,
        mems: &'a [Memory],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ExtractionResult>> + Send + 'a>>
    {
        Box::pin(async move {
            let futs: Vec<_> = mems.iter().map(|m| self.run(ctx, m)).collect();
            futures_util::future::join_all(futs).await
        })
    }

    fn run<'a>(&'a self, ctx: &'a ExtractionContext<'a>, mem: &'a Memory) -> ExtractionFuture<'a> {
        Box::pin(async move {
            let started = ctx.now_unix_nanos;
            let Some(inner) = self.inner.as_ref() else {
                let reason = self
                    .degraded_reason
                    .as_deref()
                    .unwrap_or("llm extractor not wired");
                return ExtractionResult::skipped(
                    ExtractionStatus::SkippedDisabled,
                    reason,
                    started,
                );
            };
            let inner = inner.clone();
            let text = mem.text.as_deref().unwrap_or("");
            let input_hash = hash_memory_text(text);
            let model_id_hash = inner.client.model_id_hash();
            let extractor_id_raw = self.id.raw();
            let extractor_version = self.extractor_version;

            // ----- 1. Cache lookup ---------------------------------------------
            if let Some(cache) = inner.cache.as_ref() {
                if let Some(row) = cache_get(
                    cache,
                    input_hash,
                    extractor_id_raw,
                    extractor_version,
                    model_id_hash,
                ) {
                    return match decode_cached(&row.response_blob, inner.response_schema.as_ref()) {
                        Ok(parsed) => {
                            let items = self.project_value(&parsed);
                            ExtractionResult::success(items, started, started)
                        }
                        Err(e) => ExtractionResult::failure(
                            format!("cache decode failed: {e}"),
                            started,
                            started,
                        ),
                    };
                }
            }

            // ----- 2. Build request + cost budget ------------------------------
            // Pull prior-tier entity mentions for this memory so the
            // LLM gets canonical anchors without re-extracting them.
            // Statement / relation mentions from earlier tiers are
            // intentionally NOT forwarded: the LLM owns predicate /
            // relation extraction, and feeding back the classifier's
            // guesses would just confuse it.
            let prior_entities = collect_prior_entities(ctx, mem.id);
            // Bounded inferential context is added to the per-call
            // user message. The context map is keyed by memory id and
            // may be `None` (no neighbours computed) or absent for
            // this memory (no relevant neighbours found).
            let extractor_context = ctx.extractor_context.and_then(|map| map.get(&mem.id));
            let (mut request, _stats) = self.build_request(
                &inner,
                mem.id,
                text,
                &prior_entities,
                extractor_context,
                ctx.declared_predicates,
                ctx.declared_kinds,
                started,
            );
            if let Some(budget) = self.cost_budget {
                let est = estimate_cost(&request, &inner.pricing);
                if est > budget.per_call_micro_usd {
                    let reason = format!(
                        "estimated {} µ$ exceeds per-call budget {} µ$",
                        est, budget.per_call_micro_usd
                    );
                    return ExtractionResult::skipped(
                        ExtractionStatus::SkippedBudget,
                        reason,
                        started,
                    );
                }
            }

            // Provider cost this run incurs, summed across the first call
            // and any schema-retry call. Threaded onto the final result so
            // the worker's per-cycle budget gate and the cost metric see
            // real spend instead of zero.
            let mut cost_micro: u64 = 0;

            // ----- 3. First LLM call -------------------------------------------
            let resp1 = match inner.client.complete(request.clone()).await {
                Ok(r) => r,
                Err(e) => {
                    return ExtractionResult::failure(llm_error_reason(&e), started, started)
                        .with_failure_class(extraction_failure_class(&e));
                }
            };
            cost_micro = cost_micro.saturating_add(resp1.cost_micro_usd);

            // ----- 4. Validate + retry-once ------------------------------------
            let parsed = match inner.schema_compiled.as_ref() {
                None => match serde_json::from_str::<Value>(&resp1.content) {
                    Ok(v) => v,
                    Err(_) => Value::String(resp1.content.clone()),
                },
                Some(schema) => match validate_against(schema, &resp1.content) {
                    Ok(v) => v,
                    Err(err1) => {
                        // Retry with the validation error in the prompt.
                        request.messages.push(brain_llm::LlmMessage {
                            role: brain_llm::LlmRole::Assistant,
                            content: resp1.content.clone(),
                        });
                        request.messages.push(brain_llm::LlmMessage {
                            role: brain_llm::LlmRole::User,
                            content: format!(
                                "Your previous response did not match the expected schema. \
                                 Error: {err1}. Please retry with valid JSON."
                            ),
                        });
                        let resp2 = match inner.client.complete(request).await {
                            Ok(r) => r,
                            Err(e) => {
                                return ExtractionResult::failure(
                                    llm_error_reason(&e),
                                    started,
                                    started,
                                )
                                .with_failure_class(extraction_failure_class(&e));
                            }
                        };
                        cost_micro = cost_micro.saturating_add(resp2.cost_micro_usd);
                        match validate_against(schema, &resp2.content) {
                            Ok(v) => v,
                            Err(_) => {
                                // Two valid round-trips that both failed schema
                                // validation is a prompt/schema mismatch, not a
                                // provider blip — retrying the same prompt won't
                                // help, so it's permanent (terminal, no retry loop).
                                return ExtractionResult::failure(
                                    "schema validation failed twice",
                                    started,
                                    started,
                                )
                                .with_failure_class(ExtractionFailureClass::Permanent);
                            }
                        }
                    }
                },
            };

            // ----- 5. Cache write ----------------------------------------------
            if let Some(cache) = inner.cache.as_ref() {
                let blob = parsed.to_string().into_bytes();
                let token_count = (resp1.tokens_in + resp1.tokens_out)
                    .try_into()
                    .unwrap_or(u32::MAX);
                if let Err(e) = cache_put(
                    cache,
                    input_hash,
                    extractor_id_raw,
                    extractor_version,
                    model_id_hash,
                    blob,
                    token_count,
                    started,
                    self.cache_ttl,
                ) {
                    tracing::warn!(
                        target: "brain_extractors::llm",
                        extractor_id = extractor_id_raw,
                        error = %e,
                        "llm cache write failed; continuing",
                    );
                }
            }

            // ----- 6. Project to ExtractedItem[] -------------------------------
            let items = self.project_value(&parsed);
            ExtractionResult::success(items, started, started).with_cost(cost_micro)
        })
    }
}

fn decode_cached(blob: &[u8], _schema: Option<&Value>) -> Result<Value, String> {
    let s = std::str::from_utf8(blob).map_err(|e| format!("non-UTF8 blob: {e}"))?;
    serde_json::from_str::<Value>(s).map_err(|e| format!("blob json parse: {e}"))
}

fn llm_error_reason(e: &LlmError) -> String {
    if let Some(retry_after) = e.retry_after_ms() {
        format!("rate limited: retry after {retry_after} ms")
    } else {
        e.to_string()
    }
}

/// Map a transport-layer [`LlmError`] onto the tier-agnostic
/// [`ExtractionFailureClass`] the worker uses to decide a failed memory's
/// fate. A transient provider fault keeps the memory queued for a backoff
/// retry; a permanent one (bad key, no balance, malformed request) is
/// terminal so it surfaces to the operator instead of looping.
fn extraction_failure_class(e: &LlmError) -> ExtractionFailureClass {
    match e.failure_class() {
        brain_llm::FailureClass::Transient => ExtractionFailureClass::Transient,
        brain_llm::FailureClass::Permanent => ExtractionFailureClass::Permanent,
    }
}

// ---------------------------------------------------------------------------
