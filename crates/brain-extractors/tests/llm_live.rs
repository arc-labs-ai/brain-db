//! Live LLM provider integration. Runs the LLM extractor tier
//! against the real Anthropic Messages API.
//!
//! ## Invocation
//!
//! ```text
//! BRAIN__LLM__API_KEY=sk-ant-... \
//!     cargo test -p brain-extractors --features live-llm \
//!         --test llm_live -- --nocapture
//! ```
//!
//! The tests are gated twice over:
//!   - At compile time by `--features live-llm` (this file is opted
//!     out of `cargo test` by `required-features` in `Cargo.toml`).
//!   - At runtime by `BRAIN__LLM__API_KEY` (the single shared
//!     credential): when the env var is absent the body prints a
//!     `skip:` notice and returns. CI without the secret therefore
//!     turns into a no-op pass.
//!
//! Two scenarios live here:
//!
//! - `live_anthropic_extracts_entities_and_relation` — single end-to-
//!   end pipeline call: a known sentence ("Priya Patel works at Acme
//!   Corp") is fed to the LLM extractor. The assertions confirm
//!   `EntityMention("Priya Patel")`, `EntityMention("Acme Corp")` and
//!   a `works-at` statement (subject=Priya, object=Acme).
//!
//! - `live_anthropic_cache_short_circuits_second_call` — runs the
//!   pipeline twice against the same input. Inspects
//!   `LLM_RESPONSES_TABLE` directly to confirm the row was written
//!   on the first call and that the second call returned the same
//!   projection without re-contacting the API (no new row, identical
//!   `created_at_unix_nanos`).
//!
//! These tests intentionally avoid scripting any mock client — the
//! whole point is to exercise transport, auth, prompt construction,
//! schema validation, and projection against a real provider.

#![cfg(feature = "live-llm")]

use std::sync::Arc;
use std::time::Duration;

use brain_core::{AgentId, ContextId, ExtractorId, Memory, MemoryId, MemoryKind, Salience};
use brain_extractors::{
    extractor::{ExtractionContext, ExtractionStatus, Extractor},
    hash_memory_text, ExtractedItem, ExtractorRegistry, LlmExtractor,
};
use brain_llm::client::{model_id_hash, LlmClient};
use brain_llm::AnthropicClient;
use brain_metadata::llm_cache::LLM_RESPONSES_TABLE;
use brain_metadata::LlmCacheDb;
use brain_protocol::schema::ExtractorTarget;
use parking_lot::Mutex;

const MODEL: &str = "claude-haiku-4-5";
const EXT_ID_RAW: u32 = 99_001;
const EXT_VERSION: u32 = 1;
const INPUT_TEXT: &str = "Priya Patel works at Acme Corp";

// JSON-schema we constrain the model to. EntityOrStatement projection
// (see `LlmExtractor::project_one`) inspects `predicate` to discriminate
// entities from statements, so each array item is either:
//   { "name": "<entity surface>",        "confidence": 0.0..1.0 }
// or
//   { "subject": "...", "predicate": "brain:works-at",
//     "object": "...",  "confidence": 0.0..1.0 }
fn response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "name":       { "type": "string" },
                "subject":    { "type": "string" },
                "predicate":  { "type": "string" },
                "object":     { "type": "string" },
                "confidence": { "type": "number" }
            },
            "additionalProperties": true
        }
    })
}

const PROMPT: &str = "\
You are extracting structured knowledge for the Brain substrate. \
Given the input text, return a JSON array. For every distinct named \
entity, emit an object with a `name` field (the verbatim surface form). \
For every relationship between two named entities, emit an object with \
`subject`, `predicate`, `object` (use the predicate qname \
`brain:works-at` when the subject is employed by the object). Always \
include a `confidence` between 0 and 1. Return ONLY the JSON array, no \
prose.";

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
        now_unix_nanos: 1,
        registry: reg,
        prior_tier_items: None,
        extractor_context: None,
    }
}

fn build_extractor(
    client: Arc<dyn LlmClient>,
    cache: Option<Arc<Mutex<LlmCacheDb>>>,
) -> LlmExtractor {
    let schema = response_schema();
    let compiled = LlmExtractor::compile_schema(Some(&schema)).expect("response schema compiles");
    LlmExtractor::build(
        ExtractorId::from(EXT_ID_RAW),
        "brain:live_anthropic_test".into(),
        ExtractorTarget::EntityOrStatement,
        EXT_VERSION,
        client,
        cache,
        PROMPT.into(),
        None,
        Some(schema),
        compiled,
        // Accept anything the model emits; the prompt + schema do
        // the gating. The live model occasionally returns 0.95-ish
        // confidences and we don't want noise about thresholds.
        0.0,
        // No per-call budget cap — Haiku is cheap and the prompt is
        // a couple of hundred input tokens.
        None,
        Duration::from_secs(7 * 24 * 60 * 60),
    )
}

/// Skip helper. Returns `Some(api_key)` when the env var is set, else
/// prints a skip notice and returns `None`. Tests must early-return
/// on `None` — there's no graceful "ignore" status in libtest 1.x.
fn api_key_or_skip(label: &str) -> Option<String> {
    match std::env::var("BRAIN__LLM__API_KEY") {
        Ok(k) if !k.is_empty() => Some(k),
        _ => {
            println!("skip[{label}]: BRAIN__LLM__API_KEY not set");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Test 1 — single end-to-end extraction.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_anthropic_extracts_entities_and_relation() {
    let Some(key) = api_key_or_skip("entities_and_relation") else {
        return;
    };
    let client: Arc<dyn LlmClient> =
        Arc::new(AnthropicClient::with_key(MODEL, key).expect("non-empty key"));
    let ext = build_extractor(client, None);
    let reg = ExtractorRegistry::new();
    let mem = memory(INPUT_TEXT);

    let result = ext.run(&ctx(&reg), &mem).await;
    assert_eq!(
        result.status,
        ExtractionStatus::Success,
        "extractor returned non-success: status={:?} reason={}",
        result.status,
        result.status_reason
    );

    // Group projected items by kind for inspection.
    let mut entities: Vec<String> = Vec::new();
    let mut statements: Vec<(String, String, String)> = Vec::new();
    for item in &result.items {
        match item {
            ExtractedItem::EntityMention(em) => entities.push(em.text.clone()),
            ExtractedItem::StatementMention(sm) => statements.push((
                sm.subject_text.clone().unwrap_or_default(),
                sm.predicate_qname.clone(),
                sm.object_text.clone().unwrap_or_default(),
            )),
            ExtractedItem::RelationMention(rm) => statements.push((
                rm.subject_text.clone(),
                rm.relation_type_qname.clone(),
                rm.object_text.clone(),
            )),
        }
    }

    println!("entities  = {entities:?}");
    println!("statements = {statements:?}");

    assert!(
        entities.iter().any(|e| e.contains("Priya")),
        "expected an entity mention containing `Priya`, got {entities:?}",
    );
    assert!(
        entities.iter().any(|e| e.contains("Acme")),
        "expected an entity mention containing `Acme`, got {entities:?}",
    );
    assert!(
        statements.iter().any(|(s, p, o)| {
            s.contains("Priya") && o.contains("Acme") && p.to_ascii_lowercase().contains("works")
        }),
        "expected a works-at statement Priya -> Acme, got {statements:?}",
    );
}

// ---------------------------------------------------------------------------
// Test 2 — cache short-circuit.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_anthropic_cache_short_circuits_second_call() {
    let Some(key) = api_key_or_skip("cache_short_circuits") else {
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let cache = Arc::new(Mutex::new(
        LlmCacheDb::open(tmp.path().join("llm_cache.redb")).expect("open llm cache"),
    ));

    let client: Arc<dyn LlmClient> =
        Arc::new(AnthropicClient::with_key(MODEL, key).expect("non-empty key"));
    let ext = build_extractor(client, Some(cache.clone()));
    let reg = ExtractorRegistry::new();
    let mem = memory(INPUT_TEXT);

    // ---- Call 1 — populates the cache. -----------------------------
    let r1 = ext.run(&ctx(&reg), &mem).await;
    assert_eq!(
        r1.status,
        ExtractionStatus::Success,
        "first call failed: {} ({:?})",
        r1.status_reason,
        r1.status
    );

    // The cache row must now be present. Read the exact key the
    // extractor uses (memory hash + extractor id + version + model
    // hash) and capture the `created_at` so we can verify it doesn't
    // move on the second call.
    let key = (
        hash_memory_text(INPUT_TEXT),
        EXT_ID_RAW,
        EXT_VERSION,
        model_id_hash(MODEL),
    );
    let first_created_at = {
        let db = cache.lock();
        let rtxn = db.read_txn().expect("read txn");
        let table = rtxn
            .open_table(LLM_RESPONSES_TABLE)
            .expect("open cache table");
        let row = table
            .get(&key)
            .expect("cache get")
            .expect("cache row present after first call");
        row.value().created_at_unix_nanos
    };
    assert_ne!(
        first_created_at, 0,
        "cache row must carry a non-zero created_at"
    );

    // ---- Call 2 — must short-circuit on the cache. -----------------
    let r2 = ext.run(&ctx(&reg), &mem).await;
    assert_eq!(r2.status, ExtractionStatus::Success);
    assert_eq!(
        r2.items, r1.items,
        "cached projection should be byte-identical to the first call",
    );

    // Same row, same created_at — cache_put isn't re-invoked when the
    // first read hits.
    let second_created_at = {
        let db = cache.lock();
        let rtxn = db.read_txn().expect("read txn");
        let table = rtxn
            .open_table(LLM_RESPONSES_TABLE)
            .expect("open cache table");
        let row = table
            .get(&key)
            .expect("cache get")
            .expect("cache row still present on second call");
        row.value().created_at_unix_nanos
    };
    assert_eq!(
        first_created_at, second_created_at,
        "second call must not re-write the cache row",
    );
}
