#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Entity-HNSW population on the extractor apply path.
//!
//! The resolver's embedding tier (3b) is only reachable when the entity
//! HNSW actually holds the entities earlier writes minted, and the index
//! is in-RAM: nothing but the apply path fills it between boots. These
//! tests pin that contract end to end, with a disambiguator wired — the
//! production configuration, since an LLM key is a hard boot requirement
//! — so the two-phase `Collect`/`Replay` flow is the one under test:
//!
//! * an extraction that mints entities leaves the index non-empty;
//! * a paraphrase above the cosine threshold folds onto the existing
//!   entity instead of minting a second node;
//! * a surface below the partial-match floor stays its own entity;
//! * a plan pass that gets rolled back contributes NO points (the index
//!   has no removal, so a leaked point would be permanent).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::ExtractorKind;
use brain_core::{SpaceId, EntityId, ExtractorId, Memory as CoreMemory, MemoryId};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_extractors::resolver::{EmbeddingDeps, EntityDisambiguator, EMBED_RESOLVE_THRESHOLD};
use brain_extractors::{
    EntityMention, ExtractedItem, ExtractionContext, ExtractionFuture, ExtractionResult, Extractor,
    ExtractorRegistry,
};
use brain_index::entity_hnsw::{EntityHnswIndex, EntityHnswParams};
use brain_index::{IndexParams, SharedHnsw};
use brain_llm::client::{model_id_hash, LlmFuture};
use brain_llm::{LlmClient, LlmRequest, LlmResponse};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{ExtractorWorker, Worker, WorkerContext};
use parking_lot::RwLock as PLRwLock;
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

// ---------------------------------------------------------------------------
// Surfaces + their staged embeddings.
//
// Cosines mirror the ones measured on the real BGE embedder for the case
// that motivated these tests: "billing team" vs "billing platform team"
// sits at 0.937 (auto-alias, threshold 0.78) while "Diego's team" sits at
// ~0.60 (below the 0.70 partial-match floor, correctly distinct).
// "ops crew" is placed inside the ambiguous band on purpose — that is the
// only thing that makes the plan pass roll back.
//
// Three axes, not two: a plane forces every pair's cosine to be a
// function of the other two, which would drag "Diego's team" up next to
// "ops crew" by construction rather than by intent.
// ---------------------------------------------------------------------------

const BILLING_TEAM: &str = "billing team";
const BILLING_PLATFORM_TEAM: &str = "billing platform team";
const DIEGOS_TEAM: &str = "Diego's team";
const OPS_CREW: &str = "ops crew";

const SURFACES: [&str; 4] = [BILLING_TEAM, BILLING_PLATFORM_TEAM, DIEGOS_TEAM, OPS_CREW];

const ENTITY_TYPE_QNAME: &str = "brain:Organization";

/// Unit vector with the given weights on three fixed, otherwise-unused
/// axes. Cosine between two such vectors is the dot product of their
/// (normalised) weights.
fn unit3(a: f32, b: f32, c: f32) -> [f32; VECTOR_DIM] {
    let norm = (a * a + b * b + c * c).sqrt();
    let mut v = [0.0_f32; VECTOR_DIM];
    v[10] = a / norm;
    v[11] = b / norm;
    v[12] = c / norm;
    v
}

fn surface_vector(surface: &str) -> Option<[f32; VECTOR_DIM]> {
    match surface {
        BILLING_TEAM => Some(unit3(1.0, 0.0, 0.0)),
        // cosine 0.937 against BILLING_TEAM — well above the 0.78 threshold.
        BILLING_PLATFORM_TEAM => Some(unit3(0.937, 0.349_4, 0.0)),
        // cosine 0.60 against BILLING_TEAM and 0.438 against OPS_CREW —
        // below the 0.70 partial-match floor on both counts.
        DIEGOS_TEAM => Some(unit3(0.6, 0.0, 0.8)),
        // cosine 0.73 against BILLING_TEAM — inside [0.70, 0.78), the
        // ambiguous band that defers to the disambiguator.
        OPS_CREW => Some(unit3(0.73, 0.683_4, 0.0)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

/// Embedder that answers from the surface table above and sends
/// everything else to its own far-away axis (cosine ~0 against the
/// fixtures), so incidental text can never perturb a resolve.
struct ScriptedEmbedder {
    fallbacks: StdMutex<HashMap<String, [f32; VECTOR_DIM]>>,
}

impl ScriptedEmbedder {
    fn new() -> Self {
        Self {
            fallbacks: StdMutex::new(HashMap::new()),
        }
    }
}

impl Dispatcher for ScriptedEmbedder {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        if let Some(v) = surface_vector(text) {
            return Ok(v);
        }
        let mut fallbacks = self.fallbacks.lock().expect("fallback table");
        let next = fallbacks.len();
        Ok(*fallbacks.entry(text.to_string()).or_insert_with(|| {
            let mut v = [0.0_f32; VECTOR_DIM];
            v[64 + next % (VECTOR_DIM - 64)] = 1.0;
            v
        }))
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    fn fingerprint(&self) -> [u8; 16] {
        [0x5A; 16]
    }
}

/// Disambiguator backend that always confirms. Both the confirm and the
/// uncertain replies merge an above-threshold candidate, so the verdict
/// isn't what these tests are measuring — what matters is that asking for
/// one forces the plan pass to be rolled back and replayed.
struct AlwaysYesLlm;

impl LlmClient for AlwaysYesLlm {
    fn complete<'a>(&'a self, _request: LlmRequest) -> LlmFuture<'a> {
        Box::pin(async {
            Ok(LlmResponse {
                content: "YES 0.95".to_string(),
                tokens_in: 8,
                tokens_out: 4,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_micro_usd: 1,
                model_version: "fake-disambiguator-v1".to_string(),
            })
        })
    }

    fn model(&self) -> &str {
        "fake-disambiguator"
    }

    fn model_id_hash(&self) -> u64 {
        model_id_hash("fake-disambiguator")
    }
}

/// Extractor that files an `EntityMention` for every fixture surface that
/// appears in the memory text. Stands in for the pattern / classifier
/// tiers without pulling a model into the test.
struct SurfaceMentionStub {
    id: ExtractorId,
}

impl Extractor for SurfaceMentionStub {
    fn id(&self) -> ExtractorId {
        self.id
    }
    fn kind(&self) -> ExtractorKind {
        ExtractorKind::Pattern
    }
    fn name(&self) -> &str {
        "test:surface_mentions"
    }
    fn extractor_version(&self) -> u32 {
        1
    }
    fn run<'a>(
        &'a self,
        _ctx: &'a ExtractionContext<'a>,
        mem: &'a CoreMemory,
    ) -> ExtractionFuture<'a> {
        let id = self.id;
        Box::pin(async move {
            let text = mem.text.as_deref().unwrap_or_default();
            let items: Vec<ExtractedItem> = SURFACES
                .iter()
                .filter_map(|surface| {
                    let start = text.find(surface)?;
                    Some(ExtractedItem::EntityMention(EntityMention {
                        entity_type_qname: ENTITY_TYPE_QNAME.to_string(),
                        text: (*surface).to_string(),
                        start,
                        end: start + surface.len(),
                        confidence: 0.9,
                        extractor_id: id.raw(),
                        extractor_version: 1,
                    }))
                })
                .collect();
            ExtractionResult::success(items, 0, 0)
        })
    }
}

struct Fixture {
    metadata: SharedMetadataDb,
    entity_hnsw: Arc<PLRwLock<EntityHnswIndex>>,
    extractor_tx: flume::Sender<brain_ops::ExtractorEnqueue>,
    worker: ExtractorWorker,
    ctx: WorkerContext,
    _ops: Arc<OpsContext>,
    _tempdir: tempfile::TempDir,
}

impl Fixture {
    /// Mirror `apply_upsert_memory`: memory row + text + durable queue
    /// row in one commit, then nudge the worker's wakeup channel.
    fn enqueue(&self, memory_id: MemoryId, text: &str) {
        use brain_core::{ContextId, MemoryKind, NamespaceId};
        use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
        use brain_metadata::tables::text::TEXTS_TABLE;

        let now = now_unix_nanos();
        let row = MemoryMetadata::new_active(
            memory_id,
            NamespaceId::SYSTEM,
            SpaceId(uuid::Uuid::from_bytes([0xC3; 16])),
            ContextId(0),
            0,
            0,
            MemoryKind::Episodic,
            [0u8; 16],
            1.0,
            0,
            now,
        );
        let wtxn = self.metadata.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&memory_id.to_be_bytes(), &row).unwrap();
        }
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&memory_id.to_be_bytes(), text.as_bytes()).unwrap();
        }
        brain_metadata::extraction_queue_enqueue(&wtxn, memory_id, now).unwrap();
        wtxn.commit().unwrap();
        let _ = self.extractor_tx.send((memory_id, Arc::from(text)));
    }

    /// Every live entity as `(id, canonical_name, has_stored_vector)`.
    fn live_entities(&self) -> Vec<(EntityId, String, bool)> {
        let rtxn = self.metadata.read_txn().unwrap();
        brain_metadata::entity::ops::entity_iter_all_live_with_vectors(&rtxn)
            .unwrap()
            .into_iter()
            .map(|(id, name, vector)| (id, name, vector.is_some()))
            .collect()
    }

    fn hnsw_len(&self) -> usize {
        self.entity_hnsw.read().len()
    }
}

fn build_fixture() -> Fixture {
    build_fixture_with_embedder(Arc::new(ScriptedEmbedder::new()))
}

fn build_fixture_with_embedder(embedder: Arc<dyn Dispatcher>) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let metadata: SharedMetadataDb =
        Arc::new(MetadataDb::open(tempdir.path().join("metadata.redb")).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        embedder.clone(),
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );

    let mut registry = ExtractorRegistry::new();
    registry.register(Arc::new(SurfaceMentionStub {
        id: ExtractorId::from(4242),
    }));
    let ops = Arc::new(
        brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)
            .with_extractor_registry(registry),
    );

    let entity_hnsw = Arc::new(PLRwLock::new(
        EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap(),
    ));
    let (tx, rx) = flume::bounded::<brain_ops::ExtractorEnqueue>(64);
    let worker = ExtractorWorker::new(rx)
        .with_embed_deps(EmbeddingDeps {
            hnsw: entity_hnsw.clone(),
            embedder,
            embed_threshold: EMBED_RESOLVE_THRESHOLD,
        })
        .with_entity_disambiguator(Arc::new(EntityDisambiguator::new(
            Arc::new(AlwaysYesLlm) as Arc<dyn LlmClient>,
            "fake-disambiguator",
        )));

    let ctx = WorkerContext {
        ops: ops.clone(),
        shutdown: Arc::new(AtomicBool::new(false)),
    };

    Fixture {
        metadata,
        entity_hnsw,
        extractor_tx: tx,
        worker,
        ctx,
        _ops: ops,
        _tempdir: tempdir,
    }
}

fn make_memory_id(slot: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[8..16].copy_from_slice(&slot.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

async fn encode(fixture: &Fixture, slot: u64, text: &str) {
    fixture.enqueue(make_memory_id(slot), text);
    let drained = fixture.worker.run_cycle(&fixture.ctx).await.unwrap();
    assert_eq!(drained, 1, "expected the cycle to drain one memory");
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// The regression: an extraction that mints entities must leave the
/// entity HNSW non-empty. It was empty for the whole session, because the
/// pass that commits (plan pass, nothing ambiguous) deliberately skipped
/// index population — which starved tier-3b permanently, since a
/// partial-match can only come FROM the index.
#[tokio::test(flavor = "current_thread")]
async fn extraction_populates_the_entity_hnsw() {
    let fixture = build_fixture();
    assert_eq!(fixture.hnsw_len(), 0, "index starts empty");

    encode(&fixture, 1, "the billing team owns invoicing").await;

    let entities = fixture.live_entities();
    assert_eq!(entities.len(), 1, "one entity minted: {entities:?}");
    assert_eq!(
        fixture.hnsw_len(),
        1,
        "the committed pass must publish its entity vector into the HNSW",
    );
    let (id, _, has_vector) = &entities[0];
    assert!(
        fixture.entity_hnsw.read().contains(*id),
        "the minted entity is the point that landed",
    );
    assert!(
        has_vector,
        "the durable vector row must land too, so the boot rebuild can restore the index",
    );
}

/// Two surfaces whose cosine clears the threshold (0.937 vs 0.78), each
/// encoded in its own memory, resolve to ONE entity. This only works if
/// the first memory's entity reached the index.
#[tokio::test(flavor = "current_thread")]
async fn paraphrase_above_threshold_folds_onto_the_existing_entity() {
    let fixture = build_fixture();

    encode(&fixture, 1, "the billing team owns invoicing").await;
    encode(
        &fixture,
        2,
        "the billing platform team shipped the migration",
    )
    .await;

    let entities = fixture.live_entities();
    assert_eq!(
        entities.len(),
        1,
        "the paraphrase must alias onto the first entity, not mint a second: {entities:?}",
    );
    assert_eq!(entities[0].1, BILLING_TEAM, "first surface stays canonical");
    assert_eq!(fixture.hnsw_len(), 1, "an alias adds no point");
}

/// The over-merge guard: a surface below the partial-match floor (~0.60
/// vs a 0.70 floor) stays its own entity. Nothing about reviving tier-3b
/// may pull genuinely distinct teams together.
#[tokio::test(flavor = "current_thread")]
async fn below_floor_surface_stays_a_separate_entity() {
    let fixture = build_fixture();

    encode(&fixture, 1, "the billing team owns invoicing").await;
    encode(&fixture, 2, "Diego's team reviewed the rollout").await;

    let mut names: Vec<String> = fixture.live_entities().into_iter().map(|e| e.1).collect();
    names.sort();
    assert_eq!(
        names,
        vec![DIEGOS_TEAM.to_string(), BILLING_TEAM.to_string()],
        "a below-floor surface must stay distinct",
    );
    assert_eq!(fixture.hnsw_len(), 2, "both entities are indexed");
}

/// A memory that lands a candidate in the ambiguous band makes the
/// extractor discard its plan pass and replay it. The discarded pass
/// minted entity rows redb rolled back — the HNSW, which cannot remove a
/// point, must have kept none of them. One point per live entity, exactly.
#[tokio::test(flavor = "current_thread")]
async fn rolled_back_plan_pass_leaves_no_ghost_points() {
    let fixture = build_fixture();

    encode(&fixture, 1, "the billing team owns invoicing").await;
    assert_eq!(fixture.hnsw_len(), 1);

    // "ops crew" sits at cosine 0.73 against "billing team" — inside the
    // ambiguous band, so the plan pass defers to the LLM and is rolled
    // back. "Diego's team" rides along and IS minted, twice: once in the
    // discarded plan pass, once in the replay that commits.
    encode(
        &fixture,
        2,
        "the ops crew paged Diego's team about the incident",
    )
    .await;

    let entities = fixture.live_entities();
    assert_eq!(
        fixture.hnsw_len(),
        entities.len(),
        "one HNSW point per live entity — a rolled-back pass must contribute none: {entities:?}",
    );
    for (id, name, _) in &entities {
        assert!(
            fixture.entity_hnsw.read().contains(*id),
            "live entity {name} missing from the index",
        );
    }
}

/// The same three surfaces driven through the REAL BGE-small embedder
/// instead of the scripted table — the check that the fixture cosines
/// above still describe the model actually shipped. Ignored by default
/// because it needs the model on disk; run with
/// `--ignored` once `~/.local/share/brain/models/bge-small-en-v1.5`
/// (or `BRAIN_EMBED_MODEL_DIR`) is populated.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires the BGE-small model directory on disk"]
async fn real_embeddings_merge_the_paraphrase_and_keep_the_distinct_team() {
    use brain_embed::{CpuDispatcher, EmbedderConfig, ModelHandle};

    let model_dir = std::env::var("BRAIN_EMBED_MODEL_DIR").unwrap_or_else(|_| {
        format!(
            "{}/.local/share/brain/models/bge-small-en-v1.5",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let model = ModelHandle::load(&EmbedderConfig::new(model_dir.into())).expect("load BGE-small");
    let embedder: Arc<dyn Dispatcher> = Arc::new(CpuDispatcher::new(model));

    // Report the real cosines the resolver will see, so a model swap that
    // moves them shows up in the test output rather than as a silent
    // behaviour change.
    let cos = |a: &str, b: &str| {
        let (va, vb) = (embedder.embed(a).unwrap(), embedder.embed(b).unwrap());
        va.iter().zip(vb.iter()).map(|(x, y)| x * y).sum::<f32>()
    };
    println!(
        "cosines: platform={:.3} diego={:.3} (threshold {EMBED_RESOLVE_THRESHOLD})",
        cos(BILLING_TEAM, BILLING_PLATFORM_TEAM),
        cos(BILLING_TEAM, DIEGOS_TEAM),
    );

    let fixture = build_fixture_with_embedder(embedder);
    encode(&fixture, 1, "the billing team owns invoicing").await;
    encode(
        &fixture,
        2,
        "the billing platform team shipped the migration",
    )
    .await;
    encode(&fixture, 3, "Diego's team reviewed the rollout").await;

    let mut names: Vec<String> = fixture.live_entities().into_iter().map(|e| e.1).collect();
    names.sort();
    assert_eq!(
        names,
        vec![DIEGOS_TEAM.to_string(), BILLING_TEAM.to_string()],
        "real embeddings: the paraphrase folds in, the distinct team does not",
    );
}
