//! Tier-3 embedding resolver — integration coverage against a
//! deterministic mocked embedder. The unit tests inside
//! `resolver.rs::tests` cover the same surface; this file pins the
//! external API shape that downstream callers (worker, future client
//! consumers) bind to.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use brain_core::Entity;
use brain_core::{EntityId, EntityType, EntityTypeId};
use brain_embed::{Dispatcher, EmbedError};
use brain_extractors::resolver::{
    resolve_or_create_with_hnsw, EmbeddingDeps, ResolutionTier, EMBED_RESOLVE_THRESHOLD,
};
use brain_index::entity_hnsw::{EntityHnswIndex, EntityHnswParams};
use brain_index::VECTOR_DIM;
use brain_metadata::entity::ops::{entity_get, entity_put, normalize_name};
use brain_metadata::MetadataDb;
use parking_lot::RwLock;
use tempfile::TempDir;

const NOW: u64 = 1_700_000_000_000_000_000;

// ---------------------------------------------------------------------------
// Test fixtures.
// ---------------------------------------------------------------------------

/// Deterministic embedder backed by a name → vector table. Lookups
/// for unknown keys land on a distant unit axis so the cosine is ~0.
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
        // Fallback: hash-derived far-axis vector so unknown inputs
        // never spuriously match the seed corpus.
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
        [0x88; 16]
    }
}

/// Two-axis unit vector with cosine = peak_w when aligned with another
/// vector of the same peak axis.
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

fn deps_for(embedder: Arc<ScriptedEmbedder>, hnsw: Arc<RwLock<EntityHnswIndex>>) -> EmbeddingDeps {
    EmbeddingDeps {
        hnsw,
        embedder: embedder as Arc<dyn Dispatcher>,
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

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn tier_embedding_resolves_near_paraphrase_and_writes_alias() {
    // Two vectors aligned on the same dominant axis with cosine ~0.95
    // — well above the 0.78 threshold.
    let stripe_v = axis_pair(10, 11, 1.0, 0.0);
    let paraphrase_v = axis_pair(10, 11, 0.95, 0.31);

    let embedder = Arc::new(ScriptedEmbedder::new());
    embedder.set("Stripe Payments", paraphrase_v);

    let (_dir, mut db) = fresh_db();
    let hnsw = fresh_hnsw();
    let target_id = seed_entity(
        &mut db,
        &hnsw,
        EntityType::PERSON_ID,
        "Stripe Inc.",
        stripe_v,
    );

    let deps = deps_for(embedder, hnsw.clone());
    let wtxn = db.write_txn().unwrap();
    let res = resolve_or_create_with_hnsw(
        &wtxn,
        "Stripe Payments",
        "brain:Person",
        0.9,
        NOW + 1,
        Some(&deps),
    )
    .unwrap();
    wtxn.commit().unwrap();

    assert_eq!(res.entity_id, target_id, "tier-3 should reuse the seed");
    assert_eq!(res.tier, ResolutionTier::Embedding);

    let rtxn = db.read_txn().unwrap();
    let got = entity_get(&rtxn, target_id).unwrap().unwrap();
    assert!(
        got.aliases.iter().any(|a| a == "Stripe Payments"),
        "alias not written; got {:?}",
        got.aliases,
    );
}

#[test]
fn tier_embedding_below_threshold_creates_new_entity() {
    let stripe_v = axis_pair(10, 11, 1.0, 0.0);
    let bitcoin_v = axis_pair(200, 201, 1.0, 0.0);

    let embedder = Arc::new(ScriptedEmbedder::new());
    embedder.set("Bitcoin", bitcoin_v);

    let (_dir, mut db) = fresh_db();
    let hnsw = fresh_hnsw();
    let seed_id = seed_entity(
        &mut db,
        &hnsw,
        EntityType::PERSON_ID,
        "Stripe Inc.",
        stripe_v,
    );

    let deps = deps_for(embedder, hnsw.clone());
    let wtxn = db.write_txn().unwrap();
    let res =
        resolve_or_create_with_hnsw(&wtxn, "Bitcoin", "brain:Person", 0.9, NOW + 1, Some(&deps))
            .unwrap();
    wtxn.commit().unwrap();

    assert_eq!(res.tier, ResolutionTier::Created);
    assert_ne!(res.entity_id, seed_id, "below-threshold must not reuse");
    // Tier-4 also populates the entity HNSW.
    assert!(hnsw.read().contains(res.entity_id));
}

#[test]
fn tier_embedding_respects_entity_type() {
    // Person + Organization share an embedding peak — the type
    // filter must reject the Organization candidate.
    let shared_peak = axis_pair(42, 43, 1.0, 0.0);

    let embedder = Arc::new(ScriptedEmbedder::new());
    embedder.set("Alice", shared_peak);

    let (_dir, mut db) = fresh_db();
    let hnsw = fresh_hnsw();

    // Intern Organization type.
    let org_type_id = {
        let wtxn = db.write_txn().unwrap();
        let id = brain_metadata::entity::types::entity_type_intern(
            &wtxn,
            "Organization",
            Vec::new(),
            NOW,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    };

    let alice_person_id = seed_entity(
        &mut db,
        &hnsw,
        EntityType::PERSON_ID,
        "Alice Wong",
        shared_peak,
    );
    let _cafe_id = seed_entity(
        &mut db,
        &hnsw,
        org_type_id,
        "Alice's Cafe",
        axis_pair(42, 43, 0.998, 0.063),
    );

    let deps = deps_for(embedder, hnsw);
    let wtxn = db.write_txn().unwrap();
    let res =
        resolve_or_create_with_hnsw(&wtxn, "Alice", "brain:Person", 0.9, NOW + 1, Some(&deps))
            .unwrap();
    wtxn.commit().unwrap();

    assert_eq!(res.entity_id, alice_person_id);
    assert_eq!(res.tier, ResolutionTier::Embedding);
}

#[test]
fn tier_create_populates_hnsw_for_next_paraphrase() {
    // First resolve mints; second resolve of a paraphrase short-
    // circuits at tier-3b. Confirms the synchronous HNSW
    // population path closes the loop.
    let canonical_v = axis_pair(77, 78, 1.0, 0.0);
    let paraphrase_v = axis_pair(77, 78, 0.95, 0.31);

    let embedder = Arc::new(ScriptedEmbedder::new());
    embedder.set("Brand New Co", canonical_v);
    embedder.set("Brand New Company", paraphrase_v);

    let (_dir, db) = fresh_db();
    let hnsw = fresh_hnsw();

    let deps = deps_for(embedder, hnsw.clone());

    let wtxn = db.write_txn().unwrap();
    let r1 =
        resolve_or_create_with_hnsw(&wtxn, "Brand New Co", "brain:Person", 0.9, NOW, Some(&deps))
            .unwrap();
    wtxn.commit().unwrap();
    assert_eq!(r1.tier, ResolutionTier::Created);
    assert!(hnsw.read().contains(r1.entity_id));

    let wtxn = db.write_txn().unwrap();
    let r2 = resolve_or_create_with_hnsw(
        &wtxn,
        "Brand New Company",
        "brain:Person",
        0.9,
        NOW + 1,
        Some(&deps),
    )
    .unwrap();
    wtxn.commit().unwrap();
    assert_eq!(r2.entity_id, r1.entity_id);
    assert_eq!(r2.tier, ResolutionTier::Embedding);
}

#[test]
fn tier_embedding_threshold_constant_matches_spec() {
    // Regression guard: tightening the constant changes Recall@10
    // behaviour materially. Pin the default at 0.78.
    assert!((EMBED_RESOLVE_THRESHOLD - 0.78).abs() < 1e-6);
}
