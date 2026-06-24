//! Tests covering the LLM extractor surface (pricing, cache, schema
//! validation, retry, projection, degraded mode).

// ---------------------------------------------------------------------------

#![cfg(test)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brain_core::{
    AgentId, ContextId, ExtractorId, Memory, MemoryId, MemoryKind, Salience, Statement,
    StatementObject, StatementValue, SubjectRef,
};
use brain_llm::client::LlmFuture;
use brain_llm::{LlmClient, LlmError, LlmRequest, LlmResponse};
use brain_metadata::statement::{JudgeError, JudgeVerdict};
use brain_metadata::LlmCacheDb;
use brain_protocol::schema::ExtractorTarget;
use parking_lot::Mutex;
use serde_json::Value;

use super::extractor::{
    collect_prior_entities, find_unfilled_placeholder, parse_verdict, relative_time_hint,
    render_prompt, truncate_chars, LlmExtractor, LlmExtractorInner, LLM_INPUT_TOKEN_BUDGET,
};
use super::pricing::{estimate_cost, CostBudget, Pricing};
use crate::framework::extractor::{
    ExtractionContext, ExtractionStatus, Extractor, ExtractorContext, NeighborMemory,
};
use crate::framework::item::{EntityMention, ExtractedItem, StatementMention};
use crate::framework::registry::ExtractorRegistry;
use brain_protocol::schema::ast::StatementKindAst;

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
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
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
        occurred_at_unix_nanos: None,
    }
}

fn ctx<'a>(reg: &'a ExtractorRegistry) -> ExtractionContext<'a> {
    ExtractionContext {
            declared_predicates: None,
        declared_kinds: None,
        entity_type_labels: None,
        schema_version: 1,
        now_unix_nanos: 100,
        registry: reg,
        prior_tier_items: None,
        extractor_context: None,
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
fn degraded_dispatch_writes_skipped_disabled() {
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
    // An unconfigured LLM tier is *not* a failure — it never
    // tried. Reporting `Failure` here cascades into the pipeline
    // classifier and produces a misleading "partially applied"
    // audit on otherwise-clean runs.
    assert_eq!(r.status, ExtractionStatus::SkippedDisabled);
    assert!(r.status_reason.contains("no client configured"));
    assert!(r.items.is_empty());
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
        Some(CostBudget {
            per_call_micro_usd: 1,
        }),
    );
    let reg = ExtractorRegistry::new();
    let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("Alice met Bob")));
    assert_eq!(r.status, ExtractionStatus::SkippedBudget);
    assert!(r.status_reason.contains("exceeds per-call budget"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no LLM call when over budget"
    );
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
fn rate_limit_error_surfaces_retry_after() {
    let client = Arc::new(MockClient::new(
        "claude-haiku-4-5",
        vec![Err(LlmError::RateLimit {
            retry_after_ms: 1500,
        })],
    ));
    let ext = build_ext(client, None, None, None);
    let reg = ExtractorRegistry::new();
    let r = futures_lite::future::block_on(ext.run(&ctx(&reg), &memory("hi")));
    assert_eq!(r.status, ExtractionStatus::Failure);
    assert!(r.status_reason.contains("1500"));
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

// ------------------------------------------------------------------
// Prior-tier prompt injection.
// ------------------------------------------------------------------

fn em_fixture(text: &str, kind: &str, confidence: f32) -> EntityMention {
    EntityMention {
        entity_type_qname: kind.into(),
        text: text.into(),
        start: 0,
        end: text.len(),
        confidence,
        extractor_id: 1,
        extractor_version: 1,
    }
}

fn sm_fixture() -> StatementMention {
    StatementMention {
        kind: 1,
        subject_text: Some("X".into()),
        subject_is_memory: false,
        predicate_qname: "brain:fact".into(),
        object_text: Some("Y".into()),
        confidence: 0.9,
        extractor_id: 1,
        extractor_version: 1,
        is_stateful: false,
        object_is_entity: false,
        event_at_unix_nanos: None,
        subject_is_self: false,
        retract: false,
    }
}

fn ext_with_prompt(prompt: &str) -> LlmExtractor {
    let client = Arc::new(MockClient::new("claude-haiku-4-5", vec![]));
    let schema_compiled = LlmExtractor::compile_schema(None).unwrap();
    LlmExtractor::new(
        ExtractorId::from(42),
        "acme:prompt_test".into(),
        entity_target(),
        1,
        0.0,
        None,
        Duration::from_secs(60),
        LlmExtractorInner {
            client,
            cache: None,
            prompt: prompt.into(),
            examples: None,
            response_schema: None,
            schema_compiled,
            pricing: Pricing::for_model("claude-haiku-4-5"),
            max_tokens: 1024,
            temperature: 0.0,
            timeout: Duration::from_secs(30),
        },
    )
}

#[test]
fn build_request_injects_prior_entities_into_prompt() {
    let prompt = "DO YOUR JOB.\n{PRIOR_ENTITIES}\nText: {TEXT}";
    let ext = ext_with_prompt(prompt);
    let inner = ext.inner.as_ref().unwrap().clone();
    let priors = vec![
        em_fixture("Alice Wong", "brain:Person", 0.96),
        em_fixture("Acme Corp", "brain:Organization", 0.94),
        em_fixture("Bengaluru", "brain:Place", 0.91),
    ];
    let prior_refs: Vec<&EntityMention> = priors.iter().collect();
    let (req, _) = ext.build_request(
        &inner,
        brain_core::MemoryId::pack(0, 1, 0),
        "Alice Wong works at Acme Corp from Bengaluru.",
        &prior_refs,
        None,
        None,
        None,
        0,
    );
    let body = &req.messages[0].content;
    assert!(
        body.contains("Previously extracted entities for this text:"),
        "prompt missing prior-entities heading: {body}",
    );
    for em in &priors {
        assert!(
            body.contains(&em.text),
            "prompt missing entity surface {}: {body}",
            em.text,
        );
        assert!(
            body.contains(&em.entity_type_qname),
            "prompt missing entity type {}: {body}",
            em.entity_type_qname,
        );
    }
    assert!(
        body.contains("verbatim"),
        "prompt missing reuse-anchor instruction: {body}",
    );
    assert!(
        body.contains("Alice Wong works at Acme Corp from Bengaluru."),
        "prompt missing memory text: {body}",
    );
}

#[test]
fn build_request_with_empty_prior_entities_omits_section() {
    let prompt = "DO YOUR JOB.\n{PRIOR_ENTITIES}\nText: {TEXT}";
    let ext = ext_with_prompt(prompt);
    let inner = ext.inner.as_ref().unwrap().clone();
    let priors: Vec<&EntityMention> = Vec::new();
    let (req, _) = ext.build_request(
        &inner,
        brain_core::MemoryId::pack(0, 2, 0),
        "Plain text.",
        &priors,
        None,
        None,
        None,
        0,
    );
    let body = &req.messages[0].content;
    assert!(
        !body.contains("Previously extracted entities for this text:"),
        "no priors => no heading; got: {body}",
    );
    assert!(body.contains("Plain text."));
}

#[test]
fn build_request_filters_non_entity_items_from_prior() {
    let prompt = "{PRIOR_ENTITIES}\n{TEXT}";
    let ext = ext_with_prompt(prompt);
    let inner = ext.inner.as_ref().unwrap().clone();
    // Two entities + one statement; only the entities should land in
    // the prompt section because the LLM tier owns predicate
    // extraction and should not see lower-tier predicate guesses.
    let em1 = em_fixture("Alice", "brain:Person", 0.8);
    let em2 = em_fixture("Acme", "brain:Organization", 0.7);
    let priors: Vec<&EntityMention> = vec![&em1, &em2];
    let (req, _) = ext.build_request(
        &inner,
        brain_core::MemoryId::pack(0, 3, 0),
        "Alice met Acme.",
        &priors,
        None,
        None,
        None,
        0,
    );
    let body = &req.messages[0].content;
    assert!(body.contains("\"Alice\""));
    assert!(body.contains("\"Acme\""));
    // A StatementMention's `brain:fact` predicate should not have
    // been forwarded — it isn't in the prior_refs slice at all.
    assert!(
        !body.contains("brain:fact"),
        "statements must not appear in the prior-entities section: {body}",
    );

    // Independently verify the filter at the data layer: a
    // collect_prior_entities call with a HashMap containing a
    // StatementMention must drop it.
    let mut map = HashMap::new();
    let mid = brain_core::MemoryId::pack(0, 9, 0);
    map.insert(
        mid,
        vec![
            ExtractedItem::EntityMention(em1.clone()),
            ExtractedItem::EntityMention(em2.clone()),
            ExtractedItem::StatementMention(sm_fixture()),
        ],
    );
    let reg = ExtractorRegistry::new();
    let ctx = ExtractionContext {
            declared_predicates: None,
        declared_kinds: None,
        entity_type_labels: None,
        schema_version: 1,
        now_unix_nanos: 1,
        registry: &reg,
        prior_tier_items: Some(&map),
        extractor_context: None,
    };
    let collected = collect_prior_entities(&ctx, mid);
    assert_eq!(collected.len(), 2, "statement must be filtered out");
}

#[test]
fn build_request_splits_into_cached_blocks() {
    // Operator declared examples ⇒ schema block is populated.
    // Both blocks must be flagged for prompt caching so Anthropic
    // amortises their input-token cost across calls.
    let examples = serde_json::json!({
        "entity_types": [
            "brain:Person",
            "brain:Organization",
            "brain:Place",
            "brain:Event",
            "brain:Product",
            "brain:Topic",
        ],
        "predicates": ["brain:fact", "brain:prefers", "brain:knows"],
    });
    let client = Arc::new(MockClient::new("claude-haiku-4-5", vec![]));
    let schema_compiled = LlmExtractor::compile_schema(None).unwrap();
    let ext = LlmExtractor::new(
        ExtractorId::from(7),
        "acme:llm_split".into(),
        entity_target(),
        1,
        0.0,
        None,
        Duration::from_secs(60),
        LlmExtractorInner {
            client,
            cache: None,
            prompt: "Extract things.\n{TEXT}".into(),
            examples: Some(examples),
            response_schema: None,
            schema_compiled,
            pricing: Pricing::for_model("claude-haiku-4-5"),
            max_tokens: 1024,
            temperature: 0.0,
            timeout: Duration::from_secs(30),
        },
    );
    let inner = ext.inner.as_ref().unwrap().clone();
    let (req, _) = ext.build_request(
        &inner,
        brain_core::MemoryId::pack(0, 1, 0),
        "Alice met Bob.",
        &[],
        None,
        None,
        None,
        0,
    );
    assert_eq!(
        req.system_blocks.len(),
        2,
        "schema declared => role + schema blocks",
    );
    assert!(
        req.system_blocks.iter().all(|b| b.cache),
        "both blocks must be cache-tagged",
    );
    assert!(
        req.system_blocks[0].text.contains("Brain"),
        "first block is the role block",
    );
    assert!(
        req.system_blocks[1].text.contains("brain:Person"),
        "second block carries the schema/examples payload",
    );
    // Per-call body (memory text) is in the user message, NOT in
    // a system block — keeping it out of the cached prefix is what
    // makes the cache hit ratio meaningful.
    assert_eq!(req.messages.len(), 1);
    assert!(req.messages[0].content.contains("Alice met Bob."));
    // The dynamic body must not leak into either cached block.
    for b in &req.system_blocks {
        assert!(
            !b.text.contains("Alice met Bob."),
            "per-call text must not appear in cached system block: {}",
            b.text,
        );
    }
}

#[test]
fn build_request_without_examples_emits_role_block_only() {
    // No schema declared ⇒ just the constant role block, still
    // cached so the LLM's "you are a Brain extractor" preamble
    // amortises across calls even in degraded schema mode.
    let client = Arc::new(MockClient::new("claude-haiku-4-5", vec![]));
    let schema_compiled = LlmExtractor::compile_schema(None).unwrap();
    let ext = LlmExtractor::new(
        ExtractorId::from(8),
        "acme:llm_noschema".into(),
        entity_target(),
        1,
        0.0,
        None,
        Duration::from_secs(60),
        LlmExtractorInner {
            client,
            cache: None,
            prompt: "Extract.\n{TEXT}".into(),
            examples: None,
            response_schema: None,
            schema_compiled,
            pricing: Pricing::for_model("claude-haiku-4-5"),
            max_tokens: 1024,
            temperature: 0.0,
            timeout: Duration::from_secs(30),
        },
    );
    let inner = ext.inner.as_ref().unwrap().clone();
    let (req, _) = ext.build_request(
        &inner,
        brain_core::MemoryId::pack(0, 1, 0),
        "Hello.",
        &[],
        None,
        None,
        None,
        0,
    );
    assert_eq!(req.system_blocks.len(), 1);
    assert!(req.system_blocks[0].cache);
}

#[test]
fn render_prompt_falls_back_to_appending_text_when_no_text_placeholder() {
    let prompt = "EXTRACT NOW.";
    let memory_id = brain_core::MemoryId::pack(0, 5, 0);
    let out = render_prompt(prompt, "the body", &[], memory_id);
    assert!(out.starts_with("EXTRACT NOW."));
    assert!(
        out.contains("Input text:\n```\nthe body\n```"),
        "prompts without {{TEXT}} must still see the memory text: {out}",
    );
}

#[test]
fn find_unfilled_placeholder_flags_reserved_idents() {
    assert_eq!(
        find_unfilled_placeholder("nothing here"),
        None,
        "plain text => no placeholder",
    );
    assert_eq!(
        find_unfilled_placeholder("{FOO}"),
        Some("FOO".into()),
        "uppercase ident => flagged",
    );
    assert_eq!(
        find_unfilled_placeholder("{lower}"),
        None,
        "lowercase ident => not flagged (likely json example)",
    );
}

// ----- Judge -----

#[test]
fn parse_verdict_accepts_three_words() {
    assert_eq!(
        parse_verdict("SUPERSEDES").unwrap(),
        JudgeVerdict::Supersedes
    );
    assert_eq!(
        parse_verdict("supersedes.").unwrap(),
        JudgeVerdict::Supersedes
    );
    assert_eq!(
        parse_verdict(" Contradicts! ").unwrap(),
        JudgeVerdict::Contradicts
    );
    assert_eq!(parse_verdict("coexists").unwrap(), JudgeVerdict::Coexists);
}

#[test]
fn parse_verdict_rejects_unknown() {
    let err = parse_verdict("maybe").unwrap_err();
    matches!(err, JudgeError::Parse(_))
        .then_some(())
        .expect("expected Parse error");
}

fn extractor_with_mock(mock: Arc<MockClient>) -> LlmExtractor {
    LlmExtractor::build(
        ExtractorId::from(1),
        "judge-test".into(),
        ExtractorTarget::Statement {
            kind: StatementKindAst::Fact,
        },
        1,
        mock,
        None,
        "ignored prompt".into(),
        None,
        None,
        None,
        0.0,
        None,
        Duration::from_secs(60),
    )
}

fn entity_id() -> brain_core::EntityId {
    brain_core::EntityId::new()
}

fn fact_pair(subj: brain_core::EntityId, pred: brain_core::PredicateId) -> (Statement, Statement) {
    let old = Statement::new_root(
        brain_core::StatementId::new(),
        brain_core::StatementKind::Fact,
        SubjectRef::Entity(subj),
        pred,
        StatementObject::Value(StatementValue::Text("old".into())),
        0.9,
        brain_core::EvidenceRef::default(),
        ExtractorId::from(0),
        1_700_000_000_000_000_000,
        1,
    );
    let new = Statement::new_root(
        brain_core::StatementId::new(),
        brain_core::StatementKind::Fact,
        SubjectRef::Entity(subj),
        pred,
        StatementObject::Value(StatementValue::Text("new".into())),
        0.9,
        brain_core::EvidenceRef::default(),
        ExtractorId::from(0),
        1_700_000_000_000_000_001,
        1,
    );
    (old, new)
}

fn open_md(tmp: &tempfile::TempDir) -> brain_metadata::MetadataDb {
    let db = brain_metadata::MetadataDb::open(tmp.path().join("md.redb")).unwrap();
    // Touch the tables the judge's renderer reads so a read txn
    // on a fresh DB doesn't error with "Table does not exist".
    let wtxn = db.write_txn().unwrap();
    let _ = wtxn
        .open_table(brain_metadata::tables::entity::ENTITIES_TABLE)
        .unwrap();
    let _ = wtxn
        .open_table(brain_metadata::tables::predicate::PREDICATES_TABLE)
        .unwrap();
    wtxn.commit().unwrap();
    db
}

#[test]
fn judge_supersedes_returns_supersede_verdict() {
    let mock = Arc::new(MockClient::new(
        "claude-haiku-test",
        vec![Ok(ok_response("SUPERSEDES", 50))],
    ));
    let calls = mock.calls.clone();
    let ext = extractor_with_mock(mock);

    let tmp = tempfile::tempdir().unwrap();
    let md = open_md(&tmp);
    let rtxn = md.read_txn().unwrap();

    let (old, new) = fact_pair(entity_id(), brain_core::PredicateId::from(1));
    let verdict =
        futures_lite::future::block_on(ext.judge_supersedes_call(&new, &old, &rtxn)).unwrap();
    assert_eq!(verdict, JudgeVerdict::Supersedes);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn judge_supersedes_degraded_extractor_errors() {
    let ext = LlmExtractor::degraded(
        ExtractorId::from(2),
        "judge-degraded".into(),
        ExtractorTarget::Statement {
            kind: StatementKindAst::Fact,
        },
        1,
        0.0,
        "no API key",
    );
    let tmp = tempfile::tempdir().unwrap();
    let md = open_md(&tmp);
    let rtxn = md.read_txn().unwrap();
    let (old, new) = fact_pair(entity_id(), brain_core::PredicateId::from(1));
    let err =
        futures_lite::future::block_on(ext.judge_supersedes_call(&new, &old, &rtxn)).unwrap_err();
    matches!(err, JudgeError::Transport(_))
        .then_some(())
        .expect("expected Transport error from degraded extractor");
}

#[test]
fn judge_supersedes_budget_blocks_call() {
    let mock = Arc::new(MockClient::new("claude-sonnet-test", vec![]));
    let calls = mock.calls.clone();
    let ext = LlmExtractor::build(
        ExtractorId::from(3),
        "judge-budget".into(),
        ExtractorTarget::Statement {
            kind: StatementKindAst::Fact,
        },
        1,
        mock,
        None,
        "ignored".into(),
        None,
        None,
        None,
        0.0,
        // 1 micro-USD budget — sonnet at 3 µ$/1k will exceed.
        Some(CostBudget {
            per_call_micro_usd: 1,
        }),
        Duration::from_secs(60),
    );
    let tmp = tempfile::tempdir().unwrap();
    let md = open_md(&tmp);
    let rtxn = md.read_txn().unwrap();
    let (old, new) = fact_pair(entity_id(), brain_core::PredicateId::from(1));
    let err =
        futures_lite::future::block_on(ext.judge_supersedes_call(&new, &old, &rtxn)).unwrap_err();
    matches!(err, JudgeError::Budget(_))
        .then_some(())
        .expect("expected Budget error");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "budget rejection must not reach the client"
    );
}

#[test]
fn judge_supersedes_unparseable_response_errors() {
    let mock = Arc::new(MockClient::new(
        "claude-haiku-test",
        vec![Ok(ok_response("I'm not sure", 50))],
    ));
    let ext = extractor_with_mock(mock);
    let tmp = tempfile::tempdir().unwrap();
    let md = open_md(&tmp);
    let rtxn = md.read_txn().unwrap();
    let (old, new) = fact_pair(entity_id(), brain_core::PredicateId::from(1));
    let err =
        futures_lite::future::block_on(ext.judge_supersedes_call(&new, &old, &rtxn)).unwrap_err();
    matches!(err, JudgeError::Parse(_))
        .then_some(())
        .expect("expected Parse error");
}

// ------------------------------------------------------------------
// Bounded inferential context in the prompt.
// ------------------------------------------------------------------

fn neighbor(text: &str, score: f32, created_at_unix_nanos: u64) -> NeighborMemory {
    NeighborMemory {
        memory_id: brain_core::MemoryId::pack(0, 999, 0),
        text: text.into(),
        similarity_score: score,
        created_at_unix_nanos,
    }
}

#[test]
fn extract_with_context_includes_neighbors_in_prompt() {
    let prompt = "Extract from: {TEXT}";
    let ext = ext_with_prompt(prompt);
    let inner = ext.inner.as_ref().unwrap().clone();
    let mid = brain_core::MemoryId::pack(0, 1, 0);
    let now = 10 * 86_400 * 1_000_000_000u64; // 10 days after epoch
    let day_ns = 86_400 * 1_000_000_000u64;
    let ec = ExtractorContext {
        neighbors: vec![
            neighbor(
                "Alice and Bob discussed the new payment service architecture.",
                0.84,
                now - 3 * 3_600 * 1_000_000_000u64, // 3h ago
            ),
            neighbor(
                "Alice mentioned a 3pm meeting with the platform team.",
                0.78,
                now - 12 * 3_600 * 1_000_000_000u64, // 12h ago
            ),
            neighbor(
                "Bob shipped the auth-rewrite branch yesterday.",
                0.71,
                now - day_ns,
            ),
        ],
        summary: None,
    };
    let (req, stats) = ext.build_request(
        &inner,
        mid,
        "Alice approved the design.",
        &[],
        Some(&ec),
        None,
        None,
        now,
    );
    let body = &req.messages[0].content;
    assert!(
        body.contains("## Recent context"),
        "prompt missing recent-context heading: {body}",
    );
    assert!(body.contains("Alice and Bob discussed the new payment service architecture."));
    assert!(body.contains("Alice mentioned a 3pm meeting"));
    assert!(body.contains("Bob shipped the auth-rewrite branch"));
    assert!(body.contains("similarity=0.84"));
    assert!(body.contains("similarity=0.78"));
    assert!(body.contains("T-3h"));
    assert!(body.contains("T-12h"));
    assert!(body.contains("T-1d"));
    let context_pos = body.find("## Recent context").unwrap();
    let memory_pos = body.find("Alice approved the design.").unwrap();
    assert!(
        context_pos < memory_pos,
        "recent-context section must precede the memory text: {body}",
    );
    assert_eq!(stats.neighbors_included, 3);
    assert!(!stats.summary_included, "no summary in this fixture");
}

#[test]
fn extract_with_context_drops_summary_when_over_budget() {
    let prompt = "{TEXT}";
    let ext = ext_with_prompt(prompt);
    let inner = ext.inner.as_ref().unwrap().clone();
    let mid = brain_core::MemoryId::pack(0, 1, 0);
    // Memory text sized so that adding the summary tips us over
    // the 4k-token cap but dropping it brings us back. The
    // enforcer drops summary first (per spec) — neighbors fit
    // even after the summary is gone.
    let memory_text = "m".repeat(15_400); // ~3850 tokens
    let ec = ExtractorContext {
        neighbors: vec![neighbor("a recent prior", 0.9, 1_000)],
        // 600-char summary (truncated to 500 in the render).
        summary: Some("Summary that pushes us over the cap. ".repeat(20)),
    };
    let (_req, stats) = ext.build_request(&inner, mid, &memory_text, &[], Some(&ec), None, None, 0);
    assert!(
        !stats.summary_included,
        "summary must be dropped when over budget (got {} tokens)",
        stats.approx_input_tokens,
    );
    assert!(
        stats.neighbors_included >= 1,
        "neighbor must survive after summary drops (got {} neighbors)",
        stats.neighbors_included,
    );
    assert!(
        stats.approx_input_tokens <= LLM_INPUT_TOKEN_BUDGET,
        "post-trim prompt must fit in 4k tokens: {} tokens",
        stats.approx_input_tokens,
    );
}

#[test]
fn extract_with_context_drops_lowest_similarity_neighbors_when_over_budget() {
    let prompt = "{TEXT}";
    let ext = ext_with_prompt(prompt);
    let inner = ext.inner.as_ref().unwrap().clone();
    let mid = brain_core::MemoryId::pack(0, 1, 0);
    // ~2750 tokens of memory + 30 neighbors each at the 200-char
    // render cap → ~9k chars of neighbor section → ~2250 tokens.
    // Total ~5000 tokens — well past the 4k budget. Trimming
    // peels neighbors from the tail (lowest similarity) until
    // the prompt fits.
    let memory_text = "m".repeat(11_000);
    let long_body = "x".repeat(220); // exceeds 200-char cap → truncates
    let neighbors: Vec<NeighborMemory> = (0..30)
        .map(|i| {
            neighbor(
                &format!("neighbor-{i:02}-{long_body}"),
                1.0 - (i as f32) * 0.02,
                1_000_000_000 * (i as u64 + 1),
            )
        })
        .collect();
    let ec = ExtractorContext {
        neighbors,
        summary: None,
    };
    let (req, stats) = ext.build_request(&inner, mid, &memory_text, &[], Some(&ec), None, None, 0);
    let body = &req.messages[0].content;
    assert!(
        stats.neighbors_included < 30,
        "some must have been dropped (kept {})",
        stats.neighbors_included,
    );
    assert!(
        stats.neighbors_included > 0,
        "highest-similarity prefix must survive",
    );
    assert!(
        body.contains("neighbor-00-"),
        "the highest-similarity neighbor must survive trimming",
    );
    assert!(
        stats.approx_input_tokens <= LLM_INPUT_TOKEN_BUDGET,
        "post-trim prompt must fit in 4k tokens: {} tokens",
        stats.approx_input_tokens,
    );
    assert!(body.contains("## Recent context"));
}

#[test]
fn extract_with_context_includes_summary_when_under_budget() {
    let prompt = "{TEXT}";
    let ext = ext_with_prompt(prompt);
    let inner = ext.inner.as_ref().unwrap().clone();
    let mid = brain_core::MemoryId::pack(0, 1, 0);
    let ec = ExtractorContext {
        neighbors: vec![neighbor("a recent prior", 0.9, 1_000)],
        summary: Some("Last week Alice shipped the auth rewrite.".into()),
    };
    let (req, stats) = ext.build_request(&inner, mid, "today", &[], Some(&ec), None, None, 1_000_000_000);
    let body = &req.messages[0].content;
    assert!(body.contains("## Rolling summary"));
    assert!(body.contains("Last week Alice shipped"));
    assert!(stats.summary_included);
}

#[test]
fn extract_with_context_skips_sections_when_context_is_empty() {
    let prompt = "{TEXT}";
    let ext = ext_with_prompt(prompt);
    let inner = ext.inner.as_ref().unwrap().clone();
    let mid = brain_core::MemoryId::pack(0, 1, 0);
    let ec = ExtractorContext::empty();
    let (req, stats) = ext.build_request(&inner, mid, "first memory ever", &[], Some(&ec), None, None, 0);
    let body = &req.messages[0].content;
    assert!(
        !body.contains("## Recent context"),
        "empty context must not render any section: {body}",
    );
    assert!(!body.contains("## Rolling summary"));
    assert_eq!(stats.neighbors_included, 0);
    assert!(!stats.summary_included);
}

#[test]
fn relative_time_hint_renders_days_hours_minutes_seconds() {
    let now = 100 * 86_400 * 1_000_000_000u64;
    assert_eq!(
        relative_time_hint(now, now - 5 * 86_400 * 1_000_000_000),
        "T-5d"
    );
    assert_eq!(
        relative_time_hint(now, now - 3 * 3_600 * 1_000_000_000),
        "T-3h"
    );
    assert_eq!(
        relative_time_hint(now, now - 7 * 60 * 1_000_000_000),
        "T-7m"
    );
    assert_eq!(relative_time_hint(now, now - 45 * 1_000_000_000), "T-45s");
    assert_eq!(relative_time_hint(now, now), "T-0s");
    assert_eq!(relative_time_hint(0, 1), "T+0");
}

#[test]
fn truncate_chars_caps_long_strings_utf8_safe() {
    assert_eq!(truncate_chars("hello", 10), "hello");
    let long = "héllo🌍".repeat(50); // 6 chars × 50 = 300 chars
    let out = truncate_chars(&long, 10);
    assert_eq!(out.chars().count(), 11); // 10 + ellipsis
    assert!(out.ends_with('…'));
}
