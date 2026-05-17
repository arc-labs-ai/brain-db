//! End-to-end LLM extractor pipeline integration. Phase 21.6.
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

use brain_core::{
    AgentId, ContextId, ExtractorId, Memory, MemoryId, MemoryKind, Salience,
};
use brain_extractors::{
    extractor::{ExtractionContext, ExtractionStatus, Extractor},
    hash_memory_text, CostBudget, ExtractedItem, ExtractionResult, ExtractorRegistry,
    LlmExtractor, Pricing,
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

    // Manually evict the cache row.
    let key = (
        hash_memory_text("Alice met Bob"),
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

// Quiet unused-import warnings without spreading `#[allow]` across
// the file.
#[allow(dead_code)]
fn _ensure_imports(m: LlmMessage, role: LlmRole, _p: Pricing) -> (LlmMessage, LlmRole) {
    (m, role)
}
