//! End-to-end coverage for the partial-match disambiguator.
//!
//! Exercises the resolver path that fires when the embedding tier
//! lands a candidate in the ambiguous band (`[PARTIAL_MATCH_FLOOR,
//! EMBED_RESOLVE_THRESHOLD)`). The disambiguator is faked so the
//! tests pin the resolver's *act on the verdict* behaviour:
//!
//! - `Confirmed` -> alias onto the existing entity, tier =
//!   `Disambiguated`, no merge proposal.
//! - `Rejected`  -> mint a fresh entity, tier = `Created`, no merge
//!   proposal (the disambiguator already ruled them apart).
//! - `Uncertain` -> mint a fresh entity, tier = `Created`, merge
//!   proposal enqueued (existing fallback).
//!
//! The fakes deliberately bypass `BrainLlmDisambiguator` and the
//! `LlmCandidateView` views — the production resolver's
//! single-candidate confirmation calls its own one-line prompt grammar
//! (`YES <conf>` / `NO` / `UNCERTAIN`), so the fake `LlmClient` just
//! returns the canned reply text.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use brain_core::{Entity, EntityId, EntityType, EntityTypeId};
use brain_embed::{Dispatcher, EmbedError};
use brain_extractors::resolver::{
    resolve_or_create_with_deps, EmbeddingDeps, EntityDisambiguator, ResolutionTier,
};
use brain_index::entity_hnsw::{EntityHnswIndex, EntityHnswParams};
use brain_index::VECTOR_DIM;
use brain_llm::client::LlmFuture;
use brain_llm::types::{LlmRequest, LlmResponse};
use brain_llm::LlmClient;
use brain_metadata::entity::ops::{entity_get, entity_put, normalize_name};
use brain_metadata::MetadataDb;
use parking_lot::RwLock;
use tempfile::TempDir;

const NOW: u64 = 1_700_000_000_000_000_000;

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

/// Stand-in for a real LLM backend. The reply text is fixed at
/// construction; `complete` always echoes it back and bumps a call
/// counter so tests can assert the disambiguator actually fired.
struct FakeDisambiguator {
    reply: String,
    calls: Mutex<u32>,
}

impl FakeDisambiguator {
    fn confirming() -> Arc<Self> {
        Arc::new(Self {
            reply: "YES 0.95".into(),
            calls: Mutex::new(0),
        })
    }

    fn rejecting() -> Arc<Self> {
        Arc::new(Self {
            reply: "NO".into(),
            calls: Mutex::new(0),
        })
    }

    fn uncertain() -> Arc<Self> {
        Arc::new(Self {
            reply: "UNCERTAIN".into(),
            calls: Mutex::new(0),
        })
    }

    fn call_count(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

impl LlmClient for FakeDisambiguator {
    fn complete<'a>(&'a self, _request: LlmRequest) -> LlmFuture<'a> {
        *self.calls.lock().unwrap() += 1;
        let content = self.reply.clone();
        Box::pin(async move {
            Ok(LlmResponse {
                content,
                tokens_in: 0,
                tokens_out: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_micro_usd: 0,
                model_version: "fake-model".into(),
            })
        })
    }

    fn model(&self) -> &str {
        "fake-model"
    }

    fn model_id_hash(&self) -> u64 {
        0
    }
}

/// Scripted embedder: surface forms in the table map to fixed
/// vectors. Unknown queries land on a hash-derived far axis so they
/// don't accidentally match the seed corpus.
struct ScriptedEmbedder {
    table: Mutex<HashMap<String, [f32; VECTOR_DIM]>>,
}

impl ScriptedEmbedder {
    fn new() -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
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
        let h = blake3::hash(text.as_bytes());
        let axis = (u32::from_le_bytes([
            h.as_bytes()[0],
            h.as_bytes()[1],
            h.as_bytes()[2],
            h.as_bytes()[3],
        ]) as usize
            % (VECTOR_DIM - 64))
            + 64;
        let mut v = [0.0_f32; VECTOR_DIM];
        v[axis] = 1.0;
        Ok(v)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    fn fingerprint(&self) -> [u8; 16] {
        [0xAB; 16]
    }
}

/// Build a unit vector with weight `peak_w` on `peak` axis and `co_w`
/// on `co` axis. Dot product between two such vectors with the same
/// peak axis equals `peak_w_a * peak_w_b + co_w_a * co_w_b` after
/// L2 normalisation — a tractable cosine knob for the tests.
fn axis_pair(peak: usize, co: usize, peak_w: f32, co_w: f32) -> [f32; VECTOR_DIM] {
    let mut v = [0.0_f32; VECTOR_DIM];
    let norm = (peak_w * peak_w + co_w * co_w).sqrt();
    v[peak] = peak_w / norm;
    v[co] = co_w / norm;
    v
}

fn fresh_db() -> (TempDir, MetadataDb) {
    let dir = TempDir::new().unwrap();
    let db = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    (dir, db)
}

fn fresh_hnsw() -> Arc<RwLock<EntityHnswIndex>> {
    Arc::new(RwLock::new(
        EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap(),
    ))
}

fn embed_deps_for(
    embedder: Arc<ScriptedEmbedder>,
    hnsw: Arc<RwLock<EntityHnswIndex>>,
) -> EmbeddingDeps {
    EmbeddingDeps {
        hnsw,
        embedder: embedder as Arc<dyn Dispatcher>,
        embed_threshold: brain_extractors::resolver::EMBED_RESOLVE_THRESHOLD,
    }
}

fn seed_entity(
    db: &mut MetadataDb,
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
    let wtxn = db.write_txn().unwrap();
    entity_put(&wtxn, &ent).unwrap();
    wtxn.commit().unwrap();
    hnsw.write().insert(id, &vector).unwrap();
    id
}

/// Stage: two entities with embedding-similar names + a third
/// surface form whose embedding lands in the partial-match band
/// (cosine ~0.75 against the closer seed). The closer seed is what
/// the disambiguator gets asked about.
fn ambiguous_band_scenario(
    db: &mut MetadataDb,
    hnsw: &Arc<RwLock<EntityHnswIndex>>,
    embedder: &Arc<ScriptedEmbedder>,
) -> EntityId {
    // Closer seed at cosine ~1.0 with its canonical embedding.
    let acme_v = axis_pair(50, 51, 1.0, 0.0);
    // Distant seed — same type, different region of vector space; the
    // HNSW will still return both but the type filter + threshold
    // keep the disambiguator focused on the closer one.
    let omega_v = axis_pair(300, 301, 1.0, 0.0);
    // Probe lands at cosine = 0.75 against the closer seed — squarely
    // inside [PARTIAL_MATCH_FLOOR=0.7, EMBED_RESOLVE_THRESHOLD=0.78).
    let probe_v = axis_pair(50, 51, 0.75, 0.661);

    embedder.set("Acme Holdings", probe_v);

    let closer_id = seed_entity(db, hnsw, EntityType::PERSON_ID, "Acme", acme_v);
    let _other_id = seed_entity(db, hnsw, EntityType::PERSON_ID, "Omega", omega_v);
    closer_id
}

/// Stage: a seed entity plus a surface form whose embedding lands
/// ABOVE the auto-alias threshold (cosine ~0.95 against the seed) yet
/// is semantically a different real-world entity (the "Japan" vs
/// "Tokyo" failure: two same-type Places that embed close). The type
/// filter cannot separate them, so the disambiguator is the only thing
/// that can. Returns the seed id the auto-alias path would merge into.
fn auto_alias_band_scenario(
    db: &mut MetadataDb,
    hnsw: &Arc<RwLock<EntityHnswIndex>>,
    embedder: &Arc<ScriptedEmbedder>,
) -> EntityId {
    let seed_v = axis_pair(70, 71, 1.0, 0.0);
    // Probe at cosine ~0.954 against the seed — comfortably above
    // EMBED_RESOLVE_THRESHOLD = 0.78, so tier-3b returns AutoAlias.
    let probe_v = axis_pair(70, 71, 0.954, 0.3);
    embedder.set("Japan", probe_v);
    seed_entity(db, hnsw, EntityType::PERSON_ID, "Tokyo", seed_v)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn auto_alias_rejected_by_disambiguator_creates_distinct_entity() {
    // The m9/"Japan" regression: a high-cosine same-type neighbour must
    // NOT be auto-merged when the disambiguator says the two are
    // different entities.
    let embedder = Arc::new(ScriptedEmbedder::new());
    let (_dir, mut db) = fresh_db();
    let hnsw = fresh_hnsw();
    let seed_id = auto_alias_band_scenario(&mut db, &hnsw, &embedder);

    let backend = FakeDisambiguator::rejecting();
    let disambiguator = Arc::new(EntityDisambiguator::new(backend.clone(), "fake-model"));
    let embed_deps = embed_deps_for(embedder, hnsw);

    let wtxn = db.write_txn().unwrap();
    let res = resolve_or_create_with_deps(
        &wtxn,
        "Japan",
        "brain:Person",
        0.9,
        NOW + 1,
        Some(&embed_deps),
        Some(disambiguator.as_ref()),
    )
    .unwrap();
    wtxn.commit().unwrap();

    assert_eq!(
        res.tier,
        ResolutionTier::Created,
        "rejected high-cosine match -> fresh entity",
    );
    assert_ne!(
        res.entity_id, seed_id,
        "must not merge a distinct entity onto the seed",
    );
    assert_eq!(
        backend.call_count(),
        1,
        "disambiguator must vet the auto-alias candidate",
    );

    // The seed must NOT have picked up the surface form as an alias.
    let rtxn = db.read_txn().unwrap();
    let seed = entity_get(&rtxn, seed_id).unwrap().unwrap();
    assert!(
        !seed.aliases.iter().any(|a| a == "Japan"),
        "distinct entity wrongly aliased onto seed; got {:?}",
        seed.aliases,
    );

    // A confirmed-distinct verdict leaves nothing to review.
    let pending = brain_metadata::entity::review::list_proposals_by_status(
        &rtxn,
        brain_metadata::tables::merge_review_queue::proposal_status::PENDING,
        16,
    )
    .unwrap();
    assert!(
        pending.is_empty(),
        "rejected auto-alias must not enqueue a merge proposal; got {}",
        pending.len(),
    );
}

#[test]
fn auto_alias_confirmed_by_disambiguator_still_merges() {
    // The paraphrase-merge guard: a genuine same-entity match
    // ("Stripe Inc." vs "Stripe Payments") confirmed by the
    // disambiguator must still alias onto the existing entity.
    let embedder = Arc::new(ScriptedEmbedder::new());
    let (_dir, mut db) = fresh_db();
    let hnsw = fresh_hnsw();
    let seed_id = auto_alias_band_scenario(&mut db, &hnsw, &embedder);

    let backend = FakeDisambiguator::confirming();
    let disambiguator = Arc::new(EntityDisambiguator::new(backend.clone(), "fake-model"));
    let embed_deps = embed_deps_for(embedder, hnsw);

    let wtxn = db.write_txn().unwrap();
    let res = resolve_or_create_with_deps(
        &wtxn,
        "Japan",
        "brain:Person",
        0.9,
        NOW + 1,
        Some(&embed_deps),
        Some(disambiguator.as_ref()),
    )
    .unwrap();
    wtxn.commit().unwrap();

    assert_eq!(res.entity_id, seed_id, "confirmed -> alias onto seed");
    assert_eq!(res.tier, ResolutionTier::Disambiguated);
    assert_eq!(backend.call_count(), 1);

    let rtxn = db.read_txn().unwrap();
    let seed = entity_get(&rtxn, seed_id).unwrap().unwrap();
    assert!(
        seed.aliases.iter().any(|a| a == "Japan"),
        "confirmed match must alias onto seed; got {:?}",
        seed.aliases,
    );
}

#[test]
fn auto_alias_with_no_disambiguator_merges_on_threshold_alone() {
    // Back-compat: when no disambiguator is wired, the cosine threshold
    // stays the sole arbiter and the high-cosine match auto-aliases.
    let embedder = Arc::new(ScriptedEmbedder::new());
    let (_dir, mut db) = fresh_db();
    let hnsw = fresh_hnsw();
    let seed_id = auto_alias_band_scenario(&mut db, &hnsw, &embedder);

    let embed_deps = embed_deps_for(embedder, hnsw);

    let wtxn = db.write_txn().unwrap();
    let res = resolve_or_create_with_deps(
        &wtxn,
        "Japan",
        "brain:Person",
        0.9,
        NOW + 1,
        Some(&embed_deps),
        None,
    )
    .unwrap();
    wtxn.commit().unwrap();

    assert_eq!(res.entity_id, seed_id);
    assert_eq!(res.tier, ResolutionTier::Embedding);
}

#[test]
fn confirmed_verdict_aliases_onto_existing_entity() {
    let embedder = Arc::new(ScriptedEmbedder::new());
    let (_dir, mut db) = fresh_db();
    let hnsw = fresh_hnsw();
    let closer_id = ambiguous_band_scenario(&mut db, &hnsw, &embedder);

    let backend = FakeDisambiguator::confirming();
    let disambiguator = Arc::new(EntityDisambiguator::new(backend.clone(), "fake-model"));
    let embed_deps = embed_deps_for(embedder, hnsw);

    let wtxn = db.write_txn().unwrap();
    let res = resolve_or_create_with_deps(
        &wtxn,
        "Acme Holdings",
        "brain:Person",
        0.9,
        NOW + 1,
        Some(&embed_deps),
        Some(disambiguator.as_ref()),
    )
    .unwrap();
    wtxn.commit().unwrap();

    assert_eq!(res.entity_id, closer_id, "should alias onto closer seed");
    assert_eq!(res.tier, ResolutionTier::Disambiguated);
    assert_eq!(backend.call_count(), 1, "disambiguator must fire once");

    // Alias landed on the existing entity.
    let rtxn = db.read_txn().unwrap();
    let got = entity_get(&rtxn, closer_id).unwrap().unwrap();
    assert!(
        got.aliases.iter().any(|a| a == "Acme Holdings"),
        "alias not written; got {:?}",
        got.aliases,
    );

    // No merge proposal because the disambiguator confirmed.
    let pending = brain_metadata::entity::review::list_proposals_by_status(
        &rtxn,
        brain_metadata::tables::merge_review_queue::proposal_status::PENDING,
        16,
    )
    .unwrap();
    assert!(
        pending.is_empty(),
        "Confirmed verdict must not enqueue a merge proposal; got {} proposal(s)",
        pending.len(),
    );
}

#[test]
fn rejected_verdict_creates_fresh_entity_without_merge_proposal() {
    let embedder = Arc::new(ScriptedEmbedder::new());
    let (_dir, mut db) = fresh_db();
    let hnsw = fresh_hnsw();
    let closer_id = ambiguous_band_scenario(&mut db, &hnsw, &embedder);

    let backend = FakeDisambiguator::rejecting();
    let disambiguator = Arc::new(EntityDisambiguator::new(backend.clone(), "fake-model"));
    let embed_deps = embed_deps_for(embedder, hnsw);

    let wtxn = db.write_txn().unwrap();
    let res = resolve_or_create_with_deps(
        &wtxn,
        "Acme Holdings",
        "brain:Person",
        0.9,
        NOW + 1,
        Some(&embed_deps),
        Some(disambiguator.as_ref()),
    )
    .unwrap();
    wtxn.commit().unwrap();

    assert_eq!(
        res.tier,
        ResolutionTier::Created,
        "rejected -> fresh entity"
    );
    assert_ne!(res.entity_id, closer_id, "must not reuse the rejected seed");
    assert_eq!(backend.call_count(), 1, "disambiguator must fire once");

    // No merge proposal — the disambiguator already said the two are
    // distinct, so there's nothing to review later.
    let rtxn = db.read_txn().unwrap();
    let pending = brain_metadata::entity::review::list_proposals_by_status(
        &rtxn,
        brain_metadata::tables::merge_review_queue::proposal_status::PENDING,
        16,
    )
    .unwrap();
    assert!(
        pending.is_empty(),
        "Rejected verdict must not enqueue a merge proposal; got {} proposal(s)",
        pending.len(),
    );
}

#[test]
fn uncertain_verdict_falls_through_to_create_plus_merge_proposal() {
    let embedder = Arc::new(ScriptedEmbedder::new());
    let (_dir, mut db) = fresh_db();
    let hnsw = fresh_hnsw();
    let closer_id = ambiguous_band_scenario(&mut db, &hnsw, &embedder);

    let backend = FakeDisambiguator::uncertain();
    let disambiguator = Arc::new(EntityDisambiguator::new(backend.clone(), "fake-model"));
    let embed_deps = embed_deps_for(embedder, hnsw);

    let wtxn = db.write_txn().unwrap();
    let res = resolve_or_create_with_deps(
        &wtxn,
        "Acme Holdings",
        "brain:Person",
        0.9,
        NOW + 1,
        Some(&embed_deps),
        Some(disambiguator.as_ref()),
    )
    .unwrap();
    wtxn.commit().unwrap();

    assert_eq!(res.tier, ResolutionTier::Created);
    assert_ne!(res.entity_id, closer_id);
    assert_eq!(backend.call_count(), 1);

    // Existing fallback: a Pending proposal pointing at the closer
    // seed so the ambiguity worker can re-check after the HNSW grows.
    let rtxn = db.read_txn().unwrap();
    let pending = brain_metadata::entity::review::list_proposals_by_status(
        &rtxn,
        brain_metadata::tables::merge_review_queue::proposal_status::PENDING,
        16,
    )
    .unwrap();
    assert_eq!(
        pending.len(),
        1,
        "Uncertain verdict must enqueue exactly one Pending merge proposal",
    );
    let proposal = &pending[0];
    assert_eq!(proposal.source_entity, res.entity_id.to_bytes());
    assert_eq!(proposal.candidate_entity, closer_id.to_bytes());
}
