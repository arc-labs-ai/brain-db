#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! End-to-end tests for the statement-reclaim background worker.
//!
//! The reclamation sweep itself is unit-tested in brain-metadata
//! (`extractor::sweep::reclaim_tests`). These tests drive the *worker*
//! against a real `MetadataDb` — open wtxn -> sweep -> commit — to prove
//! the wiring, the grace plumbing (`with_grace_seconds`), the
//! off-by-default safety, and re-run idempotency.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{
    Entity, EntityId, EntityType, EvidenceEntry, EvidenceRef, ExtractorId, MemoryId, PredicateId,
    Statement, StatementId, StatementKind, StatementObject, StatementValue, SubjectRef,
    TombstoneReason,
};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::entity::ops::{entity_put, normalize_name};
use brain_metadata::schema::predicate::predicate_intern;
use brain_metadata::statement::{
    statement_create, statement_get, statement_retract, statement_tombstone,
};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::workers::statement_reclaim::StatementReclaimWorker;
use brain_workers::{Worker, WorkerContext};
use smallvec::SmallVec;

const DAY_NS: u64 = 24 * 60 * 60 * 1_000_000_000;
const GRACE_SECS: u64 = 30 * 24 * 60 * 60;

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct MockDispatcher;
impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0f32; VECTOR_DIM];
        for (i, b) in text.as_bytes().iter().enumerate() {
            v[i % VECTOR_DIM] += f32::from(*b) / 255.0;
        }
        Ok(v)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0xCD; 16]
    }
}

struct Fixture {
    metadata: SharedMetadataDb,
    ops: Arc<OpsContext>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );
    let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
    Fixture {
        metadata,
        ops,
        _tempdir: tempdir,
    }
}

async fn run_one(
    worker: &StatementReclaimWorker,
    ops: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let wctx = WorkerContext {
        ops,
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    worker.run_cycle(&wctx).await
}

// Real wall-clock: the worker reads `SystemTime::now()` internally, so
// eligibility is driven by seeding tombstone timestamps relative to now.
fn now_unix_nanos() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap()
}

fn make_entity(metadata: &SharedMetadataDb, name: &str, created: u64) -> EntityId {
    let id = EntityId::new();
    let e = Entity::new_active(
        id,
        EntityType::PERSON_ID,
        name.into(),
        normalize_name(name),
        created,
    );
    let wtxn = metadata.write_txn().unwrap();
    entity_put(&wtxn, &e).unwrap();
    wtxn.commit().unwrap();
    id
}

fn intern_predicate(metadata: &SharedMetadataDb, name: &str, created: u64) -> PredicateId {
    let wtxn = metadata.write_txn().unwrap();
    let id = predicate_intern(
        &wtxn,
        "test",
        name,
        Some(StatementKind::Fact),
        /* object: Value */ 2,
        /* schema_version */ 1,
        "",
        false,
        created,
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

fn seed_statement(
    metadata: &SharedMetadataDb,
    predicate: PredicateId,
    subject: EntityId,
    created: u64,
) -> StatementId {
    let id = StatementId::new();
    let entries = vec![EvidenceEntry::from_parts(
        MemoryId::pack(0, 1, 1),
        0.9,
        created,
        ExtractorId::from(0),
    )];
    let conf = entries.iter().map(EvidenceEntry::confidence).sum::<f32>() / entries.len() as f32;
    let mut s = Statement::new_root(
        id,
        StatementKind::Fact,
        SubjectRef::Entity(subject),
        predicate,
        StatementObject::Value(StatementValue::Text("v".into())),
        conf,
        EvidenceRef::Inline(Box::new(SmallVec::from_vec(entries))),
        ExtractorId::from(0),
        created,
        1,
    );
    s.confidence = conf;
    let wtxn = metadata.write_txn().unwrap();
    statement_create(&wtxn, &s, created).unwrap();
    wtxn.commit().unwrap();
    id
}

fn retract(metadata: &SharedMetadataDb, id: StatementId, at: u64) {
    let wtxn = metadata.write_txn().unwrap();
    // The reason byte is ignored by retract (it stamps `Retract`), but
    // pass a real one for call-site honesty.
    statement_retract(&wtxn, id, TombstoneReason::UserRequest, at).unwrap();
    wtxn.commit().unwrap();
}

fn plain_tombstone(metadata: &SharedMetadataDb, id: StatementId, at: u64) {
    let wtxn = metadata.write_txn().unwrap();
    statement_tombstone(&wtxn, id, TombstoneReason::UserRequest, at).unwrap();
    wtxn.commit().unwrap();
}

fn present(metadata: &SharedMetadataDb, id: StatementId) -> bool {
    let rtxn = metadata.read_txn().unwrap();
    // statement_get returns the row regardless of tombstone state; it is
    // None only after physical reclamation removed it from the table.
    statement_get(&rtxn, id).unwrap().is_some()
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("statement-reclaim-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn reclaims_retracted_past_grace_keeps_recent_and_plain_tombstone() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        let subject = make_entity(&fix.metadata, "Ada", now - 40 * DAY_NS);
        let predicate = intern_predicate(&fix.metadata, "likes", now - 40 * DAY_NS);

        // A: retracted 31 days ago -> past the 30-day grace -> reclaimed.
        let a = seed_statement(&fix.metadata, predicate, subject, now - 40 * DAY_NS);
        retract(&fix.metadata, a, now - 31 * DAY_NS);
        // B: retracted 1 day ago -> inside grace -> kept.
        let b = seed_statement(&fix.metadata, predicate, subject, now - 40 * DAY_NS);
        retract(&fix.metadata, b, now - DAY_NS);
        // C: plainly tombstoned 31 days ago -> not a retract -> kept.
        let c = seed_statement(&fix.metadata, predicate, subject, now - 40 * DAY_NS);
        plain_tombstone(&fix.metadata, c, now - 31 * DAY_NS);

        assert!(present(&fix.metadata, a));
        assert!(present(&fix.metadata, b));
        assert!(present(&fix.metadata, c));

        let worker = StatementReclaimWorker::new()
            .enabled()
            .with_grace_seconds(GRACE_SECS);
        let deleted = run_one(&worker, fix.ops.clone()).await.unwrap();

        assert_eq!(deleted, 1, "only the past-grace retract is reclaimed");
        assert!(!present(&fix.metadata, a), "A reclaimed");
        assert!(present(&fix.metadata, b), "B inside grace, kept");
        assert!(present(&fix.metadata, c), "C plain tombstone, kept");
    });
}

#[test]
fn disabled_worker_reclaims_nothing() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        let subject = make_entity(&fix.metadata, "Bo", now - 40 * DAY_NS);
        let predicate = intern_predicate(&fix.metadata, "knows", now - 40 * DAY_NS);
        let a = seed_statement(&fix.metadata, predicate, subject, now - 40 * DAY_NS);
        retract(&fix.metadata, a, now - 31 * DAY_NS);

        // Off by default — no `.enabled()`.
        let worker = StatementReclaimWorker::new().with_grace_seconds(GRACE_SECS);
        let deleted = run_one(&worker, fix.ops.clone()).await.unwrap();

        assert_eq!(deleted, 0);
        assert!(
            present(&fix.metadata, a),
            "disabled worker leaves rows untouched"
        );
    });
}

#[test]
fn second_run_is_idempotent() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        let subject = make_entity(&fix.metadata, "Cy", now - 40 * DAY_NS);
        let predicate = intern_predicate(&fix.metadata, "uses", now - 40 * DAY_NS);
        let a = seed_statement(&fix.metadata, predicate, subject, now - 40 * DAY_NS);
        retract(&fix.metadata, a, now - 31 * DAY_NS);

        let worker = StatementReclaimWorker::new()
            .enabled()
            .with_grace_seconds(GRACE_SECS);

        assert_eq!(run_one(&worker, fix.ops.clone()).await.unwrap(), 1);
        assert!(!present(&fix.metadata, a));
        // Re-running over an already-clean table reclaims nothing and does
        // not error — the row is gone and stays gone.
        assert_eq!(run_one(&worker, fix.ops.clone()).await.unwrap(), 0);
    });
}
