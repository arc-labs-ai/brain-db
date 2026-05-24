#![allow(clippy::arc_with_non_send_sync)]
//! End-to-end test for the SCHEMA_UPLOAD → flag-sweep pipeline.
//!
//! Verifies that `Phase::UpsertSchema` submitted through the unified
//! write path enqueues a `SchemaFlagSweepJob`, and that the
//! SchemaMigrationWorker's drive_one_batch drains it and re-aligns the
//! `OUTSIDE_ACTIVE_SCHEMA` flag bit across the namespace's statements.
//!
//! Contract: the upload's wtxn no longer carries the sweep — the worker
//! is the only place flag-bit maintenance happens. A pre-existing
//! statement against an open-vocabulary predicate stays clean
//! immediately after upload commit, and gains the bit only after the
//! worker drains the post-commit enqueue.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use brain_core::{
    Entity, EntityType, EvidenceEntry, EvidenceRef, Statement, StatementId, StatementKind,
    StatementObject, StatementValue, SubjectRef,
};
use brain_core::{AgentId, ContextId, EntityId, ExtractorId, MemoryId};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::entity::ops::{entity_put, normalize_name};
use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::statement::statement_create;
use brain_metadata::tables::statement::{statement_flags, STATEMENTS_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{
    OpsContext, Phase, RealWriterHandle, SchemaFlagSweepJob, SchemaMigrationMetrics, Write, WriteId,
};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::workers::schema_migration::SchemaMigrationWorker;
use brain_workers::WorkerContext;
use parking_lot::Mutex;
use tempfile::TempDir;

const NOW: u64 = 1_700_000_000_000_000_000;

struct MockDispatcher;
impl Dispatcher for MockDispatcher {
    fn embed(&self, _text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0f32; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0xAB; 16]
    }
}

struct Fixture {
    writer: Arc<RealWriterHandle>,
    metadata: SharedMetadataDb,
    worker: SchemaMigrationWorker,
    ctx: WorkerContext,
    _tempdir: TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let mut writer_raw = RealWriterHandle::new(metadata.clone(), hnsw_writer);

    let (tx, rx) = flume::unbounded::<SchemaFlagSweepJob>();
    writer_raw.set_schema_flag_sweep_sender(tx);
    let metrics = Arc::new(SchemaMigrationMetrics::new());
    writer_raw.set_schema_flag_sweep_metrics(metrics.clone());
    let writer = Arc::new(writer_raw);

    let worker = SchemaMigrationWorker::new(rx).with_metrics(metrics);

    let executor = ExecutorContext::new(
        Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer.clone() as Arc<dyn WriterHandle>,
    );
    let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
    let ctx = WorkerContext {
        ops,
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    Fixture {
        writer,
        metadata,
        worker,
        ctx,
        _tempdir: tempdir,
    }
}

fn put_subject(metadata: &SharedMetadataDb) -> EntityId {
    let id = EntityId::new();
    let mut db = metadata.lock();
    let wtxn = db.write_txn().unwrap();
    entity_put(
        &wtxn,
        &Entity::new_active(
            id,
            EntityType::PERSON_ID,
            "anchor".into(),
            normalize_name("anchor"),
            NOW,
        ),
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

fn write_statement(
    metadata: &SharedMetadataDb,
    subject: EntityId,
    namespace: &str,
    predicate_name: &str,
) -> StatementId {
    let mut db = metadata.lock();
    let wtxn = db.write_txn().unwrap();
    let pid = predicate_intern_or_get(&wtxn, namespace, predicate_name, 0, NOW).unwrap();
    let evidence_entry = EvidenceEntry::from_parts(
        MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
        1.0,
        0,
        ExtractorId::default(),
    );
    let stmt = Statement::new_root(
        StatementId::new(),
        StatementKind::Fact,
        SubjectRef::Entity(subject),
        pid,
        StatementObject::Value(StatementValue::Text("v".into())),
        0.9,
        EvidenceRef::inline_from_slice(&[evidence_entry]),
        ExtractorId::default(),
        NOW,
        1,
    );
    let sid = stmt.id;
    statement_create(&wtxn, &stmt, NOW).unwrap();
    wtxn.commit().unwrap();
    sid
}

fn statement_has_outside_flag(metadata: &SharedMetadataDb, sid: StatementId) -> bool {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
    let row = t.get(&sid.to_bytes()).unwrap().unwrap().value();
    row.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA)
}

fn submit_upload(writer: &RealWriterHandle, source: &str) {
    let phase = Phase::UpsertSchema {
        namespace: "acme".into(),
        version: 1,
        blob: source.as_bytes().to_vec(),
        declared_predicates: Vec::new(),
        declared_relation_types: Vec::new(),
        declared_entity_types: Vec::new(),
        created_at_unix_nanos: NOW,
    };
    let write = Write::single(WriteId::new(), AgentId::default(), phase);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(writer.submit(write)).unwrap();
}

fn drive_worker_once(worker: &SchemaMigrationWorker, ctx: &WorkerContext) -> usize {
    use brain_workers::Worker as WorkerTrait;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(WorkerTrait::run_cycle(worker, ctx)).unwrap()
}

#[test]
fn schema_upload_enqueues_post_commit_and_worker_drains_to_flag_outside_rows() {
    let fx = build_fixture();
    let subject = put_subject(&fx.metadata);
    // Pre-upload schemaless writes against `acme:ghost` (open-vocab).
    let sid_ghost = write_statement(&fx.metadata, subject, "acme", "ghost");

    // Schema upload via the unified write path. Declares only
    // `prefers` — `ghost` falls outside.
    let source = r#"
namespace acme

define entity_type Person {}

define predicate prefers {
    kind: Fact
    object: Value<text>
}
"#;
    submit_upload(&fx.writer, source);

    // Immediately after upload commit, the storage layer must NOT have
    // flagged anything — the sweep is post-commit work the worker
    // owns.
    assert!(
        !statement_has_outside_flag(&fx.metadata, sid_ghost),
        "upload wtxn must not run an inline flag-sweep",
    );

    // The writer's post-commit fan-out should have enqueued exactly
    // one job.
    assert_eq!(
        fx.worker.queue_depth(),
        1,
        "submit must enqueue exactly one flag-sweep job"
    );

    let processed = drive_worker_once(&fx.worker, &fx.ctx);
    assert_eq!(processed, 1);
    assert_eq!(fx.worker.queue_depth(), 0);

    // After the worker drains, the ghost row carries the flag.
    assert!(
        statement_has_outside_flag(&fx.metadata, sid_ghost),
        "post-sweep: ghost-predicate row must carry OUTSIDE_ACTIVE_SCHEMA",
    );

    let s = fx.worker.metrics().snapshot();
    assert_eq!(s.sweeps_completed_total, 1);
    assert_eq!(s.rows_flagged_total, 1);
    assert_eq!(s.rows_cleared_total, 0);
    assert_eq!(s.errors_total, 0);
}
