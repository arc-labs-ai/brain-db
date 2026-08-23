//! End-to-end LLM extractor pipeline integration.
//!
//! Exercises a fully-wired [`LlmExtractor`] backed by a scripted
//! mock client and a real on-disk [`LlmCacheDb`]. Covers the
//! cross-call behaviours the 21.3 in-module unit tests can only
//! show one call at a time:
//!
//! - Cache populate → cache replay → re-arm after invalidation.
//! - Retry-once sequencing (no third call after second failure).
//! - Budget gate runs strictly before the LLM call.
//! - Confidence-threshold filtering on projection.
//! - Cache rows survive `LlmExtractor` re-construction.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brain_core::{ExtractorId, Memory, MemoryId, MemoryKind, Salience, SessionId, SpaceId};
use brain_extractors::{
    framework::extractor::{ExtractionContext, ExtractionStatus, Extractor},
    CostBudget, ExtractedItem, ExtractionResult, ExtractorRegistry, LlmExtractor, Pricing,
};
use brain_llm::client::{model_id_hash, LlmFuture};
use brain_llm::{LlmClient, LlmError, LlmMessage, LlmRequest, LlmResponse, LlmRole};
use brain_metadata::llm_cache::LLM_RESPONSES_TABLE;
use brain_metadata::LlmCacheDb;
use brain_protocol::schema::ExtractorTarget;
use parking_lot::Mutex;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Scripted mock client.
// ---------------------------------------------------------------------------

struct ScriptedClient {
    model: String,
    queue: Mutex<VecDeque<Result<LlmResponse, LlmError>>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedClient {
    fn new(model: &str, responses: Vec<Result<LlmResponse, LlmError>>) -> Self {
        Self {
            model: model.into(),
            queue: Mutex::new(responses.into()),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl LlmClient for ScriptedClient {
    fn complete<'a>(&'a self, _request: LlmRequest) -> LlmFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self.queue.lock().pop_front();
        Box::pin(async move {
            next.unwrap_or_else(|| {
                Err(LlmError::ProviderError {
                    status: 500,
                    message: "scripted client exhausted".into(),
                })
            })
        })
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn model_id_hash(&self) -> u64 {
        model_id_hash(&self.model)
    }
}

fn ok_response(content: &str, tokens: u64) -> LlmResponse {
    LlmResponse {
        content: content.into(),
        tokens_in: tokens / 2,
        tokens_out: tokens / 2,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        cost_micro_usd: tokens * 2,
        model_version: "scripted-mock-v1".into(),
    }
}

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

const EXT_ID_RAW: u32 = 7777;
const EXT_VERSION: u32 = 1;

fn target() -> ExtractorTarget {
    ExtractorTarget::Entity {
        entity_type: "brain:Person".into(),
    }
}

// A fixed tenant so that identical text maps to one cache key across
// calls (the cache key now folds in `space`). Tests that exercise
// cross-tenant isolation pass their own distinct spaces via
// `memory_in`.
fn fixed_space() -> SpaceId {
    SpaceId::derive_from_string("acme", "pipeline-tests")
}

fn memory(text: &str) -> Memory {
    memory_in(text, fixed_space(), None)
}

fn memory_in(text: &str, space: SpaceId, occurred_at_unix_nanos: Option<u64>) -> Memory {
    Memory {
        id: MemoryId::pack(0, 1, 0),
        space,
        session_id: SessionId(0),
        kind: MemoryKind::Episodic,
        salience: Salience::default(),
        text: Some(text.into()),
        created_at_unix_ms: 0,
        last_accessed_at_unix_ms: 0,
        occurred_at_unix_nanos,
    }
}

fn ctx<'a>(reg: &'a ExtractorRegistry) -> ExtractionContext<'a> {
    ExtractionContext {
        declared_entity_types: None,
        candidate_predicates: None,
        declared_kinds: None,
        entity_type_labels: None,
        schema_version: 1,
        now_unix_nanos: 100,
        registry: reg,
        prior_tier_items: None,
        extractor_context: None,
    }
}

fn build_extractor(
    client: Arc<dyn LlmClient>,
    cache: Option<Arc<Mutex<LlmCacheDb>>>,
    schema: Option<Value>,
    budget: Option<CostBudget>,
    threshold: f32,
) -> LlmExtractor {
    let schema_compiled = LlmExtractor::compile_schema(schema.as_ref()).unwrap();
    LlmExtractor::build(
        ExtractorId::from(EXT_ID_RAW),
        "acme:llm_pipeline_test".into(),
        target(),
        EXT_VERSION,
        client,
        cache,
        "Extract people".into(),
        None,
        schema,
        schema_compiled,
        threshold,
        budget,
        Duration::from_secs(60),
    )
}

fn block_on<F: std::future::Future<Output = ExtractionResult>>(f: F) -> ExtractionResult {
    futures_lite::future::block_on(f)
}

fn open_cache_in(dir: &std::path::Path) -> Arc<Mutex<LlmCacheDb>> {
    Arc::new(Mutex::new(
        LlmCacheDb::open(dir.join("llm_cache.redb")).unwrap(),
    ))
}

// ---------------------------------------------------------------------------
// Scenarios.
// ---------------------------------------------------------------------------

#[test]
fn cache_populates_then_replays() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache_in(dir.path());

    let client = Arc::new(ScriptedClient::new(
        "claude-haiku-4-5",
        vec![Ok(ok_response("[\"Alice\"]", 50))],
    ));
    let calls = client.calls.clone();
    let ext = build_extractor(client.clone(), Some(cache.clone()), None, None, 0.0);
    let reg = ExtractorRegistry::new();
    let mem = memory("Alice met Bob");

    // Call 1 — real LLM call, writes through to cache.
    let r1 = block_on(ext.run(&ctx(&reg), &mem));
    assert_eq!(r1.status, ExtractionStatus::Success);
    assert_eq!(r1.items.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Calls 2 + 3 — same memory hash, cache short-circuits.
    let r2 = block_on(ext.run(&ctx(&reg), &mem));
    let r3 = block_on(ext.run(&ctx(&reg), &mem));
    assert_eq!(r2.status, ExtractionStatus::Success);
    assert_eq!(r3.status, ExtractionStatus::Success);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "cache hits should not invoke the client"
    );

    // Projected items are identical on every replay.
    assert_eq!(r2.items, r1.items);
    assert_eq!(r3.items, r1.items);
}

#[test]
fn cache_row_invalidation_re_arms_call() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache_in(dir.path());

    let client = Arc::new(ScriptedClient::new(
        "claude-haiku-4-5",
        vec![
            Ok(ok_response("[\"Alice\"]", 50)),
            Ok(ok_response("[\"Bob\"]", 50)),
        ],
    ));
    let calls = client.calls.clone();
    let ext = build_extractor(client, Some(cache.clone()), None, None, 0.0);
    let reg = ExtractorRegistry::new();
    let mem = memory("Alice met Bob");

    let _ = block_on(ext.run(&ctx(&reg), &mem));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Manually evict the cache row. The input hash folds the full
    // prompt context + tenant, so ask the extractor for the exact key
    // it wrote rather than reconstructing it from the text alone.
    let key = (
        ext.cache_input_hash(&ctx(&reg), &mem),
        EXT_ID_RAW,
        EXT_VERSION,
        model_id_hash("claude-haiku-4-5"),
    );
    {
        let mut db = cache.lock();
        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
            t.remove(&key).unwrap();
        }
        wtxn.commit().unwrap();
    }

    // Next run misses → second scripted response is consumed.
    let r2 = block_on(ext.run(&ctx(&reg), &mem));
    assert_eq!(r2.status, ExtractionStatus::Success);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    match &r2.items[0] {
        ExtractedItem::EntityMention(m) => assert_eq!(m.text, "Bob"),
        other => panic!("expected entity, got {other:?}"),
    }
}

#[test]
fn schema_validation_retry_completes_in_two_calls() {
    let schema = serde_json::json!({
        "type": "array",
        "items": {
            "type": "object",
            "required": ["name"],
            "properties": {"name": {"type": "string"}},
        },
    });
    let client = Arc::new(ScriptedClient::new(
        "claude-haiku-4-5",
        vec![
            Ok(ok_response("[\"bare string\"]", 50)),
            Ok(ok_response("[{\"name\":\"Alice\"}]", 50)),
        ],
    ));
    let calls = client.calls.clone();
    let ext = build_extractor(client, None, Some(schema), None, 0.0);
    let reg = ExtractorRegistry::new();

    let r = block_on(ext.run(&ctx(&reg), &memory("Alice")));
    assert_eq!(r.status, ExtractionStatus::Success);
    assert_eq!(r.items.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "retried exactly once");
}

#[test]
fn schema_validation_failure_twice_costs_two_calls() {
    let schema = serde_json::json!({
        "type": "array",
        "items": {"type": "object", "required": ["name"]},
    });
    let client = Arc::new(ScriptedClient::new(
        "claude-haiku-4-5",
        vec![
            Ok(ok_response("[\"oops\"]", 50)),
            Ok(ok_response("[\"oops again\"]", 50)),
            // A third response is present but should never be drained.
            Ok(ok_response("[{\"name\":\"unused\"}]", 50)),
        ],
    ));
    let calls = client.calls.clone();
    let ext = build_extractor(client, None, Some(schema), None, 0.0);
    let reg = ExtractorRegistry::new();

    let r = block_on(ext.run(&ctx(&reg), &memory("nothing")));
    assert_eq!(r.status, ExtractionStatus::Failure);
    assert!(r.status_reason.contains("schema validation failed twice"));
    assert_eq!(calls.load(Ordering::SeqCst), 2, "no third call");
}

#[test]
fn cost_budget_blocks_call() {
    // A 1 µ$ budget can't possibly cover even a one-token request.
    let client = Arc::new(ScriptedClient::new(
        "claude-haiku-4-5",
        vec![Ok(ok_response("[\"Alice\"]", 1))],
    ));
    let calls = client.calls.clone();
    let ext = build_extractor(
        client,
        None,
        None,
        Some(CostBudget {
            per_call_micro_usd: 1,
        }),
        0.0,
    );
    let reg = ExtractorRegistry::new();

    let r = block_on(ext.run(&ctx(&reg), &memory("Alice met Bob in Paris")));
    assert_eq!(r.status, ExtractionStatus::SkippedBudget);
    assert!(r.status_reason.contains("exceeds per-call budget"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "budget gate must run strictly before the client call"
    );
}

#[test]
fn projection_strips_below_threshold_items() {
    let body = "[{\"name\":\"Alice\",\"confidence\":0.95}, \
               {\"name\":\"NoiseToken\",\"confidence\":0.05}]";
    let client = Arc::new(ScriptedClient::new(
        "claude-haiku-4-5",
        vec![Ok(ok_response(body, 50))],
    ));
    let ext = build_extractor(client, None, None, None, 0.5);
    let reg = ExtractorRegistry::new();
    let r = block_on(ext.run(&ctx(&reg), &memory("Alice met NoiseToken")));
    assert_eq!(r.status, ExtractionStatus::Success);
    assert_eq!(r.items.len(), 1);
    match &r.items[0] {
        ExtractedItem::EntityMention(m) => assert_eq!(m.text, "Alice"),
        other => panic!("expected entity, got {other:?}"),
    }
}

#[test]
fn response_blob_in_cache_persists_across_extractor_rebuilds() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache_in(dir.path());

    // Round 1: extractor A populates the cache.
    let client_a = Arc::new(ScriptedClient::new(
        "claude-haiku-4-5",
        vec![Ok(ok_response("[\"Alice\"]", 50))],
    ));
    let calls_a = client_a.calls.clone();
    {
        let ext_a = build_extractor(client_a.clone(), Some(cache.clone()), None, None, 0.0);
        let reg = ExtractorRegistry::new();
        let _ = block_on(ext_a.run(&ctx(&reg), &memory("Alice")));
    }
    assert_eq!(calls_a.load(Ordering::SeqCst), 1);

    // Round 2: brand-new extractor B (same id + version + model) — must
    // hit the cache row written by A.
    let client_b = Arc::new(ScriptedClient::new(
        "claude-haiku-4-5",
        vec![], // would error if called.
    ));
    let calls_b = client_b.calls.clone();
    let ext_b = build_extractor(client_b, Some(cache.clone()), None, None, 0.0);
    let reg = ExtractorRegistry::new();
    let r = block_on(ext_b.run(&ctx(&reg), &memory("Alice")));
    assert_eq!(r.status, ExtractionStatus::Success);
    assert_eq!(r.items.len(), 1);
    assert_eq!(
        calls_b.load(Ordering::SeqCst),
        0,
        "extractor B should reuse A's cache row"
    );
}

// ---------------------------------------------------------------------------
// Cache-key correctness: the key must fold in everything that changes
// the model's output, plus the owning tenant. A wrong-context or
// cross-tenant HIT must be impossible.
// ---------------------------------------------------------------------------

fn ctx_full<'a>(
    reg: &'a ExtractorRegistry,
    schema_version: u32,
    declared_entity_types: Option<&'a str>,
) -> ExtractionContext<'a> {
    ExtractionContext {
        declared_entity_types,
        candidate_predicates: None,
        declared_kinds: None,
        entity_type_labels: None,
        schema_version,
        now_unix_nanos: 100,
        registry: reg,
        prior_tier_items: None,
        extractor_context: None,
    }
}

fn build_extractor_prompt(
    client: Arc<dyn LlmClient>,
    cache: Option<Arc<Mutex<LlmCacheDb>>>,
    prompt: &str,
) -> LlmExtractor {
    LlmExtractor::build(
        ExtractorId::from(EXT_ID_RAW),
        "acme:llm_pipeline_test".into(),
        target(),
        EXT_VERSION,
        client,
        cache,
        prompt.into(),
        None,
        None,
        None,
        0.0,
        None,
        Duration::from_secs(60),
    )
}

const ANCHOR_2020: u64 = 1_600_000_000_000_000_000;
const ANCHOR_2023: u64 = 1_700_000_000_000_000_000;

#[test]
fn cache_key_is_stable_for_identical_context() {
    let client = Arc::new(ScriptedClient::new("claude-haiku-4-5", vec![]));
    let ext = build_extractor(client, None, None, None, 0.0);
    let reg = ExtractorRegistry::new();
    let mem = memory("Alice met Bob");
    let k1 = ext.cache_input_hash(&ctx(&reg), &mem);
    let k2 = ext.cache_input_hash(&ctx(&reg), &mem);
    assert_eq!(k1, k2, "identical text + context + tenant must share a key");
}

#[test]
fn cache_key_folds_anchor_date() {
    // Same text, different occurred_at → different anchor date in the
    // prompt → distinct key, so "next Friday" can't resolve to the
    // first memory's date.
    let client = Arc::new(ScriptedClient::new("claude-haiku-4-5", vec![]));
    let ext = build_extractor_prompt(client, None, "Extract. Anchor: {ANCHOR_DATE}\n{TEXT}");
    let reg = ExtractorRegistry::new();
    let space = fixed_space();
    let m2020 = memory_in("Let's meet next Friday", space, Some(ANCHOR_2020));
    let m2023 = memory_in("Let's meet next Friday", space, Some(ANCHOR_2023));
    assert_ne!(
        ext.cache_input_hash(&ctx(&reg), &m2020),
        ext.cache_input_hash(&ctx(&reg), &m2023),
        "different anchor dates must not share a cache entry",
    );
}

#[test]
fn cache_key_folds_schema_version() {
    // A SCHEMA_UPLOAD bumps the active schema version; the same text
    // must then re-extract rather than serve the pre-upload response.
    let client = Arc::new(ScriptedClient::new("claude-haiku-4-5", vec![]));
    let ext = build_extractor(client, None, None, None, 0.0);
    let reg = ExtractorRegistry::new();
    let mem = memory("Alice met Bob");
    assert_ne!(
        ext.cache_input_hash(&ctx_full(&reg, 1, None), &mem),
        ext.cache_input_hash(&ctx_full(&reg, 2, None), &mem),
        "different schema versions must not share a cache entry",
    );
}

#[test]
fn cache_key_folds_declared_entity_types() {
    // A schema change that adds a declared type reaches the prompt via
    // {DECLARED_ENTITY_TYPES}; the key must change so the new type gets
    // a fresh extraction.
    let client = Arc::new(ScriptedClient::new("claude-haiku-4-5", vec![]));
    let ext = build_extractor_prompt(
        client,
        None,
        "Extract. Types:\n{DECLARED_ENTITY_TYPES}\n{TEXT}",
    );
    let reg = ExtractorRegistry::new();
    let mem = memory("Alice met Bob");
    assert_ne!(
        ext.cache_input_hash(&ctx_full(&reg, 1, Some("- brain:Person")), &mem),
        ext.cache_input_hash(
            &ctx_full(&reg, 1, Some("- brain:Person\n- brain:Org")),
            &mem
        ),
        "different declared entity types must not share a cache entry",
    );
}

#[test]
fn cache_key_folds_tenant() {
    // Byte-identical text in two tenants must never share an entry.
    let client = Arc::new(ScriptedClient::new("claude-haiku-4-5", vec![]));
    let ext = build_extractor(client, None, None, None, 0.0);
    let reg = ExtractorRegistry::new();
    let mem_a = memory_in(
        "shared text",
        SpaceId::derive_from_string("tenant_a", "s"),
        None,
    );
    let mem_b = memory_in(
        "shared text",
        SpaceId::derive_from_string("tenant_b", "s"),
        None,
    );
    assert_ne!(
        ext.cache_input_hash(&ctx(&reg), &mem_a),
        ext.cache_input_hash(&ctx(&reg), &mem_b),
        "two tenants must not share a cache entry for identical text",
    );
}

#[test]
fn cache_miss_across_tenants_re_invokes_client() {
    // End-to-end: tenant A populates, tenant B's identical text must
    // miss and drive a fresh call rather than reading A's row.
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache_in(dir.path());
    let client = Arc::new(ScriptedClient::new(
        "claude-haiku-4-5",
        vec![
            Ok(ok_response("[\"A\"]", 50)),
            Ok(ok_response("[\"B\"]", 50)),
        ],
    ));
    let calls = client.calls.clone();
    let ext = build_extractor(client, Some(cache), None, None, 0.0);
    let reg = ExtractorRegistry::new();

    let mem_a = memory_in(
        "shared text",
        SpaceId::derive_from_string("tenant_a", "s"),
        None,
    );
    let mem_b = memory_in(
        "shared text",
        SpaceId::derive_from_string("tenant_b", "s"),
        None,
    );

    let _ = block_on(ext.run(&ctx(&reg), &mem_a));
    let _ = block_on(ext.run(&ctx(&reg), &mem_b));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "tenant B must not read tenant A's cached extraction",
    );
}

#[test]
fn cache_miss_across_anchor_dates_re_invokes_client() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache_in(dir.path());
    let client = Arc::new(ScriptedClient::new(
        "claude-haiku-4-5",
        vec![
            Ok(ok_response("[\"first\"]", 50)),
            Ok(ok_response("[\"second\"]", 50)),
        ],
    ));
    let calls = client.calls.clone();
    let ext = build_extractor_prompt(
        client,
        Some(cache),
        "Extract. Anchor: {ANCHOR_DATE}\n{TEXT}",
    );
    let reg = ExtractorRegistry::new();
    let space = fixed_space();
    let m2020 = memory_in("Let's meet next Friday", space, Some(ANCHOR_2020));
    let m2023 = memory_in("Let's meet next Friday", space, Some(ANCHOR_2023));

    let _ = block_on(ext.run(&ctx(&reg), &m2020));
    let _ = block_on(ext.run(&ctx(&reg), &m2023));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a different anchor date must re-extract, not serve the stale date",
    );
}

// Quiet unused-import warnings without spreading `#[allow]` across
// the file.
#[allow(dead_code)]
fn _ensure_imports(m: LlmMessage, role: LlmRole, _p: Pricing) -> (LlmMessage, LlmRole) {
    (m, role)
}
