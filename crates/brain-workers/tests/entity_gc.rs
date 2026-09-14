#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! End-to-end tests for the entity-GC background worker.
//!
//! The inbound-reference counting is unit-tested in brain-metadata
//! (`entity::ops::tests`). These tests drive the *worker* against a real
//! `MetadataDb` to prove the eligibility plumbing (grace + inbound
//! count), the tombstone-only effect, the audit row, and the
//! off-by-default safety.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{
    Entity, EntityId, EntityType, EvidenceRef, ExtractorId, PredicateId, Statement, StatementId,
    StatementKind, StatementObject, StatementValue, SubjectRef,
};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::entity::ops::{entity_get, entity_put, normalize_name};
use brain_metadata::schema::predicate::predicate_intern;
use brain_metadata::statement::statement_create;
use brain_metadata::tables::audit::{resolution_outcome, ENTITY_RESOLUTION_AUDIT_TABLE};
use brain_metadata::tables::entity::flags as entity_flags;
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::workers::entity_gc::EntityGcWorker;
use brain_workers::{Worker, WorkerContext};
use redb::ReadableTable;

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
    worker: &EntityGcWorker,
    ops: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let wctx = WorkerContext {
        ops,
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    worker.run_cycle(&wctx).await
}

fn now_unix_nanos() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap()
}

fn __ts() -> brain_metadata::RowScope {
    brain_metadata::RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xA1; 16])
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
    entity_put(&wtxn, __ts(), brain_core::SessionId::DEFAULT, &e).unwrap();
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

/// Seed a statement whose subject is `subject` (so `subject` gains an
/// inbound reference). Object is a plain Value to avoid minting a second
/// entity.
fn seed_subject_statement(
    metadata: &SharedMetadataDb,
    predicate: PredicateId,
    subject: EntityId,
    created: u64,
) -> StatementId {
    let id = StatementId::new();
    let s = Statement::new_root(
        id,
        StatementKind::Fact,
        SubjectRef::Entity(subject),
        predicate,
        StatementObject::Value(StatementValue::Text("v".into())),
        0.9,
        EvidenceRef::default(),
        ExtractorId::from(0),
        created,
        1,
    );
    let wtxn = metadata.write_txn().unwrap();
    statement_create(&wtxn, __ts(), brain_core::SessionId::DEFAULT, &s, created).unwrap();
    wtxn.commit().unwrap();
    id
}

fn is_tombstoned(metadata: &SharedMetadataDb, id: EntityId) -> bool {
    let rtxn = metadata.read_txn().unwrap();
    let e = entity_get(&rtxn, id).unwrap().expect("entity row present");
    e.flags & entity_flags::TOMBSTONED != 0
}

fn has_gc_audit(metadata: &SharedMetadataDb, id: EntityId) -> bool {
    let rtxn = metadata.read_txn().unwrap();
    let t = rtxn.open_table(ENTITY_RESOLUTION_AUDIT_TABLE).unwrap();
    for entry in t.iter().unwrap() {
        let (_, v) = entry.unwrap();
        let row = v.value();
        if row.outcome == resolution_outcome::TOMBSTONED_ENTITY_GC
            && row.resolved_entity() == Some(id)
        {
            return true;
        }
    }
    false
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("entity-gc-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn tombstones_orphan_keeps_referenced_and_within_grace() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();

        // Orphan created 40 days ago -> past 30-day grace -> swept.
        let orphan = make_entity(&fix.metadata, "Orphan Olga", now - 40 * DAY_NS);
        // Orphan created 1 day ago -> inside grace -> kept.
        let recent = make_entity(&fix.metadata, "Recent Rhea", now - DAY_NS);
        // Old but referenced (subject of a statement) -> kept.
        let referenced = make_entity(&fix.metadata, "Referenced Raj", now - 40 * DAY_NS);
        let predicate = intern_predicate(&fix.metadata, "likes", now - 40 * DAY_NS);
        let _ = seed_subject_statement(&fix.metadata, predicate, referenced, now - 40 * DAY_NS);

        let worker = EntityGcWorker::new()
            .enabled()
            .with_grace_seconds(GRACE_SECS);
        let swept = run_one(&worker, fix.ops.clone()).await.unwrap();

        assert_eq!(swept, 1, "only the past-grace orphan is swept");
        assert!(is_tombstoned(&fix.metadata, orphan), "orphan tombstoned");
        assert!(has_gc_audit(&fix.metadata, orphan), "audit row written");
        assert!(
            !is_tombstoned(&fix.metadata, recent),
            "within-grace orphan kept"
        );
        assert!(
            !is_tombstoned(&fix.metadata, referenced),
            "referenced entity kept"
        );
    });
}

#[test]
fn disabled_worker_sweeps_nothing() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        let orphan = make_entity(&fix.metadata, "Ghost Gary", now - 40 * DAY_NS);

        // Off by default — no `.enabled()`.
        let worker = EntityGcWorker::new().with_grace_seconds(GRACE_SECS);
        let swept = run_one(&worker, fix.ops.clone()).await.unwrap();

        assert_eq!(swept, 0);
        assert!(
            !is_tombstoned(&fix.metadata, orphan),
            "disabled worker leaves entities untouched"
        );
        assert!(!has_gc_audit(&fix.metadata, orphan));
    });
}

#[test]
fn second_run_is_idempotent() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        let orphan = make_entity(&fix.metadata, "Solo Sol", now - 40 * DAY_NS);

        let worker = EntityGcWorker::new()
            .enabled()
            .with_grace_seconds(GRACE_SECS);
        assert_eq!(run_one(&worker, fix.ops.clone()).await.unwrap(), 1);
        assert!(is_tombstoned(&fix.metadata, orphan));
        // Already tombstoned -> excluded from the live scan -> no-op.
        assert_eq!(run_one(&worker, fix.ops.clone()).await.unwrap(), 0);
    });
}
