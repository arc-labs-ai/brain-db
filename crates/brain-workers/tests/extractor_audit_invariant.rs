#![allow(clippy::arc_with_non_send_sync)]
//! Audit-row status invariant under randomly succeeding/failing
//! extractors.
//!
//! For any combination of (pattern outcome, llm outcome), the audit
//! row's `status` byte must reflect the joint outcome:
//!
//! - both Success                    → SUCCESS
//! - one Success, one Failure        → PARTIAL_FAILURE
//! - both Failure (and registry has  → FAILURE
//!   no Success tier)
//! - registry empty (no tier ran)    → SKIPPED
//!
//! Property: the worker NEVER writes an audit row whose status
//! contradicts the observed tier outcomes.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use brain_core::knowledge::ExtractorKind;
use brain_core::{AgentId, ContextId, ExtractorId, Memory as CoreMemory, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_extractors::{
    EntityMention, ExtractedItem, ExtractionContext, ExtractionFuture, ExtractionResult, Extractor,
    ExtractorRegistry,
};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::extractor_audit::{pipeline_status, EXTRACTOR_PIPELINE_AUDIT_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{ExtractorEnqueue, OpsContext, RealWriterHandle};
use brain_planner::{EncodeOp, ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{ExtractorKnobs, ExtractorWorker, Worker, WorkerConfig, WorkerContext};
use parking_lot::Mutex;
use proptest::prelude::*;
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
    metadata: SharedMetadataDb,
    queue_rx: flume::Receiver<ExtractorEnqueue>,
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
    real.set_extractor_sender(tx);
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
        _td: td,
    }
}

struct ScriptedTier {
    id: ExtractorId,
    kind: ExtractorKind,
    succeed: bool,
    items: Vec<ExtractedItem>,
}
impl Extractor for ScriptedTier {
    fn id(&self) -> ExtractorId {
        self.id
    }
    fn kind(&self) -> ExtractorKind {
        self.kind
    }
    fn name(&self) -> &str {
        "scripted"
    }
    fn extractor_version(&self) -> u32 {
        1
    }
    fn run<'a>(
        &'a self,
        _ctx: &'a ExtractionContext<'a>,
        _mem: &'a CoreMemory,
    ) -> ExtractionFuture<'a> {
        let succeed = self.succeed;
        let items = self.items.clone();
        Box::pin(async move {
            if succeed {
                ExtractionResult::success(items, 0, 0)
            } else {
                ExtractionResult::failure("scripted-fail", 0, 0)
            }
        })
    }
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("extractor-audit-prop")
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

fn expected_status(pattern_ok: bool, llm_ok: bool) -> u8 {
    // Both succeed → SUCCESS.
    // Mixed → PARTIAL_FAILURE.
    // Both fail → FAILURE.
    match (pattern_ok, llm_ok) {
        (true, true) => pipeline_status::SUCCESS,
        (true, false) | (false, true) => pipeline_status::PARTIAL_FAILURE,
        (false, false) => pipeline_status::FAILURE,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, .. ProptestConfig::default() })]

    /// For any combination of pattern/llm success-fail, the audit row's
    /// status matches the expected aggregate.
    #[test]
    fn audit_status_reflects_joint_tier_outcome(
        pattern_ok in any::<bool>(),
        llm_ok in any::<bool>(),
        seed in 1u8..200,
    ) {
        let (got_status, want_status) = glommio_run(move || async move {
            let fix = build_fixture();
            let mut reg = ExtractorRegistry::new();
            reg.register(Arc::new(ScriptedTier {
                id: ExtractorId::from(101),
                kind: ExtractorKind::Pattern,
                succeed: pattern_ok,
                items: vec![em("Subj", "brain:Person", 0.9)],
            }));
            reg.register(Arc::new(ScriptedTier {
                id: ExtractorId::from(202),
                kind: ExtractorKind::Llm,
                succeed: llm_ok,
                items: vec![em("Obj", "brain:Person", 0.9)],
            }));
            *fix.ctx.extractor_registry.write() = reg;

            let memory_id = submit_encode(&fix.ctx, encode_op(seed, "audit-property")).await;
            let w = fast_worker(fix.queue_rx.clone());
            run_one_cycle(&w, fix.ctx.clone()).await.unwrap();
            let got = audit_status(&fix.metadata, memory_id).unwrap();
            (got, expected_status(pattern_ok, llm_ok))
        });
        prop_assert_eq!(got_status, want_status);
    }
}
