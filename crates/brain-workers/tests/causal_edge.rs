#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send

//! CausalEdgeWorker integration test — drives the worker through the
//! `StatementObject::Memory` short-circuit branch (one causal statement
//! whose object directly references a cause memory). Verifies the
//! worker emits a `Phase::Link` per derived edge via `submit(Write)`
//! with WAL coverage + bus events.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, StatementId};
use brain_core::{
    EntityId, EntityTypeId, EvidenceEntry, EvidenceRef, ExtractorId, Statement, StatementKind,
    StatementObject, SubjectRef,
};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::entity::ops::entity_put;
use brain_metadata::entity::types::entity_type_intern;
use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::statement::statement_create;
use brain_metadata::tables::edge::{origin as edge_origin, EDGES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::writer::wal_sink::RecordingWalSink;
use brain_ops::{EventBus, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_storage::wal::kinds::WalRecordKind;
use brain_workers::{
    CausalEdgeKnobs, CausalEdgeWorker, Worker, WorkerConfig, WorkerContext, WorkerKind,
};
use redb::ReadableTable;
use smallvec::SmallVec;
use uuid::Uuid;

struct NopDispatcher;
impl Dispatcher for NopDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}

struct Fixture {
    ctx: Arc<OpsContext>,
    writer: Arc<RealWriterHandle>,
    metadata: SharedMetadataDb,
    sink: Arc<RecordingWalSink>,
    bus: Arc<EventBus>,
    sender: flume::Sender<brain_ops::CausalEdgeEnqueue>,
    receiver: flume::Receiver<brain_ops::CausalEdgeEnqueue>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let bus = Arc::new(EventBus::default());
    let sink = Arc::new(RecordingWalSink::new());
    let (tx, rx) = flume::bounded(64);
    let writer = Arc::new(
        RealWriterHandle::new(metadata.clone(), hnsw_writer)
            .with_event_bus(bus.clone())
            .with_wal_sink(sink.clone()),
    );
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer.clone() as Arc<dyn WriterHandle>,
    );
    let ctx = Arc::new(
        brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)
            .with_event_bus(bus.clone()),
    );
    Fixture {
        ctx,
        writer,
        metadata,
        sink,
        bus,
        sender: tx,
        receiver: rx,
        _tempdir: tempdir,
    }
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn make_id(slot: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[8..16].copy_from_slice(&slot.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

async fn seed_memory(fixture: &Fixture, slot: u64) -> MemoryId {
    use brain_core::Salience;
    use brain_ops::{Phase, Write, WriteId};

    let id = make_id(slot);
    let phase = Phase::UpsertMemory {
        id,
        text: format!("mem-{slot}"),
        vector: Box::new([0.0_f32; VECTOR_DIM]),
        kind: MemoryKind::Episodic,
        salience: Salience::default(),
        context: ContextId(1),
        created_at_unix_nanos: now_unix_nanos(),
        arena_slot: slot,
        embedding_model_fp: [0; 16],
        content_hash: None,
        deduplicate: false,
    };
    let write = Write::single(WriteId::new(), AgentId::default(), phase);
    fixture.writer.submit(write).await.expect("seed submit");
    id
}

fn glommio_run<F, Fut>(body: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .make()
        .unwrap()
        .run(async move { body().await });
}

/// Seed the typed-graph state needed for the worker's
/// `StatementObject::Memory` short-circuit branch: an entity-type +
/// entity (subject), the `brain:caused_by` predicate, and a causal
/// statement whose object names `cause_mem` directly. The worker walks
/// `statement.evidence` for effect-side memories.
fn seed_causal_statement(
    fixture: &Fixture,
    effect_mem: MemoryId,
    cause_mem: MemoryId,
    confidence: f32,
) -> StatementId {
    let wtxn = fixture.metadata.write_txn().unwrap();
    let now = now_unix_nanos();
    // 1. Entity type for the subject (any type works; the resolver only
    //    requires existence).
    let entity_type: EntityTypeId =
        entity_type_intern(&wtxn, "Thing", Vec::new(), now).expect("entity_type_intern");
    // 2. Subject entity (the statement asserts something about it).
    let subject_entity = EntityId::new();
    let entity = brain_core::Entity::new_active(
        subject_entity,
        entity_type,
        "Outage".into(),
        "outage".into(),
        now,
    );
    entity_put(&wtxn, &entity).expect("entity_put");
    // 3. Predicate `brain:caused_by` — matches the default whitelist.
    let predicate =
        predicate_intern_or_get(&wtxn, "brain", "caused_by", 1, now).expect("predicate_intern");
    // 4. Statement: subject=outage_entity, predicate=caused_by,
    //    object=Memory(cause_mem), evidence=[effect_mem]. The worker's
    //    short-circuit emits one Phase::Link(cause_mem → effect_mem).
    let sid = StatementId::new();
    let mut entries: SmallVec<[EvidenceEntry; 8]> = SmallVec::new();
    entries.push(EvidenceEntry::from_parts(
        effect_mem,
        confidence,
        now,
        ExtractorId(0),
    ));
    let statement = Statement::new_root(
        sid,
        StatementKind::Fact,
        SubjectRef::Entity(subject_entity),
        predicate,
        StatementObject::Memory(cause_mem),
        confidence,
        EvidenceRef::inline(entries),
        ExtractorId(0),
        now,
        1,
    );
    statement_create(&wtxn, &statement, now).expect("statement_create");
    drop(statement);
    wtxn.commit().unwrap();
    sid
}

#[test]
fn cycle_writes_caused_link_through_unified_path() {
    glommio_run(|| async {
        let fix = build_fixture();
        // Seed the two memories (cause + effect).
        let cause_mem = seed_memory(&fix, 1).await;
        let effect_mem = seed_memory(&fix, 2).await;
        // Drain any seed-time events so the assertion checks only the
        // worker's bus output.
        let mut rx = fix.bus.receiver();
        while rx.try_recv().is_ok() {}

        // Record the WAL sink length before the worker runs — we want
        // to count Link records produced by the worker, not the seeds.
        let pre_link_count = fix
            .sink
            .appended()
            .iter()
            .filter(|r| r.kind == WalRecordKind::Link)
            .count();

        let sid = seed_causal_statement(&fix, effect_mem, cause_mem, 0.9);
        fix.sender.try_send(sid).expect("enqueue");

        let worker = CausalEdgeWorker::new(fix.receiver.clone()).with_knobs(CausalEdgeKnobs {
            whitelist_qnames: vec![("brain".to_string(), "caused_by".to_string())],
            min_confidence: 0.5,
            max_effect_memories_per_statement: 3,
            max_cause_memories_per_statement: 3,
            max_related_statements_per_entity: 5,
        });
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wctx = WorkerContext {
            ops: fix.ctx.clone(),
            shutdown,
        };
        let processed = worker.run_cycle(&wctx).await.unwrap();
        assert!(processed > 0, "worker drained the statement");

        // 1. WAL sink has one new Link record (the derived Caused edge
        //    cause_mem → effect_mem). Causal edges are asymmetric — no
        //    mirror.
        let post_link_count = fix
            .sink
            .appended()
            .iter()
            .filter(|r| r.kind == WalRecordKind::Link)
            .count();
        assert_eq!(
            post_link_count - pre_link_count,
            1,
            "expected exactly one new Link WAL record from the worker"
        );

        // 2. Bus published EdgeAdded(AUTO_DERIVED) for the derived edge.
        let mut saw_auto_derived = false;
        while let Ok(env) = rx.try_recv() {
            if env.event_type == brain_protocol::EventType::EdgeAdded {
                let ep = env.edge_payload.as_ref().expect("edge payload");
                if ep.origin == edge_origin::AUTO_DERIVED {
                    saw_auto_derived = true;
                }
            }
        }
        assert!(
            saw_auto_derived,
            "bus must publish EdgeAdded(AUTO_DERIVED) for the derived Caused edge"
        );

        // 3. redb has the derived Caused row(s). Causal is asymmetric
        //    (no mirror); expect exactly one auto-derived row.
        let rtxn = fix.metadata.read_txn().unwrap();
        let t = rtxn.open_table(EDGES_TABLE).unwrap();
        let mut found = 0;
        for entry in t.iter().unwrap() {
            let (_, v) = entry.unwrap();
            let data = v.value();
            if data.origin == edge_origin::AUTO_DERIVED {
                found += 1;
            }
        }
        assert_eq!(
            found, 1,
            "Caused is asymmetric — exactly one auto-derived row, got {found}"
        );

        // Drop unused vars (test ergonomics).
        let _ = (cause_mem, effect_mem, sid, Uuid::nil());
    });
}

#[test]
fn name_and_kind_are_stable() {
    let (_tx, rx) = flume::bounded(1);
    let worker = CausalEdgeWorker::new(rx);
    assert_eq!(worker.name(), WorkerKind::CausalEdge.name());
    assert_eq!(worker.kind(), WorkerKind::CausalEdge);
    let cfg = WorkerConfig::defaults_for(WorkerKind::CausalEdge);
    assert!(cfg.batch_size > 0);
}
