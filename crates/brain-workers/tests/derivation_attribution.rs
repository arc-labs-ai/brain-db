#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Where one derivation cycle's wall clock goes.
//!
//! This harness is paired with `brain_workers::derivation_probe`.
//!
//! Derivation's configured ceiling and its observed throughput disagree by
//! roughly fifty times, and the per-stage split that would say why is not
//! observable from a cycle-duration histogram. This drives a full
//! `ExtractorWorker::run_cycle` over a seeded corpus with every tier wired,
//! and prints the probe's per-stage accounting.
//!
//! Provider latency is a known constant here, not a measurement: the fake
//! client parks each call on a timer thread for a fixed duration. That makes
//! the LLM tier's contribution a number we chose, so everything the cycle
//! spends OUTSIDE it is attributable without a provider in the loop. A real
//! provider would move the LLM tier's figure and nothing else.
//!
//! Run with:
//!   cargo nextest run -p brain-workers --run-ignored all derivation_attribution

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_core::{ExtractorId, MemoryId, SpaceId};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_extractors::{
    resolver::EmbeddingDeps, ExtractorRegistry, LlmExtractor, PatternExtractor,
};
use brain_index::{IndexParams, SharedHnsw};
use brain_llm::client::{model_id_hash, LlmFuture};
use brain_llm::{LlmClient, LlmRequest, LlmResponse};
use brain_metadata::llm_cache::LlmCacheDb;
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::schema::ExtractorTarget;
use brain_workers::derivation_probe::{self, Stage};
use brain_workers::{ExtractorKnobs, ExtractorWorker, HypeGenerator, Worker, WorkerContext};
use parking_lot::{Mutex as PLMutex, RwLock as PLRwLock};

// ---------------------------------------------------------------------------
// Knobs this harness sweeps.
// ---------------------------------------------------------------------------

/// What one provider round trip costs. The production observation that
/// prompted this measurement puts a real call at one to two seconds.
const PROVIDER_LATENCY: Duration = Duration::from_millis(1500);

/// Memories drained per cycle, matching `default_extractor_drain_per_cycle`.
const DRAIN_PER_CYCLE: usize = 32;

/// Mean memory text length, in the range real episodic writes land in.
const MEMORY_TEXT_CHARS: usize = 520;

/// Prior memories seeded before the measured cycle, so the neighbour fetch
/// and the entity resolver have a populated store to search rather than an
/// empty one.
const CORPUS_SIZE: usize = 256;

// ---------------------------------------------------------------------------
// Provider stand-in.
// ---------------------------------------------------------------------------

/// A client that answers after a fixed wall-clock delay.
///
/// The delay is a real park, not a spin: a timer thread holds the reply and
/// releases it through a channel, so a caller awaiting it yields the executor
/// exactly as a network round trip would. A spin would occupy the lane under
/// measurement and manufacture the serialisation we are looking for.
struct DelayedClient {
    model: String,
    reply: String,
    latency: Duration,
    calls: Arc<AtomicUsize>,
    /// Highest number of calls outstanding at any instant. A fan-out that is
    /// really concurrent peaks at the batch width; one that is serialised
    /// somewhere below it peaks at that bound instead.
    live: Arc<AtomicUsize>,
    peak_live: Arc<AtomicUsize>,
    /// Longest any one call waited for byte admission before its round trip.
    admitted_after: Arc<std::sync::atomic::AtomicU64>,
    /// What one request weighed, as admission priced it.
    request_bytes: Arc<std::sync::atomic::AtomicU64>,
}

impl DelayedClient {
    fn new(model: &str, reply: String, latency: Duration) -> Self {
        Self {
            model: model.into(),
            reply,
            latency,
            calls: Arc::new(AtomicUsize::new(0)),
            live: Arc::new(AtomicUsize::new(0)),
            peak_live: Arc::new(AtomicUsize::new(0)),
            admitted_after: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            request_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

impl LlmClient for DelayedClient {
    fn complete<'a>(&'a self, request: LlmRequest) -> LlmFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let latency = self.latency;
        let reply = self.reply.clone();
        let live = self.live.clone();
        let peak_live = self.peak_live.clone();
        let admitted_after = self.admitted_after.clone();
        // Every real provider client admits its payload against the byte
        // budget before the round trip, so the stand-in does too: admission
        // is on the path under measurement and leaving it out would hide it.
        let weight = brain_llm::inflight::request_bytes(&request);
        self.request_bytes.store(weight, Ordering::SeqCst);
        Box::pin(async move {
            let queued = Instant::now();
            let _admitted = brain_llm::admission::global().admit_bytes(weight).await;
            admitted_after.fetch_max(queued.elapsed().as_nanos() as u64, Ordering::SeqCst);
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak_live.fetch_max(now, Ordering::SeqCst);
            let (tx, rx) = flume::bounded::<()>(1);
            std::thread::spawn(move || {
                std::thread::sleep(latency);
                let _ = tx.send(());
            });
            let _ = rx.recv_async().await;
            live.fetch_sub(1, Ordering::SeqCst);
            Ok(LlmResponse {
                truncated: false,
                content: reply,
                tokens_in: 4000,
                tokens_out: 600,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_micro_usd: 0,
                model_version: "delayed-stand-in".into(),
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

/// A full extraction reply: entities, statements and relations in the shape
/// `project_value` reads, sized like a dense memory's real yield.
fn extraction_reply(seed: usize) -> String {
    let mut entities = Vec::new();
    let mut statements = Vec::new();
    let mut relations = Vec::new();
    for i in 0..6 {
        let name = format!("Subject{}_{i}", seed % 64);
        entities.push(format!(
            r#"{{"name":"{name}","type":"brain:Person","confidence":0.9}}"#
        ));
        statements.push(format!(
            r#"{{"subject":"{name}","predicate":"brain:works_on","object":"Project{i}","kind":1,"confidence":0.88}}"#
        ));
        relations.push(format!(
            r#"{{"relation_type":"brain:collaborates_with","from":"{name}","to":"Subject{}_{i}","confidence":0.8}}"#,
            (seed + 1) % 64
        ));
    }
    format!(
        r#"{{"entities":[{}],"statements":[{}],"relations":[{}]}}"#,
        entities.join(","),
        statements.join(","),
        relations.join(",")
    )
}

/// A HyPE reply: the generator asks for a list of questions.
fn hype_reply() -> String {
    r#"["Who works on the migration?","What did the team decide about retries?","Which service owns the queue?","When did the rollout start?"]"#.into()
}

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct ZeroDispatcher;
impl Dispatcher for ZeroDispatcher {
    fn embed(&self, t: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        // A content-dependent vector rather than a constant one, so HNSW
        // search does real distance work instead of collapsing every
        // neighbour onto one point.
        let mut v = [0.0f32; VECTOR_DIM];
        let h = blake3::hash(t.as_bytes());
        let b = h.as_bytes();
        for (i, slot) in v.iter_mut().enumerate() {
            *slot = (b[i % 32] as f32 - 128.0) / 128.0;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        for slot in &mut v {
            *slot /= norm;
        }
        Ok(v)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0x5A; 16]
    }
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn memory_id(slot: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[8..16].copy_from_slice(&slot.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

/// A memory text of realistic length and vocabulary.
fn memory_text(seed: usize) -> String {
    const SENTENCES: [&str; 8] = [
        "Priya walked the on-call rotation through the retry budget she set for the ingest path, and said the change ships with the next release.",
        "The platform team moved the queue consumer onto its own executor after the backlog alarm fired twice in one week.",
        "Marcus prefers the staging environment pinned to the previous image so a bad deploy cannot take the demo down.",
        "Finance approved the Graviton migration on the condition that the per-request cost drops below the current figure.",
        "The Okonta regulator was replaced during the Tuesday maintenance window and the vibration readings settled afterwards.",
        "Lena owns the schema registry now; she took it over from the data platform group at the end of the quarter.",
        "We decided against a second budget for preparation memory because two budgets sized against one machine can both be full.",
        "The customer asked whether recall resolves supersession, and the honest answer is that it ranks by similarity instead.",
    ];
    // Seeded prefix so no two memories share a text: identical text is a
    // response-cache hit, and a cache hit measures the cache rather than the
    // provider path we are trying to account for.
    let mut out = format!("Note {seed}: ");
    let mut i = seed;
    while out.len() < MEMORY_TEXT_CHARS {
        out.push_str(SENTENCES[i % SENTENCES.len()]);
        out.push(' ');
        i += 1;
    }
    out
}

/// The system LLM extractor's prompt is about seventeen thousand characters.
/// Request size feeds byte admission, so the stand-in matches its order of
/// magnitude rather than being a one-liner.
fn realistic_prompt() -> String {
    let unit = "Extract durable, factual knowledge from one memory as subject-predicate-object triples. Reuse an existing predicate whenever it means the same thing, and name each entity the same way every time so every fact about one thing lands on one node. ";
    let mut p = String::with_capacity(18_000);
    while p.len() < 17_500 {
        p.push_str(unit);
    }
    p
}

fn response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "entities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "type": {"type": "string"},
                        "confidence": {"type": "number"}
                    },
                    "required": ["name", "type"]
                }
            },
            "statements": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "subject": {"type": "string"},
                        "predicate": {"type": "string"},
                        "object": {"type": "string"},
                        "kind": {"type": "integer"},
                        "confidence": {"type": "number"}
                    },
                    "required": ["subject", "predicate", "object"]
                }
            },
            "relations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "relation_type": {"type": "string"},
                        "from": {"type": "string"},
                        "to": {"type": "string"},
                        "confidence": {"type": "number"}
                    },
                    "required": ["relation_type", "from", "to"]
                }
            }
        }
    })
}

struct Fixture {
    _ops: Arc<OpsContext>,
    metadata: SharedMetadataDb,
    tx: flume::Sender<brain_ops::ExtractorEnqueue>,
    worker: ExtractorWorker,
    ctx: WorkerContext,
    llm_calls: Arc<AtomicUsize>,
    llm_peak_live: Arc<AtomicUsize>,
    llm_admit_wait: Arc<std::sync::atomic::AtomicU64>,
    llm_request_bytes: Arc<std::sync::atomic::AtomicU64>,
    hype_calls: Arc<AtomicUsize>,
    hype_peak_live: Arc<AtomicUsize>,
    hype_admit_wait: Arc<std::sync::atomic::AtomicU64>,
    _tempdir: tempfile::TempDir,
}

/// Build the whole derivation path: real pattern tier, a classifier tier
/// whose per-batch forward pass costs `classifier_cost`, a real
/// `LlmExtractor` over the delayed client, a wired HyPE generator, and the
/// resolver's embedding path.
fn build(batch_size: usize, classifier_cost: Duration, provider_latency: Duration) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let metadata: SharedMetadataDb =
        Arc::new(MetadataDb::open(tempdir.path().join("metadata.redb")).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let embedder: Arc<dyn Dispatcher> = Arc::new(ZeroDispatcher);
    let executor = ExecutorContext::new(
        embedder.clone(),
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );

    // ----- registry ------------------------------------------------------
    let mut registry = ExtractorRegistry::new();

    registry.register(Arc::new(
        PatternExtractor::try_new(
            ExtractorId::from(1),
            "brain:entity_mentions".into(),
            ExtractorTarget::EntityOrStatement,
            1,
            &[r"\b([A-Z]\.\s*[A-Z][a-z]+)\b".to_string()],
            0.7,
        )
        .unwrap(),
    ));

    registry.register(Arc::new(classifier::SpinningClassifier {
        id: ExtractorId::from(2),
        cost: classifier_cost,
    }));

    let llm_client = Arc::new(DelayedClient::new(
        "stand-in/extract",
        extraction_reply(0),
        provider_latency,
    ));
    let llm_calls = llm_client.calls.clone();
    let llm_peak_live = llm_client.peak_live.clone();
    let llm_admit_wait = llm_client.admitted_after.clone();
    let llm_request_bytes = llm_client.request_bytes.clone();
    let schema = response_schema();
    let compiled = LlmExtractor::compile_schema(Some(&schema)).unwrap();
    registry.register(Arc::new(LlmExtractor::build(
        ExtractorId::from(3),
        "brain:llm_predicate".into(),
        ExtractorTarget::EntityOrStatement,
        1,
        Arc::new(brain_llm::FixedResolver::new(
            llm_client as Arc<dyn LlmClient>,
        )),
        "stand-in/extract".into(),
        Some(Arc::new(PLMutex::new(
            LlmCacheDb::open(tempdir.path().join("llm_cache.redb")).unwrap(),
        ))),
        realistic_prompt(),
        None,
        Some(schema),
        compiled,
        0.0,
        Duration::from_secs(3600),
        None,
    )));

    let ops = Arc::new(
        brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)
            .with_extractor_registry(registry),
    );

    // ----- hype ----------------------------------------------------------
    let hype_client = Arc::new(DelayedClient::new(
        "stand-in/hype",
        hype_reply(),
        provider_latency,
    ));
    let hype_calls = hype_client.calls.clone();
    let hype_peak_live = hype_client.peak_live.clone();
    let hype_admit_wait = hype_client.admitted_after.clone();
    let hype = HypeGenerator::new(
        Arc::new(brain_llm::FixedResolver::new(
            hype_client as Arc<dyn LlmClient>,
        )),
        embedder.clone(),
        Arc::new(PLRwLock::new(
            brain_index::HypeHnswIndex::new(brain_index::hype_default_params()).unwrap(),
        )),
        metadata.clone(),
        Arc::new(PLMutex::new(
            LlmCacheDb::open(tempdir.path().join("hype_cache.redb")).unwrap(),
        )),
        4,
    );

    let (tx, rx) = flume::bounded::<brain_ops::ExtractorEnqueue>(1024);
    let worker = ExtractorWorker::new(rx)
        .with_hype(hype)
        .with_embed_deps(EmbeddingDeps {
            hnsw: Arc::new(PLRwLock::new(
                brain_index::entity_hnsw::EntityHnswIndex::new(
                    brain_index::entity_hnsw::EntityHnswParams::default_v1(),
                )
                .unwrap(),
            )),
            embedder,
            embed_threshold: brain_extractors::resolver::EMBED_RESOLVE_THRESHOLD,
        })
        .with_knobs(ExtractorKnobs {
            drain_per_cycle: DRAIN_PER_CYCLE,
            batch_size,
            ..ExtractorKnobs::default()
        })
        .with_config(brain_workers::WorkerConfig {
            interval: Duration::from_millis(1),
            max_runtime: Duration::from_secs(5),
            ..brain_workers::WorkerConfig::defaults_for(brain_workers::WorkerKind::Extractor)
        });

    let ctx = WorkerContext {
        ops: ops.clone(),
        shutdown: Arc::new(AtomicBool::new(false)),
    };

    Fixture {
        _ops: ops,
        metadata,
        tx,
        worker,
        ctx,
        llm_calls,
        llm_peak_live,
        llm_admit_wait,
        llm_request_bytes,
        hype_calls,
        hype_peak_live,
        hype_admit_wait,
        _tempdir: tempdir,
    }
}

mod classifier {
    use std::time::{Duration, Instant};

    use brain_core::{ExtractorId, ExtractorKind, Memory};
    use brain_extractors::{
        ExtractedItem, ExtractionContext, ExtractionFuture, ExtractionResult, Extractor,
    };

    /// Stands in for the GLiNER tier: one CPU-bound forward pass per batch,
    /// of a cost the caller sets. It spins rather than sleeps because a
    /// classifier occupies the lane it runs on, which is the property under
    /// test.
    pub struct SpinningClassifier {
        pub id: ExtractorId,
        pub cost: Duration,
    }

    impl Extractor for SpinningClassifier {
        fn id(&self) -> ExtractorId {
            self.id
        }
        fn kind(&self) -> ExtractorKind {
            ExtractorKind::Classifier
        }
        fn name(&self) -> &str {
            "brain:gliner"
        }
        fn extractor_version(&self) -> u32 {
            1
        }
        fn run<'a>(
            &'a self,
            _ctx: &'a ExtractionContext<'a>,
            _mem: &'a Memory,
        ) -> ExtractionFuture<'a> {
            Box::pin(async { ExtractionResult::success(Vec::<ExtractedItem>::new(), 0, 0) })
        }

        fn run_batch<'a>(
            &'a self,
            _ctx: &'a ExtractionContext<'a>,
            mems: &'a [Memory],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ExtractionResult>> + Send + 'a>>
        {
            let cost = self.cost;
            Box::pin(async move {
                if !cost.is_zero() {
                    let until = Instant::now() + cost;
                    while Instant::now() < until {
                        std::hint::spin_loop();
                    }
                }
                mems.iter()
                    .map(|_| ExtractionResult::success(Vec::<ExtractedItem>::new(), 0, 0))
                    .collect()
            })
        }
    }
}

/// Write a memory's row, text and durable queue entry, then nudge the wakeup
/// channel — the same three things a real ENCODE commits.
fn enqueue(fx: &Fixture, id: MemoryId, text: &str, queue_it: bool) {
    use brain_core::{MemoryKind, NamespaceId, SessionId};
    use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
    use brain_metadata::tables::text::TEXTS_TABLE;

    let space = SpaceId::derive_from_string("acme", "derivation-attribution");
    let row = MemoryMetadata::new_active(
        id,
        NamespaceId::SYSTEM,
        space,
        SessionId(0),
        0,
        0,
        MemoryKind::Episodic,
        [0u8; 16],
        1.0,
        0,
        now_unix_nanos(),
    );
    let wtxn = fx.metadata.write_txn().unwrap();
    {
        let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
        t.insert(&id.to_be_bytes(), &row).unwrap();
    }
    {
        let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
        t.insert(&id.to_be_bytes(), text.as_bytes()).unwrap();
    }
    if queue_it {
        brain_metadata::extraction_queue_enqueue(&wtxn, id, now_unix_nanos()).unwrap();
    }
    wtxn.commit().unwrap();
    if queue_it {
        let _ = fx.tx.send((id, Arc::from(text)));
    }
}

/// Seed a corpus of already-written memories so the neighbour fetch searches
/// a populated store.
fn seed_corpus(fx: &Fixture) {
    for i in 0..CORPUS_SIZE {
        enqueue(fx, memory_id(1_000_000 + i as u64), &memory_text(i), false);
    }
}

// ---------------------------------------------------------------------------
// The measurement.
// ---------------------------------------------------------------------------

async fn measure(batch_size: usize, classifier_cost: Duration) -> String {
    measure_with(batch_size, classifier_cost, PROVIDER_LATENCY).await
}

async fn measure_with(
    batch_size: usize,
    classifier_cost: Duration,
    provider_latency: Duration,
) -> String {
    let fx = build(batch_size, classifier_cost, provider_latency);
    seed_corpus(&fx);
    for i in 0..DRAIN_PER_CYCLE {
        enqueue(&fx, memory_id(i as u64 + 1), &memory_text(i + 7), true);
    }

    // The cycle's embed calls go through `glommio::executor().spawn_blocking`,
    // and `glommio::executor()` panics on a thread with no executor set, so the
    // engine future is polled inside one. `LocalExecutor::run` drives it on this
    // thread with no `Send`/`'static` bound, keeping the `!Send` fixture borrowed
    // in place. Built before the clock starts: ring setup is not cycle cost.
    let ex = glommio::LocalExecutor::default();

    derivation_probe::reset();
    let wall = Instant::now();
    let processed = ex.run(fx.worker.run_cycle(&fx.ctx)).unwrap();
    let wall = wall.elapsed();

    let (total, _) = derivation_probe::read(Stage::CycleTotal);
    let per_min = if wall.as_secs_f64() > 0.0 {
        processed as f64 * 60.0 / wall.as_secs_f64()
    } else {
        f64::INFINITY
    };

    let admission = brain_llm::admission::global();
    format!(
        "\n=== batch_size={batch_size} drain_per_cycle={DRAIN_PER_CYCLE} \
         provider_latency={provider_latency:?} classifier_cost={classifier_cost:?} ===\n\
         admission budget={:.2} MiB  request weight={} B  \
         prep reservation (this batch shape)={:.2} MiB\n\
         processed={processed}  wall={:.2}s  implied throughput={per_min:.0}/min\n\
         llm calls={} peak concurrent={} worst admit wait={:.1} ms\n\
         hype calls={} peak concurrent={} worst admit wait={:.1} ms\n\
         probe cycle_total={:.2}s (probe covers {:.1}% of wall)\n{}",
        admission.budget_bytes() as f64 / (1024.0 * 1024.0),
        fx.llm_request_bytes.load(Ordering::SeqCst),
        prep_reservation_bytes(batch_size) as f64 / (1024.0 * 1024.0),
        wall.as_secs_f64(),
        fx.llm_calls.load(Ordering::SeqCst),
        fx.llm_peak_live.load(Ordering::SeqCst),
        fx.llm_admit_wait.load(Ordering::SeqCst) as f64 / 1e6,
        fx.hype_calls.load(Ordering::SeqCst),
        fx.hype_peak_live.load(Ordering::SeqCst),
        fx.hype_admit_wait.load(Ordering::SeqCst) as f64 / 1e6,
        total as f64 / 1e9,
        100.0 * total as f64 / wall.as_nanos() as f64,
        derivation_probe::report(),
    )
}

/// Ten consecutive cycles over a store that keeps growing, to separate costs
/// that are constant per memory from costs that scale with what is already in
/// the graph. A one-cycle measurement on an empty store cannot tell them
/// apart, and a derivation path that slows down as it succeeds is a different
/// problem from one that was always slow.
#[ignore = "measurement harness; run explicitly"]
#[tokio::test(flavor = "current_thread")]
async fn cost_per_cycle_as_the_graph_grows() {
    let cycles = 10usize;
    let fx = build(32, Duration::ZERO, PROVIDER_LATENCY);
    seed_corpus(&fx);

    println!(
        "\ncycle  wall_s  pipeline_ms  tier_llm_ms  apply_ms  hype_ms  context_ms  vocab_ms  \
         cand_ms  rowfacts_ms  gate_ms  prepadmit_ms  refresh_ms  per_min"
    );
    // The cycle embeds through `glommio::executor()`, which panics on a thread
    // with no executor set; one `LocalExecutor` drives every cycle on this
    // thread, outside the per-cycle clock.
    let ex = glommio::LocalExecutor::default();
    let mut next_id = 1u64;
    for cycle in 0..cycles {
        for i in 0..DRAIN_PER_CYCLE {
            enqueue(
                &fx,
                memory_id(next_id),
                &memory_text(cycle * DRAIN_PER_CYCLE + i),
                true,
            );
            next_id += 1;
        }
        derivation_probe::reset();
        let wall = Instant::now();
        let processed = ex.run(fx.worker.run_cycle(&fx.ctx)).unwrap();
        let wall = wall.elapsed();
        let ms = |s: Stage| derivation_probe::read(s).0 as f64 / 1e6;
        println!(
            "{cycle:>5}  {:>6.2}  {:>11.1}  {:>11.1}  {:>8.1}  {:>7.1}  {:>10.1}  {:>8.1}  \
             {:>7.1}  {:>11.1}  {:>7.1}  {:>12.1}  {:>10.1}  {:>7.0}",
            wall.as_secs_f64(),
            ms(Stage::Pipeline),
            ms(Stage::LlmTier),
            ms(Stage::Apply),
            ms(Stage::HypePass),
            ms(Stage::ContextFetch),
            ms(Stage::Vocabulary),
            ms(Stage::CandidatePredicates),
            ms(Stage::RowFacts),
            ms(Stage::GateProbe),
            ms(Stage::PrepAdmit),
            ms(Stage::HypeRefresh),
            processed as f64 * 60.0 / wall.as_secs_f64(),
        );
    }
}

/// What `drain_batch` reserves for one micro-batch of this harness's shape,
/// priced by the same model the worker uses.
fn prep_reservation_bytes(batch_size: usize) -> u64 {
    let text = memory_text(0).len() as u64;
    brain_llm::prep::prep_bytes(&brain_llm::prep::PrepWorkload {
        memories: batch_size,
        memory_text_bytes: text,
        neighbours_per_memory: 10,
        neighbour_text_bytes: text,
        summary_bytes: text * 2,
        predicate_candidates: 20,
        embedding_dimensions: VECTOR_DIM,
        namespaces: batch_size,
        vocabulary_bytes_per_namespace: 8 * 1024,
    })
}

/// The byte budget is process-global and first-call-wins, so each budget
/// needs its own test — nextest gives every test its own process.
///
/// A batch reserves its whole preparation working set and holds it until the
/// batch finishes, while every provider call inside that batch draws on the
/// same budget. If preparation leaves too little behind, the fan-out that is
/// supposed to overlap thirty-two round trips admits them a few at a time
/// instead, and a batch costs round trips in series.
#[ignore = "measurement harness; run explicitly"]
#[tokio::test(flavor = "current_thread")]
async fn admission_at_the_machine_floor_budget() {
    brain_llm::admission::configure(8 * 1024 * 1024);
    println!("{}", measure(32, Duration::ZERO).await);
}

#[ignore = "measurement harness; run explicitly"]
#[tokio::test(flavor = "current_thread")]
async fn admission_at_a_budget_one_batch_nearly_fills() {
    brain_llm::admission::configure(2 * 1024 * 1024);
    println!("{}", measure(32, Duration::ZERO).await);
}

/// When a batch's preparation working set prices above the WHOLE budget,
/// admission clamps it to the budget and runs the batch alone — but the
/// batch holds that reservation across its own provider calls, which draw on
/// the same budget. Nothing is left for them, and the batch waits on itself.
/// Bounded here so the harness reports the stall instead of hanging.
#[ignore = "measurement harness; run explicitly"]
#[tokio::test(flavor = "current_thread")]
async fn admission_at_a_budget_one_batch_overruns() {
    brain_llm::admission::configure(1024 * 1024);
    let budget = brain_llm::admission::global().budget_bytes();
    let prep = prep_reservation_bytes(32);
    println!(
        "\nbudget={} B  prep reservation={} B  prep/budget={:.2}x",
        budget,
        prep,
        prep as f64 / budget as f64
    );
    match tokio::time::timeout(Duration::from_secs(25), measure(32, Duration::ZERO)).await {
        Ok(report) => println!("{report}"),
        Err(_) => println!(
            "STALLED: no cycle in 25 s. admission available={} of {} bytes; \
             outstanding holds={}, outstanding bytes={}",
            brain_llm::admission::global().available_bytes(),
            budget,
            brain_llm::inflight::global().holds(),
            brain_llm::inflight::global().bytes(),
        ),
    }
}

/// Apply cost against graph size, with the provider taken out of the way.
///
/// Apply is strictly serial per memory inside a batch, and everything it does
/// — resolving each surface against the entity table, minting what is new,
/// inserting into the entity HNSW, writing statements and relations — is work
/// against a store that this very loop keeps enlarging. A per-memory cost that
/// grows with what is already there is the one shape that turns a cycle which
/// is fast on an empty store into one that is slow on a real one, and a
/// single-cycle measurement cannot see it.
#[ignore = "measurement harness; run explicitly"]
#[tokio::test(flavor = "current_thread")]
async fn apply_cost_against_graph_size() {
    let cycles = 30usize;
    let fx = build(32, Duration::ZERO, Duration::ZERO);
    seed_corpus(&fx);

    println!(
        "\nmemories_applied  wall_s  apply_ms  apply_ms_per_memory  hype_ms  context_ms  per_min"
    );
    // The cycle embeds through `glommio::executor()`, which panics on a thread
    // with no executor set; one `LocalExecutor` drives every cycle on this
    // thread, outside the per-cycle clock.
    let ex = glommio::LocalExecutor::default();
    let mut next_id = 1u64;
    let mut applied = 0usize;
    for cycle in 0..cycles {
        for i in 0..DRAIN_PER_CYCLE {
            enqueue(
                &fx,
                memory_id(next_id),
                &memory_text(cycle * DRAIN_PER_CYCLE + i),
                true,
            );
            next_id += 1;
        }
        derivation_probe::reset();
        let wall = Instant::now();
        let processed = ex.run(fx.worker.run_cycle(&fx.ctx)).unwrap();
        let wall = wall.elapsed();
        applied += processed;
        let ms = |s: Stage| derivation_probe::read(s).0 as f64 / 1e6;
        println!(
            "{applied:>16}  {:>6.2}  {:>8.1}  {:>19.2}  {:>7.1}  {:>10.1}  {:>7.0}",
            wall.as_secs_f64(),
            ms(Stage::Apply),
            ms(Stage::Apply) / processed.max(1) as f64,
            ms(Stage::HypePass),
            ms(Stage::ContextFetch),
            processed as f64 * 60.0 / wall.as_secs_f64(),
        );
    }
}

/// The same cycle with the provider answering instantly. Whatever remains is
/// the derivation path's own serial floor: the most memories per minute this
/// shard could ever derive if a provider cost nothing.
#[ignore = "measurement harness; run explicitly"]
#[tokio::test(flavor = "current_thread")]
async fn the_serial_floor_with_no_provider_latency() {
    println!("{}", measure_with(32, Duration::ZERO, Duration::ZERO).await);
}

/// The admission cliff, measured directly rather than through the worker.
///
/// A batch holds its preparation reservation across its own provider calls,
/// and both draw on one budget. While the reservation leaves a request's
/// worth behind, the fan-out proceeds; once it does not, the batch waits for
/// bytes only it can release.
#[ignore = "measurement harness; run explicitly"]
#[tokio::test(flavor = "current_thread")]
async fn a_prep_reservation_that_fills_the_budget_blocks_its_own_calls() {
    brain_llm::admission::configure(4 * 1024 * 1024);
    let budget = brain_llm::admission::global().budget_bytes();
    let request = 19_215u64;
    println!("\nbudget={budget} B, one request weighs {request} B");
    for prep in [
        budget / 4,
        budget / 2,
        budget - 2 * request,
        budget - request / 2,
        budget,
        budget * 2,
    ] {
        let held = brain_llm::admission::global()
            .reserve_prep_bytes(prep)
            .await;
        let left = brain_llm::admission::global().available_bytes();
        let admitted = tokio::time::timeout(
            Duration::from_millis(250),
            brain_llm::admission::global().admit_bytes(request),
        )
        .await;
        println!(
            "prep={:>9} B ({:>5.2}x budget)  left={:>9} B  a provider call is {}",
            prep,
            prep as f64 / budget as f64,
            left,
            if admitted.is_ok() {
                "admitted"
            } else {
                "BLOCKED"
            },
        );
        drop(admitted);
        drop(held);
    }
}

/// Full attribution at the production width.
#[ignore = "measurement harness; run explicitly"]
#[tokio::test(flavor = "current_thread")]
async fn where_one_derivation_cycle_spends_its_wall_clock() {
    println!("{}", measure(32, Duration::ZERO).await);
}

/// The same cycle with the classifier tier given a real CPU cost, to see
/// whether a CPU-bound tier ahead of the LLM tier changes the split.
#[ignore = "measurement harness; run explicitly"]
#[tokio::test(flavor = "current_thread")]
async fn a_cpu_bound_classifier_tier_changes_the_split() {
    println!("{}", measure(32, Duration::from_millis(400)).await);
}

/// Does four times the width buy four times the throughput, or two?
#[ignore = "measurement harness; run explicitly"]
#[tokio::test(flavor = "current_thread")]
async fn width_sweep() {
    for width in [8usize, 16, 32] {
        println!("{}", measure(width, Duration::ZERO).await);
    }
}
