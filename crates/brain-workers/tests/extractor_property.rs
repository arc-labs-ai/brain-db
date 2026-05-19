#![allow(clippy::arc_with_non_send_sync)]
//! ExtractorWorker property tests.
//!
//! Invariant: for any text input, after one worker cycle the per-memory
//! audit table contains exactly one row for that memory's `MemoryId`.
//! This is the worker's idempotency guarantee — without it, the queue
//! could re-drain the same memory forever.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use brain_core::{AgentId, ContextId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_extractors::ExtractorRegistry;
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::pipeline_has_extracted;
use brain_metadata::MetadataDb;
use brain_ops::{ExtractorEnqueue, OpsContext, RealWriterHandle};
use brain_planner::{EncodeOp, ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{ExtractorWorker, Worker, WorkerConfig, WorkerContext};
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

fn build_fixture() -> (
    Arc<OpsContext>,
    SharedMetadataDb,
    flume::Receiver<ExtractorEnqueue>,
    tempfile::TempDir,
) {
    let tempdir = tempfile::tempdir().unwrap();
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(
        MetadataDb::open(tempdir.path().join("metadata.redb")).unwrap(),
    ));
    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let (tx, rx) = flume::bounded(64);
    let mut real_writer = RealWriterHandle::new(metadata.clone(), hnsw_writer);
    real_writer.set_extractor_sender(tx);
    let writer: Arc<dyn WriterHandle> = Arc::new(real_writer);
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer,
    );
    let mut ctx = OpsContext::new(executor);
    ctx.extractor_registry = Arc::new(parking_lot::RwLock::new(ExtractorRegistry::new()));
    (Arc::new(ctx), metadata, rx, tempdir)
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("extractor-prop-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, .. ProptestConfig::default() })]

    /// Any text input, after one cycle, leaves exactly one audit row
    /// for the encoded memory. The empty registry → SKIPPED status
    /// path is the easiest invariant to prove without LLM mocking.
    #[test]
    fn audit_row_written_after_cycle(text in "[a-zA-Z0-9 ]{0,256}") {
        let result: HashMap<&str, bool> = glommio_run(move || async move {
            let (ctx, metadata, rx, _td) = build_fixture();
            let op = EncodeOp {
                request_id: brain_core::RequestId::from([7u8; 16]),
                context_id: ContextId(1),
                kind: MemoryKind::Episodic,
                text,
                vector: [0.0; VECTOR_DIM],
                salience_initial: 0.5,
                fingerprint: [0; 16],
                edges: vec![],
                deduplicate: false,
                content_hash: [0; 32],
                agent_id: AgentId(Uuid::nil()),
            };
            let memory_id = ctx.executor.writer.submit_encode(op).await.expect("encode").memory_id;

            let cfg = WorkerConfig {
                enabled: true,
                interval: std::time::Duration::from_millis(50),
                batch_size: 8,
                max_runtime: std::time::Duration::from_secs(5),
            };
            let worker = ExtractorWorker::new(rx).with_config(cfg);
            let shutdown = Arc::new(AtomicBool::new(false));
            let wctx = WorkerContext { ops: ctx.clone(), shutdown };
            let _ = worker.run_cycle(&wctx).await.unwrap();

            let db = metadata.lock();
            let rtxn = db.read_txn().unwrap();
            let mut m = HashMap::new();
            m.insert("present", pipeline_has_extracted(&rtxn, memory_id).unwrap());
            m
        });
        prop_assert!(result["present"], "audit row missing after cycle");
    }
}
