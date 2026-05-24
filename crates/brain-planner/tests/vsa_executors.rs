//! End-to-end smoke tests for VSA wiring into the PLAN and REASON
//! executors.
//!
//! Each test stands up a small in-process metadata DB + HNSW index +
//! deterministic embedder and drives `execute_path` / `execute_reason`
//! through the production code paths.
//!
//! The unit tests in `vsa::semantic_centroid` exercise the algebra
//! itself; these tests verify the wiring (centroid is built from the
//! right texts, the BFS sort doesn't break the baseline, the topic-
//! alignment factor produces the expected multiplicative effect when
//! the base set is meaningful).

use std::collections::HashMap;
use std::sync::Arc;

use brain_core::{AgentId, ContextId, EdgeKind, EdgeKindRef, MemoryId, MemoryKind, NodeRef};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{link, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_metadata::MetadataDb;
use brain_planner::plan::path::{EvidenceResponseStep, ScoringStep, TraversalStep};
use brain_planner::plan::reason::AggregationStep as ReasonAggregation;
use brain_planner::{
    execute_path, execute_reason, ExecutorContext, PathPlan, PlanStatus, ReasonPlan,
    SharedMetadataDb, WriterHandle,
};
use brain_protocol::envelope::request::{ObservationInput, PlanBudget, PlanState, PlanStrategy};
use parking_lot::Mutex;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test embedder: maps a string to a fixed vector via a lookup table.
// Anything not in the table embeds to a zero vector (orthogonal to
// everything; centroid cosine = 0).
// ---------------------------------------------------------------------------

struct TableDispatcher {
    table: HashMap<String, [f32; VECTOR_DIM]>,
}

impl TableDispatcher {
    fn new(entries: &[(&str, [f32; VECTOR_DIM])]) -> Self {
        let mut table = HashMap::new();
        for (k, v) in entries {
            table.insert((*k).to_owned(), *v);
        }
        Self { table }
    }
}

impl Dispatcher for TableDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok(self
            .table
            .get(text)
            .copied()
            .unwrap_or([0.0; VECTOR_DIM]))
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0xAB; 16]
    }
}

/// Build a unit vector along one of the first eight axes — simple
/// enough that cosine math is hand-verifiable from the test fixture.
fn axis_vector(axis: usize) -> [f32; VECTOR_DIM] {
    let mut v = [0.0_f32; VECTOR_DIM];
    v[axis] = 1.0;
    v
}

// ---------------------------------------------------------------------------
// Minimal writer impl — none of these tests issue writes.
// ---------------------------------------------------------------------------

struct NopWriter;
impl WriterHandle for NopWriter {
    fn reserve_memory_id<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<MemoryId, brain_planner::WriterError>> + 'a>,
    > {
        Box::pin(async move {
            Err(brain_planner::WriterError::Internal(
                "writes not exercised in VSA tests".into(),
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Fixture: in-memory MetadataDb with memories, texts, and edges; empty
// HNSW (all endpoint resolution is ByMemoryId so the index only needs
// to satisfy `is_tombstoned` lookups).
// ---------------------------------------------------------------------------

fn make_id(i: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&i.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

struct Fixture {
    ctx: ExecutorContext,
    ids: Vec<MemoryId>,
    _tempdir: tempfile::TempDir,
}

fn build_fixture(
    n_memories: usize,
    texts: &[(usize, &str)],
    edges: &[(usize, EdgeKind, usize)],
    dispatcher: TableDispatcher,
) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let mut metadata = MetadataDb::open(&db_path).unwrap();

    let agent = AgentId(Uuid::nil());
    let mut ids = Vec::with_capacity(n_memories);

    let wtxn = metadata.write_txn().unwrap();
    {
        let mut mem_table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        for i in 0..n_memories {
            let id = make_id((i as u64) + 1);
            ids.push(id);
            let meta = MemoryMetadata::new_active(
                id,
                agent,
                ContextId(7),
                (i + 1) as u64,
                1,
                MemoryKind::Episodic,
                [0x11; 16],
                0.5,
                32,
                1_000_000 + i as u64,
            );
            mem_table.insert(id.to_be_bytes(), meta).unwrap();
        }

        let mut text_table = wtxn.open_table(TEXTS_TABLE).unwrap();
        for (i, t) in texts {
            text_table
                .insert(ids[*i].to_be_bytes(), t.as_bytes())
                .unwrap();
        }

        let mut edge_table = wtxn.open_table(EDGES_TABLE).unwrap();
        let mut rev_table = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        for (idx, (src, kind, tgt)) in edges.iter().enumerate() {
            let data = EdgeData::new(
                1.0,
                brain_metadata::tables::edge::origin::EXPLICIT,
                brain_metadata::tables::edge::derived_by::CLIENT,
                2_000_000 + idx as u64,
            );
            link(
                &mut edge_table,
                &mut rev_table,
                NodeRef::Memory(ids[*src]),
                EdgeKindRef::Builtin(*kind),
                NodeRef::Memory(ids[*tgt]),
                brain_metadata::tables::edge::zero_disambiguator(),
                &data,
            )
            .unwrap();
        }
    }
    wtxn.commit().unwrap();

    let (shared, _hnsw_writer) = {
        let codebook = brain_index::bootstrap_codebook();
        let idx = brain_index::HnswIndex::new(
            IndexParams::default_v1(),
            (*codebook).clone(),
        )
        .unwrap();
        brain_index::SharedHnsw::from_index(idx, brain_index::null_arena_reader())
    };
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(metadata));
    let writer = Arc::new(NopWriter) as Arc<dyn WriterHandle>;
    let ctx = ExecutorContext::new(
        Arc::new(dispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer,
    );

    Fixture {
        ctx,
        ids,
        _tempdir: tempdir,
    }
}

fn path_plan(start: MemoryId, goal: MemoryId, max_branches: u32, max_depth: usize) -> PathPlan {
    PathPlan {
        start: PlanState::ByMemoryId(start.into()),
        goal: PlanState::ByMemoryId(goal.into()),
        budget: PlanBudget {
            max_steps: max_depth as u32,
            max_wall_time_ms: 5000,
            max_branches_explored: max_branches,
        },
        strategy: PlanStrategy::Auto,
        starting_recall: None,
        goal_recall: None,
        traversal: TraversalStep {
            edge_kinds: vec![EdgeKind::Caused],
            max_depth,
            bidirectional: true,
            max_paths: 4,
        },
        scoring: ScoringStep::default(),
        response: EvidenceResponseStep {
            include_paths: true,
            include_text: false,
            include_metadata: false,
        },
        estimated_cost_ms: 0.0,
    }
}

fn reason_plan(observation_id: MemoryId, max_inferences: u32) -> ReasonPlan {
    ReasonPlan {
        observation: ObservationInput::ByMemoryId(observation_id.into()),
        depth: 2,
        confidence_threshold: 0.0,
        max_inferences,
        budget_wall_time_ms: 5000,
        embedding: None,
        base_recall: None,
        supports_traversal: TraversalStep {
            edge_kinds: brain_planner::default_supports_edge_kinds(),
            max_depth: 2,
            bidirectional: false,
            max_paths: 8,
        },
        contradicts_traversal: TraversalStep {
            edge_kinds: brain_planner::default_contradicts_edge_kinds(),
            max_depth: 2,
            bidirectional: false,
            max_paths: 8,
        },
        aggregation: ReasonAggregation {
            max_supporting: 16,
            max_contradicting: 16,
            include_aggregate_confidence: true,
        },
        response: EvidenceResponseStep {
            include_paths: false,
            include_text: false,
            include_metadata: false,
        },
        estimated_cost_ms: 0.0,
    }
}

// ---------------------------------------------------------------------------
// PLAN — goal-direction smoke test.
//
// The goal-direction heuristic changes the *order* in which forward-
// frontier neighbours are expanded. In a tiny graph where the BFS
// always finds the goal regardless of order, we can still assert two
// things:
//
// 1. Wiring is live: `execute_path` returns `GoalReached` even when
//    the goal-centroid lookup runs end-to-end (text → embed → sort).
// 2. The shortest correct path is still returned. The sort must not
//    re-route the BFS through a longer detour.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_goal_direction_wiring_finds_path_through_aligned_intermediate() {
    let on_topic = axis_vector(0);
    let off_topic = axis_vector(4);

    let dispatcher = TableDispatcher::new(&[
        ("start node", axis_vector(1)),
        ("goal node text", on_topic),
        ("on-topic mid", on_topic),
        ("off-topic mid", off_topic),
    ]);

    // Memories: 0=start, 1=goal, 2=on-topic intermediate, 3=off-topic
    // intermediate. Both intermediates lead to the goal — the
    // heuristic should preserve the on-topic path while not breaking
    // discovery of the off-topic one.
    let texts = vec![
        (0, "start node"),
        (1, "goal node text"),
        (2, "on-topic mid"),
        (3, "off-topic mid"),
    ];
    let edges = vec![
        (0, EdgeKind::Caused, 2),
        (0, EdgeKind::Caused, 3),
        (2, EdgeKind::Caused, 1),
        (3, EdgeKind::Caused, 1),
    ];

    let fix = build_fixture(4, &texts, &edges, dispatcher);

    let plan = path_plan(fix.ids[0], fix.ids[1], 32, 3);
    let res = execute_path(plan, &fix.ctx).await.unwrap();

    assert_eq!(res.status, PlanStatus::GoalReached);
    assert!(!res.paths.is_empty(), "at least one path must be found");
    // Both paths are length 2 (start → mid → goal) — the one via the
    // on-topic intermediate must appear and rank no worse than the
    // off-topic alternative.
    let has_on_topic = res
        .paths
        .iter()
        .any(|p| p.nodes == vec![fix.ids[0], fix.ids[2], fix.ids[1]]);
    assert!(
        has_on_topic,
        "on-topic path must be discoverable; paths={:?}",
        res.paths.iter().map(|p| &p.nodes).collect::<Vec<_>>(),
    );
}

#[tokio::test]
async fn plan_without_text_rows_skips_centroid_and_still_runs() {
    // No texts → goal centroid is None → the sort short-circuits and
    // BFS proceeds in natural order. End-to-end behaviour is identical
    // to the pre-VSA baseline.
    let dispatcher = TableDispatcher::new(&[]);

    let edges = vec![(0, EdgeKind::Caused, 1)];
    let fix = build_fixture(2, &[], &edges, dispatcher);

    let plan = path_plan(fix.ids[0], fix.ids[1], 8, 2);
    let res = execute_path(plan, &fix.ctx).await.unwrap();
    assert_eq!(res.status, PlanStatus::GoalReached);
    assert_eq!(res.paths.len(), 1);
    assert_eq!(res.paths[0].nodes, vec![fix.ids[0], fix.ids[1]]);
}

// ---------------------------------------------------------------------------
// REASON — topic-alignment damper.
//
// The damper is a [0, 1] multiplicative factor on the evidence score:
//
//   - factor = 1.0 (no damp) when the base set has only one member
//     (centroid is undefined for the comparison), or when the
//     observation is ByText (the cue already represents intent
//     direction through `base_similarity`).
//
//   - factor = (1 + cosine) / 2 when the base centroid exists; aligned
//     candidates land at ~1, orthogonal at ~0.5, opposite at ~0.
//
// The integration path that surfaces multiple base memories runs via
// the wire-level ANN seed (ByText with K > 1). That path uses
// `base_similarity` to weight evidence, which already encodes topic
// proximity — and `build_base_centroid` deliberately skips the
// damper in that case. The end-to-end shape we *can* exercise here
// is the singleton-skip contract; the damper math itself is
// independently covered by the `semantic_centroid` unit tests plus
// the algebraic check below.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_singleton_base_does_not_damp_either_evidence_candidate() {
    let aligned = axis_vector(0);
    let orthogonal = axis_vector(3);

    let dispatcher = TableDispatcher::new(&[
        ("base alpha", aligned),
        ("aligned evidence", aligned),
        ("orthogonal evidence", orthogonal),
    ]);

    let texts = vec![
        (0, "base alpha"),
        (1, "aligned evidence"),
        (2, "orthogonal evidence"),
    ];
    let edges = vec![
        (0, EdgeKind::Supports, 1),
        (0, EdgeKind::Supports, 2),
    ];

    let fix = build_fixture(3, &texts, &edges, dispatcher);
    let plan = reason_plan(fix.ids[0], 8);
    let res = execute_reason(plan, &fix.ctx).await.unwrap();

    let aligned_score = res
        .supporting
        .iter()
        .find(|e| e.memory_id == fix.ids[1])
        .expect("aligned evidence present")
        .score;
    let orthogonal_score = res
        .supporting
        .iter()
        .find(|e| e.memory_id == fix.ids[2])
        .expect("orthogonal evidence present")
        .score;

    assert!(
        (aligned_score - orthogonal_score).abs() < 1e-6,
        "singleton base must not engage the damper; \
         aligned={aligned_score} orthogonal={orthogonal_score}",
    );
}

#[tokio::test]
async fn reason_topic_alignment_factor_math_separates_aligned_from_orthogonal() {
    // The walk-outward damper applies (1 + cosine) / 2 — directly
    // exercising the public algebra. Builds a centroid from two
    // aligned base vectors, then computes the multiplicative factor
    // for an aligned vs orthogonal candidate. Aligned → 1.0;
    // orthogonal → 0.5; ratio = 2.0×.
    let aligned = axis_vector(0);
    let centroid =
        brain_planner::vsa::semantic_centroid::<VECTOR_DIM>(&[&aligned, &aligned]).unwrap();
    let orthogonal = axis_vector(3);

    let cos_aligned = brain_planner::vsa::cosine_to_centroid(&aligned, &centroid);
    let cos_orth = brain_planner::vsa::cosine_to_centroid(&orthogonal, &centroid);
    let factor_aligned = (1.0 + cos_aligned) / 2.0;
    let factor_orth = (1.0 + cos_orth) / 2.0;

    assert!(
        (factor_aligned - 1.0).abs() < 1e-5,
        "factor_aligned={factor_aligned}",
    );
    assert!(
        (factor_orth - 0.5).abs() < 1e-5,
        "factor_orth={factor_orth}",
    );
    assert!(
        factor_aligned / factor_orth > 1.9,
        "expected ~2× separation; got {}",
        factor_aligned / factor_orth,
    );
}
