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

use brain_core::{EdgeKind, MemoryId, MemoryKind, SessionId, SpaceId};
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
    session_id: u64,
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
            brain_core::NamespaceId::SYSTEM,
            SpaceId(Uuid::nil()),
            SessionId(session_id),
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
    // Consolidation resolves candidate vectors from the redb artifact
    // store. Give every seeded memory the same unit vector so memories in
    // one context are cosine-1.0 and cluster together (session bucketing,
    // not the vector, keeps different sessions apart).
    brain_ops::memory_artifact::merge_memory_artifact(&wtxn, id.to_be_bytes(), |b| {
        b.vector = unit_vec(0).to_vec();
    })
    .unwrap();
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
fn cross_session_memories_do_not_cluster() {
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

// ===========================================================================
// Similarity clustering in the cycle (4).
// ===========================================================================

/// Unit vector with all energy in `dim`. Two such vectors have cosine
/// 1.0 when they share a dim and 0.0 when they don't.
fn unit_vec(dim: usize) -> [f32; VECTOR_DIM] {
    let mut v = [0.0f32; VECTOR_DIM];
    v[dim] = 1.0;
    v
}

/// Seed an Episodic memory as id `MemoryId::pack(0, slot, version)`.
/// When `vector` is `Some`, its write-time embedding is written into the
/// redb artifact bundle — the LIVE by-id vector store consolidation
/// resolves from (the arena is recovery-only and empty in-run). `None`
/// leaves the artifact absent, so `get_artifact_vector` returns `None`
/// and the worker drops the candidate fail-soft.
fn seed_packed(
    metadata: &SharedMetadataDb,
    slot: u64,
    version: u32,
    session_id: u64,
    created_at_unix_nanos: u64,
    vector: Option<[f32; VECTOR_DIM]>,
) -> MemoryId {
    let id = MemoryId::pack(0, slot, version);
    let wtxn = metadata.write_txn().unwrap();
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        let meta = MemoryMetadata::new_active(
            id,
            brain_core::NamespaceId::SYSTEM,
            SpaceId(Uuid::nil()),
            SessionId(session_id),
            slot,
            version,
            MemoryKind::Episodic,
            [0; 16],
            0.5,
            16,
            created_at_unix_nanos,
        );
        table.insert(id.to_be_bytes(), meta).unwrap();
    }
    if let Some(v) = vector {
        brain_ops::memory_artifact::merge_memory_artifact(&wtxn, id.to_be_bytes(), |b| {
            b.vector = v.to_vec();
        })
        .unwrap();
    }
    wtxn.commit().unwrap();
    id
}

/// Seed an Episodic memory in an explicit `(namespace, space)`. Used by
/// the tenant-isolation test: two spaces that share a `SessionId` and an
/// identical vector must still land in separate clusters.
#[allow(clippy::too_many_arguments)]
fn seed_scoped(
    metadata: &SharedMetadataDb,
    slot: u64,
    namespace: brain_core::NamespaceId,
    space: SpaceId,
    session_id: u64,
    created_at_unix_nanos: u64,
    vector: [f32; VECTOR_DIM],
) -> MemoryId {
    let id = MemoryId::pack(0, slot, 1);
    let wtxn = metadata.write_txn().unwrap();
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        let meta = MemoryMetadata::new_active(
            id,
            namespace,
            space,
            SessionId(session_id),
            slot,
            1,
            MemoryKind::Episodic,
            [0; 16],
            0.5,
            16,
            created_at_unix_nanos,
        );
        table.insert(id.to_be_bytes(), meta).unwrap();
    }
    brain_ops::memory_artifact::merge_memory_artifact(&wtxn, id.to_be_bytes(), |b| {
        b.vector = vector.to_vec();
    })
    .unwrap();
    wtxn.commit().unwrap();
    id
}

fn count_consolidated(metadata: &SharedMetadataDb) -> usize {
    let rtxn = metadata.read_txn().unwrap();
    let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
    table
        .iter()
        .unwrap()
        .filter(|e| {
            let (_, v) = e.as_ref().unwrap();
            v.value().kind().ok() == Some(MemoryKind::Consolidated)
        })
        .count()
}

#[test]
fn two_dissimilar_subgroups_produce_two_consolidated() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // Sub-group A: slots 1..=5 all point at dim 0.
        for slot in 1..=5 {
            seed_packed(&fix.metadata, slot, 1, 1, now, Some(unit_vec(0)));
        }
        // Sub-group B: slots 6..=10 all point at dim 100 (orthogonal
        // to A → cross-group cosine 0.0 < 0.6).
        for slot in 6..=10 {
            seed_packed(&fix.metadata, slot, 1, 1, now, Some(unit_vec(100)));
        }
        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer))
            .with_min_cluster_size(5)
            .with_similarity_threshold(0.6);
        let processed = run_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(processed, 2, "two dissimilar sub-groups → two clusters");
        assert_eq!(count_consolidated(&fix.metadata), 2);
    });
}

#[test]
fn singleton_below_threshold_is_not_consolidated() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // A cluster of 5 aligned memories (dim 0)…
        let mut cluster_ids = Vec::new();
        for slot in 1..=5 {
            cluster_ids.push(seed_packed(
                &fix.metadata,
                slot,
                1,
                1,
                now,
                Some(unit_vec(0)),
            ));
        }
        // …plus one orthogonal singleton (dim 200) that clears no pair.
        let singleton = seed_packed(&fix.metadata, 6, 1, 1, now, Some(unit_vec(200)));

        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer))
            .with_min_cluster_size(5)
            .with_similarity_threshold(0.6);
        let processed = run_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(processed, 1, "only the size-5 cluster consolidates");
        // The 5 aligned sources are stamped; the singleton is not.
        for id in cluster_ids {
            assert!(read_meta(&fix.metadata, id)
                .unwrap()
                .consolidated_at_unix_nanos
                .is_some());
        }
        assert!(
            read_meta(&fix.metadata, singleton)
                .unwrap()
                .consolidated_at_unix_nanos
                .is_none(),
            "the below-threshold singleton must not be consolidated"
        );
    });
}

#[test]
fn missing_artifact_vector_candidates_are_dropped() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // Six aligned memories (dim 0) — but slot 6 has no artifact
        // vector, so `get_artifact_vector` returns None and that
        // candidate is dropped. The remaining 5 still form one cluster.
        let mut ids = Vec::new();
        for slot in 1..=6 {
            let v = if slot != 6 { Some(unit_vec(0)) } else { None };
            ids.push(seed_packed(&fix.metadata, slot, 1, 1, now, v));
        }
        let missing = ids[5];

        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer))
            .with_min_cluster_size(5)
            .with_similarity_threshold(0.6);
        let processed = run_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(processed, 1, "the 5 resolvable candidates cluster");
        assert_eq!(count_consolidated(&fix.metadata), 1);
        // The dropped candidate is neither clustered nor stamped.
        assert!(
            read_meta(&fix.metadata, missing)
                .unwrap()
                .consolidated_at_unix_nanos
                .is_none(),
            "a candidate with no stored vector must be dropped, not mis-clustered"
        );
    });
}

#[test]
fn two_spaces_sharing_session_are_not_clustered_and_summary_lands_in_source_space() {
    glommio_run(|| async {
        let fix = build_fixture();
        let now = now_unix_nanos();
        // Two distinct spaces under the same namespace, BOTH using
        // SessionId(1), and every memory carries the identical vector
        // (unit_vec(0)) — so cosine is 1.0 across the whole set. Under the
        // old session-only bucketing these ten would collapse into one
        // cross-tenant cluster written to the NIL space. With full
        // (namespace, space, session) bucketing they must form two
        // separate clusters, each summarised into its own source space.
        let ns = brain_core::NamespaceId::from(7);
        let space_a = SpaceId::derive_from_string("tenant7", "space-a");
        let space_b = SpaceId::derive_from_string("tenant7", "space-b");
        assert_ne!(space_a, space_b);
        for slot in 1..=5 {
            seed_scoped(&fix.metadata, slot, ns, space_a, 1, now, unit_vec(0));
        }
        for slot in 6..=10 {
            seed_scoped(&fix.metadata, slot, ns, space_b, 1, now, unit_vec(0));
        }

        let worker = ConsolidationWorker::new(Arc::new(EchoSummarizer))
            .with_min_cluster_size(5)
            .with_similarity_threshold(0.6);
        let processed = run_cycle(&worker, fix.ctx.clone()).await.unwrap();
        assert_eq!(
            processed, 2,
            "two spaces sharing a session must NOT be clustered together"
        );

        // Collect the consolidated rows and check where they landed.
        let rtxn = fix.metadata.read_txn().unwrap();
        let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let mut consolidated_spaces = std::collections::HashSet::new();
        for entry in table.iter().unwrap() {
            let (_, v) = entry.unwrap();
            let m = v.value();
            if m.kind().ok() == Some(MemoryKind::Consolidated) {
                assert_ne!(
                    m.space_id(),
                    SpaceId::NIL,
                    "consolidated memory must not land in the NIL space"
                );
                assert_eq!(
                    m.namespace(),
                    ns,
                    "consolidated memory must inherit the source namespace"
                );
                consolidated_spaces.insert(m.space_id());
            }
        }
        assert_eq!(
            consolidated_spaces,
            std::collections::HashSet::from([space_a, space_b]),
            "each summary must land in its own source space, never mixed or NIL"
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
