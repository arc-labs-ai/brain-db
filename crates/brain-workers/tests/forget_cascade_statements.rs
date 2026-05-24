#![allow(clippy::arc_with_non_send_sync)]
//! End-to-end test for the FORGET cascade trigger.
//!
//! Verifies that `Phase::Tombstone(Memory)` submitted through the
//! unified write path enqueues a `ForgetCascadeJob`, and that the
//! worker's drive_one_batch consumes it and rewrites every statement
//! whose `evidence_inline` cited the forgotten memory.

use std::sync::Arc;

use brain_core::{
    Entity, EntityType, EvidenceEntry, EvidenceRef, PredicateId, Statement, StatementId,
    StatementKind, StatementObject, StatementValue, SubjectRef,
};
use brain_core::{
    AgentId, ContextId, EntityId, ExtractorId, MemoryId, MemoryKind, NodeRef, Salience,
};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::entity::ops::{entity_put, normalize_name};
use brain_metadata::schema::predicate::predicate_intern;
use brain_metadata::statement::{statement_create, statements_citing_memory};
use brain_metadata::tables::statement::STATEMENTS_TABLE;
use brain_metadata::MetadataDb;
use brain_ops::write::phase::TombstoneMode;
use brain_ops::{
    ForgetCascadeJob, ForgetCascadeMetrics, OpsContext, Phase, RealWriterHandle, TombstoneTarget,
    Write, WriteId,
};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::workers::forget_cascade::ForgetCascadeWorker;
use brain_workers::WorkerContext;
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::sync::atomic::AtomicBool;
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
        [0xCD; 16]
    }
}

struct Fixture {
    writer: Arc<RealWriterHandle>,
    metadata: SharedMetadataDb,
    worker: ForgetCascadeWorker,
    ctx: WorkerContext,
    _tempdir: TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let mut writer_raw = RealWriterHandle::new(metadata.clone(), hnsw_writer);

    let (tx, rx) = flume::unbounded::<ForgetCascadeJob>();
    writer_raw.set_forget_cascade_sender(tx);
    let metrics = Arc::new(ForgetCascadeMetrics::new());
    writer_raw.set_forget_cascade_metrics(metrics.clone());
    let writer = Arc::new(writer_raw);

    let worker = ForgetCascadeWorker::new(rx).with_metrics(metrics);

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

fn make_entity(metadata: &SharedMetadataDb, name: &str) -> EntityId {
    let id = EntityId::new();
    let e = Entity::new_active(
        id,
        EntityType::PERSON_ID,
        name.into(),
        normalize_name(name),
        NOW,
    );
    let mut db = metadata.lock();
    let wtxn = db.write_txn().unwrap();
    entity_put(&wtxn, &e).unwrap();
    wtxn.commit().unwrap();
    id
}

fn intern_predicate(metadata: &SharedMetadataDb, name: &str) -> PredicateId {
    let mut db = metadata.lock();
    let wtxn = db.write_txn().unwrap();
    let id = predicate_intern(
        &wtxn,
        "test",
        name,
        Some(StatementKind::Fact),
        /* object: Value */ 2,
        /* schema_version */ 1,
        "",
        false,
        NOW,
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

fn seed_statement(
    metadata: &SharedMetadataDb,
    predicate: PredicateId,
    subject: EntityId,
    evidence: Vec<(MemoryId, f32)>,
) -> StatementId {
    let entries: Vec<EvidenceEntry> = evidence
        .iter()
        .map(|(m, c)| EvidenceEntry::from_parts(*m, *c, NOW, ExtractorId::from(0)))
        .collect();
    let stmt_conf = if entries.is_empty() {
        0.0
    } else {
        entries.iter().map(|e| e.confidence()).sum::<f32>() / entries.len() as f32
    };
    let id = StatementId::new();
    let mut s = Statement::new_root(
        id,
        StatementKind::Fact,
        SubjectRef::Entity(subject),
        predicate,
        StatementObject::Value(StatementValue::Text("v".into())),
        stmt_conf,
        EvidenceRef::Inline(Box::new(SmallVec::from_vec(entries))),
        ExtractorId::from(0),
        NOW,
        1,
    );
    s.confidence = stmt_conf;
    let mut db = metadata.lock();
    let wtxn = db.write_txn().unwrap();
    statement_create(&wtxn, &s, NOW).unwrap();
    wtxn.commit().unwrap();
    id
}

fn upsert_memory(writer: &RealWriterHandle, id: MemoryId) {
    let phase = Phase::UpsertMemory {
        id,
        text: format!("memory-{}", id.slot()),
        vector: Box::new([0.0f32; VECTOR_DIM]),
        kind: MemoryKind::Episodic,
        salience: Salience::default(),
        context: ContextId::DEFAULT,
        created_at_unix_nanos: NOW,
        arena_slot: id.slot(),
        embedding_model_fp: [0u8; 16],
        content_hash: None,
        deduplicate: false,
    };
    let write = Write::single(WriteId::new(), AgentId::default(), phase);
    // We block on the future via a dummy tokio runtime — the writer
    // returns immediately for the in-process test path.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(writer.submit(write)).unwrap();
    let _ = NodeRef::Memory(id); // silence unused-import in path-free builds
}

fn tombstone_memory(writer: &RealWriterHandle, id: MemoryId, mode: TombstoneMode) {
    let phase = Phase::Tombstone {
        target: TombstoneTarget::Memory { id, mode },
        reason: 0,
        at_unix_nanos: NOW + 1,
    };
    let write = Write::single(WriteId::new(), AgentId::default(), phase);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(writer.submit(write)).unwrap();
}

fn statement_confidence(db: &MetadataDb, id: StatementId) -> Option<f32> {
    let rtxn = db.read_txn().ok()?;
    let t = rtxn.open_table(STATEMENTS_TABLE).ok()?;
    let row = t.get(&id.to_bytes()).ok().flatten()?.value();
    Some(row.confidence)
}

fn statement_is_tombstoned(db: &MetadataDb, id: StatementId) -> bool {
    let Ok(rtxn) = db.read_txn() else {
        return false;
    };
    let Ok(t) = rtxn.open_table(STATEMENTS_TABLE) else {
        return false;
    };
    matches!(
        t.get(&id.to_bytes()).ok().flatten().map(|g| g.value()),
        Some(row) if row.is_tombstoned()
    )
}

fn drive_worker_once(worker: &ForgetCascadeWorker, ctx: &WorkerContext) -> usize {
    use brain_workers::Worker as WorkerTrait;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(WorkerTrait::run_cycle(worker, ctx)).unwrap()
}

#[test]
fn hard_forget_cascade_tombstones_single_evidence_statement() {
    let fx = build_fixture();
    let m = MemoryId::pack(0, 1, 1);
    upsert_memory(&fx.writer, m);

    let pred = intern_predicate(&fx.metadata, "prefers_color");
    let subj = make_entity(&fx.metadata, "alice-1");
    let s = seed_statement(&fx.metadata, pred, subj, vec![(m, 0.8)]);

    // Pre-FORGET: the dependent surfaces.
    {
        let db = fx.metadata.lock();
        let rtxn = db.read_txn().unwrap();
        let deps = statements_citing_memory(&rtxn, m).unwrap();
        assert_eq!(deps, vec![s], "pre-FORGET dependent must surface");
    }

    // FORGET (hard) → writer should enqueue a cascade job. We can't
    // observe the queue directly from outside the worker, so drive
    // the worker after the submit and assert the side-effects.
    tombstone_memory(&fx.writer, m, TombstoneMode::Hard);
    assert_eq!(
        fx.worker.queue_depth(),
        1,
        "submit must enqueue exactly one cascade job"
    );

    let processed = drive_worker_once(&fx.worker, &fx.ctx);
    assert_eq!(processed, 1);
    assert_eq!(fx.worker.queue_depth(), 0);

    let db = fx.metadata.lock();
    assert!(
        statement_is_tombstoned(&db, s),
        "single-evidence statement must be tombstoned post-cascade"
    );
    let metrics = fx.worker.metrics().snapshot();
    assert_eq!(metrics.jobs_processed, 1);
    assert_eq!(metrics.statements_tombstoned, 1);
}

#[test]
fn soft_forget_also_enqueues_cascade() {
    // Rule 3.1: soft FORGET also triggers re-derivation;
    // readers must not see stale-confidence statements during the
    // grace window.
    let fx = build_fixture();
    let m = MemoryId::pack(0, 2, 1);
    upsert_memory(&fx.writer, m);

    let pred = intern_predicate(&fx.metadata, "prefers_color");
    let subj = make_entity(&fx.metadata, "alice-soft");
    let s = seed_statement(&fx.metadata, pred, subj, vec![(m, 0.9)]);

    tombstone_memory(&fx.writer, m, TombstoneMode::Soft);
    assert_eq!(fx.worker.queue_depth(), 1, "soft FORGET must enqueue");
    drive_worker_once(&fx.worker, &fx.ctx);

    let db = fx.metadata.lock();
    assert!(statement_is_tombstoned(&db, s));
}

#[test]
fn cascade_rederives_confidence_when_other_evidence_remains() {
    let fx = build_fixture();
    let m1 = MemoryId::pack(0, 3, 1);
    let m2 = MemoryId::pack(0, 4, 1);
    upsert_memory(&fx.writer, m1);
    upsert_memory(&fx.writer, m2);
    let pred = intern_predicate(&fx.metadata, "prefers_color");
    let subj = make_entity(&fx.metadata, "alice-multi-evidence");
    let s = seed_statement(&fx.metadata, pred, subj, vec![(m1, 0.8), (m2, 0.8)]);

    tombstone_memory(&fx.writer, m1, TombstoneMode::Hard);
    drive_worker_once(&fx.worker, &fx.ctx);

    let db = fx.metadata.lock();
    assert!(!statement_is_tombstoned(&db, s));
    let post = statement_confidence(&db, s).unwrap();
    // Noisy-OR over a single c=0.8 entry at age=0 → ~0.8.
    assert!(
        (post - 0.8).abs() < 1e-3,
        "expected re-derived confidence ~0.8 from the surviving evidence, got {post}"
    );
}

#[test]
fn cascade_drains_multiple_pending_jobs() {
    // Two forget submits in succession; one drive cycle drains both
    // (batch_size default >= 2).
    let fx = build_fixture();
    let m1 = MemoryId::pack(0, 5, 1);
    let m2 = MemoryId::pack(0, 6, 1);
    upsert_memory(&fx.writer, m1);
    upsert_memory(&fx.writer, m2);

    let pred = intern_predicate(&fx.metadata, "prefers_color");
    let subj = make_entity(&fx.metadata, "alice-many");
    let s1 = seed_statement(&fx.metadata, pred, subj, vec![(m1, 0.7)]);
    let s2 = seed_statement(&fx.metadata, pred, subj, vec![(m2, 0.7)]);

    tombstone_memory(&fx.writer, m1, TombstoneMode::Hard);
    tombstone_memory(&fx.writer, m2, TombstoneMode::Hard);
    assert_eq!(fx.worker.queue_depth(), 2);

    let processed = drive_worker_once(&fx.worker, &fx.ctx);
    assert!(processed >= 2);
    let db = fx.metadata.lock();
    assert!(statement_is_tombstoned(&db, s1));
    assert!(statement_is_tombstoned(&db, s2));
}
