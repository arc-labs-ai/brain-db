#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Consolidation worker integration tests.
//!
//! Guards episodic-memory consolidation: similar episodics in the same
//! context cluster (cosine over a transitive chain), each cluster above
//! the min size collapses into one consolidated memory with `DerivedFrom`
//! edges back to its sources, sources get stamped `consolidated_at`, and
//! re-running is idempotent. Pins exclusions (cross-context, non-episodic,
//! tombstoned, already-consolidated) and deterministic request-id derivation.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::list_memory_edges_from;
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{
    cluster_by_similarity, cosine, deterministic_request_id, ClusterCandidate, ConsolidationWorker,
    DisabledSummarizer, Summarizer, SummarizerError, Worker, WorkerContext,
};
use redb::ReadableTable;
use uuid::Uuid;

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
    ctx: Arc<OpsContext>,
    metadata: SharedMetadataDb,
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
    Fixture {
        ctx: Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)),
        metadata,
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

#[allow(clippy::too_many_arguments)]
fn seed_memory(
    metadata: &SharedMetadataDb,
    slot: u64,
    context_id: u64,
    kind: MemoryKind,
    salience: f32,
    created_at_unix_nanos: u64,
    consolidated_at_unix_nanos: Option<u64>,
    tombstoned_at_unix_nanos: Option<u64>,
) -> MemoryId {
    let id = make_id(slot);
    let wtxn = metadata.write_txn().unwrap();
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        let mut meta = MemoryMetadata::new_active(
            id,
            AgentId(Uuid::nil()),
            ContextId(context_id),
            slot,
            1,
            kind,
            [0; 16],
            salience,
            16,
            created_at_unix_nanos,
        );
        meta.consolidated_at_unix_nanos = consolidated_at_unix_nanos;
        meta.tombstoned_at_unix_nanos = tombstoned_at_unix_nanos;
        table.insert(id.to_be_bytes(), meta).unwrap();
    }
    wtxn.commit().unwrap();
    id
}

fn read_meta(metadata: &SharedMetadataDb, id: MemoryId) -> Option<MemoryMetadata> {
    let rtxn = metadata.read_txn().unwrap();
    let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
    table.get(id.to_be_bytes()).unwrap().map(|a| a.value())
}

async fn run_cycle(
    worker: &ConsolidationWorker,
    ops: Arc<OpsContext>,
) -> Result<usize, brain_workers::WorkerError> {
    let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wctx = WorkerContext {
        ops,
        shutdown: shutdown_flag.clone(),
    };
    worker.run_cycle(&wctx).await
}

/// Deterministic stub summarizer: returns "[{join("|")}]".
struct EchoSummarizer;
impl Summarizer for EchoSummarizer {
    fn summarize<'a>(
        &'a self,
        memories: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = Result<String, SummarizerError>> + 'a>> {
        Box::pin(async move { Ok(format!("[{}]", memories.join("|"))) })
    }
}

// ===========================================================================
// Summarizer (2).
// ===========================================================================

#[test]
fn disabled_summarizer_returns_disabled_error() {
    glommio_run(|| async {
        let s = DisabledSummarizer;
        let r = s.summarize(&["a", "b"]).await;
        assert!(matches!(r, Err(SummarizerError::Disabled)));
    });
}

#[test]
fn echo_summarizer_returns_joined_input() {
    glommio_run(|| async {
        let s = EchoSummarizer;
        let r = s.summarize(&["one", "two"]).await.unwrap();
        assert_eq!(r, "[one|two]");
    });
}

// ===========================================================================
// Clustering pure-fn (5).
// ===========================================================================

fn make_candidate(slot: u64, vector_seed: f32) -> ClusterCandidate {
    let mut v = [0.0f32; VECTOR_DIM];
    // Concentrate energy in one slot so cosine is roughly the
    // direction overlap between two vectors with the same seed.
    let dim = (slot as usize) % VECTOR_DIM;
    v[dim] = vector_seed;
    ClusterCandidate {
        memory_id: make_id(slot),
        vector: v,
        created_at_unix_nanos: 0,
    }
}

#[test]
fn cosine_basic_identities() {
    let mut v = [0.0f32; VECTOR_DIM];
    v[0] = 1.0;
    assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    let zero = [0.0f32; VECTOR_DIM];
    assert_eq!(cosine(&zero, &v), 0.0);
}

#[test]
fn two_aligned_memories_form_one_cluster() {
    let c1 = make_candidate(10, 1.0);
    let c2 = make_candidate(10, 0.9); // same dim → cosine = 1.0
    let clusters = cluster_by_similarity(&[c1.clone(), c2.clone()], 0.6, 2);
    assert_eq!(clusters.len(), 1);
    assert_eq!(clusters[0].len(), 2);
}

#[test]
fn orthogonal_memories_do_not_cluster() {
    let mut v_a = [0.0f32; VECTOR_DIM];
    v_a[0] = 1.0;
    let mut v_b = [0.0f32; VECTOR_DIM];
    v_b[1] = 1.0;
    let a = ClusterCandidate {
        memory_id: make_id(1),
        vector: v_a,
        created_at_unix_nanos: 0,
    };
    let b = ClusterCandidate {
        memory_id: make_id(2),
        vector: v_b,
        created_at_unix_nanos: 0,
    };
    let clusters = cluster_by_similarity(&[a, b], 0.6, 2);
    assert!(clusters.is_empty(), "orthogonal vectors must not cluster");
}

#[test]
fn transitive_chain_merges_into_one_cluster() {
    // A and B share dim 0, B and C share dim 0 → all in same component
    let a = make_candidate(10, 1.0);
    let b = make_candidate(10, 0.8);
    let c = make_candidate(10, 0.6);
    let clusters = cluster_by_similarity(&[a, b, c], 0.6, 3);
    assert_eq!(clusters.len(), 1);
    assert_eq!(clusters[0].len(), 3);
}

#[test]
fn cluster_below_min_size_is_dropped() {
    let c1 = make_candidate(10, 1.0);
    let c2 = make_candidate(10, 0.9);
    let clusters = cluster_by_similarity(&[c1, c2], 0.6, 5);
    assert!(clusters.is_empty());
}

#[test]
fn isolated_memory_is_dropped() {
    let mut v_a = [0.0f32; VECTOR_DIM];
    v_a[0] = 1.0;
    let mut v_b = [0.0f32; VECTOR_DIM];
    v_b[1] = 1.0;
    let mut v_c = [0.0f32; VECTOR_DIM];
    v_c[2] = 1.0;
    let cs = [
        ClusterCandidate {
            memory_id: make_id(1),
            vector: v_a,
            created_at_unix_nanos: 0,
        },
        ClusterCandidate {
            memory_id: make_id(2),
            vector: v_b,
            created_at_unix_nanos: 0,
        },
        ClusterCandidate {
            memory_id: make_id(3),
            vector: v_c,
            created_at_unix_nanos: 0,
        },
    ];
    let clusters = cluster_by_similarity(&cs, 0.6, 2);
    assert!(clusters.is_empty(), "no pair clears the threshold");
}

// ===========================================================================
// Idempotent request_id (2).
// ===========================================================================

#[test]
fn same_source_set_produces_same_request_id() {
    let s1 = vec![make_id(1), make_id(2), make_id(3)];
    let s2 = vec![make_id(3), make_id(1), make_id(2)]; // order shuffled
    assert_eq!(deterministic_request_id(&s1), deterministic_request_id(&s2));
}

#[test]
fn different_source_sets_produce_different_request_ids() {
    let r1 = deterministic_request_id(&[make_id(1), make_id(2)]);
    let r2 = deterministic_request_id(&[make_id(1), make_id(3)]);
    assert_ne!(r1, r2);
}

// ===========================================================================
// Cycle behaviour (7).
// ===========================================================================

#[test]
fn disabled_summarizer_produces_no_consolidations() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        for slot in 1..=10 {
            seed_memory(
                &fix.metadata,
                slot,
                1,
                MemoryKind::Episodic,
                0.5,
                now,
                None,
                None,
            );
        }
        let worker =
            ConsolidationWorker::new(Arc::new(DisabledSummarizer)).with_min_cluster_size(5);
        let processed = run_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
    });
}

#[test]
fn cluster_of_five_episodics_produces_one_consolidated() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        for slot in 1..=5 {
            seed_memory(
                &fix.metadata,
                slot,
                1,
                MemoryKind::Episodic,
                0.5,
                now,
                None,
                None,
            );
        }
        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer)).with_min_cluster_size(5);
        let processed = run_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(processed, 1, "one Consolidated memory must be created");

        // Walk MEMORIES_TABLE to find the Consolidated one.
        let rtxn = fix.metadata.read_txn().unwrap();
        let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let consolidated: Vec<_> = table
            .iter()
            .unwrap()
            .filter_map(|e| {
                let (_, v) = e.unwrap();
                let m = v.value();
                if m.kind().ok()? == MemoryKind::Consolidated {
                    Some(m)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(consolidated.len(), 1);
    });
}

#[test]
fn consolidated_has_derived_from_edges_to_each_source() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        let mut source_ids = Vec::new();
        for slot in 1..=5 {
            source_ids.push(seed_memory(
                &fix.metadata,
                slot,
                1,
                MemoryKind::Episodic,
                0.5,
                now,
                None,
                None,
            ));
        }
        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer)).with_min_cluster_size(5);
        run_cycle(&worker, fix.ctx.clone()).await.unwrap();

        // Find the Consolidated id.
        let consolidated_id = {
            let rtxn = fix.metadata.read_txn().unwrap();
            let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
            let mut id = None;
            for entry in table.iter().unwrap() {
                let (_, v) = entry.unwrap();
                let m = v.value();
                if m.kind().ok() == Some(MemoryKind::Consolidated) {
                    id = Some(m.memory_id());
                    break;
                }
            }
            id.expect("Consolidated must exist")
        };

        // Walk outgoing DerivedFrom edges anchored at the consolidated id.
        let rtxn = fix.metadata.read_txn().unwrap();
        let rows =
            list_memory_edges_from(&rtxn, consolidated_id, Some(EdgeKind::DerivedFrom)).unwrap();
        let found_targets: std::collections::HashSet<MemoryId> =
            rows.into_iter().map(|(_, tgt, _)| tgt).collect();
        assert_eq!(found_targets.len(), 5);
        for id in &source_ids {
            assert!(found_targets.contains(id));
        }
    });
}

#[test]
fn sources_are_stamped_with_consolidated_at() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        let mut ids = Vec::new();
        for slot in 1..=5 {
            ids.push(seed_memory(
                &fix.metadata,
                slot,
                1,
                MemoryKind::Episodic,
                0.5,
                now,
                None,
                None,
            ));
        }
        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer)).with_min_cluster_size(5);
        run_cycle(&worker, fix.ctx).await.unwrap();
        for id in ids {
            let m = read_meta(&fix.metadata, id).unwrap();
            assert!(
                m.consolidated_at_unix_nanos.is_some(),
                "source {id:?} must be stamped"
            );
        }
    });
}

#[test]
fn already_consolidated_sources_are_skipped() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // Seed 5 — but one is already stamped; the worker should skip
        // the cluster (any source already-consolidated → skip).
        for slot in 1..=5 {
            let stamp = if slot == 1 {
                Some(now - 1_000_000)
            } else {
                None
            };
            seed_memory(
                &fix.metadata,
                slot,
                1,
                MemoryKind::Episodic,
                0.5,
                now,
                stamp,
                None,
            );
        }
        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer)).with_min_cluster_size(5);
        // The already-stamped row is filtered out *before* the
        // any-already-consolidated check (it doesn't appear as a
        // candidate at all). The remaining 4 are below min_cluster_size,
        // so nothing happens. Either way: 0 consolidations.
        let processed = run_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
    });
}

#[test]
fn cross_context_memories_do_not_cluster() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // 5 in context 1, 5 in context 2.
        for slot in 1..=5 {
            seed_memory(
                &fix.metadata,
                slot,
                1,
                MemoryKind::Episodic,
                0.5,
                now,
                None,
                None,
            );
        }
        for slot in 6..=10 {
            seed_memory(
                &fix.metadata,
                slot,
                2,
                MemoryKind::Episodic,
                0.5,
                now,
                None,
                None,
            );
        }
        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer)).with_min_cluster_size(5);
        let processed = run_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(
            processed, 2,
            "exactly one Consolidated per context, both eligible"
        );
    });
}

#[test]
fn non_episodic_memories_are_excluded() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // 4 Episodics + 1 Semantic → Episodics alone fall below
        // min_cluster_size=5 → no consolidation.
        for slot in 1..=4 {
            seed_memory(
                &fix.metadata,
                slot,
                1,
                MemoryKind::Episodic,
                0.5,
                now,
                None,
                None,
            );
        }
        seed_memory(
            &fix.metadata,
            5,
            1,
            MemoryKind::Semantic,
            0.5,
            now,
            None,
            None,
        );
        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer)).with_min_cluster_size(5);
        let processed = run_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
    });
}

#[test]
fn tombstoned_memories_are_excluded() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // 4 active + 1 tombstoned → below min_cluster_size.
        for slot in 1..=4 {
            seed_memory(
                &fix.metadata,
                slot,
                1,
                MemoryKind::Episodic,
                0.5,
                now,
                None,
                None,
            );
        }
        seed_memory(
            &fix.metadata,
            5,
            1,
            MemoryKind::Episodic,
            0.5,
            now,
            None,
            Some(now), // tombstoned
        );
        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer)).with_min_cluster_size(5);
        let processed = run_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(processed, 0);
    });
}

#[test]
fn second_cycle_is_idempotent() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        for slot in 1..=5 {
            seed_memory(
                &fix.metadata,
                slot,
                1,
                MemoryKind::Episodic,
                0.5,
                now,
                None,
                None,
            );
        }
        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer)).with_min_cluster_size(5);
        let first = run_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(first, 1);
        let second = run_cycle(&worker, fix.ctx).await.unwrap();
        assert_eq!(
            second, 0,
            "sources are stamped; second cycle finds no candidates"
        );
    });
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("worker-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}
