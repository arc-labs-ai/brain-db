//! Regressions for the two PLAN / REASON defects seen on the `mirror`
//! session replay:
//!
//! 1. PLAN over a `FollowedBy` session chain must return the chain when
//!    it fits `max_steps` (and must not return a path longer than
//!    `max_steps`).
//! 2. REASON on a `ByText` observation must not treat every ANN
//!    neighbour as support: a newer "the blocker is fixed" memory is
//!    contradicting evidence, the aggregate confidence is not pinned at
//!    1.0, and the observation is not the only inference — claims are
//!    drawn outward from the typed graph.
//!
//! Fixture style mirrors `tests/vsa_executors.rs`: an in-process
//! MetadataDb + HNSW + deterministic embedder, driven through the
//! production planner (`plan_*_inner`) and executor entry points.

use std::collections::HashMap;
use std::sync::Arc;

use brain_core::{
    EdgeKind, EdgeKindRef, Entity, EntityId, EntityType, EvidenceEntry, EvidenceRef, ExtractorId,
    MemoryId, MemoryKind, NodeRef, PredicateId, SessionId, SpaceId, Statement, StatementId,
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
use brain_planner::{
    execute_path, execute_reason, execute_reason_stream, plan_path_inner, plan_reason_inner,
    ExecutorContext, PlanStatus, PlannerContext, SharedMetadataDb, WriterHandle,
};
use brain_protocol::envelope::request::{
    ObservationInput, PlanBudget, PlanRequest, PlanState, ReasonRequest,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

fn test_scope() -> RowScope {
    RowScope::new(brain_core::NamespaceId::SYSTEM, SpaceId::default())
}

/// Every query embeds onto axis 0; memory vectors are inserted into the
/// index directly (see [`sim_vector`]), so each memory's ANN cosine to
/// any query is exactly the value the test picks.
struct AxisZeroDispatcher;

impl Dispatcher for AxisZeroDispatcher {
    fn embed(&self, _text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0_f32; VECTOR_DIM];
        v[0] = 1.0;
        Ok(v)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0xCD; 16]
    }
}

/// Unit vector whose cosine to axis 0 is `sim`; `axis` (≥ 1) keeps the
/// vectors distinct from each other.
fn sim_vector(sim: f32, axis: usize) -> [f32; VECTOR_DIM] {
    let mut v = [0.0_f32; VECTOR_DIM];
    v[0] = sim;
    v[axis] = (1.0 - sim * sim).max(0.0).sqrt();
    v
}

struct NopWriter;
impl WriterHandle for NopWriter {
    fn reserve_memory_id<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<MemoryId, brain_planner::WriterError>> + 'a>,
    > {
        Box::pin(async move {
            Err(brain_planner::WriterError::Internal(
                "writes not exercised".into(),
            ))
        })
    }
}

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

/// `texts[i]` is memory `i`'s text; memories are created in index order
/// (so a higher index is a newer record). `sims[i]` is memory `i`'s ANN
/// cosine to every query (`None` → not indexed).
fn build_fixture(
    texts: &[&str],
    sims: &[Option<f32>],
    edges: &[(usize, EdgeKind, usize)],
) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let metadata = MetadataDb::open(tempdir.path().join("metadata.redb")).unwrap();
    let space = SpaceId(Uuid::nil());
    let mut ids = Vec::with_capacity(texts.len());

    let wtxn = metadata.write_txn().unwrap();
    {
        let mut mem_table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        let mut text_table = wtxn.open_table(TEXTS_TABLE).unwrap();
        for (i, text) in texts.iter().enumerate() {
            let id = make_id((i as u64) + 1);
            ids.push(id);
            let meta = MemoryMetadata::new_active(
                id,
                brain_core::NamespaceId::SYSTEM,
                space,
                SessionId(7001),
                (i + 1) as u64,
                1,
                MemoryKind::Episodic,
                [0x11; 16],
                0.5,
                text.len() as u32,
                1_000_000 + i as u64,
            );
            mem_table.insert(id.to_be_bytes(), meta).unwrap();
            text_table
                .insert(id.to_be_bytes(), text.as_bytes())
                .unwrap();
        }
        let mut edge_table = wtxn.open_table(EDGES_TABLE).unwrap();
        let mut rev_table = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        for (idx, (src, kind, tgt)) in edges.iter().enumerate() {
            let data = EdgeData::new(
                0.9,
                brain_metadata::tables::edge::origin::AUTO_DERIVED,
                brain_metadata::tables::edge::derived_by::TEMPORAL_WORKER,
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

    let (shared, mut hnsw_writer) = {
        let idx = brain_index::HnswIndex::new(IndexParams::default_v1()).unwrap();
        brain_index::SharedHnsw::from_index(idx)
    };
    for (i, sim) in sims.iter().enumerate() {
        if let Some(sim) = sim {
            hnsw_writer
                .insert(ids[i], &sim_vector(*sim, i + 1))
                .unwrap();
        }
    }
    let metadata: SharedMetadataDb = Arc::new(metadata);
    let ctx = ExecutorContext::new(
        Arc::new(AxisZeroDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        Arc::new(NopWriter) as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx,
        ids,
        _tempdir: tempdir,
    }
}

fn make_entity(metadata: &MetadataDb, name: &str) -> EntityId {
    let id = EntityId::new();
    let e = Entity::new_active(
        id,
        EntityType::PERSON_ID,
        name.to_string(),
        normalize_name(name),
        1_700_000_000_000_000_000,
    );
    let wtxn = metadata.write_txn().unwrap();
    entity_put(&wtxn, test_scope(), SessionId::DEFAULT, &e).unwrap();
    wtxn.commit().unwrap();
    id
}

fn make_predicate(metadata: &MetadataDb, name: &str) -> PredicateId {
    let wtxn = metadata.write_txn().unwrap();
    let id = predicate_intern(
        &wtxn,
        "test",
        name,
        Some(StatementKind::Fact),
        1,
        1,
        "",
        false,
        1_700_000_000_000_000_000,
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

/// `(subject, predicate, object)` Fact evidenced by `memory`. Entities
/// are interned by name through `entities` (canonical names are unique
/// per type), so two statements naming `linear-sandbox` share one entity.
fn attach_statement(
    metadata: &MetadataDb,
    entities: &mut HashMap<String, EntityId>,
    memory: MemoryId,
    subject: &str,
    predicate: &str,
    object: &str,
) -> StatementId {
    let mut entity = |name: &str| -> EntityId {
        *entities
            .entry(name.to_string())
            .or_insert_with(|| make_entity(metadata, name))
    };
    let subject = entity(subject);
    let object = entity(object);
    let predicate = make_predicate(metadata, predicate);
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
    s.evidence = EvidenceRef::inline_from_slice(&[EvidenceEntry::from_parts(
        memory,
        0.9,
        1_700_000_000_000_000_000,
        ExtractorId::from(0),
    )]);
    let wtxn = metadata.write_txn().unwrap();
    let id = statement_create(
        &wtxn,
        test_scope(),
        SessionId::DEFAULT,
        &s,
        1_700_000_000_000_000_001,
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

// ---------------------------------------------------------------------------
// 1. PLAN over a FollowedBy session chain.
// ---------------------------------------------------------------------------

/// Ten memories chained 0 → 1 → … → 9 by `FollowedBy`, like the
/// TemporalEdgeWorker writes for one session.
fn session_chain() -> Fixture {
    let texts: Vec<String> = (0..10).map(|i| format!("session turn {i}")).collect();
    let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let edges: Vec<(usize, EdgeKind, usize)> =
        (0..9).map(|i| (i, EdgeKind::FollowedBy, i + 1)).collect();
    build_fixture(&text_refs, &[None; 10], &edges)
}

fn plan_request(start: MemoryId, goal: MemoryId, max_steps: u32) -> PlanRequest {
    // Budget mirrors the gateway defaults (5 s wall, 32 branches).
    PlanRequest {
        start: PlanState::ByMemoryId(start.into()),
        goal: PlanState::ByMemoryId(goal.into()),
        budget: PlanBudget {
            max_steps,
            max_wall_time_ms: 5000,
            max_branches_explored: 32,
        },
        strategy_hint: None,
        session_filter: None,
        request_id: None,
        txn_id: None,
        act_as: None,
        trace: false,
    }
}

#[tokio::test]
async fn plan_follows_followed_by_chain_within_max_steps() {
    let fix = session_chain();
    // Memory 0 → memory 8: eight FollowedBy hops, max_steps = 8.
    let plan = plan_path_inner(
        &plan_request(fix.ids[0], fix.ids[8], 8),
        &PlannerContext::default(),
    )
    .unwrap();
    let res = execute_path(plan, &fix.ctx, false).await.unwrap();

    assert_eq!(res.status, PlanStatus::GoalReached);
    assert_eq!(res.paths.len(), 1, "one chain, one path");
    let path = &res.paths[0];
    // Start first, goal last, every hop in chain order.
    assert_eq!(path.nodes, fix.ids[0..=8].to_vec());
    assert_eq!(path.edges, vec![EdgeKind::FollowedBy; 8]);
    assert_eq!(
        path.node_text.first().map(String::as_str),
        Some("session turn 0")
    );
    assert_eq!(
        path.node_text.last().map(String::as_str),
        Some("session turn 8")
    );
}

#[tokio::test]
async fn plan_never_returns_a_path_longer_than_max_steps() {
    let fix = session_chain();
    // The only connection is 8 hops long; 7 steps cannot reach it. Each
    // BFS side is capped at max_depth individually, so before the
    // combined-depth check this returned the 8-hop chain anyway.
    let plan = plan_path_inner(
        &plan_request(fix.ids[0], fix.ids[8], 7),
        &PlannerContext::default(),
    )
    .unwrap();
    let res = execute_path(plan, &fix.ctx, false).await.unwrap();
    assert!(
        res.paths.iter().all(|p| p.edges.len() <= 7),
        "paths: {:?}",
        res.paths.iter().map(|p| p.edges.len()).collect::<Vec<_>>()
    );
    assert!(res.paths.is_empty());
    assert_ne!(res.status, PlanStatus::GoalReached);
}

// ---------------------------------------------------------------------------
// 2. REASON on a ByText observation with a newer refutation.
// ---------------------------------------------------------------------------

const OBSERVATION: &str = "linear-sandbox is blocked by the OAuth login flow";

/// 0 = the true (older) support, 1 = on-topic context that says nothing
/// about the blocked state, 2 = the newer refutation, 3 = off-topic.
fn blocker_fixture() -> Fixture {
    build_fixture(
        &[
            "Status update: linear-sandbox is still building; it's blocked by the OAuth login \
             flow, which the clone can't reproduce yet.",
            "Actually, switch linear-sandbox over to Playwright instead of Gymnasium; the \
             browser-native actions matter more than we thought.",
            "Good news: the OAuth blocker is fixed and linear-sandbox is now ready.",
            "The billing team holds a retro every second Friday.",
        ],
        &[Some(0.8), Some(0.7), Some(0.8), Some(0.3)],
        &[],
    )
}

fn reason_request(max_inferences: u32) -> ReasonRequest {
    ReasonRequest {
        observation: ObservationInput::ByText(OBSERVATION.into()),
        depth: 3,
        confidence_threshold: 0.0,
        session_filter: None,
        max_inferences,
        budget_wall_time_ms: 5000,
        request_id: None,
        txn_id: None,
        trace: false,
        act_as: None,
    }
}

#[tokio::test]
async fn reason_routes_newer_refutation_to_contradicting() {
    let fix = blocker_fixture();
    let plan = plan_reason_inner(&reason_request(5), &PlannerContext::default()).unwrap();
    let res = execute_reason(plan, &fix.ctx, false).await.unwrap();

    let supporting: Vec<MemoryId> = res.supporting.iter().map(|e| e.memory_id).collect();
    let contradicting: Vec<MemoryId> = res.contradicting.iter().map(|e| e.memory_id).collect();
    assert_eq!(
        supporting,
        vec![fix.ids[0]],
        "only the blocked report supports"
    );
    assert_eq!(
        contradicting,
        vec![fix.ids[2]],
        "the newer 'blocker is fixed' memory contradicts"
    );

    // The older support is stale against the newer refutation (0.8 × 0.5)
    // while the refutation keeps its full weight.
    assert!(
        (res.supporting[0].score - 0.4).abs() < 1e-3,
        "{:?}",
        res.supporting
    );
    assert!(
        (res.contradicting[0].score - 0.8).abs() < 1e-3,
        "{:?}",
        res.contradicting
    );

    // balance = (0.4 - 0.8) / 1.2, scaled by the strongest item (0.8).
    assert!(res.confidence < 0.0, "confidence = {}", res.confidence);
    assert!(
        (res.confidence - (-0.4 / 1.2 * 0.8)).abs() < 1e-3,
        "{}",
        res.confidence
    );
    assert_eq!(res.claim.as_deref(), Some(OBSERVATION));
}

#[tokio::test]
async fn reason_uncontested_text_support_is_not_certain() {
    // Same observation, refutation removed: the support now stands, but a
    // 0.8-cosine neighbour is not proof — confidence is < 1.0.
    let fix = build_fixture(
        &[
            "Status update: linear-sandbox is still building; it's blocked by the OAuth login \
             flow, which the clone can't reproduce yet.",
        ],
        &[Some(0.8)],
        &[],
    );
    let plan = plan_reason_inner(&reason_request(5), &PlannerContext::default()).unwrap();
    let res = execute_reason(plan, &fix.ctx, false).await.unwrap();
    assert_eq!(res.supporting.len(), 1);
    assert!(res.contradicting.is_empty());
    assert!((res.confidence - 0.8).abs() < 1e-3, "{}", res.confidence);
}

#[tokio::test]
async fn reason_draws_claims_outward_instead_of_echoing_the_observation() {
    let fix = blocker_fixture();
    let mut entities = HashMap::new();
    // The extracted graph behind the two stance-bearing memories.
    attach_statement(
        &fix.ctx.metadata,
        &mut entities,
        fix.ids[0],
        "linear-sandbox",
        "blocked_by",
        "OAuth login flow",
    );
    attach_statement(
        &fix.ctx.metadata,
        &mut entities,
        fix.ids[2],
        "OAuth blocker",
        "fixed",
        "linear-sandbox",
    );

    let plan = plan_reason_inner(&reason_request(5), &PlannerContext::default()).unwrap();
    let stream = execute_reason_stream(plan, &fix.ctx, false).await.unwrap();

    assert!(
        stream.steps.len() >= 2,
        "the observation must not be the only inference: {:?}",
        stream.steps
    );
    // Step 0 is the verdict on the observation, and not a certain one.
    assert_eq!(stream.steps[0].claim.as_deref(), Some(OBSERVATION));
    assert!(stream.steps[0].confidence < 1.0);

    // Every later step is a derived claim distinct from the observation.
    for (i, step) in stream.steps.iter().enumerate().skip(1) {
        assert_eq!(step.step_index as usize, i);
        let claim = step.claim.as_deref().expect("derived steps carry a claim");
        assert_ne!(claim, OBSERVATION);
        assert!(
            claim != "linear-sandbox blocked by OAuth login flow",
            "restating the observation's own statement is not an inference"
        );
    }

    let fixed = stream
        .steps
        .iter()
        .skip(1)
        .find(|s| s.claim.as_deref() == Some("OAuth blocker fixed linear-sandbox"))
        .expect("the resolution statement is drawn as a claim");
    let sup: Vec<MemoryId> = fixed.supporting.iter().map(|e| e.memory_id).collect();
    let con: Vec<MemoryId> = fixed.contradicting.iter().map(|e| e.memory_id).collect();
    assert_eq!(sup, vec![fix.ids[2]]);
    // The older "still blocked" report disagrees with the fix claim.
    assert_eq!(con, vec![fix.ids[0]]);
    // Newer than its only contradiction → 0.9 × 0.8.
    assert!(
        (fixed.confidence - 0.72).abs() < 1e-3,
        "{}",
        fixed.confidence
    );
}

#[tokio::test]
async fn reason_max_inferences_one_emits_only_the_verdict() {
    let fix = blocker_fixture();
    let mut entities = HashMap::new();
    attach_statement(
        &fix.ctx.metadata,
        &mut entities,
        fix.ids[2],
        "OAuth blocker",
        "fixed",
        "linear-sandbox",
    );
    let plan = plan_reason_inner(&reason_request(1), &PlannerContext::default()).unwrap();
    let stream = execute_reason_stream(plan, &fix.ctx, false).await.unwrap();
    assert_eq!(stream.steps.len(), 1);
}
