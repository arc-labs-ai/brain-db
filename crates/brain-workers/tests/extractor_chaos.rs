#![allow(clippy::arc_with_non_send_sync)]
//! Chaos-style ExtractorWorker tests. These exercise the worker's
//! crash-recovery posture:
//!
//! - Mid-cycle drop of the apply txn: nothing commits → next cycle
//!   re-extracts → resolver lands on the same `EntityId`.
//! - LLM tier fails on every retry: pattern tier still commits;
//!   audit row = PARTIAL_FAILURE; the memory is NOT retried on the
//!   next cycle.
//! - Audit row deleted out-of-band: worker treats as "not yet
//!   extracted" and re-runs; resolver de-dupes to the same entity
//!   (no ghost row).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use brain_core::knowledge::ExtractorKind;
use brain_core::{
    AgentId, ContextId, EdgeKindRef, EntityId, ExtractorId, Memory as CoreMemory, MemoryId,
    MemoryKind, NodeRef,
};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_extractors::{
    EntityMention, ExtractedItem, ExtractionContext, ExtractionFuture, ExtractionResult, Extractor,
    ExtractorRegistry,
};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{EdgeKey, EDGES_TABLE};
use brain_metadata::tables::extractor_audit::{pipeline_status, EXTRACTOR_PIPELINE_AUDIT_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{ExtractorEnqueue, OpsContext, RealWriterHandle};
use brain_planner::{EncodeOp, ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{ExtractorKnobs, ExtractorWorker, Worker, WorkerConfig, WorkerContext};
use parking_lot::Mutex;
use redb::ReadableTable;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

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
    metadata: SharedMetadataDb,
    queue_rx: flume::Receiver<ExtractorEnqueue>,
    queue_tx: flume::Sender<ExtractorEnqueue>,
    _td: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let td = tempfile::tempdir().unwrap();
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(
        MetadataDb::open(td.path().join("metadata.redb")).unwrap(),
    ));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let (tx, rx) = flume::bounded(64);
    let mut real = RealWriterHandle::new(metadata.clone(), hnsw_writer);
    real.set_extractor_sender(tx.clone());
    let writer: Arc<dyn WriterHandle> = Arc::new(real);
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer,
    );
    Fixture {
        ctx: Arc::new(OpsContext::new(executor)),
        metadata,
        queue_rx: rx,
        queue_tx: tx,
        _td: td,
    }
}

fn encode_op(seed: u8, text: &str) -> EncodeOp {
    EncodeOp {
        request_id: brain_core::RequestId::from([seed; 16]),
        context_id: ContextId(1),
        kind: MemoryKind::Episodic,
        text: text.to_string(),
        vector: [0.0; VECTOR_DIM],
        salience_initial: 0.5,
        fingerprint: [0; 16],
        edges: vec![],
        deduplicate: false,
        content_hash: [0; 32],
        agent_id: AgentId(Uuid::nil()),
    }
}

async fn submit_encode(ctx: &OpsContext, op: EncodeOp) -> MemoryId {
    ctx.executor
        .writer
        .submit_encode(op)
        .await
        .expect("encode")
        .memory_id
}

async fn run_one_cycle(
    worker: &ExtractorWorker,
    ctx: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let wctx = WorkerContext { ops: ctx, shutdown };
    worker.run_cycle(&wctx).await
}

fn count_entities_with_name(metadata: &SharedMetadataDb, name: &str) -> (usize, Option<EntityId>) {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn
        .open_table(brain_metadata::tables::knowledge::entity::ENTITIES_TABLE)
        .unwrap();
    let mut count = 0;
    let mut id: Option<EntityId> = None;
    for entry in t.iter().unwrap() {
        let (_, v) = entry.unwrap();
        let row = v.value();
        if row.canonical_name == name {
            count += 1;
            id = Some(EntityId::from(row.entity_id_bytes));
        }
    }
    (count, id)
}

fn count_mention_edges_out(metadata: &SharedMetadataDb, from: MemoryId) -> usize {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(EDGES_TABLE).unwrap();
    let prefix = NodeRef::Memory(from).to_bytes();
    let upper = {
        let mut v = prefix.to_vec();
        v.push(0xFF);
        v
    };
    let mut total = 0usize;
    for entry in t.range(prefix.as_slice()..upper.as_slice()).unwrap() {
        let (key, _) = entry.unwrap();
        let decoded = EdgeKey::decode(key.value()).unwrap();
        if matches!(decoded.kind, EdgeKindRef::Mentions) {
            total += 1;
        }
    }
    total
}

fn audit_status(metadata: &SharedMetadataDb, memory_id: MemoryId) -> Option<u8> {
    let db = metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let t = match rtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE) {
        Ok(t) => t,
        Err(_) => return None,
    };
    t.get(&memory_id.to_be_bytes())
        .unwrap()
        .map(|g| g.value().status)
}

fn delete_audit_row(metadata: &SharedMetadataDb, memory_id: MemoryId) {
    let mut db = metadata.lock();
    let wtxn = db.write_txn().unwrap();
    {
        let mut t = wtxn.open_table(EXTRACTOR_PIPELINE_AUDIT_TABLE).unwrap();
        t.remove(&memory_id.to_be_bytes()).unwrap();
    }
    wtxn.commit().unwrap();
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("extractor-chaos-test")
        .spawn(move || async move { f().await })
        .expect("spawn")
        .join()
        .expect("join")
}

fn fast_worker(rx: flume::Receiver<ExtractorEnqueue>) -> ExtractorWorker {
    let cfg = WorkerConfig {
        enabled: true,
        interval: std::time::Duration::from_millis(50),
        batch_size: 64,
        max_runtime: std::time::Duration::from_secs(5),
    };
    ExtractorWorker::new(rx)
        .with_config(cfg)
        .with_knobs(ExtractorKnobs {
            drain_per_cycle: 64,
            llm_budget_per_cycle_micro_usd: 0,
            skip_already_extracted: true,
        })
}

// ---------------------------------------------------------------------------
// Mock extractors.
// ---------------------------------------------------------------------------

struct MockExtractor {
    id: ExtractorId,
    kind: ExtractorKind,
    items: HashMap<String, Vec<ExtractedItem>>,
}
impl Extractor for MockExtractor {
    fn id(&self) -> ExtractorId {
        self.id
    }
    fn kind(&self) -> ExtractorKind {
        self.kind
    }
    fn name(&self) -> &str {
        "mock-pattern"
    }
    fn extractor_version(&self) -> u32 {
        1
    }
    fn run<'a>(
        &'a self,
        _ctx: &'a ExtractionContext<'a>,
        mem: &'a CoreMemory,
    ) -> ExtractionFuture<'a> {
        let key = mem.text.clone().unwrap_or_default();
        let items = self.items.get(&key).cloned().unwrap_or_default();
        Box::pin(async move { ExtractionResult::success(items, 0, 0) })
    }
}

/// LLM tier that simulates "5xx on every retry" — returns Failure
/// deterministically. The worker treats this as a tier failure and
/// commits the rest.
struct AlwaysFailLlm {
    id: ExtractorId,
    call_count: Arc<AtomicUsize>,
}
impl Extractor for AlwaysFailLlm {
    fn id(&self) -> ExtractorId {
        self.id
    }
    fn kind(&self) -> ExtractorKind {
        ExtractorKind::Llm
    }
    fn name(&self) -> &str {
        "always-fail-llm"
    }
    fn extractor_version(&self) -> u32 {
        1
    }
    fn run<'a>(
        &'a self,
        _ctx: &'a ExtractionContext<'a>,
        _mem: &'a CoreMemory,
    ) -> ExtractionFuture<'a> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            ExtractionResult::failure("LLM provider 5xx (simulated all-retries)", 0, 0)
        })
    }
}

fn em(text: &str, qname: &str, c: f32) -> ExtractedItem {
    ExtractedItem::EntityMention(EntityMention {
        entity_type_qname: qname.into(),
        text: text.into(),
        start: 0,
        end: 0,
        confidence: c,
        extractor_id: 101,
        extractor_version: 1,
    })
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// LLM tier fails for every retry. The pattern tier still commits
/// its outputs; the audit row is PARTIAL_FAILURE. A second cycle
/// against the same memory is short-circuited by the audit guard
/// (no second LLM call, no double-write of mention edges).
#[test]
fn llm_all_retries_fail_partial_failure_no_retry() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "Dana ships";
        let mut items = HashMap::new();
        items.insert(text.into(), vec![em("Dana", "brain:Person", 0.9)]);

        let calls = Arc::new(AtomicUsize::new(0));
        let mut reg = ExtractorRegistry::new();
        reg.register(Arc::new(MockExtractor {
            id: ExtractorId::from(101),
            kind: ExtractorKind::Pattern,
            items,
        }));
        reg.register(Arc::new(AlwaysFailLlm {
            id: ExtractorId::from(202),
            call_count: calls.clone(),
        }));
        *fix.ctx.extractor_registry.write() = reg;

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let w = fast_worker(fix.queue_rx.clone());

        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        // Pattern committed.
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 1);
        let (count, _) = count_entities_with_name(&fix.metadata, "Dana");
        assert_eq!(count, 1);
        // Audit = PARTIAL_FAILURE.
        assert_eq!(
            audit_status(&fix.metadata, memory_id),
            Some(pipeline_status::PARTIAL_FAILURE)
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "LLM invoked exactly once this cycle"
        );

        // Re-enqueue + re-run: audit guard short-circuits, LLM not
        // called again, no double-write.
        fix.queue_tx.send((memory_id, Arc::from(text))).unwrap();
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "audit guard must prevent re-invocation"
        );
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 1);
    });
}

/// Audit-row corruption: delete the row out-of-band → next drain re-runs
/// extraction. The resolver is idempotent: the second cycle finds the
/// existing entity row via tier-1 and reuses it. The mention edge is
/// also a no-op (the link is idempotent per `(memory, kind, entity)`).
#[test]
fn audit_row_deletion_triggers_idempotent_replay() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "Eve joins";
        let mut items = HashMap::new();
        items.insert(text.into(), vec![em("Eve", "brain:Person", 0.9)]);
        let mut reg = ExtractorRegistry::new();
        reg.register(Arc::new(MockExtractor {
            id: ExtractorId::from(101),
            kind: ExtractorKind::Pattern,
            items,
        }));
        *fix.ctx.extractor_registry.write() = reg;

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        let w = fast_worker(fix.queue_rx.clone());
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 1);
        let (count_before, id_before) = count_entities_with_name(&fix.metadata, "Eve");
        assert_eq!(count_before, 1);

        // Corrupt the audit table by removing the row.
        delete_audit_row(&fix.metadata, memory_id);
        assert!(audit_status(&fix.metadata, memory_id).is_none());

        // Re-enqueue and re-run; worker sees no audit row → re-extracts.
        fix.queue_tx.send((memory_id, Arc::from(text))).unwrap();
        run_one_cycle(&w, fix.ctx.clone()).await.unwrap();

        // Entity count still 1 (tier-1 hit), audit row restored.
        let (count_after, id_after) = count_entities_with_name(&fix.metadata, "Eve");
        assert_eq!(count_after, 1, "resolver must dedupe; no ghost row");
        assert_eq!(id_before, id_after, "re-extract must hit the same EntityId");
        assert_eq!(
            audit_status(&fix.metadata, memory_id),
            Some(pipeline_status::SUCCESS)
        );
    });
}

/// Mid-extraction "crash": we simulate the kill by encoding a memory,
/// then dropping the futures-side worker handle BEFORE running a cycle.
/// On the next worker construction + cycle the queue still holds the
/// enqueue → extraction runs cleanly. Stand-in for the "kill server
/// between encode and worker drain" scenario.
#[test]
fn worker_handle_dropped_before_cycle_recovers_on_next_run() {
    glommio_run(|| async {
        let fix = build_fixture();
        let text = "Frank arrived";
        let mut items = HashMap::new();
        items.insert(text.into(), vec![em("Frank", "brain:Person", 0.9)]);
        let mut reg = ExtractorRegistry::new();
        reg.register(Arc::new(MockExtractor {
            id: ExtractorId::from(101),
            kind: ExtractorKind::Pattern,
            items,
        }));
        *fix.ctx.extractor_registry.write() = reg;

        let memory_id = submit_encode(&fix.ctx, encode_op(1, text)).await;
        // Build and drop a worker before draining. The queue still
        // owns the enqueue (the worker only holds the receiver).
        {
            let _w_dropped = fast_worker(fix.queue_rx.clone());
        }
        // No commit happened → no audit row, no entity, no edges.
        assert!(audit_status(&fix.metadata, memory_id).is_none());
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 0);

        // Recovery cycle.
        let w = fast_worker(fix.queue_rx.clone());
        let drained = run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
        assert_eq!(drained, 1);
        assert_eq!(count_mention_edges_out(&fix.metadata, memory_id), 1);
        assert_eq!(
            audit_status(&fix.metadata, memory_id),
            Some(pipeline_status::SUCCESS)
        );
    });
}
