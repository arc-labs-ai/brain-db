#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send

//! AmbiguityResolverWorker integration test — drives the worker
//! end-to-end against a real MetadataDb + EntityHnswIndex. Verifies
//! the three terminal paths (promote / reject / expire) flip the
//! `merge_review_queue` row to the expected status and that the
//! promotion path lands the underlying `merge_entity` audit.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::EntityId;
use brain_core::{Entity, EntityType, MergeId};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::entity_hnsw::{EntityHnswIndex, EntityHnswParams};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::entity::ops::{entity_get, entity_put, normalize_name};
use brain_metadata::entity::review::{enqueue_merge_proposal, proposal_get};
use brain_metadata::tables::merge_review_queue::{proposal_status, proposal_tier};
use brain_metadata::MetadataDb;
use brain_ops::{AmbiguityResolverMetrics, RealWriterHandle};
use brain_planner::{ExecutorContext, WriterHandle};
use brain_workers::ambiguity_resolver::{AmbiguityResolverConfig, AmbiguityResolverWorker};
use brain_workers::WorkerContext;
use parking_lot::RwLock;

fn real_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

struct ScriptedEmbedder {
    table: StdMutex<HashMap<String, [f32; VECTOR_DIM]>>,
}

impl ScriptedEmbedder {
    fn new() -> Self {
        Self {
            table: StdMutex::new(HashMap::new()),
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
            % (VECTOR_DIM - 32))
            + 32;
        let mut v = [0.0_f32; VECTOR_DIM];
        v[axis] = 1.0;
        Ok(v)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    fn fingerprint(&self) -> [u8; 16] {
        [0xCC; 16]
    }
}

fn unit_vec(peak: usize, co: usize, peak_w: f32, co_w: f32) -> [f32; VECTOR_DIM] {
    let mut v = [0.0_f32; VECTOR_DIM];
    v[peak] = peak_w;
    v[co] = co_w;
    let n = (peak_w * peak_w + co_w * co_w).sqrt();
    if n > 0.0 {
        v[peak] /= n;
        v[co] /= n;
    }
    v
}

struct Fixture {
    metadata: Arc<MetadataDb>,
    hnsw: Arc<RwLock<EntityHnswIndex>>,
    embedder: Arc<ScriptedEmbedder>,
    worker_ctx: WorkerContext,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let metadata = Arc::new(MetadataDb::open(dir.path().join("metadata.redb")).unwrap());
    let hnsw = Arc::new(RwLock::new(
        EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap(),
    ));
    let embedder = Arc::new(ScriptedEmbedder::new());

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer: Arc<dyn WriterHandle> =
        Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        embedder.clone() as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer,
    );
    let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
    let worker_ctx = WorkerContext {
        ops,
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    Fixture {
        metadata,
        hnsw,
        embedder,
        worker_ctx,
        _dir: dir,
    }
}

fn seed_entity(fx: &Fixture, canonical: &str, vec_: [f32; VECTOR_DIM]) -> EntityId {
    let id = EntityId::new();
    let ent = Entity::new_active(
        id,
        EntityType::PERSON_ID,
        canonical.into(),
        normalize_name(canonical),
        real_now(),
    );
    {
        let wtxn = fx.metadata.write_txn().unwrap();
        entity_put(&wtxn, &ent).unwrap();
        wtxn.commit().unwrap();
    }
    fx.hnsw.write().insert(id, &vec_).unwrap();
    id
}

fn enqueue(
    fx: &Fixture,
    source: EntityId,
    candidate: EntityId,
    confidence: f32,
    proposed_at: u64,
) -> MergeId {
    let pid = MergeId::new();
    let wtxn = fx.metadata.write_txn().unwrap();
    enqueue_merge_proposal(
        &wtxn,
        pid,
        source,
        candidate,
        confidence,
        proposal_tier::EMBEDDING,
        proposed_at,
    )
    .unwrap();
    wtxn.commit().unwrap();
    pid
}

#[test]
fn end_to_end_promote_path() {
    let fx = fixture();
    let acme = seed_entity(&fx, "Acme", unit_vec(10, 11, 1.0, 0.0));
    let holdings = seed_entity(&fx, "Acme Holdings", unit_vec(10, 11, 0.75, 0.661));

    // Stage the embedder to return a query vector that scores ~0.98
    // against acme_v when the worker re-checks.
    fx.embedder
        .set("Acme Holdings", unit_vec(10, 11, 0.98, 0.199));

    let pid = enqueue(&fx, holdings, acme, 0.75, real_now());

    let metrics = Arc::new(AmbiguityResolverMetrics::new());
    let worker = AmbiguityResolverWorker::new(
        fx.metadata.clone(),
        fx.hnsw.clone(),
        fx.embedder.clone() as Arc<dyn Dispatcher>,
    )
    .with_metrics(metrics.clone());

    let processed = futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
    assert_eq!(processed, 1);

    // Assert: proposal AutoApplied + merge committed.
    let rtxn = fx.metadata.read_txn().unwrap();
    let p = proposal_get(&rtxn, pid).unwrap().unwrap();
    assert_eq!(p.status, proposal_status::AUTO_APPLIED);

    let holdings_row = entity_get(&rtxn, holdings).unwrap().unwrap();
    assert!(
        holdings_row.is_merged(),
        "holdings must be merged after promote",
    );
    assert_eq!(holdings_row.merged_into, Some(acme));

    let m = metrics.snapshot();
    assert_eq!(m.proposals_promoted_to_merge_total, 1);
    assert_eq!(m.proposals_rejected_total, 0);
    assert_eq!(m.proposals_expired_total, 0);
    assert_eq!(m.sweeps_total, 1);
}

#[test]
fn end_to_end_reject_path() {
    let fx = fixture();
    let acme = seed_entity(&fx, "Acme", unit_vec(20, 21, 1.0, 0.0));
    let bitcoin = seed_entity(&fx, "Bitcoin", unit_vec(80, 81, 1.0, 0.0));

    // Query vector is orthogonal to acme → cosine ≈ 0.
    fx.embedder.set("Bitcoin", unit_vec(80, 81, 1.0, 0.0));
    let pid = enqueue(&fx, bitcoin, acme, 0.75, real_now());

    let worker = AmbiguityResolverWorker::new(
        fx.metadata.clone(),
        fx.hnsw.clone(),
        fx.embedder.clone() as Arc<dyn Dispatcher>,
    );
    futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
    let rtxn = fx.metadata.read_txn().unwrap();
    let p = proposal_get(&rtxn, pid).unwrap().unwrap();
    assert_eq!(p.status, proposal_status::REJECTED);

    // Bitcoin is untouched.
    let bitcoin_row = entity_get(&rtxn, bitcoin).unwrap().unwrap();
    assert!(!bitcoin_row.is_merged());
}

#[test]
fn end_to_end_expire_path() {
    let fx = fixture();
    let acme = seed_entity(&fx, "Acme", unit_vec(30, 31, 1.0, 0.0));
    let stale = seed_entity(&fx, "Stale", unit_vec(30, 31, 0.75, 0.661));
    let thirty_one_days_ago = real_now().saturating_sub(31 * 24 * 60 * 60 * 1_000_000_000);
    let pid = enqueue(&fx, stale, acme, 0.75, thirty_one_days_ago);

    let metrics = Arc::new(AmbiguityResolverMetrics::new());
    let worker = AmbiguityResolverWorker::new(
        fx.metadata.clone(),
        fx.hnsw.clone(),
        fx.embedder.clone() as Arc<dyn Dispatcher>,
    )
    .with_knobs(AmbiguityResolverConfig::default())
    .with_metrics(metrics.clone());

    futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
    let rtxn = fx.metadata.read_txn().unwrap();
    let p = proposal_get(&rtxn, pid).unwrap().unwrap();
    assert_eq!(p.status, proposal_status::EXPIRED);
    assert_eq!(metrics.snapshot().proposals_expired_total, 1);
}

