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

use brain_core::{
    SpaceId, SessionId, EdgeKind, EdgeKindRef, Entity, EntityId, EntityType, EvidenceEntry,
    EvidenceRef, ExtractorId, MemoryId, MemoryKind, NodeRef, PredicateId, Statement, StatementId,
    StatementKind, StatementObject, SubjectRef,
};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::IndexParams;
use brain_metadata::entity::ops::{entity_put, normalize_name};
use brain_metadata::schema::predicate::predicate_intern;
use brain_metadata::statement::statement_create;
use brain_metadata::tables::edge::{link, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_metadata::{MetadataDb, RowScope};
use brain_planner::executor::analogical::{ANALOGICAL_FIT_MAX, ANALOGICAL_FIT_MIN};
use brain_planner::plan::path::{EvidenceResponseStep, ScoringStep, TraversalStep};
use brain_planner::plan::reason::AggregationStep as ReasonAggregation;
use brain_planner::{
    execute_path, execute_reason, ExecutorContext, InferenceKind, PathPlan, PlanStatus,
    PlanTraceDirection, ReasonPlan, SharedMetadataDb, WriterHandle,
};
use brain_protocol::envelope::request::{ObservationInput, PlanBudget, PlanState, PlanStrategy};
use uuid::Uuid;

/// Test-fixture scope — matches `ExecutorContext::new`'s defaults
/// (`NamespaceId::SYSTEM` + `SpaceId::default()` == `SpaceId::NIL`),
/// which is also what `build_fixture` stamps onto every memory row.
fn test_scope() -> RowScope {
    RowScope::new(brain_core::NamespaceId::SYSTEM, SpaceId::default())
}

/// Insert a `Person` entity directly via `entity_put` (same minimal
/// path `brain-metadata`'s own statement-crud tests use) and return
/// its id.
fn make_test_entity(metadata: &MetadataDb, name: &str) -> EntityId {
    let id = EntityId::new();
    let normalized = normalize_name(name);
    let e = Entity::new_active(
        id,
        EntityType::PERSON_ID,
        name.to_string(),
        normalized,
        1_700_000_000_000_000_000,
    );
    let wtxn = metadata.write_txn().unwrap();
    entity_put(&wtxn, test_scope(), &e).unwrap();
    wtxn.commit().unwrap();
    id
}

/// Intern a Fact/Entity-object predicate under the `test` namespace.
fn make_test_predicate(metadata: &MetadataDb, name: &str) -> PredicateId {
    let wtxn = metadata.write_txn().unwrap();
    let id = predicate_intern(
        &wtxn,
        "test",
        name,
        Some(StatementKind::Fact),
        /* object: Entity */ 1,
        1,
        "",
        false,
        1_700_000_000_000_000_000,
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

/// Create a `(subject, predicate, object)` Fact statement whose sole
/// evidence entry is `evidence_memory` — the same evidence shape
/// `executor::analogical::resolve_statement_triple` reads back via
/// `STATEMENTS_BY_EVIDENCE_TABLE`.
fn make_test_statement(
    metadata: &MetadataDb,
    subject: EntityId,
    predicate: PredicateId,
    object: EntityId,
    evidence_memory: MemoryId,
) -> StatementId {
    let mut s = Statement::new_root(
        StatementId::new(),
        StatementKind::Fact,
        SubjectRef::Entity(subject),
        predicate,
        StatementObject::Entity(object),
        0.9,
        EvidenceRef::default(),
        ExtractorId::from(0),
        1_700_000_000_000_000_000,
        1,
    );
    let entry = EvidenceEntry::from_parts(
        evidence_memory,
        0.9,
        1_700_000_000_000_000_000,
        ExtractorId::from(0),
    );
    s.evidence = EvidenceRef::inline_from_slice(&[entry]);
    let wtxn = metadata.write_txn().unwrap();
    let id = statement_create(&wtxn, test_scope(), &s, 1_700_000_000_000_000_001).unwrap();
    wtxn.commit().unwrap();
    id
}

/// Build a Fact statement `(subject_name, predicate_name, object_name)`
/// backed by fresh entities, evidenced by `memory`. Convenience
/// wrapper around `make_test_entity` / `make_test_predicate` /
/// `make_test_statement` for the analogical-fit tests below.
fn attach_statement(
    metadata: &MetadataDb,
    memory: MemoryId,
    subject_name: &str,
    predicate_name: &str,
    object_name: &str,
) {
    let subject = make_test_entity(metadata, subject_name);
    let object = make_test_entity(metadata, object_name);
    let predicate = make_test_predicate(metadata, predicate_name);
    make_test_statement(metadata, subject, predicate, object, memory);
}

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
        Ok(self.table.get(text).copied().unwrap_or([0.0; VECTOR_DIM]))
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
    /// The HNSW single-writer handle, shared (via `Arc<RwLock<..>>`)
    /// with `ctx.index`. Kept so trace tests can tombstone a memory
    /// (`index_writer.mark_tombstoned(id)`) and have it immediately
    /// visible via `ctx.index.is_tombstoned`.
    index_writer: brain_index::Writer,
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
    let metadata = MetadataDb::open(&db_path).unwrap();

    let space = SpaceId(Uuid::nil());
    let mut ids = Vec::with_capacity(n_memories);

    let wtxn = metadata.write_txn().unwrap();
    {
        let mut mem_table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        for i in 0..n_memories {
            let id = make_id((i as u64) + 1);
            ids.push(id);
            let meta = MemoryMetadata::new_active(
                id,
                brain_core::NamespaceId::SYSTEM,
                space,
                SessionId(7),
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

    let (shared, hnsw_writer) = {
        let idx = brain_index::HnswIndex::new(IndexParams::default_v1()).unwrap();
        brain_index::SharedHnsw::from_index(idx)
    };
    let metadata: SharedMetadataDb = Arc::new(metadata);
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
        index_writer: hnsw_writer,
        _tempdir: tempdir,
    }
}

fn path_plan(start: MemoryId, goal: MemoryId, max_branches: u32, max_depth: usize) -> PathPlan {
    path_plan_capped(start, goal, max_branches, max_depth, 4)
}

fn path_plan_capped(
    start: MemoryId,
    goal: MemoryId,
    max_branches: u32,
    max_depth: usize,
    max_paths: usize,
) -> PathPlan {
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
            max_paths,
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
    let res = execute_path(plan, &fix.ctx, false).await.unwrap();

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
    let res = execute_path(plan, &fix.ctx, false).await.unwrap();
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
    let edges = vec![(0, EdgeKind::Supports, 1), (0, EdgeKind::Supports, 2)];

    let fix = build_fixture(3, &texts, &edges, dispatcher);
    let plan = reason_plan(fix.ids[0], 8);
    let res = execute_reason(plan, &fix.ctx, false).await.unwrap();

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

// ---------------------------------------------------------------------------
// PLAN — full per-stage trace (`trace = true`).
//
// Mirrors RECALL's `trace_detail` / REASON's `trace` contract: `false`
// (the default) leaves `PathResult.trace` empty and changes nothing else;
// `true` additionally exposes the BFS's full visited-map contents, every
// meeting point found (including ones the `max_paths` cap silently drops
// today), and the per-neighbour goal-alignment scores the heuristic
// computes but normally only uses to reorder expansion.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_trace_disabled_by_default_leaves_trace_none() {
    let dispatcher = TableDispatcher::new(&[]);
    let edges = vec![(0, EdgeKind::Caused, 1)];
    let fix = build_fixture(2, &[], &edges, dispatcher);

    let plan = path_plan(fix.ids[0], fix.ids[1], 8, 2);
    let res = execute_path(plan, &fix.ctx, false).await.unwrap();

    assert_eq!(res.status, PlanStatus::GoalReached);
    assert!(
        res.trace.is_none(),
        "trace = false must leave PathResult.trace empty",
    );
}

#[tokio::test]
async fn plan_trace_captures_every_explored_node_both_directions() {
    let dispatcher = TableDispatcher::new(&[]);

    // 0=start, 1=goal, 2 and 3 are two disjoint one-hop-each-side
    // bridges between them. No text rows anywhere → no goal centroid →
    // the forward frontier is never scored.
    let edges = vec![
        (0, EdgeKind::Caused, 2),
        (0, EdgeKind::Caused, 3),
        (2, EdgeKind::Caused, 1),
        (3, EdgeKind::Caused, 1),
    ];
    let fix = build_fixture(4, &[], &edges, dispatcher);

    let plan = path_plan(fix.ids[0], fix.ids[1], 32, 3);
    let res = execute_path(plan, &fix.ctx, true).await.unwrap();

    assert_eq!(res.status, PlanStatus::GoalReached);
    let trace = res
        .trace
        .expect("trace = true must populate PathResult.trace");

    assert!(
        trace.explored.iter().all(|n| n.alignment_score.is_none()),
        "no goal centroid available; no node should carry an alignment score",
    );

    let forward: Vec<_> = trace
        .explored
        .iter()
        .filter(|n| n.direction == PlanTraceDirection::Forward)
        .collect();
    let backward: Vec<_> = trace
        .explored
        .iter()
        .filter(|n| n.direction == PlanTraceDirection::Backward)
        .collect();

    // fwd = {start, mid2, mid3}; bwd = {goal, mid2, mid3} — both
    // intermediates are touched from both directions before the BFS
    // stops at the first meeting point.
    assert_eq!(forward.len(), 3, "forward explored: {forward:?}");
    assert_eq!(backward.len(), 3, "backward explored: {backward:?}");

    let start_entry = forward
        .iter()
        .find(|n| n.memory_id == fix.ids[0])
        .expect("start must be in the forward visited map");
    assert_eq!(start_entry.depth, 0);
    assert!(start_entry.parent_edge.is_none(), "seed has no parent edge");

    let goal_entry = backward
        .iter()
        .find(|n| n.memory_id == fix.ids[1])
        .expect("goal must be in the backward visited map");
    assert_eq!(goal_entry.depth, 0);
    assert!(goal_entry.parent_edge.is_none(), "seed has no parent edge");

    for mid_id in [fix.ids[2], fix.ids[3]] {
        let f = forward
            .iter()
            .find(|n| n.memory_id == mid_id)
            .unwrap_or_else(|| panic!("{mid_id:?} must be forward-explored"));
        assert_eq!(f.depth, 1);
        assert_eq!(f.parent_edge, Some(EdgeKind::Caused));

        let b = backward
            .iter()
            .find(|n| n.memory_id == mid_id)
            .unwrap_or_else(|| panic!("{mid_id:?} must be backward-explored"));
        assert_eq!(b.depth, 1);
        assert_eq!(b.parent_edge, Some(EdgeKind::Caused));
    }
}

#[tokio::test]
async fn plan_trace_meeting_points_flag_cap_inclusion_without_changing_paths() {
    let dispatcher = TableDispatcher::new(&[]);

    // 0=start, 1=goal, 2..5 are four parallel one-hop-each-side bridges
    // between them — the BFS finds four meeting points, but
    // `traversal.max_paths` is capped at two.
    let edges = vec![
        (0, EdgeKind::Caused, 2),
        (0, EdgeKind::Caused, 3),
        (0, EdgeKind::Caused, 4),
        (0, EdgeKind::Caused, 5),
        (2, EdgeKind::Caused, 1),
        (3, EdgeKind::Caused, 1),
        (4, EdgeKind::Caused, 1),
        (5, EdgeKind::Caused, 1),
    ];
    let fix = build_fixture(6, &[], &edges, dispatcher);

    let plan = path_plan_capped(fix.ids[0], fix.ids[1], 32, 3, 2);
    let res = execute_path(plan, &fix.ctx, true).await.unwrap();

    assert_eq!(res.status, PlanStatus::GoalReached);
    // Tracing must not change the real result set: the cap still
    // applies exactly as it does when `trace = false`.
    assert_eq!(
        res.paths.len(),
        2,
        "max_paths = 2 must still cap the returned paths"
    );

    let trace = res
        .trace
        .expect("trace = true must populate PathResult.trace");
    assert_eq!(
        trace.meeting_points.len(),
        4,
        "all four meeting points must be visible in full-detail trace",
    );

    let included: Vec<_> = trace
        .meeting_points
        .iter()
        .filter(|m| m.included_in_result)
        .collect();
    let dropped: Vec<_> = trace
        .meeting_points
        .iter()
        .filter(|m| !m.included_in_result)
        .collect();
    assert_eq!(included.len(), 2, "cap = 2 → exactly two included");
    assert_eq!(dropped.len(), 2, "the other two must be flagged dropped");

    // The included ids are exactly the meeting points whose path made
    // it into `res.paths` (each reconstructed path's second node is
    // its meeting point: `[start, meet, goal]`).
    let result_meeting_ids: std::collections::HashSet<MemoryId> =
        res.paths.iter().map(|p| p.nodes[1]).collect();
    for m in &included {
        assert!(
            result_meeting_ids.contains(&m.memory_id),
            "included meeting point {:?} must appear in a returned path",
            m.memory_id,
        );
    }
    for m in &dropped {
        assert!(
            !result_meeting_ids.contains(&m.memory_id),
            "dropped meeting point {:?} must not appear in any returned path",
            m.memory_id,
        );
    }
}

#[tokio::test]
async fn plan_trace_captures_forward_alignment_scores_when_centroid_present() {
    let on_topic = axis_vector(0);
    let off_topic = axis_vector(4);

    let dispatcher = TableDispatcher::new(&[
        ("start node", axis_vector(1)),
        ("goal node text", on_topic),
        ("on-topic mid", on_topic),
        ("off-topic mid", off_topic),
    ]);

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
    let res = execute_path(plan, &fix.ctx, true).await.unwrap();

    assert_eq!(res.status, PlanStatus::GoalReached);
    let trace = res
        .trace
        .expect("trace = true must populate PathResult.trace");

    let on_topic_entry = trace
        .explored
        .iter()
        .find(|n| n.memory_id == fix.ids[2] && n.direction == PlanTraceDirection::Forward)
        .expect("on-topic mid must be forward-explored");
    let off_topic_entry = trace
        .explored
        .iter()
        .find(|n| n.memory_id == fix.ids[3] && n.direction == PlanTraceDirection::Forward)
        .expect("off-topic mid must be forward-explored");

    let on_score = on_topic_entry
        .alignment_score
        .expect("forward neighbour must be scored under a goal centroid");
    let off_score = off_topic_entry
        .alignment_score
        .expect("forward neighbour must be scored under a goal centroid");

    assert!((on_score - 1.0).abs() < 1e-5, "on_score={on_score}");
    assert!(off_score.abs() < 1e-5, "off_score={off_score}");

    // Backward-direction nodes are never scored — the heuristic only
    // orders forward expansion.
    assert!(
        trace
            .explored
            .iter()
            .filter(|n| n.direction == PlanTraceDirection::Backward)
            .all(|n| n.alignment_score.is_none()),
        "backward nodes must never carry an alignment score",
    );
}

// ---------------------------------------------------------------------------
// REASON — full per-stage trace (`trace = true`).
//
// Same two-mode contract as PLAN/RECALL: `false` (the default) leaves
// `ReasonResult.trace` empty and changes nothing else; `true` additionally
// exposes the full `resolve_base` candidate set, every edge `walk_outward`
// considered (not just survivors) tagged with why it was dropped
// (edge-kind / tombstone / already-visited), the confidence-floor / trim-
// cap drops, the un-collapsed score components per surviving evidence
// item, and why `build_base_centroid` did or didn't produce a centroid.
// ---------------------------------------------------------------------------

/// Graph shape shared by both trace tests:
///
/// - 0 = base (`ByMemoryId` seed).
/// - 1 = reached via `Supports` from 0 (survivor, depth 1) — and again
///   from 4 (already-visited drop).
/// - 2 = reached via `FollowedBy` from 0 — not in the supports edge-kind
///   set (edge-kind drop).
/// - 3 = reached via `Supports` from 0, then tombstoned before the walk
///   runs (tombstone drop).
/// - 4 = reached via `Supports` from 0 (survivor, depth 1); its own
///   `Supports` edge back to 1 is the already-visited drop.
fn reason_trace_fixture() -> Fixture {
    let dispatcher = TableDispatcher::new(&[]);
    let texts = vec![(0, "base")];
    let edges = vec![
        (0, EdgeKind::Supports, 1),
        (0, EdgeKind::FollowedBy, 2),
        (0, EdgeKind::Supports, 3),
        (0, EdgeKind::Supports, 4),
        (4, EdgeKind::Supports, 1),
    ];
    build_fixture(5, &texts, &edges, dispatcher)
}

#[tokio::test]
async fn reason_trace_disabled_by_default_leaves_trace_none() {
    let mut fix = reason_trace_fixture();
    fix.index_writer
        .mark_tombstoned(fix.ids[3])
        .expect("mark_tombstoned must succeed in-test");

    let plan = reason_plan(fix.ids[0], 16);
    let res = execute_reason(plan, &fix.ctx, false).await.unwrap();

    assert!(
        res.trace.is_none(),
        "trace = false must leave ReasonResult.trace empty",
    );
    // The fast path must still produce the same real result: two
    // depth-1 survivors (1 and 4) plus the direct-similarity base item
    // (0) — tracing must be additive, never load-bearing.
    assert_eq!(res.supporting.len(), 3, "supporting: {:?}", res.supporting);
}

#[tokio::test]
async fn reason_trace_records_considered_dropped_edges_and_score_breakdown() {
    let mut fix = reason_trace_fixture();
    fix.index_writer
        .mark_tombstoned(fix.ids[3])
        .expect("mark_tombstoned must succeed in-test");

    let plan = reason_plan(fix.ids[0], 16);
    let res = execute_reason(plan, &fix.ctx, true).await.unwrap();

    let trace = res
        .trace
        .expect("trace = true must populate ReasonResult.trace");

    // Base resolution: the single ByMemoryId seed, with its text and
    // base_similarity = 1.0.
    assert_eq!(trace.base.candidates.len(), 1);
    assert_eq!(trace.base.candidates[0].memory_id, fix.ids[0]);
    assert_eq!(trace.base.candidates[0].score, 1.0);
    assert_eq!(trace.base.candidates[0].text.as_deref(), Some("base"));

    // Supports walk: node 0 considers 4 out-edges, node 4 (a kept
    // survivor) considers 1 more — 5 considered total, none double-
    // counted with what the direct base-similarity item adds.
    let walk = &trace.supports_walk;
    assert_eq!(
        walk.considered.len(),
        5,
        "considered: {:?}",
        walk.considered
    );
    assert_eq!(walk.dropped_by_edge_kind.len(), 1);
    assert_eq!(walk.dropped_by_edge_kind[0].memory_id, fix.ids[2]);
    assert_eq!(walk.dropped_by_tombstone.len(), 1);
    assert_eq!(walk.dropped_by_tombstone[0].memory_id, fix.ids[3]);
    assert_eq!(walk.dropped_by_visited.len(), 1);
    assert_eq!(walk.dropped_by_visited[0].memory_id, fix.ids[1]);
    assert_eq!(walk.dropped_by_visited[0].from_memory_id, fix.ids[4]);

    // considered = survivors + every drop bucket, no double-counting.
    assert_eq!(
        walk.considered.len(),
        res.supporting.iter().filter(|e| e.distance > 0).count()
            + walk.dropped_by_edge_kind.len()
            + walk.dropped_by_tombstone.len()
            + walk.dropped_by_visited.len(),
    );

    // Contradicts walk: same 4 raw edges from node 0 are all
    // edge-kind-dropped (none is `Contradicts`); nothing gets far
    // enough to be visited, so no other drop bucket fires.
    let cwalk = &trace.contradicts_walk;
    assert_eq!(cwalk.considered.len(), 4);
    assert_eq!(cwalk.dropped_by_edge_kind.len(), 4);
    assert!(cwalk.dropped_by_tombstone.is_empty());
    assert!(cwalk.dropped_by_visited.is_empty());

    // Nothing hit the confidence floor (0.0) or the trim cap
    // (max_supporting/contradicting = 16) in this small fixture.
    assert!(trace.supports_trim.dropped_by_confidence.is_empty());
    assert!(trace.supports_trim.dropped_by_trim_cap.is_empty());
    assert!(trace.contradicts_trim.dropped_by_confidence.is_empty());
    assert!(trace.contradicts_trim.dropped_by_trim_cap.is_empty());

    // Score breakdown: one entry per surviving evidence item (the two
    // depth-1 walk survivors plus the direct-similarity base item).
    // The base is a singleton (`ByMemoryId`, one seed) so
    // `build_base_centroid` never engages — alignment is always 1.0
    // and the product must equal the evidence item's own score.
    assert_eq!(trace.scoring.len(), 3, "scoring: {:?}", trace.scoring);
    for entry in &trace.scoring {
        let product = entry.base_similarity * entry.decay * entry.weight_product * entry.alignment;
        assert!(
            (product - entry.final_score).abs() < 1e-6,
            "component product must equal final_score: {entry:?}",
        );
        assert!((entry.alignment - 1.0).abs() < 1e-6, "{entry:?}");
        let matching = res
            .supporting
            .iter()
            .find(|e| e.memory_id == entry.memory_id)
            .expect("every scoring entry must correspond to a surviving evidence item");
        assert!(
            (matching.score - entry.final_score).abs() < 1e-6,
            "scoring breakdown must match the evidence item's stored score",
        );
    }
    let base_entry = trace
        .scoring
        .iter()
        .find(|e| e.memory_id == fix.ids[0])
        .expect("direct-similarity base item must appear in the score breakdown");
    assert_eq!(base_entry.decay, 1.0);
    assert_eq!(base_entry.final_score, 1.0);

    // Centroid: singleton base → skipped, with a machine-readable reason.
    assert!(!trace.centroid.computed);
    assert_eq!(
        trace.centroid.skipped_reason.as_deref(),
        Some("singleton_base"),
    );
}

// ---------------------------------------------------------------------------
// REASON — VSA analogical-inference nudge (`executor::analogical`).
//
// Bridges evidence memories to the typed-graph statements they're
// evidence for (via `STATEMENTS_BY_EVIDENCE_TABLE`, the same reverse
// lookup RECALL's `include_graph` enrichment uses) and computes a
// bounded re-rank nudge from the resulting triples. Three properties
// under test, matching the design's "never a hard filter" contract:
//
//   (a) the nudge fires and stays within [ANALOGICAL_FIT_MIN, MAX]
//       when the observation and an evidence item share a predicate;
//   (b) it's exactly neutral (1.0) when no triple resolves on either
//       side — the pre-existing, statement-free fixtures above must
//       see zero behavioural change from this feature;
//   (c) it can never resurrect an item the confidence floor already
//       dropped, even when that item's triple would earn the maximal
//       boost if it were included.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_analogical_fit_bounded_nudge_favors_matching_predicate() {
    let dispatcher = TableDispatcher::new(&[]);
    // 0 = base/observation ("Alice works_at Acme"). 1 and 2 are both
    // depth-1 Supports survivors with identical pre-nudge scores
    // (same decay, weight_product = 1, alignment = 1 — singleton base
    // never engages the centroid damper) — any score gap between them
    // after this test's assertions is attributable to the analogical
    // nudge alone, not to a pre-existing walk asymmetry.
    let edges = vec![(0, EdgeKind::Supports, 1), (0, EdgeKind::Supports, 2)];
    let fix = build_fixture(3, &[], &edges, dispatcher);

    attach_statement(&fix.ctx.metadata, fix.ids[0], "Alice", "works_at", "Acme");
    // Same predicate as the observation — this is the "X works_at Acme"
    // / "Y works_at ?" analogy shape.
    attach_statement(&fix.ctx.metadata, fix.ids[1], "Bob", "works_at", "Stripe");
    // Different predicate — no structural axis to compare against, so
    // this item must stay exactly neutral.
    attach_statement(&fix.ctx.metadata, fix.ids[2], "Carol", "lives_in", "Berlin");

    let plan = reason_plan(fix.ids[0], 16);
    let res = execute_reason(plan, &fix.ctx, true).await.unwrap();
    let trace = res
        .trace
        .expect("trace = true must populate ReasonResult.trace");

    let matching = trace
        .scoring
        .iter()
        .find(|e| e.memory_id == fix.ids[1])
        .expect("same-predicate evidence item must appear in the score breakdown");
    let differing = trace
        .scoring
        .iter()
        .find(|e| e.memory_id == fix.ids[2])
        .expect("different-predicate evidence item must appear in the score breakdown");

    // (a) bounded, non-neutral nudge for the matching-predicate item.
    assert!(
        matching.analogical_fit > 1.0 && matching.analogical_fit <= ANALOGICAL_FIT_MAX,
        "matching.analogical_fit={} must be in (1.0, {ANALOGICAL_FIT_MAX}]",
        matching.analogical_fit,
    );
    assert!(matching.analogical_fit >= ANALOGICAL_FIT_MIN);

    // (b) exactly neutral for the differing-predicate item — no boost,
    // no penalty, matching the "never a hard filter" contract.
    assert_eq!(differing.analogical_fit, 1.0);

    // The nudge is reflected in both the trace and the real result:
    // same pre-nudge score (0.5 = base_similarity 1.0 × decay 0.5 ×
    // weight_product 1.0 × alignment 1.0), but the matching item now
    // outranks the differing one.
    assert!((matching.base_similarity - differing.base_similarity).abs() < 1e-6);
    assert!((matching.decay - differing.decay).abs() < 1e-6);
    let matching_item = res
        .supporting
        .iter()
        .find(|e| e.memory_id == fix.ids[1])
        .expect("matching-predicate item must survive to the result");
    let differing_item = res
        .supporting
        .iter()
        .find(|e| e.memory_id == fix.ids[2])
        .expect("differing-predicate item must survive to the result");
    assert!(
        matching_item.score > differing_item.score,
        "matching={} differing={}",
        matching_item.score,
        differing_item.score,
    );
    assert!(
        (differing_item.score - 0.5).abs() < 1e-6,
        "unnudged score must stay exactly 0.5"
    );
}

#[tokio::test]
async fn reason_analogical_fit_neutral_when_no_statement_resolves() {
    // Reuses the pre-existing statement-free trace fixture: no
    // extractor ever ran on these memories, so no
    // STATEMENTS_BY_EVIDENCE_TABLE row exists for any of them. Every
    // scoring entry's analogical_fit must be exactly 1.0 — the
    // feature must be invisible on a corpus with no typed-graph
    // structure at all.
    let mut fix = reason_trace_fixture();
    fix.index_writer
        .mark_tombstoned(fix.ids[3])
        .expect("mark_tombstoned must succeed in-test");

    let plan = reason_plan(fix.ids[0], 16);
    let res = execute_reason(plan, &fix.ctx, true).await.unwrap();
    let trace = res
        .trace
        .expect("trace = true must populate ReasonResult.trace");

    assert!(!trace.scoring.is_empty());
    for entry in &trace.scoring {
        assert_eq!(
            entry.analogical_fit, 1.0,
            "no statement in this fixture must resolve; entry={entry:?}",
        );
    }
    assert_eq!(res.inference_kind, InferenceKind::EvidenceAccumulation);
}

#[tokio::test]
async fn reason_analogical_fit_never_resurrects_a_floor_dropped_item() {
    let dispatcher = TableDispatcher::new(&[]);
    // 0 = base ("Alice works_at Acme"). 0 --Supports--> 1 (depth 1,
    // score 0.5, survives). 1 --Supports--> 2 (depth 2, score 1/3,
    // would fail a 0.4 floor on its evidence-only score alone) — node
    // 2 also carries a same-predicate statement, so it would earn the
    // maximal analogical boost *if* it were ever scored. It must not
    // be resurrected by that boost: the floor runs strictly before the
    // analogical-fit pass.
    let edges = vec![(0, EdgeKind::Supports, 1), (1, EdgeKind::Supports, 2)];
    let fix = build_fixture(3, &[], &edges, dispatcher);

    attach_statement(&fix.ctx.metadata, fix.ids[0], "Alice", "works_at", "Acme");
    attach_statement(&fix.ctx.metadata, fix.ids[2], "Dave", "works_at", "Globex");

    let mut plan = reason_plan(fix.ids[0], 16);
    plan.confidence_threshold = 0.4;
    let res = execute_reason(plan, &fix.ctx, true).await.unwrap();

    assert!(
        !res.supporting.iter().any(|e| e.memory_id == fix.ids[2]),
        "depth-2 item must stay excluded despite its maximal-boost-eligible statement; \
         supporting={:?}",
        res.supporting
            .iter()
            .map(|e| (e.memory_id, e.score))
            .collect::<Vec<_>>(),
    );
    // The depth-1 survivor (evidence-only score 0.5 ≥ floor 0.4) is
    // unaffected by this test's floor and must still be present.
    assert!(res.supporting.iter().any(|e| e.memory_id == fix.ids[1]));

    let trace = res
        .trace
        .expect("trace = true must populate ReasonResult.trace");
    let dropped_ids: Vec<MemoryId> = trace
        .supports_trim
        .dropped_by_confidence
        .iter()
        .map(|(id, _)| *id)
        .collect();
    assert!(
        dropped_ids.contains(&fix.ids[2]),
        "the confidence-floor trace must record the drop: {dropped_ids:?}",
    );
}

#[tokio::test]
async fn reason_analogical_fit_retags_step_when_it_moves_confidence_materially() {
    let dispatcher = TableDispatcher::new(&[]);
    // 0 = base ("Alice works_at Acme"). 0 --Supports--> 1, same
    // predicate (gets boosted). 0 --Contradicts--> 3, no statement
    // (stays neutral) — with real contradicting mass present, the
    // aggregate confidence is no longer pinned at 1.0, so the nudge's
    // effect on it is observable.
    let edges = vec![(0, EdgeKind::Supports, 1), (0, EdgeKind::Contradicts, 3)];
    let fix = build_fixture(4, &[], &edges, dispatcher);

    attach_statement(&fix.ctx.metadata, fix.ids[0], "Alice", "works_at", "Acme");
    attach_statement(&fix.ctx.metadata, fix.ids[1], "Bob", "works_at", "Stripe");

    let plan = reason_plan(fix.ids[0], 16);
    let res = execute_reason(plan, &fix.ctx, false).await.unwrap();

    assert_eq!(res.inference_kind, InferenceKind::AnalogicalInference);
}
