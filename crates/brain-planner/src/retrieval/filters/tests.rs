//! Unit tests for the post-fusion filter chain.

use brain_core::{
    AgentId, Cardinality, ContextId, Entity, EntityId, EntityTypeId, ExtractorId, MemoryId,
    MemoryKind, PredicateId, RelationId, StatementId,
};
use brain_core::{
    EvidenceRef, Relation, Statement, StatementKind, StatementObject, StatementValue, SubjectRef,
    TombstoneReason,
};
use brain_index::RankedItemId;
use brain_metadata::entity::ops::entity_put;
use brain_metadata::entity::types::entity_type_intern;
use brain_metadata::relation::ops::relation_create;
use brain_metadata::relation::types::relation_type_intern;
use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::statement::{statement_create, statement_tombstone};
use brain_metadata::tables::memory::{flags as memory_flags, MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use tempfile::TempDir;

use super::{apply_filter_chain, FilterChain};
use crate::retrieval::fusion::{FusedItem, RetrieverContribution};
use crate::retrieval::router::{Retriever, TimeRange};

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

const PERSON_TYPE: &str = "brain:Person";

fn fresh() -> (TempDir, MetadataDb) {
    let dir = TempDir::new().expect("tempdir");
    let mut metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    let _ = ensure_person_type(&mut metadata);
    (dir, metadata)
}

fn ensure_person_type(metadata: &mut MetadataDb) -> EntityTypeId {
    let wtxn = metadata.write_txn().expect("wtxn");
    let id = entity_type_intern(&wtxn, PERSON_TYPE, Vec::new(), 0).expect("type");
    wtxn.commit().expect("commit");
    id
}

fn put_entity(metadata: &mut MetadataDb, name: &str, type_id: EntityTypeId) -> EntityId {
    let id = EntityId::new();
    let entity = Entity::new_active(id, type_id, name.into(), name.to_lowercase(), 0);
    let wtxn = metadata.write_txn().expect("wtxn");
    entity_put(&wtxn, &entity).expect("entity_put");
    wtxn.commit().expect("commit");
    id
}

fn intern_predicate(metadata: &mut MetadataDb, namespace: &str, name: &str) -> PredicateId {
    // Use intern_or_get so tests tolerate predicates already seeded by the
    // default brain: schema (e.g. brain:lives_in, brain:works_at).
    let wtxn = metadata.write_txn().expect("wtxn");
    let id = predicate_intern_or_get(&wtxn, namespace, name, 0, 0).expect("predicate");
    wtxn.commit().expect("commit");
    id
}

fn create_statement(
    metadata: &mut MetadataDb,
    subject: EntityId,
    predicate: PredicateId,
    kind: StatementKind,
    confidence: f32,
    object_text: &str,
) -> StatementId {
    let stmt = Statement::new_root(
        StatementId::new(),
        kind,
        SubjectRef::Entity(subject),
        predicate,
        StatementObject::Value(StatementValue::Text(object_text.into())),
        confidence,
        EvidenceRef::default(),
        ExtractorId::from(0),
        0,
        1,
    );
    let wtxn = metadata.write_txn().expect("wtxn");
    let id = statement_create(&wtxn, &stmt, 0).expect("create");
    wtxn.commit().expect("commit");
    id
}

fn create_event_statement(
    metadata: &mut MetadataDb,
    subject: EntityId,
    predicate: PredicateId,
    event_at_ms: u64,
) -> StatementId {
    let mut stmt = Statement::new_root(
        StatementId::new(),
        StatementKind::Event,
        SubjectRef::Entity(subject),
        predicate,
        StatementObject::Value(StatementValue::Text("x".into())),
        0.9,
        EvidenceRef::default(),
        ExtractorId::from(0),
        0,
        1,
    );
    stmt.event_at_unix_nanos = Some(event_at_ms.saturating_mul(1_000_000));
    let wtxn = metadata.write_txn().expect("wtxn");
    let id = statement_create(&wtxn, &stmt, 0).expect("create");
    wtxn.commit().expect("commit");
    id
}

fn tombstone(metadata: &mut MetadataDb, id: StatementId) {
    let wtxn = metadata.write_txn().expect("wtxn");
    statement_tombstone(&wtxn, id, TombstoneReason::UserRequest, 0).expect("tombstone");
    wtxn.commit().expect("commit");
}

fn put_memory_row(
    metadata: &mut MetadataDb,
    id: MemoryId,
    kind: MemoryKind,
    salience: f32,
    created_at_ms: u64,
    active: bool,
) {
    let mut row = MemoryMetadata::new_active(
        id,
        AgentId::new(),
        ContextId::from(0),
        id.slot(),
        id.version(),
        kind,
        [0u8; 16],
        salience,
        0,
        created_at_ms.saturating_mul(1_000_000),
    );
    if !active {
        row.flags &= !memory_flags::ACTIVE;
    }
    let wtxn = metadata.write_txn().expect("wtxn");
    {
        let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open");
        t.insert(&id.raw().to_be_bytes(), &row).expect("insert");
    }
    wtxn.commit().expect("commit");
}

fn create_relation(
    metadata: &mut MetadataDb,
    type_id: EntityTypeId,
    from: EntityId,
    to: EntityId,
    rt_name: &str,
    confidence: f32,
) -> RelationId {
    let _ = type_id;
    let rt = {
        let wtxn = metadata.write_txn().expect("wtxn");
        let id = relation_type_intern(
            &wtxn,
            "brain",
            rt_name,
            None,
            None,
            Cardinality::ManyToMany,
            false,
            1,
            "",
            0,
        )
        .expect("rtype");
        wtxn.commit().expect("commit");
        id
    };
    let r = Relation::new_root(
        RelationId::new(),
        rt,
        from,
        to,
        confidence,
        Vec::new(),
        ExtractorId::from(0),
        0,
        false,
    );
    let wtxn = metadata.write_txn().expect("wtxn");
    let id = relation_create(&wtxn, &r, 0).expect("relation_create");
    wtxn.commit().expect("commit");
    id
}

fn fused(id: RankedItemId, rank: u32) -> FusedItem {
    FusedItem {
        id,
        fused_score: 1.0 / (60.0 + f64::from(rank)),
        contributing: vec![RetrieverContribution {
            retriever: Retriever::Semantic,
            rank,
            raw_score: 1.0 - 0.01 * f32::from(rank as u16),
        }],
        rerank_score: None,
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn empty_chain_passes_all() {
    let (_dir, mut metadata) = fresh();
    let id1 = MemoryId::pack(0, 1, 0);
    let id2 = MemoryId::pack(0, 2, 0);
    put_memory_row(&mut metadata, id1, MemoryKind::Episodic, 0.5, 100, true);
    put_memory_row(&mut metadata, id2, MemoryKind::Semantic, 0.7, 200, true);

    let items = vec![
        fused(RankedItemId::Memory(id1), 1),
        fused(RankedItemId::Memory(id2), 2),
    ];
    let (out, stats) =
        apply_filter_chain(items, &FilterChain::default(), &metadata, 0).expect("ok");
    assert_eq!(out.len(), 2);
    assert_eq!(stats.before, 2);
    assert_eq!(stats.after_limit, 2);
}

#[test]
fn memory_kind_filter_narrows() {
    let (_dir, mut metadata) = fresh();
    let id1 = MemoryId::pack(0, 1, 0);
    let id2 = MemoryId::pack(0, 2, 0);
    put_memory_row(&mut metadata, id1, MemoryKind::Episodic, 0.5, 100, true);
    put_memory_row(&mut metadata, id2, MemoryKind::Semantic, 0.7, 200, true);

    let items = vec![
        fused(RankedItemId::Memory(id1), 1),
        fused(RankedItemId::Memory(id2), 2),
    ];
    let chain = FilterChain {
        memory_kind_filter: vec![MemoryKind::Episodic],
        ..Default::default()
    };
    let (out, _) = apply_filter_chain(items, &chain, &metadata, 0).expect("ok");
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].id, RankedItemId::Memory(m) if m == id1));
}

#[test]
fn statement_kind_and_predicate_filter() {
    let (_dir, mut metadata) = fresh();
    let type_id = ensure_person_type(&mut metadata);
    let alice = put_entity(&mut metadata, "Alice", type_id);
    let p_lives = intern_predicate(&mut metadata, "test", "lives_in");
    let p_likes = intern_predicate(&mut metadata, "test", "likes");
    let s_fact = create_statement(
        &mut metadata,
        alice,
        p_lives,
        StatementKind::Fact,
        0.8,
        "Paris",
    );
    let s_pref = create_statement(
        &mut metadata,
        alice,
        p_likes,
        StatementKind::Preference,
        0.7,
        "tea",
    );

    let items = vec![
        fused(RankedItemId::Statement(s_fact), 1),
        fused(RankedItemId::Statement(s_pref), 2),
    ];
    let by_kind = FilterChain {
        kind_filter: vec![StatementKind::Preference],
        ..Default::default()
    };
    let (out, _) = apply_filter_chain(items.clone(), &by_kind, &metadata, 0).expect("ok");
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].id, RankedItemId::Statement(id) if id == s_pref));

    let by_pred = FilterChain {
        predicate_filter: vec![p_lives],
        ..Default::default()
    };
    let (out, _) = apply_filter_chain(items, &by_pred, &metadata, 0).expect("ok");
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].id, RankedItemId::Statement(id) if id == s_fact));
}

#[test]
fn time_filter_for_memory_uses_created_at() {
    let (_dir, mut metadata) = fresh();
    let id1 = MemoryId::pack(0, 1, 0);
    let id2 = MemoryId::pack(0, 2, 0);
    let id3 = MemoryId::pack(0, 3, 0);
    put_memory_row(&mut metadata, id1, MemoryKind::Episodic, 0.5, 100, true);
    put_memory_row(&mut metadata, id2, MemoryKind::Episodic, 0.5, 500, true);
    put_memory_row(&mut metadata, id3, MemoryKind::Episodic, 0.5, 900, true);

    let items = vec![
        fused(RankedItemId::Memory(id1), 1),
        fused(RankedItemId::Memory(id2), 2),
        fused(RankedItemId::Memory(id3), 3),
    ];
    let chain = FilterChain {
        time_filter: Some(TimeRange {
            from_unix_ms: Some(200),
            to_unix_ms: Some(800),
        }),
        ..Default::default()
    };
    let (out, _) = apply_filter_chain(items, &chain, &metadata, 0).expect("ok");
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].id, RankedItemId::Memory(m) if m == id2));
}

#[test]
fn time_filter_for_event_statement_uses_event_at() {
    let (_dir, mut metadata) = fresh();
    let type_id = ensure_person_type(&mut metadata);
    let alice = put_entity(&mut metadata, "Alice", type_id);
    let p = intern_predicate(&mut metadata, "test", "happened");
    let s_in = create_event_statement(&mut metadata, alice, p, 500);
    let s_out = create_event_statement(&mut metadata, alice, p, 9_000);

    let items = vec![
        fused(RankedItemId::Statement(s_in), 1),
        fused(RankedItemId::Statement(s_out), 2),
    ];
    let chain = FilterChain {
        time_filter: Some(TimeRange {
            from_unix_ms: Some(100),
            to_unix_ms: Some(800),
        }),
        ..Default::default()
    };
    let (out, _) = apply_filter_chain(items, &chain, &metadata, 0).expect("ok");
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].id, RankedItemId::Statement(id) if id == s_in));
}

#[test]
fn confidence_filter_for_statement() {
    let (_dir, mut metadata) = fresh();
    let type_id = ensure_person_type(&mut metadata);
    let alice = put_entity(&mut metadata, "Alice", type_id);
    let p = intern_predicate(&mut metadata, "test", "lives_in");
    let lo = create_statement(&mut metadata, alice, p, StatementKind::Fact, 0.4, "x");
    let mid = create_statement(&mut metadata, alice, p, StatementKind::Fact, 0.6, "y");
    let hi = create_statement(&mut metadata, alice, p, StatementKind::Fact, 0.9, "z");

    let items = vec![
        fused(RankedItemId::Statement(lo), 1),
        fused(RankedItemId::Statement(mid), 2),
        fused(RankedItemId::Statement(hi), 3),
    ];
    let chain = FilterChain {
        confidence_min: Some(0.5),
        ..Default::default()
    };
    let (out, _) = apply_filter_chain(items, &chain, &metadata, 0).expect("ok");
    assert_eq!(out.len(), 2);
}

#[test]
fn confidence_filter_for_memory_uses_salience() {
    let (_dir, mut metadata) = fresh();
    let id_lo = MemoryId::pack(0, 1, 0);
    let id_hi = MemoryId::pack(0, 2, 0);
    put_memory_row(&mut metadata, id_lo, MemoryKind::Episodic, 0.2, 100, true);
    put_memory_row(&mut metadata, id_hi, MemoryKind::Episodic, 0.9, 100, true);

    let items = vec![
        fused(RankedItemId::Memory(id_lo), 1),
        fused(RankedItemId::Memory(id_hi), 2),
    ];
    let chain = FilterChain {
        confidence_min: Some(0.5),
        ..Default::default()
    };
    let (out, _) = apply_filter_chain(items, &chain, &metadata, 0).expect("ok");
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].id, RankedItemId::Memory(m) if m == id_hi));
}

#[test]
fn tombstone_filter_drops_inactive_memory() {
    let (_dir, mut metadata) = fresh();
    let id_a = MemoryId::pack(0, 1, 0);
    let id_t = MemoryId::pack(0, 2, 0);
    put_memory_row(&mut metadata, id_a, MemoryKind::Episodic, 0.5, 100, true);
    put_memory_row(&mut metadata, id_t, MemoryKind::Episodic, 0.5, 100, false);

    let items = vec![
        fused(RankedItemId::Memory(id_a), 1),
        fused(RankedItemId::Memory(id_t), 2),
    ];
    let (out, _) =
        apply_filter_chain(items.clone(), &FilterChain::default(), &metadata, 0).expect("ok");
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].id, RankedItemId::Memory(m) if m == id_a));

    let with_tomb = FilterChain {
        include_tombstoned: true,
        ..Default::default()
    };
    let (out, _) = apply_filter_chain(items, &with_tomb, &metadata, 0).expect("ok");
    assert_eq!(out.len(), 2);
}

#[test]
fn tombstone_filter_drops_tombstoned_statement() {
    let (_dir, mut metadata) = fresh();
    let type_id = ensure_person_type(&mut metadata);
    let alice = put_entity(&mut metadata, "Alice", type_id);
    let p = intern_predicate(&mut metadata, "test", "lives_in");
    let s_live = create_statement(&mut metadata, alice, p, StatementKind::Fact, 0.9, "Paris");
    let s_dead = create_statement(&mut metadata, alice, p, StatementKind::Fact, 0.9, "Mars");
    tombstone(&mut metadata, s_dead);

    let items = vec![
        fused(RankedItemId::Statement(s_live), 1),
        fused(RankedItemId::Statement(s_dead), 2),
    ];
    let (out, _) = apply_filter_chain(items, &FilterChain::default(), &metadata, 0).expect("ok");
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].id, RankedItemId::Statement(id) if id == s_live));
}

#[test]
fn entity_passes_unfiltered() {
    let (_dir, mut metadata) = fresh();
    let type_id = ensure_person_type(&mut metadata);
    let alice = put_entity(&mut metadata, "Alice", type_id);

    let items = vec![fused(RankedItemId::Entity(alice), 1)];
    let chain = FilterChain {
        kind_filter: vec![StatementKind::Fact],
        confidence_min: Some(0.9),
        time_filter: Some(TimeRange {
            from_unix_ms: Some(0),
            to_unix_ms: Some(1),
        }),
        ..Default::default()
    };
    let (out, _) = apply_filter_chain(items, &chain, &metadata, 0).expect("ok");
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].id, RankedItemId::Entity(e) if e == alice));
}

#[test]
fn relation_passes_tombstone_and_supersession_when_clean() {
    let (_dir, mut metadata) = fresh();
    let type_id = ensure_person_type(&mut metadata);
    let a = put_entity(&mut metadata, "A", type_id);
    let b = put_entity(&mut metadata, "B", type_id);
    let r = create_relation(&mut metadata, type_id, a, b, "knows", 0.9);

    let items = vec![fused(RankedItemId::Relation(r), 1)];
    let (out, _) = apply_filter_chain(items, &FilterChain::default(), &metadata, 0).expect("ok");
    assert_eq!(out.len(), 1);
}

#[test]
fn filter_chain_stats_reflect_drops_per_step() {
    let (_dir, mut metadata) = fresh();
    let type_id = ensure_person_type(&mut metadata);
    let alice = put_entity(&mut metadata, "Alice", type_id);
    let p = intern_predicate(&mut metadata, "test", "lives_in");
    let s_pass = create_statement(&mut metadata, alice, p, StatementKind::Fact, 0.9, "Paris");
    let s_low_conf = create_statement(&mut metadata, alice, p, StatementKind::Fact, 0.2, "London");
    let s_wrong_kind = create_statement(
        &mut metadata,
        alice,
        p,
        StatementKind::Preference,
        0.9,
        "tea",
    );

    let items = vec![
        fused(RankedItemId::Statement(s_pass), 1),
        fused(RankedItemId::Statement(s_low_conf), 2),
        fused(RankedItemId::Statement(s_wrong_kind), 3),
    ];
    let chain = FilterChain {
        kind_filter: vec![StatementKind::Fact],
        confidence_min: Some(0.5),
        ..Default::default()
    };
    let (out, stats) = apply_filter_chain(items, &chain, &metadata, 0).expect("ok");
    assert_eq!(stats.before, 3);
    assert_eq!(stats.after_type, 2, "wrong-kind dropped");
    assert_eq!(stats.after_confidence, 1, "low-confidence dropped");
    assert_eq!(out.len(), 1);
}

#[test]
fn limit_applied_after_filters() {
    let (_dir, mut metadata) = fresh();
    let mut items = Vec::new();
    for slot in 1u64..=5 {
        let id = MemoryId::pack(0, slot, 0);
        put_memory_row(&mut metadata, id, MemoryKind::Episodic, 0.9, 100, true);
        items.push(fused(RankedItemId::Memory(id), slot as u32));
    }
    let (out, stats) =
        apply_filter_chain(items, &FilterChain::default(), &metadata, 3).expect("ok");
    assert_eq!(out.len(), 3);
    assert_eq!(stats.after_supersession, 5, "all five passed filters");
    assert_eq!(stats.after_limit, 3, "limit truncated post-filter");
}

// ---------------------------------------------------------------------------
// Bi-temporal as-of (record-time time-travel).
// ---------------------------------------------------------------------------

use super::as_of_matches;

fn stmt_with_record_time(extracted: u64, invalidated: Option<u64>) -> Statement {
    let mut s = Statement::new_root(
        StatementId::new(),
        StatementKind::Fact,
        SubjectRef::Entity(EntityId::new()),
        PredicateId::from(1),
        StatementObject::Value(StatementValue::Text("x".into())),
        0.9,
        EvidenceRef::default(),
        ExtractorId::from(0),
        extracted,
        1,
    );
    s.record_invalidated_at_unix_nanos = invalidated;
    s
}

#[test]
fn as_of_filter_includes_active_statement() {
    // extracted_at <= t, record_invalidated_at = None ⇒ included.
    let stmt = stmt_with_record_time(1_000, None);
    assert!(as_of_matches(&stmt, 1_500));
    assert!(as_of_matches(&stmt, 1_000));
}

#[test]
fn as_of_filter_excludes_invalidated_before_t() {
    // record_invalidated_at < t ⇒ excluded.
    let stmt = stmt_with_record_time(1_000, Some(1_200));
    assert!(!as_of_matches(&stmt, 1_500));
}

#[test]
fn as_of_filter_includes_invalidated_after_t() {
    // record_invalidated_at > t ⇒ still believed at t ⇒ included.
    let stmt = stmt_with_record_time(1_000, Some(2_000));
    assert!(as_of_matches(&stmt, 1_500));
}

#[test]
fn as_of_filter_excludes_extracted_after_t() {
    // extracted_at > t ⇒ excluded (didn't exist yet at t).
    let stmt = stmt_with_record_time(2_000, None);
    assert!(!as_of_matches(&stmt, 1_500));
}

#[test]
fn as_of_filter_in_chain_drops_invalidated_statement() {
    // End-to-end through the filter chain: supersede a statement,
    // then ask "what did we believe before the supersession?" and
    // assert the prior comes back even though it's now historical.
    let (_dir, mut metadata) = fresh();
    let type_id = ensure_person_type(&mut metadata);
    let alice = put_entity(&mut metadata, "Alice", type_id);
    let pred = intern_predicate(&mut metadata, "test", "as_of_pred");

    // Seed at t = 1_000 with extracted_at = 1_000.
    let mut p1 = Statement::new_root(
        StatementId::new(),
        StatementKind::Fact,
        SubjectRef::Entity(alice),
        pred,
        StatementObject::Value(StatementValue::Text("Paris".into())),
        0.9,
        EvidenceRef::default(),
        ExtractorId::from(0),
        1_000,
        1,
    );
    p1.is_stateful = true;
    let wtxn = metadata.write_txn().unwrap();
    let p1_id = brain_metadata::statement::statement_create(&wtxn, &p1, 1_000).unwrap();
    wtxn.commit().unwrap();

    // Supersede at t = 2_000.
    let mut p2 = Statement::new_root(
        StatementId::new(),
        StatementKind::Fact,
        SubjectRef::Entity(alice),
        pred,
        StatementObject::Value(StatementValue::Text("Berlin".into())),
        0.9,
        EvidenceRef::default(),
        ExtractorId::from(0),
        2_000,
        1,
    );
    p2.is_stateful = true;
    let wtxn = metadata.write_txn().unwrap();
    let p2_id = brain_metadata::statement::statement_supersede(&wtxn, p1_id, &p2, 2_000).unwrap();
    wtxn.commit().unwrap();

    // as_of = 1_500 → only p1 should pass (p2 didn't exist yet, p1
    // not yet invalidated). Need include_superseded so the
    // supersession filter doesn't drop p1 before the as-of step.
    let items = vec![
        fused(RankedItemId::Statement(p1_id), 1),
        fused(RankedItemId::Statement(p2_id), 2),
    ];
    let chain = FilterChain {
        include_superseded: true,
        as_of_record_time_unix_nanos: Some(1_500),
        ..Default::default()
    };
    let (out, stats) = apply_filter_chain(items, &chain, &metadata, 0).expect("ok");
    assert_eq!(stats.after_supersession, 2);
    assert_eq!(stats.after_as_of, 1);
    let only: Vec<_> = out.iter().map(|f| f.id).collect();
    assert_eq!(only, vec![RankedItemId::Statement(p1_id)]);
}
