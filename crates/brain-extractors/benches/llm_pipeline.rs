//! LLM extractor perf bench (sub-task 21.7).
//!
//! Spec targets per
//! `spec/16_benchmarks_acceptance/02_latency_targets.md` §2.8:
//!
//! - `LlmExtractor::predict` cache hit: p50 1 ms / p99 5 ms.
//! - Cost-budget skip path (no LLM call): p50 200 µs / p99 1 ms.
//!
//! The third bench (mock-client cache miss) is informational —
//! production wall-time is dominated by real-API round-trip, so
//! the spec has no per-call target there. It exists to flag
//! regressions in the in-process overhead (cache lookup + serde +
//! schema-validate + projection + cache write).
//!
//! Run:
//!
//! ```
//! cargo bench -p brain-extractors --bench llm_pipeline
//! cargo bench -p brain-extractors --bench llm_pipeline -- --quick
//! ```

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brain_core::{AgentId, ContextId, ExtractorId, Memory, MemoryId, MemoryKind, Salience};
use brain_extractors::{
    extractor::ExtractionContext, CostBudget, Extractor, ExtractorRegistry, LlmExtractor,
};
use brain_llm::client::{model_id_hash, LlmFuture};
use brain_llm::{LlmClient, LlmError, LlmRequest, LlmResponse};
use brain_metadata::LlmCacheDb;
use brain_protocol::schema::ExtractorTarget;
use criterion::{black_box, criterion_group, Criterion};
use futures_lite::future::block_on;
use parking_lot::Mutex;

const EXT_ID: u32 = 11_001;
const MODEL: &str = "claude-haiku-4-5";

// ---------------------------------------------------------------------------
// Scripted mock client. Each `complete` pops the next response;
// when the queue is empty the cache-hit bench is expected to
// never call it.
// ---------------------------------------------------------------------------

struct ScriptedClient {
    queue: Mutex<VecDeque<Result<LlmResponse, LlmError>>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedClient {
    fn new(responses: Vec<Result<LlmResponse, LlmError>>) -> Self {
        Self {
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
                    message: "bench: queue exhausted".into(),
                })
            })
        })
    }

    fn model(&self) -> &str {
        MODEL
    }

    fn model_id_hash(&self) -> u64 {
        model_id_hash(MODEL)
    }
}

fn ok_response(content: &str, tokens: u64) -> LlmResponse {
    LlmResponse {
        content: content.into(),
        tokens_in: tokens / 2,
        tokens_out: tokens / 2,
        cost_micro_usd: tokens * 2,
        model_version: "bench-mock-v1".into(),
    }
}

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

fn memory_text() -> &'static str {
    "Alice met Bob in Paris during the planning summit on 2026-04-12."
}

fn build_memory() -> Memory {
    Memory {
        id: MemoryId::pack(0, 1, 0),
        agent: AgentId::new(),
        context: ContextId(0),
        kind: MemoryKind::Episodic,
        salience: Salience::default(),
        text: Some(memory_text().into()),
        created_at_unix_ms: 0,
        last_accessed_at_unix_ms: 0,
    }
}

fn target() -> ExtractorTarget {
    ExtractorTarget::Entity {
        entity_type: "brain:Person".into(),
    }
}

fn build_extractor(
    client: Arc<dyn LlmClient>,
    cache: Option<Arc<Mutex<LlmCacheDb>>>,
    budget: Option<CostBudget>,
) -> LlmExtractor {
    LlmExtractor::build(
        ExtractorId::from(EXT_ID),
        "bench:llm".into(),
        target(),
        1,
        client,
        cache,
        "Extract entities".into(),
        None,
        None,
        None,
        0.0,
        budget,
        Duration::from_secs(60),
    )
}

fn ctx<'a>(reg: &'a ExtractorRegistry) -> ExtractionContext<'a> {
    ExtractionContext {
        schema_version: 1,
        now_unix_nanos: 0,
        registry: reg,
    }
}

// ---------------------------------------------------------------------------
// Bench 1 — cache hit path. Spec target p50 1 ms / p99 5 ms.
// ---------------------------------------------------------------------------

fn bench_llm_cache_hit(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tmp");
    let cache = Arc::new(Mutex::new(
        LlmCacheDb::open(dir.path().join("llm_cache.redb")).expect("open cache"),
    ));
    // Prime the cache with one real call, then sub in an empty
    // scripted client so any cache-miss is loudly observable
    // (panic if hit).
    let prime_client: Arc<dyn LlmClient> = Arc::new(ScriptedClient::new(vec![Ok(ok_response(
        "[\"Alice\",\"Bob\"]",
        80,
    ))]));
    let mem = build_memory();
    let reg = ExtractorRegistry::new();
    {
        let ext = build_extractor(prime_client.clone(), Some(cache.clone()), None);
        let _ = block_on(ext.run(&ctx(&reg), &mem));
    }

    let cold_client = Arc::new(ScriptedClient::new(Vec::new()));
    let calls = cold_client.calls.clone();
    let ext = build_extractor(cold_client, Some(cache.clone()), None);

    c.bench_function("llm_pipeline cache_hit", |b| {
        b.iter(|| {
            let r = block_on(ext.run(&ctx(&reg), black_box(&mem)));
            black_box(r);
        });
    });
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "cache_hit bench must never reach the client"
    );
}

// ---------------------------------------------------------------------------
// Bench 2 — cost-budget skip path. Spec target p50 200 µs / p99 1 ms.
// ---------------------------------------------------------------------------

fn bench_llm_budget_skip(c: &mut Criterion) {
    let client = Arc::new(ScriptedClient::new(Vec::new()));
    let calls = client.calls.clone();
    let ext = build_extractor(
        client,
        None,
        Some(CostBudget {
            per_call_micro_usd: 1,
        }),
    );
    let mem = build_memory();
    let reg = ExtractorRegistry::new();

    c.bench_function("llm_pipeline budget_skip", |b| {
        b.iter(|| {
            let r = block_on(ext.run(&ctx(&reg), black_box(&mem)));
            black_box(r);
        });
    });
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "budget_skip bench must never reach the client"
    );
}

// ---------------------------------------------------------------------------
// Bench 3 — full pipeline against a mock client (informational).
// ---------------------------------------------------------------------------

fn bench_llm_mock_miss(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tmp");
    let cache = Arc::new(Mutex::new(
        LlmCacheDb::open(dir.path().join("llm_cache.redb")).expect("open cache"),
    ));
    // Refill the queue every iteration via a closure — criterion's
    // `iter_batched` would be cleaner but `iter` keeps the file
    // small. We give the queue a very large number of responses to
    // outlive bench warm-up + measurement.
    let responses: Vec<_> = (0..200_000)
        .map(|_| Ok(ok_response("[\"Alice\",\"Bob\"]", 80)))
        .collect();
    let client = Arc::new(ScriptedClient::new(responses));
    let ext = build_extractor(client, Some(cache.clone()), None);
    let reg = ExtractorRegistry::new();

    // Each iteration uses a fresh memory id so the cache hash
    // never matches → exercises the full miss path.
    let mut counter: u64 = 0;
    c.bench_function("llm_pipeline mock_miss (informational)", |b| {
        b.iter(|| {
            counter = counter.wrapping_add(1);
            let mem = Memory {
                id: MemoryId::pack(0, counter.wrapping_add(1), 0),
                agent: AgentId::new(),
                context: ContextId(0),
                kind: MemoryKind::Episodic,
                salience: Salience::default(),
                text: Some(format!("{} iteration={counter}", memory_text())),
                created_at_unix_ms: 0,
                last_accessed_at_unix_ms: 0,
            };
            let r = block_on(ext.run(&ctx(&reg), black_box(&mem)));
            black_box(r);
        });
    });
}

criterion_group!(
    name = llm_pipeline_benches;
    config = Criterion::default();
    targets = bench_llm_cache_hit, bench_llm_budget_skip, bench_llm_mock_miss
);

fn main() {
    llm_pipeline_benches();
    Criterion::default().configure_from_args().final_summary();
}
