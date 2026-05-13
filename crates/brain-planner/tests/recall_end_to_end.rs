//! End-to-end test for the Recall planner + executor (sub-task 6.3).
//!
//! Harness:
//! - `MetadataDb` opened in a tempdir, pre-populated with a handful
//!   of `MemoryMetadata` rows.
//! - `SharedHnsw::<384>` with the matching vectors inserted.
//! - `MockDispatcher` returning a deterministic vector per text — the
//!   "cue" text the test queries with maps to one of the inserted
//!   vectors so we know which hit the executor should return first.
//!
//! Tests assert ordering, K trim, confidence filter, kind filter,
//! and that hit metadata is correctly surfaced.
//!
//! A BGE-gated `recall_with_real_embedder` test lives at the bottom
//! behind `BRAIN_EMBED_MODEL_DIR`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_planner::{
    execute_recall, plan_recall, plan_recall_inner, EncodeAck, EncodeOp, ExecutionPlan,
    ExecutorContext, ForgetAck, ForgetOp, PlannerContext, SharedMetadataDb, WriterError,
    WriterHandle,
};
use brain_protocol::request::{MemoryKindWire, RecallRequest};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Mock dispatcher.
// ---------------------------------------------------------------------------

struct MockDispatcher {
    fp: [u8; 16],
    /// Maps cue text → the vector to return. Anything not in the map
    /// returns a deterministic vector seeded from the text's first byte.
    map: parking_lot::Mutex<std::collections::HashMap<String, [f32; VECTOR_DIM]>>,
    calls: AtomicU64,
}

impl MockDispatcher {
    fn new(fp: [u8; 16]) -> Self {
        Self {
            fp,
            map: parking_lot::Mutex::new(std::collections::HashMap::new()),
            calls: AtomicU64::new(0),
        }
    }

    fn install(&self, text: &str, vector: [f32; VECTOR_DIM]) {
        self.map.lock().insert(text.to_string(), vector);
    }
}

impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(v) = self.map.lock().get(text) {
            return Ok(*v);
        }
        // Fallback: deterministic vector from first byte.
        let mut v = [0.0f32; VECTOR_DIM];
        let seed = text.bytes().next().unwrap_or(0) as f32;
        v[0] = seed;
        // L2-normalise so cosine similarity is sensible.
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-8 {
            for x in &mut v {
                *x /= norm;
            }
        }
        Ok(v)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        self.fp
    }
}

// ---------------------------------------------------------------------------
// No-op writer for read-path tests.
// ---------------------------------------------------------------------------

struct NoopWriter;

impl WriterHandle for NoopWriter {
    fn submit_encode<'a>(
        &'a self,
        _op: EncodeOp,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<EncodeAck, WriterError>> + 'a>>
    {
        Box::pin(async move {
            Err(WriterError::Internal(
                "noop writer used in recall test".into(),
            ))
        })
    }

    fn submit_forget<'a>(
        &'a self,
        _op: ForgetOp,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ForgetAck, WriterError>> + 'a>>
    {
        Box::pin(async move {
            Err(WriterError::Internal(
                "noop writer used in recall test".into(),
            ))
        })
    }

    fn submit_link<'a>(
        &'a self,
        _: brain_planner::LinkOp,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<brain_planner::LinkAck, WriterError>> + 'a>,
    > {
        Box::pin(async move {
            Err(WriterError::Internal(
                "noop writer used in recall test".into(),
            ))
        })
    }

    fn submit_unlink<'a>(
        &'a self,
        _: brain_planner::UnlinkOp,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<brain_planner::UnlinkAck, WriterError>> + 'a>,
    > {
        Box::pin(async move {
            Err(WriterError::Internal(
                "noop writer used in recall test".into(),
            ))
        })
    }

    fn reserve_memory_id<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<brain_core::MemoryId, WriterError>> + 'a>,
    > {
        Box::pin(async move {
            Err(WriterError::Internal(
                "test writer: reserve_memory_id unused".into(),
            ))
        })
    }

    fn submit_batch<'a>(
        &'a self,
        _: brain_planner::TxnBatch,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<brain_planner::TxnBatchAck, WriterError>> + 'a>,
    > {
        Box::pin(async move {
            Err(WriterError::Internal(
                "test writer: submit_batch unused".into(),
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Test fixture builder.
// ---------------------------------------------------------------------------

struct Fixture {
    pub ctx: ExecutorContext,
    pub mock: Arc<MockDispatcher>,
    pub memory_ids: Vec<MemoryId>,
    // Hold the tempdir alive so the MetadataDb files don't get GC'd.
    _tempdir: tempfile::TempDir,
}

fn unit_vector_with_dim_one(value: f32) -> [f32; VECTOR_DIM] {
    let mut v = [0.0f32; VECTOR_DIM];
    v[0] = if value >= 0.0 { 1.0 } else { -1.0 };
    v
}

fn unit_vector_at_dim(dim: usize) -> [f32; VECTOR_DIM] {
    let mut v = [0.0f32; VECTOR_DIM];
    v[dim] = 1.0;
    v
}

fn build_fixture(memories: &[(MemoryKind, f32, [f32; VECTOR_DIM])]) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let mut metadata = MetadataDb::open(&db_path).unwrap();

    // Build SharedHnsw + Writer.
    let (shared, mut writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();

    let agent = AgentId(Uuid::nil());
    let mut memory_ids = Vec::with_capacity(memories.len());

    // Populate metadata + index.
    {
        let wtxn = metadata.write_txn().unwrap();
        {
            let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
            for (i, (kind, salience, vector)) in memories.iter().enumerate() {
                let mid = MemoryId::from_be_bytes({
                    let mut b = [0u8; 16];
                    b[0..8].copy_from_slice(&((i as u64) + 1).to_be_bytes());
                    b
                });
                memory_ids.push(mid);
                let meta = MemoryMetadata::new_active(
                    mid,
                    agent,
                    ContextId(42),
                    /* slot_id */ (i + 1) as u64,
                    /* slot_version */ 1,
                    *kind,
                    /* embedding_model_fp */ [0x11; 16],
                    *salience,
                    /* text_size */ 16,
                    /* created_at_unix_nanos */ 1_000_000 + i as u64,
                );
                table.insert(mid.to_be_bytes(), meta).unwrap();
                writer.insert(mid, vector).unwrap();
            }
        }
        wtxn.commit().unwrap();
    }

    let mock = Arc::new(MockDispatcher::new([0x11; 16]));
    let metadata: SharedMetadataDb = Arc::new(parking_lot::Mutex::new(metadata));
    let ctx = ExecutorContext::new(
        mock.clone() as Arc<dyn Dispatcher>,
        shared,
        metadata,
        Arc::new(NoopWriter) as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx,
        mock,
        memory_ids,
        _tempdir: tempdir,
    }
}

fn base_request(cue: &str, top_k: u32) -> RecallRequest {
    RecallRequest {
        cue_text: cue.into(),
        cue_vector_offset: 0,
        cue_vector_dim: 0,
        top_k,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        strategy_hint: None,
        include_vectors: false,
        include_edges: false,
        request_id: None,
        txn_id: None,
    }
}

fn unwrap_recall(plan: ExecutionPlan) -> brain_planner::RecallPlan {
    match plan {
        ExecutionPlan::Recall(p) => p,
        other => panic!("expected Recall, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recall_returns_top_k_in_score_order() {
    // Build 5 unit vectors along distinct dimensions.
    let memories: Vec<_> = (0..5)
        .map(|i| (MemoryKind::Episodic, 0.5, unit_vector_at_dim(i)))
        .collect();
    let fix = build_fixture(&memories);

    // Cue: exactly matches dimension-2.
    let cue_vec = unit_vector_at_dim(2);
    fix.mock.install("cue", cue_vec);

    let plan = plan_recall(&base_request("cue", 3), &PlannerContext::default()).unwrap();
    let result = execute_recall(unwrap_recall(plan), &fix.ctx).await.unwrap();

    assert!(!result.hits.is_empty());
    // The top hit must be the dim-2 vector.
    assert_eq!(result.hits[0].memory_id, fix.memory_ids[2]);
    // Top hit score = cosine(cue, cue) = 1.0 ish.
    assert!(result.hits[0].score > 0.99);
    // Subsequent hits should have lower or equal score (orthogonal vectors → 0).
    for i in 1..result.hits.len() {
        assert!(result.hits[i - 1].score >= result.hits[i].score);
    }
}

#[tokio::test]
async fn recall_respects_top_k_limit() {
    let memories: Vec<_> = (0..10)
        .map(|i| (MemoryKind::Episodic, 0.5, unit_vector_at_dim(i)))
        .collect();
    let fix = build_fixture(&memories);
    fix.mock.install("cue", unit_vector_at_dim(0));

    let plan = plan_recall(&base_request("cue", 3), &PlannerContext::default()).unwrap();
    let result = execute_recall(unwrap_recall(plan), &fix.ctx).await.unwrap();
    assert!(result.hits.len() <= 3);
}

#[tokio::test]
async fn recall_filters_by_confidence_threshold() {
    let memories: Vec<_> = (0..5)
        .map(|i| (MemoryKind::Episodic, 0.5, unit_vector_at_dim(i)))
        .collect();
    let fix = build_fixture(&memories);
    fix.mock.install("cue", unit_vector_at_dim(0));

    // Threshold 0.99 will keep only the dim-0 match (the rest are orthogonal).
    let mut req = base_request("cue", 5);
    req.confidence_threshold = 0.99;

    let plan = plan_recall(&req, &PlannerContext::default()).unwrap();
    let result = execute_recall(unwrap_recall(plan), &fix.ctx).await.unwrap();
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].memory_id, fix.memory_ids[0]);
}

#[tokio::test]
async fn recall_kind_filter_drops_non_matching() {
    // Mix of kinds; query with kind_filter=[Semantic] but all rows are Episodic.
    let memories: Vec<_> = (0..3)
        .map(|i| (MemoryKind::Episodic, 0.5, unit_vector_at_dim(i)))
        .collect();
    let fix = build_fixture(&memories);
    fix.mock.install("cue", unit_vector_at_dim(0));

    let mut req = base_request("cue", 5);
    req.kind_filter = Some(vec![MemoryKindWire::Semantic]);

    let plan = plan_recall(&req, &PlannerContext::default()).unwrap();
    let result = execute_recall(unwrap_recall(plan), &fix.ctx).await.unwrap();
    assert!(result.hits.is_empty(), "no rows match Semantic");
}

#[tokio::test]
async fn recall_hit_carries_metadata() {
    let memories = vec![
        (MemoryKind::Episodic, 0.75, unit_vector_at_dim(0)),
        (MemoryKind::Semantic, 0.25, unit_vector_at_dim(1)),
    ];
    let fix = build_fixture(&memories);
    fix.mock.install("cue", unit_vector_at_dim(0));

    let plan = plan_recall(&base_request("cue", 2), &PlannerContext::default()).unwrap();
    let result = execute_recall(unwrap_recall(plan), &fix.ctx).await.unwrap();

    let first = &result.hits[0];
    assert_eq!(first.kind, MemoryKind::Episodic);
    assert_eq!(first.context_id, ContextId(42));
    assert!((first.salience - 0.75).abs() < 1e-6);
    assert!(first.created_at_unix_nanos >= 1_000_000);
}

#[tokio::test]
async fn empty_index_returns_no_hits() {
    let fix = build_fixture(&[]);
    fix.mock.install("cue", unit_vector_with_dim_one(1.0));

    let plan = plan_recall(&base_request("cue", 5), &PlannerContext::default()).unwrap();
    let result = execute_recall(unwrap_recall(plan), &fix.ctx).await.unwrap();
    assert!(result.hits.is_empty());
}

#[test]
fn planner_inspection_pinned_for_filtered_recall() {
    // Pure planner check: inspect the structure of a filtered plan
    // without running the executor.
    let mut req = base_request("hi", 10);
    req.kind_filter = Some(vec![MemoryKindWire::Episodic]);
    req.salience_floor = 0.5;
    let plan = plan_recall_inner(&req, &PlannerContext::default()).unwrap();
    assert_eq!(plan.shards.len(), 1);
    assert_eq!(plan.shards[0].filter_apply.rules.len(), 2);
    assert!(plan.estimated_cost_ms > 0.0);
}

// ---------------------------------------------------------------------------
// BGE-gated end-to-end test.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recall_with_real_embedder_end_to_end() {
    let Ok(model_dir) = std::env::var("BRAIN_EMBED_MODEL_DIR") else {
        eprintln!("skipping: set BRAIN_EMBED_MODEL_DIR to run");
        return;
    };
    let model_dir = std::path::PathBuf::from(model_dir);
    let handle = brain_embed::ModelHandle::load(&brain_embed::EmbedderConfig::new(model_dir))
        .expect("model loads");
    let dispatcher = Arc::new(brain_embed::CpuDispatcher::new(handle)) as Arc<dyn Dispatcher>;

    // Embed three distinct texts; insert into the index.
    let texts = ["the cat sat", "quantum physics", "the cat sat on the mat"];
    let vectors: Vec<[f32; VECTOR_DIM]> =
        texts.iter().map(|t| dispatcher.embed(t).unwrap()).collect();

    // Build a fixture with these vectors.
    let memories: Vec<_> = vectors
        .iter()
        .map(|v| (MemoryKind::Episodic, 0.5, *v))
        .collect();
    let fix_tempdir = tempfile::tempdir().unwrap();
    let db_path = fix_tempdir.path().join("metadata.redb");
    let mut metadata = MetadataDb::open(&db_path).unwrap();
    let (shared, mut writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let agent = AgentId(Uuid::nil());

    {
        let wtxn = metadata.write_txn().unwrap();
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        for (i, (kind, salience, vector)) in memories.iter().enumerate() {
            let mid = MemoryId::from_be_bytes({
                let mut b = [0u8; 16];
                b[0..8].copy_from_slice(&((i as u64) + 1).to_be_bytes());
                b
            });
            let meta = MemoryMetadata::new_active(
                mid,
                agent,
                ContextId(42),
                (i + 1) as u64,
                1,
                *kind,
                [0x11; 16],
                *salience,
                16,
                1_000_000,
            );
            table.insert(mid.to_be_bytes(), meta).unwrap();
            writer.insert(mid, vector).unwrap();
        }
        drop(table);
        wtxn.commit().unwrap();
    }

    let metadata: SharedMetadataDb = Arc::new(parking_lot::Mutex::new(metadata));
    let ctx = ExecutorContext::new(
        dispatcher,
        shared,
        metadata,
        Arc::new(NoopWriter) as Arc<dyn WriterHandle>,
    );
    // Cue is texts[0] — should rank itself top.
    let plan = plan_recall(&base_request(texts[0], 3), &PlannerContext::default()).unwrap();
    let result = execute_recall(unwrap_recall(plan), &ctx).await.unwrap();
    assert!(!result.hits.is_empty());
    // The first hit's vector should be the closest to texts[0], which is texts[0] itself.
    // Since we used MemoryId derived from index order, hit 0 should be memory_id #1.
    assert_eq!(
        result.hits[0].memory_id,
        MemoryId::from_be_bytes({
            let mut b = [0u8; 16];
            b[0..8].copy_from_slice(&1u64.to_be_bytes());
            b
        })
    );
    // The second-best hit should be texts[2] (same cat-mat sentence variant).
    assert!(result.hits.len() >= 2);
}
