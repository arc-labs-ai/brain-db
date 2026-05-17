//! LLM extractor — third extractor tier. Spec §22/09.
//!
//! Wraps an [`LlmClient`] (Anthropic / OpenAI) behind the
//! [`Extractor`] trait. Adds:
//!
//! - Per-shard response cache ([`LlmCacheDb`], spec §15.4 /
//!   §26).
//! - Pre-call cost budget (`CostBudget { per_call_micro_usd }`).
//! - JSON-schema validation on the response, with one retry on
//!   schema failure (validation error fed back into the prompt
//!   per §22/09 §4).
//! - Output projection to `EntityMention` / `StatementMention` /
//!   `RelationMention` per the extractor's [`ExtractorTarget`].
//!
//! ## Degraded mode
//!
//! Mirrors [`crate::classifier::ClassifierExtractor`]: when no
//! `LlmClient` is configured (env keys unset, unknown model
//! prefix, etc.) the materializer constructs a degraded
//! extractor that returns `Failure(reason)` on every dispatch.

use std::sync::Arc;
use std::time::Duration;

use brain_core::knowledge::ExtractorKind;
use brain_core::{ExtractorId, Memory};
use brain_llm::{LlmClient, LlmError, LlmRequest};
use brain_metadata::llm_cache::{LlmResponse as CachedResponse, LLM_RESPONSES_TABLE};
use brain_metadata::LlmCacheDb;
use brain_protocol::schema::ast::StatementKindAst;
use brain_protocol::schema::ExtractorTarget;
use jsonschema::JSONSchema;
use parking_lot::Mutex;
use serde_json::Value;

use crate::extractor::{
    ExtractionContext, ExtractionFuture, ExtractionResult, ExtractionStatus, Extractor,
};
use crate::idempotency::hash_memory_text;
use crate::item::{EntityMention, ExtractedItem, RelationMention, StatementMention};

const DEFAULT_CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60; // 7 days.

/// Per-call cost ceiling. Phase 21 ships per-call only; the
/// per-deployment global budget is post-v1 (§22/09 §5 + §22/07).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostBudget {
    pub per_call_micro_usd: u64,
}

/// Pricing for a single model in dollar micro-units per token.
/// Operator-overridable; v1 ships a small embedded default table
/// for the common models (§22/09 §5). Unknown models fall back
/// to the conservative default `100 µ$/1K input + 300 µ$/1K
/// output` ⇒ `0.1 µ$ / 0.3 µ$` per token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pricing {
    pub input_micro_usd_per_token: f64,
    pub output_micro_usd_per_token: f64,
}

impl Pricing {
    /// Spec §22/09 §5 default — used when no model-specific entry
    /// is registered.
    #[must_use]
    pub const fn conservative_default() -> Self {
        Self {
            input_micro_usd_per_token: 0.1,
            output_micro_usd_per_token: 0.3,
        }
    }

    /// Lookup pricing by model prefix. Embedded table covers the
    /// models referenced in spec §22/09 §5; operators override
    /// via the pricing config in phase 21.5+.
    #[must_use]
    pub fn for_model(model: &str) -> Self {
        if model.starts_with("claude-haiku") {
            Self {
                input_micro_usd_per_token: 1.0,
                output_micro_usd_per_token: 5.0,
            }
        } else if model.starts_with("claude-sonnet") {
            Self {
                input_micro_usd_per_token: 3.0,
                output_micro_usd_per_token: 15.0,
            }
        } else if model.starts_with("gpt-4o-mini") {
            Self {
                input_micro_usd_per_token: 0.15,
                output_micro_usd_per_token: 0.6,
            }
        } else {
            Self::conservative_default()
        }
    }
}

/// Estimated dollar-micro cost of issuing `request` against
/// `pricing`. Uses `LlmRequest::approx_input_tokens()` for the
/// input side and `max_tokens` as the worst-case output.
#[must_use]
pub fn estimate_cost(request: &LlmRequest, pricing: &Pricing) -> u64 {
    let in_tokens = request.approx_input_tokens() as f64;
    let out_tokens = f64::from(request.max_tokens);
    (in_tokens * pricing.input_micro_usd_per_token
        + out_tokens * pricing.output_micro_usd_per_token)
        .round() as u64
}

// ---------------------------------------------------------------------------
// LlmExtractor.
// ---------------------------------------------------------------------------

/// LLM-tier extractor. Constructed by the materializer
/// (`materialize_llm_extractor`, phase 21.4) from an
/// `ExtractorDefinition` row.
pub struct LlmExtractor {
    id: ExtractorId,
    name: String,
    target: ExtractorTarget,
    extractor_version: u32,
    confidence_threshold: f32,
    cost_budget: Option<CostBudget>,
    cache_ttl: Duration,
    inner: Option<Arc<LlmExtractorInner>>,
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
    /// is absent (free-form response mode per §22/09 §6).
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
    /// returns `Failure(reason)` with the captured cause.
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

    fn build_request(&self, inner: &LlmExtractorInner, memory_text: &str) -> LlmRequest {
        let user_body = format!(
            "{prompt}\n\nInput text:\n```\n{text}\n```",
            prompt = inner.prompt,
            text = memory_text,
        );
        LlmRequest {
            model: inner.client.model().to_string(),
            system: inner.examples.as_ref().map(|e| {
                format!(
                    "You are an extractor for Brain. Examples:\n{}",
                    serde_json::to_string(e).unwrap_or_default()
                )
            }),
            messages: vec![brain_llm::LlmMessage {
                role: brain_llm::LlmRole::User,
                content: user_body,
            }],
            response_schema: inner.response_schema.clone(),
            temperature: inner.temperature,
            max_tokens: inner.max_tokens,
            timeout: inner.timeout,
        }
    }

    fn project_value(&self, parsed: &Value) -> Vec<ExtractedItem> {
        let items = match parsed {
            Value::Array(arr) => arr.clone(),
            v => vec![v.clone()],
        };
        let mut out = Vec::new();
        for v in items {
            if let Some(item) = self.project_one(&v) {
                if item.confidence() >= self.confidence_threshold {
                    out.push(item);
                }
            }
        }
        out
    }

    fn project_one(&self, v: &Value) -> Option<ExtractedItem> {
        match &self.target {
            ExtractorTarget::Entity { entity_type } => project_entity(
                v,
                entity_type,
                self.id.raw(),
                self.extractor_version,
            ),
            ExtractorTarget::Statement { kind } => {
                project_statement(v, kind_to_byte(*kind), self.id.raw(), self.extractor_version)
            }
            ExtractorTarget::Relation { relation_type } => project_relation(
                v,
                relation_type,
                self.id.raw(),
                self.extractor_version,
            ),
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
}

fn kind_to_byte(k: StatementKindAst) -> u8 {
    match k {
        StatementKindAst::Fact => 1,
        StatementKindAst::Preference => 2,
        StatementKindAst::Event => 3,
        // `Any` only appears in query AST; extractors targeting
        // `Any` default to Fact (1).
        StatementKindAst::Any => 1,
    }
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
    Some(ExtractedItem::StatementMention(StatementMention {
        kind,
        subject_text: read_str(v, "subject"),
        predicate_qname: predicate,
        object_text: read_str(v, "object"),
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

// ---------------------------------------------------------------------------
// Cache helpers.
// ---------------------------------------------------------------------------

fn cache_get(
    cache: &Arc<Mutex<LlmCacheDb>>,
    input_hash: [u8; 32],
    extractor_id: u32,
    extractor_version: u32,
    model_id_hash: u64,
) -> Option<CachedResponse> {
    let db = cache.lock();
    let rtxn = db.read_txn().ok()?;
    let t = rtxn.open_table(LLM_RESPONSES_TABLE).ok()?;
    let key = (input_hash, extractor_id, extractor_version, model_id_hash);
    let row = t.get(&key).ok().flatten()?;
    Some(row.value())
}

#[allow(clippy::too_many_arguments)]
fn cache_put(
    cache: &Arc<Mutex<LlmCacheDb>>,
    input_hash: [u8; 32],
    extractor_id: u32,
    extractor_version: u32,
    model_id_hash: u64,
    response_blob: Vec<u8>,
    token_count: u32,
    now_nanos: u64,
    ttl: Duration,
) -> Result<(), String> {
    let mut db = cache.lock();
    let wtxn = db
        .write_txn()
        .map_err(|e| format!("cache write_txn: {e}"))?;
    let key = (input_hash, extractor_id, extractor_version, model_id_hash);
    let expires_at_nanos = now_nanos.saturating_add(ttl.as_nanos() as u64);
    let value = CachedResponse::new(
        response_blob,
        now_nanos,
        expires_at_nanos,
        token_count,
        model_id_hash,
    );
    {
        let mut tbl = wtxn
            .open_table(LLM_RESPONSES_TABLE)
            .map_err(|e| format!("cache open_table: {e}"))?;
        tbl.insert(&key, &value)
            .map_err(|e| format!("cache insert: {e}"))?;
    }
    wtxn.commit().map_err(|e| format!("cache commit: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Validation + retry.
// ---------------------------------------------------------------------------

fn validate_against(schema: &JSONSchema, content: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(content)
        .map_err(|e| format!("response is not valid JSON: {e}"))?;
    if let Err(mut errs) = schema.validate(&parsed) {
        let msg = match errs.next() {
            Some(e) => e.to_string(),
            None => "unknown validation failure".into(),
        };
        return Err(msg);
    }
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// Extractor impl.
// ---------------------------------------------------------------------------

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

    fn run<'a>(
        &'a self,
        ctx: &'a ExtractionContext<'a>,
        mem: &'a Memory,
    ) -> ExtractionFuture<'a> {
        Box::pin(async move {
            let started = ctx.now_unix_nanos;
            let Some(inner) = self.inner.as_ref() else {
                let reason = self
                    .degraded_reason
                    .as_deref()
                    .unwrap_or("llm extractor not wired");
                return ExtractionResult::failure(reason, started, started);
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
                    return match decode_cached(&row.response_blob, inner.response_schema.as_ref())
                    {
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
            let mut request = self.build_request(&inner, text);
            if let Some(budget) = self.cost_budget {
                let est = estimate_cost(&request, &inner.pricing);
                if est > budget.per_call_micro_usd {
                    let reason = format!(
                        "estimated {} µ$ exceeds per-call budget {} µ$",
                        est, budget.per_call_micro_usd
                    );
                    return ExtractionResult {
                        items: Vec::new(),
                        status: ExtractionStatus::SkippedBudget,
                        status_reason: reason,
                        started_at_unix_nanos: started,
                        completed_at_unix_nanos: started,
                    };
                }
            }

            // ----- 3. First LLM call -------------------------------------------
            let resp1 = match inner.client.complete(request.clone()).await {
                Ok(r) => r,
                Err(e) => {
                    return ExtractionResult::failure(
                        llm_error_reason(&e),
                        started,
                        started,
                    );
                }
            };

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
                                );
                            }
                        };
                        match validate_against(schema, &resp2.content) {
                            Ok(v) => v,
                            Err(_) => {
                                return ExtractionResult::failure(
                                    "schema validation failed twice",
                                    started,
                                    started,
                                );
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
            ExtractionResult::success(items, started, started)
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

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ExtractorRegistry;
    use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, Salience};
    use brain_llm::client::LlmFuture;
    use brain_llm::LlmResponse;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ------------------------------------------------------------------- mock

    struct MockClient {
        model: String,
        responses: parking_lot::Mutex<Vec<Result<LlmResponse, LlmError>>>,
        calls: Arc<AtomicUsize>,
    }

    impl MockClient {
        fn new(model: &str, responses: Vec<Result<LlmResponse, LlmError>>) -> Self {
            Self {
                model: model.into(),
                responses: parking_lot::Mutex::new(responses),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl LlmClient for MockClient {
        fn complete<'a>(&'a self, _request: LlmRequest) -> LlmFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let next = self.responses.lock().drain(..1).next();
            Box::pin(async move {
                next.unwrap_or_else(|| {
                    Err(LlmError::ProviderError {
                        status: 500,
                        message: "mock: no more responses queued".into(),
                    })
                })
            })
        }

        fn model(&self) -> &str {
            &self.model
        }

        fn model_id_hash(&self) -> u64 {
            brain_llm::client::model_id_hash(&self.model)
        }
    }

    fn ok_response(json: &str, tokens: u64) -> LlmResponse {
        LlmResponse {
            content: json.into(),
            tokens_in: tokens / 2,
            tokens_out: tokens / 2,
            cost_micro_usd: tokens * 2,
            model_version: "mock-model-v1".into(),
        }
    }

    fn entity_target() -> ExtractorTarget {
        ExtractorTarget::Entity {
            entity_type: "brain:Person".into(),
        }
    }

    fn memory(text: &str) -> Memory {
        Memory {
            id: MemoryId::pack(0, 1, 0),
            agent: AgentId::new(),
            context: ContextId(0),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            text: Some(text.into()),
            created_at_unix_ms: 0,
            last_accessed_at_unix_ms: 0,
        }
    }

    fn ctx<'a>(reg: &'a ExtractorRegistry) -> ExtractionContext<'a> {
        ExtractionContext {
            schema_version: 1,
            now_unix_nanos: 100,
            registry: reg,
        }
    }

    fn build_ext(
        client: Arc<dyn LlmClient>,
        cache: Option<Arc<Mutex<LlmCacheDb>>>,
        schema: Option<Value>,
        budget: Option<CostBudget>,
    ) -> LlmExtractor {
        let schema_compiled = LlmExtractor::compile_schema(schema.as_ref()).unwrap();
        LlmExtractor::new(
            ExtractorId::from(99),
            "acme:llm_test".into(),
            entity_target(),
            1,
            0.5,
            budget,
            Duration::from_secs(60),
            LlmExtractorInner {
                client,
                cache,
                prompt: "Extract people".into(),
                examples: None,
                response_schema: schema,
                schema_compiled,
                pricing: Pricing::for_model("claude-haiku-4-5"),
                max_tokens: 1024,
                temperature: 0.0,
                timeout: Duration::from_secs(30),
            },
        )
    }

    // ------------------------------------------------------------------- tests

    #[test]
    fn degraded_dispatch_writes_failure() {
        let reg = ExtractorRegistry::new();
        let ext = LlmExtractor::degraded(
            ExtractorId::from(1),
            "acme:degraded".into(),
            entity_target(),
            1,
            0.5,
            "no client configured for model unknown-x",
        );
        let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("anything")));
        assert_eq!(r.status, ExtractionStatus::Failure);
        assert!(r.status_reason.contains("no client configured"));
    }

    #[test]
    fn cost_budget_skips_call() {
        let client = Arc::new(MockClient::new(
            "claude-haiku-4-5",
            vec![Ok(ok_response("[\"Alice\"]", 200))],
        ));
        let calls = client.calls.clone();
        let ext = build_ext(
            client,
            None,
            None,
            Some(CostBudget { per_call_micro_usd: 1 }),
        );
        let reg = ExtractorRegistry::new();
        let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("Alice met Bob")));
        assert_eq!(r.status, ExtractionStatus::SkippedBudget);
        assert!(r.status_reason.contains("exceeds per-call budget"));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no LLM call when over budget");
    }

    #[test]
    fn success_no_schema_parses_json_array() {
        let client = Arc::new(MockClient::new(
            "claude-haiku-4-5",
            vec![Ok(ok_response("[\"Alice\",\"Bob\"]", 50))],
        ));
        let ext = build_ext(client, None, None, None);
        let reg = ExtractorRegistry::new();
        let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("Alice met Bob")));
        assert_eq!(r.status, ExtractionStatus::Success);
        assert_eq!(r.items.len(), 2);
        match &r.items[0] {
            ExtractedItem::EntityMention(m) => {
                assert_eq!(m.text, "Alice");
                assert_eq!(m.entity_type_qname, "brain:Person");
            }
            other => panic!("expected entity, got {other:?}"),
        }
    }

    #[test]
    fn confidence_below_threshold_filtered() {
        let body = "[{\"name\":\"Alice\",\"confidence\":0.9}, \
                   {\"name\":\"X\",\"confidence\":0.1}]";
        let client = Arc::new(MockClient::new(
            "claude-haiku-4-5",
            vec![Ok(ok_response(body, 50))],
        ));
        let ext = build_ext(client, None, None, None);
        let reg = ExtractorRegistry::new();
        let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("Alice")));
        assert_eq!(r.status, ExtractionStatus::Success);
        // Only the high-confidence one survives the 0.5 threshold.
        assert_eq!(r.items.len(), 1);
    }

    #[test]
    fn schema_validation_failure_retries_once() {
        // First response is not an array of objects with `name`;
        // second response is the well-formed one.
        let schema = serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
            },
        });
        let bad = ok_response("[\"plain string\"]", 50);
        let good = ok_response("[{\"name\":\"Alice\"}]", 50);
        let client = Arc::new(MockClient::new(
            "claude-haiku-4-5",
            vec![Ok(bad), Ok(good)],
        ));
        let calls = client.calls.clone();
        let ext = build_ext(client, None, Some(schema), None);
        let reg = ExtractorRegistry::new();
        let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("Alice")));
        assert_eq!(r.status, ExtractionStatus::Success);
        assert_eq!(r.items.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "retried exactly once");
    }

    #[test]
    fn schema_validation_failure_twice_returns_failure() {
        let schema = serde_json::json!({
            "type": "array",
            "items": {"type": "object", "required": ["name"]},
        });
        let bad1 = ok_response("[\"x\"]", 50);
        let bad2 = ok_response("[\"y\"]", 50);
        let client = Arc::new(MockClient::new(
            "claude-haiku-4-5",
            vec![Ok(bad1), Ok(bad2)],
        ));
        let ext = build_ext(client, None, Some(schema), None);
        let reg = ExtractorRegistry::new();
        let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("hello")));
        assert_eq!(r.status, ExtractionStatus::Failure);
        assert!(r.status_reason.contains("schema validation failed twice"));
    }

    #[test]
    fn rate_limit_error_surfaces_retry_after() {
        let client = Arc::new(MockClient::new(
            "claude-haiku-4-5",
            vec![Err(LlmError::RateLimit { retry_after_ms: 1500 })],
        ));
        let ext = build_ext(client, None, None, None);
        let reg = ExtractorRegistry::new();
        let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("hi")));
        assert_eq!(r.status, ExtractionStatus::Failure);
        assert!(r.status_reason.contains("1500"));
    }

    #[test]
    fn cache_hit_skips_llm_call() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Mutex::new(
            LlmCacheDb::open(dir.path().join("llm_cache.redb")).unwrap(),
        ));

        // Round 1: real call populates cache.
        let client = Arc::new(MockClient::new(
            "claude-haiku-4-5",
            vec![Ok(ok_response("[\"Alice\"]", 50))],
        ));
        let calls = client.calls.clone();
        let ext = build_ext(client.clone(), Some(cache.clone()), None, None);
        let reg = ExtractorRegistry::new();
        let _ = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("Alice")));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Round 2: same input, new client with no responses queued.
        // Cache hit must short-circuit.
        let client2 = Arc::new(MockClient::new("claude-haiku-4-5", vec![]));
        let calls2 = client2.calls.clone();
        let ext2 = build_ext(client2, Some(cache.clone()), None, None);
        let r = futures_lite::future::block_on(ext2.run(&ctx(&reg), &memory("Alice")));
        assert_eq!(r.status, ExtractionStatus::Success);
        assert_eq!(r.items.len(), 1);
        assert_eq!(calls2.load(Ordering::SeqCst), 0, "cache hit: zero LLM calls");
    }

    #[test]
    fn cache_miss_writes_through() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Mutex::new(
            LlmCacheDb::open(dir.path().join("llm_cache.redb")).unwrap(),
        ));
        let client = Arc::new(MockClient::new(
            "claude-haiku-4-5",
            vec![Ok(ok_response("[\"Alice\"]", 50))],
        ));
        let ext = build_ext(client, Some(cache.clone()), None, None);
        let reg = ExtractorRegistry::new();
        let _ = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("Alice")));

        // Verify the row landed.
        let db = cache.lock();
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
        let key = (
            hash_memory_text("Alice"),
            99u32,
            1u32,
            brain_llm::client::model_id_hash("claude-haiku-4-5"),
        );
        assert!(t.get(&key).unwrap().is_some(), "cache row present");
    }

    #[test]
    fn estimate_cost_honours_pricing() {
        let mut req = LlmRequest::new("claude-haiku-4-5", "abcdefgh"); // 8 chars → 2 in-tokens
        req.max_tokens = 100;
        let p = Pricing::for_model("claude-haiku-4-5");
        // 2 * 1 + 100 * 5 = 502
        assert_eq!(estimate_cost(&req, &p), 502);
    }

    #[test]
    fn pricing_unknown_model_uses_conservative() {
        let p = Pricing::for_model("custom-llm");
        assert_eq!(p, Pricing::conservative_default());
    }
}
