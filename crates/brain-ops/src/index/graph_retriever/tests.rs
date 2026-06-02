//! Unit tests for `BrainGraphRetriever`.

use std::sync::Arc;

use brain_core::{Cardinality, Relation};
use brain_core::{Entity, EntityId, EntityTypeId, ExtractorId, RelationId, RelationTypeId};
use brain_index::{
    Direction, GraphAnchor, GraphError, GraphQuery, GraphRetriever, GraphRetrieverConfig,
    RankedItemId,
};
use brain_metadata::entity::ops::entity_put;
use brain_metadata::entity::types::entity_type_intern;
use brain_metadata::relation::ops::relation_create;
use brain_metadata::relation::types::relation_type_intern;
use brain_metadata::MetadataDb;
use tempfile::TempDir;

use super::BrainGraphRetriever;

// ---------------------------------------------------------------------------
// Setup helpers.
// ---------------------------------------------------------------------------

const PERSON_TYPE: &str = "brain:Person";

fn ensure_person_type(metadata: &MetadataDb) -> EntityTypeId {
    let wtxn = metadata.write_txn().expect("wtxn");
    let id = entity_type_intern(&wtxn, PERSON_TYPE, Vec::new(), 0).expect("type intern");
    wtxn.commit().expect("commit");
    id
}

fn make_retriever_with_db(metadata: Arc<MetadataDb>) -> BrainGraphRetriever {
    BrainGraphRetriever::new(metadata)
}

fn fresh_with_metadata() -> (TempDir, Arc<MetadataDb>) {
    let dir = TempDir::new().expect("tempdir");
    let metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    ensure_person_type(&metadata);
    (dir, Arc::new(metadata))
}

fn put_entity(metadata: &Arc<MetadataDb>, name: &str, type_id: EntityTypeId) -> EntityId {
    let id = EntityId::new();
    let entity = Entity::new_active(id, type_id, name.into(), name.to_lowercase(), 0);
    let wtxn = metadata.write_txn().expect("wtxn");
    entity_put(&wtxn, &entity).expect("entity_put");
    wtxn.commit().expect("commit");
    id
}

fn intern_relation_type(metadata: &Arc<MetadataDb>, namespace: &str, name: &str) -> RelationTypeId {
    let wtxn = metadata.write_txn().expect("wtxn");
    let id = relation_type_intern(
        &wtxn,
        namespace,
        name,
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
}

fn create_relation(
    metadata: &Arc<MetadataDb>,
    relation_type: RelationTypeId,
    from: EntityId,
    to: EntityId,
) -> RelationId {
    let id = RelationId::new();
    let r = Relation::new_root(
        id,
        relation_type,
        from,
        to,
        0.9,
        Vec::new(),
        ExtractorId::from(0),
        0,
        false,
    );
    let wtxn = metadata.write_txn().expect("wtxn");
    let created = relation_create(&wtxn, &r, 0).expect("relation_create");
    wtxn.commit().expect("commit");
    created
}

fn current_person_type(metadata: &Arc<MetadataDb>) -> EntityTypeId {
    // Person type was already interned by fresh_with_metadata;
    // re-call intern to get its id back.
    let wtxn = metadata.write_txn().expect("wtxn");
    let id = entity_type_intern(&wtxn, PERSON_TYPE, Vec::new(), 0).expect("intern");
    wtxn.commit().expect("commit");
    id
}

// ---------------------------------------------------------------------------
// Star.
// ---------------------------------------------------------------------------

#[test]
fn star_depth_1_returns_neighbours() {
    let (_dir, metadata) = fresh_with_metadata();
    let type_id = current_person_type(&metadata);
    let a = put_entity(&metadata, "A", type_id);
    let b = put_entity(&metadata, "B", type_id);
    let rt = intern_relation_type(&metadata, "brain", "knows");
    let _r_ab = create_relation(&metadata, rt, a, b);

    let retriever = make_retriever_with_db(metadata);
    let result = retriever
        .retrieve(
            &GraphQuery::Star {
                anchor: GraphAnchor::Entity(a),
                depth: 1,
                direction: Direction::Outgoing,
                relation_types: None,
                include_statements: false,
            },
            &GraphRetrieverConfig::default(),
        )
        .expect("retrieve");

    // Should include B (entity) and the relation. Anchor A omitted.
    let mut entity_count = 0;
    let mut relation_count = 0;
    let mut sees_b = false;
    for item in &result {
        match item.id {
            RankedItemId::Entity(id) => {
                entity_count += 1;
                if id == b {
                    sees_b = true;
                }
            }
            RankedItemId::Relation(_) => relation_count += 1,
            _ => {}
        }
    }
    assert_eq!(entity_count, 1, "exactly one entity (B); A omitted");
    assert!(sees_b);
    assert_eq!(relation_count, 1, "the A→B relation");
}

#[test]
fn star_relation_type_filter_narrows() {
    let (_dir, metadata) = fresh_with_metadata();
    let type_id = current_person_type(&metadata);
    let a = put_entity(&metadata, "A", type_id);
    let b = put_entity(&metadata, "B", type_id);
    let c = put_entity(&metadata, "C", type_id);
    let knows = intern_relation_type(&metadata, "brain", "knows");
    let likes = intern_relation_type(&metadata, "brain", "likes");
    create_relation(&metadata, knows, a, b);
    create_relation(&metadata, likes, a, c);

    let retriever = make_retriever_with_db(metadata);
    let result = retriever
        .retrieve(
            &GraphQuery::Star {
                anchor: GraphAnchor::Entity(a),
                depth: 1,
                direction: Direction::Outgoing,
                relation_types: Some(vec![knows]),
                include_statements: false,
            },
            &GraphRetrieverConfig::default(),
        )
        .expect("retrieve");

    let entities: Vec<EntityId> = result
        .iter()
        .filter_map(|i| match i.id {
            RankedItemId::Entity(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(entities, vec![b], "only B reachable via `knows`");
}

#[test]
fn star_direction_outgoing_vs_incoming() {
    let (_dir, metadata) = fresh_with_metadata();
    let type_id = current_person_type(&metadata);
    let a = put_entity(&metadata, "A", type_id);
    let b = put_entity(&metadata, "B", type_id);
    let c = put_entity(&metadata, "C", type_id);
    let rt = intern_relation_type(&metadata, "brain", "knows");
    create_relation(&metadata, rt, a, b); // A→B
    create_relation(&metadata, rt, c, a); // C→A

    let retriever = make_retriever_with_db(metadata);

    let out = retriever
        .retrieve(
            &GraphQuery::Star {
                anchor: GraphAnchor::Entity(a),
                depth: 1,
                direction: Direction::Outgoing,
                relation_types: None,
                include_statements: false,
            },
            &GraphRetrieverConfig::default(),
        )
        .expect("retrieve");
    let out_entities: Vec<EntityId> = out
        .iter()
        .filter_map(|i| match i.id {
            RankedItemId::Entity(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(out_entities, vec![b], "outgoing reaches B only");

    let inc = retriever
        .retrieve(
            &GraphQuery::Star {
                anchor: GraphAnchor::Entity(a),
                depth: 1,
                direction: Direction::Incoming,
                relation_types: None,
                include_statements: false,
            },
            &GraphRetrieverConfig::default(),
        )
        .expect("retrieve");
    let inc_entities: Vec<EntityId> = inc
        .iter()
        .filter_map(|i| match i.id {
            RankedItemId::Entity(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(inc_entities, vec![c], "incoming reaches C only");
}

#[test]
fn star_depth_2_reaches_second_hop() {
    let (_dir, metadata) = fresh_with_metadata();
    let type_id = current_person_type(&metadata);
    let a = put_entity(&metadata, "A", type_id);
    let b = put_entity(&metadata, "B", type_id);
    let c = put_entity(&metadata, "C", type_id);
    let rt = intern_relation_type(&metadata, "brain", "knows");
    create_relation(&metadata, rt, a, b);
    create_relation(&metadata, rt, b, c);

    let retriever = make_retriever_with_db(metadata);
    let result = retriever
        .retrieve(
            &GraphQuery::Star {
                anchor: GraphAnchor::Entity(a),
                depth: 2,
                direction: Direction::Outgoing,
                relation_types: None,
                include_statements: false,
            },
            &GraphRetrieverConfig::default(),
        )
        .expect("retrieve");

    let entities: Vec<EntityId> = result
        .iter()
        .filter_map(|i| match i.id {
            RankedItemId::Entity(id) => Some(id),
            _ => None,
        })
        .collect();
    assert!(entities.contains(&b));
    assert!(entities.contains(&c));
}

// ---------------------------------------------------------------------------
// Path.
// ---------------------------------------------------------------------------

#[test]
fn path_finds_direct_link() {
    let (_dir, metadata) = fresh_with_metadata();
    let type_id = current_person_type(&metadata);
    let a = put_entity(&metadata, "A", type_id);
    let b = put_entity(&metadata, "B", type_id);
    let rt = intern_relation_type(&metadata, "brain", "knows");
    let _r = create_relation(&metadata, rt, a, b);

    let retriever = make_retriever_with_db(metadata);
    let result = retriever
        .retrieve(
            &GraphQuery::Path {
                from: a,
                to: b,
                max_depth: 3,
            },
            &GraphRetrieverConfig::default(),
        )
        .expect("retrieve");

    // Must include both A and B as entities, plus the relation.
    let entity_ids: Vec<EntityId> = result
        .iter()
        .filter_map(|i| match i.id {
            RankedItemId::Entity(id) => Some(id),
            _ => None,
        })
        .collect();
    assert!(entity_ids.contains(&a));
    assert!(entity_ids.contains(&b));
}

#[test]
fn path_no_path_returns_both_endpoints() {
    let (_dir, metadata) = fresh_with_metadata();
    let type_id = current_person_type(&metadata);
    let a = put_entity(&metadata, "A", type_id);
    let b = put_entity(&metadata, "B", type_id);
    // No relations linking A and B.

    let retriever = make_retriever_with_db(metadata);
    let result = retriever
        .retrieve(
            &GraphQuery::Path {
                from: a,
                to: b,
                max_depth: 3,
            },
            &GraphRetrieverConfig::default(),
        )
        .expect("retrieve");

    let entity_ids: Vec<EntityId> = result
        .iter()
        .filter_map(|i| match i.id {
            RankedItemId::Entity(id) => Some(id),
            _ => None,
        })
        .collect();
    assert!(entity_ids.contains(&a));
    assert!(entity_ids.contains(&b));
    // Half-credit score.
    for item in &result {
        if matches!(item.id, RankedItemId::Relation(_)) {
            panic!("no-path result should not contain relations");
        }
    }
}

// ---------------------------------------------------------------------------
// Subgraph.
// ---------------------------------------------------------------------------

#[test]
fn subgraph_returns_closed_neighbourhood() {
    let (_dir, metadata) = fresh_with_metadata();
    let type_id = current_person_type(&metadata);
    let a = put_entity(&metadata, "A", type_id);
    let b = put_entity(&metadata, "B", type_id);
    let c = put_entity(&metadata, "C", type_id);
    let rt = intern_relation_type(&metadata, "brain", "knows");
    create_relation(&metadata, rt, a, b);
    create_relation(&metadata, rt, b, c);

    let retriever = make_retriever_with_db(metadata);
    let result = retriever
        .retrieve(
            &GraphQuery::Subgraph {
                anchor: GraphAnchor::Entity(a),
                depth: 2,
            },
            &GraphRetrieverConfig::default(),
        )
        .expect("retrieve");

    let entity_ids: Vec<EntityId> = result
        .iter()
        .filter_map(|i| match i.id {
            RankedItemId::Entity(id) => Some(id),
            _ => None,
        })
        .collect();
    // Anchor A omitted; B and C in the closed neighbourhood.
    assert!(entity_ids.contains(&b));
    assert!(entity_ids.contains(&c));
    assert!(!entity_ids.contains(&a));
}

// ---------------------------------------------------------------------------
// Caps + errors.
// ---------------------------------------------------------------------------

#[test]
fn max_depth_above_cap_errors() {
    let (_dir, metadata) = fresh_with_metadata();
    let type_id = current_person_type(&metadata);
    let a = put_entity(&metadata, "A", type_id);

    let retriever = make_retriever_with_db(metadata);
    let err = retriever
        .retrieve(
            &GraphQuery::Star {
                anchor: GraphAnchor::Entity(a),
                depth: 6,
                direction: Direction::Outgoing,
                relation_types: None,
                include_statements: false,
            },
            &GraphRetrieverConfig::default(),
        )
        .expect_err("rejects depth above cap");
    assert!(matches!(err, GraphError::MaxDepthExceeded { got: 6 }));
}

#[test]
fn anchor_not_found_errors() {
    let (_dir, metadata) = fresh_with_metadata();
    let retriever = make_retriever_with_db(metadata);
    let random = EntityId::new();

    let err = retriever
        .retrieve(
            &GraphQuery::Star {
                anchor: GraphAnchor::Entity(random),
                depth: 1,
                direction: Direction::Outgoing,
                relation_types: None,
                include_statements: false,
            },
            &GraphRetrieverConfig::default(),
        )
        .expect_err("rejects missing anchor");
    assert!(matches!(err, GraphError::AnchorNotFound(_)));
}

#[test]
fn ranks_are_dense_and_one_based() {
    let (_dir, metadata) = fresh_with_metadata();
    let type_id = current_person_type(&metadata);
    let a = put_entity(&metadata, "A", type_id);
    let b = put_entity(&metadata, "B", type_id);
    let c = put_entity(&metadata, "C", type_id);
    let rt = intern_relation_type(&metadata, "brain", "knows");
    create_relation(&metadata, rt, a, b);
    create_relation(&metadata, rt, a, c);

    let retriever = make_retriever_with_db(metadata);
    let result = retriever
        .retrieve(
            &GraphQuery::Star {
                anchor: GraphAnchor::Entity(a),
                depth: 1,
                direction: Direction::Outgoing,
                relation_types: None,
                include_statements: false,
            },
            &GraphRetrieverConfig::default(),
        )
        .expect("retrieve");

    for (i, item) in result.iter().enumerate() {
        assert_eq!(item.rank, (i as u32) + 1);
    }
}

// ---------------------------------------------------------------------------
// Memory-anchor mode.
// ---------------------------------------------------------------------------

mod memory_anchor {
    use super::*;

    use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
    use brain_metadata::tables::edge::{
        derived_by, link, origin, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
    };
    use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    /// Insert an active memory row for the anchor existence guard.
    fn put_memory(metadata: &Arc<MetadataDb>, id: MemoryId) {
        let row = MemoryMetadata::new_active(
            id,
            AgentId::from([0u8; 16]),
            ContextId(0),
            id.slot(),
            id.version(),
            MemoryKind::Episodic,
            [0u8; 16],
            0.5,
            0,
            1_700_000_000_000_000_000,
        );
        let wtxn = metadata.write_txn().expect("wtxn");
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open memories");
            t.insert(&id.to_be_bytes(), &row).expect("insert memory");
        }
        wtxn.commit().expect("commit");
    }

    fn link_memories(
        metadata: &Arc<MetadataDb>,
        from: MemoryId,
        kind: EdgeKind,
        to: MemoryId,
        weight: f32,
    ) {
        let data = EdgeData::new(
            weight,
            origin::EXPLICIT,
            derived_by::CLIENT,
            1_700_000_000_000_000_000,
        );
        let wtxn = metadata.write_txn().expect("wtxn");
        {
            let mut out = wtxn.open_table(EDGES_TABLE).expect("out");
            let mut rev = wtxn.open_table(EDGES_REVERSE_TABLE).expect("rev");
            link(
                &mut out,
                &mut rev,
                brain_core::NodeRef::Memory(from),
                brain_core::EdgeKindRef::Builtin(kind),
                brain_core::NodeRef::Memory(to),
                zero_disambiguator(),
                &data,
            )
            .expect("link");
        }
        wtxn.commit().expect("commit");
    }

    #[test]
    fn walk_memory_edges_depth_1_returns_direct_neighbours() {
        let (_dir, metadata) = fresh_with_metadata();
        let a = mid(1);
        let b = mid(2);
        let c = mid(3);
        for m in [a, b, c] {
            put_memory(&metadata, m);
        }
        link_memories(&metadata, a, EdgeKind::Caused, b, 0.9);
        link_memories(&metadata, a, EdgeKind::References, c, 0.5);

        let retriever = make_retriever_with_db(metadata);
        let result = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(a),
                    depth: 1,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("retrieve");

        let ids: Vec<MemoryId> = result
            .iter()
            .filter_map(|i| match i.id {
                RankedItemId::Memory(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(ids.contains(&b), "b reachable directly");
        assert!(ids.contains(&c), "c reachable directly");
        assert!(!ids.contains(&a), "anchor itself not emitted");
        // Higher-weight edge ranks first at equal depth.
        assert_eq!(ids[0], b, "0.9-weight Caused edge outranks 0.5 References");
    }

    #[test]
    fn walk_memory_edges_depth_2_reaches_grandchildren() {
        let (_dir, metadata) = fresh_with_metadata();
        let a = mid(10);
        let b = mid(11);
        let c = mid(12);
        for m in [a, b, c] {
            put_memory(&metadata, m);
        }
        link_memories(&metadata, a, EdgeKind::Caused, b, 1.0);
        // Relevance-bearing edge (not temporal): the walk traverses
        // SimilarTo/Caused/DerivedFrom, never FollowedBy.
        link_memories(&metadata, b, EdgeKind::DerivedFrom, c, 1.0);

        let retriever = make_retriever_with_db(metadata);
        let depth_1 = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(a),
                    depth: 1,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("retrieve depth-1");
        let depth_1_ids: Vec<MemoryId> = depth_1
            .iter()
            .filter_map(|i| match i.id {
                RankedItemId::Memory(m) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(depth_1_ids, vec![b], "depth=1 stops at b");

        let depth_2 = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(a),
                    depth: 2,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("retrieve depth-2");
        let depth_2_ids: Vec<MemoryId> = depth_2
            .iter()
            .filter_map(|i| match i.id {
                RankedItemId::Memory(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(depth_2_ids.contains(&b));
        assert!(
            depth_2_ids.contains(&c),
            "c reachable at depth 2 via a→b→c, got {depth_2_ids:?}"
        );
        // b at depth 1 outranks c at depth 2 (proximity_score
        // 1/2 vs 1/3 at equal edge weight).
        let b_pos = depth_2_ids.iter().position(|x| *x == b).expect("b present");
        let c_pos = depth_2_ids.iter().position(|x| *x == c).expect("c present");
        assert!(b_pos < c_pos);
    }

    #[test]
    fn walk_excludes_followed_by_temporal_edges() {
        // FollowedBy records session order, not relevance, so the
        // graph walk must never traverse or emit it — otherwise the
        // query-independent session chain floods retrieval.
        let (_dir, metadata) = fresh_with_metadata();
        let a = mid(20);
        let temporal = mid(21);
        let similar = mid(22);
        for m in [a, temporal, similar] {
            put_memory(&metadata, m);
        }
        link_memories(&metadata, a, EdgeKind::FollowedBy, temporal, 1.0);
        link_memories(&metadata, a, EdgeKind::SimilarTo, similar, 1.0);

        let retriever = make_retriever_with_db(metadata);
        let hits = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(a),
                    depth: 1,
                    direction: Direction::Both,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("retrieve");
        let ids: Vec<MemoryId> = hits
            .iter()
            .filter_map(|i| match i.id {
                RankedItemId::Memory(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(ids.contains(&similar), "SimilarTo neighbour is retrieved");
        assert!(
            !ids.contains(&temporal),
            "FollowedBy neighbour must be excluded, got {ids:?}"
        );
    }

    #[test]
    fn walk_memory_edges_missing_anchor_errors() {
        let (_dir, metadata) = fresh_with_metadata();
        let retriever = make_retriever_with_db(metadata);
        let err = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(mid(99)),
                    depth: 1,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect_err("anchor missing");
        assert!(matches!(err, GraphError::MemoryAnchorNotFound(_)));
    }
}

// ---------------------------------------------------------------------------
// Unified walk — Wave 3a coverage. Exercises the new single-path BFS
// over `NodeRef` directly so regressions in the collapsed dispatch
// surface independently of the trait-level Star/Path/Subgraph tests.
// ---------------------------------------------------------------------------

mod unified_walk {
    use super::*;

    use std::collections::{HashMap, HashSet};

    use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
    use brain_metadata::tables::edge::{
        derived_by, link, origin, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
    };
    use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    fn put_memory(metadata: &Arc<MetadataDb>, id: MemoryId) {
        let row = MemoryMetadata::new_active(
            id,
            AgentId::from([0u8; 16]),
            ContextId(0),
            id.slot(),
            id.version(),
            MemoryKind::Episodic,
            [0u8; 16],
            0.5,
            0,
            1_700_000_000_000_000_000,
        );
        let wtxn = metadata.write_txn().expect("wtxn");
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open memories");
            t.insert(&id.to_be_bytes(), &row).expect("insert memory");
        }
        wtxn.commit().expect("commit");
    }

    fn link_memories(
        metadata: &Arc<MetadataDb>,
        from: MemoryId,
        kind: EdgeKind,
        to: MemoryId,
        weight: f32,
    ) {
        let data = EdgeData::new(
            weight,
            origin::EXPLICIT,
            derived_by::CLIENT,
            1_700_000_000_000_000_000,
        );
        let wtxn = metadata.write_txn().expect("wtxn");
        {
            let mut out = wtxn.open_table(EDGES_TABLE).expect("out");
            let mut rev = wtxn.open_table(EDGES_REVERSE_TABLE).expect("rev");
            link(
                &mut out,
                &mut rev,
                brain_core::NodeRef::Memory(from),
                brain_core::EdgeKindRef::Builtin(kind),
                brain_core::NodeRef::Memory(to),
                zero_disambiguator(),
                &data,
            )
            .expect("link");
        }
        wtxn.commit().expect("commit");
    }

    // -------------------------------------------------------------
    // Memory anchor — exercises Builtin substrate edges.
    // -------------------------------------------------------------

    #[test]
    fn walk_memory_anchor_returns_substrate_neighbours() {
        let (_dir, metadata) = fresh_with_metadata();
        let a = mid(101);
        let b = mid(102);
        let c = mid(103);
        for m in [a, b, c] {
            put_memory(&metadata, m);
        }
        link_memories(&metadata, a, EdgeKind::Caused, b, 0.9);
        link_memories(&metadata, a, EdgeKind::References, c, 0.5);

        let retriever = make_retriever_with_db(metadata);
        let result = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(a),
                    depth: 1,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("retrieve");

        let ids: Vec<MemoryId> = result
            .iter()
            .filter_map(|i| match i.id {
                RankedItemId::Memory(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(ids.contains(&b), "b reachable directly");
        assert!(ids.contains(&c), "c reachable directly");
        assert!(!ids.contains(&a), "anchor itself not emitted");
    }

    // -------------------------------------------------------------
    // Entity anchor — exercises typed-graph relations.
    // -------------------------------------------------------------

    #[test]
    fn walk_entity_anchor_returns_typed_relation_neighbours() {
        let (_dir, metadata) = fresh_with_metadata();
        let type_id = current_person_type(&metadata);
        let a = put_entity(&metadata, "A", type_id);
        let b = put_entity(&metadata, "B", type_id);
        let rt = intern_relation_type(&metadata, "brain", "knows");
        let rel = create_relation(&metadata, rt, a, b);

        let retriever = make_retriever_with_db(metadata);
        let result = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Entity(a),
                    depth: 1,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("retrieve");

        let mut sees_b = false;
        let mut sees_relation = false;
        for item in &result {
            match item.id {
                RankedItemId::Entity(id) if id == b => sees_b = true,
                RankedItemId::Relation(id) if id == rel => sees_relation = true,
                _ => {}
            }
        }
        assert!(sees_b, "neighbour entity emitted");
        assert!(sees_relation, "relation hit emitted alongside neighbour");
    }

    // -------------------------------------------------------------
    // relation_type_filter behaviour.
    // -------------------------------------------------------------

    #[test]
    fn walk_with_relation_type_filter_excludes_substrate_kinds() {
        let (_dir, metadata) = fresh_with_metadata();
        let m1 = mid(201);
        let m2 = mid(202);
        for m in [m1, m2] {
            put_memory(&metadata, m);
        }
        link_memories(&metadata, m1, EdgeKind::Caused, m2, 1.0);

        // Filter `Some([knows])` on a memory anchor must yield no
        // hits — substrate edges have no relation type and a
        // non-empty allow-list is an opt-in to typed-only.
        let knows = intern_relation_type(&metadata, "brain", "knows");

        let retriever = make_retriever_with_db(metadata);
        let result = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(m1),
                    depth: 1,
                    direction: Direction::Outgoing,
                    relation_types: Some(vec![knows]),
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("retrieve");
        let ids: Vec<MemoryId> = result
            .iter()
            .filter_map(|i| match i.id {
                RankedItemId::Memory(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(
            ids.is_empty(),
            "typed-relation filter excludes substrate neighbours, got {ids:?}",
        );
    }

    #[test]
    fn walk_with_no_filter_includes_all_kinds() {
        let (_dir, metadata) = fresh_with_metadata();
        let type_id = current_person_type(&metadata);

        // Mixed graph: typed relation A_e → B_e plus a substrate
        // edge m1 → m2. Two anchor walks (entity, then memory) with
        // filter = None should each return their respective neighbour.
        let a_e = put_entity(&metadata, "A", type_id);
        let b_e = put_entity(&metadata, "B", type_id);
        let rt = intern_relation_type(&metadata, "brain", "likes");
        create_relation(&metadata, rt, a_e, b_e);

        let m1 = mid(301);
        let m2 = mid(302);
        for m in [m1, m2] {
            put_memory(&metadata, m);
        }
        link_memories(&metadata, m1, EdgeKind::SimilarTo, m2, 0.7);

        let retriever = make_retriever_with_db(metadata);

        let entity_hits = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Entity(a_e),
                    depth: 1,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("entity retrieve");
        assert!(entity_hits
            .iter()
            .any(|h| matches!(h.id, RankedItemId::Entity(e) if e == b_e)));

        let memory_hits = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(m1),
                    depth: 1,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("memory retrieve");
        assert!(memory_hits
            .iter()
            .any(|h| matches!(h.id, RankedItemId::Memory(m) if m == m2)));
    }

    // -------------------------------------------------------------
    // depth = 0 — anchor-only, no emissions.
    // -------------------------------------------------------------

    #[test]
    fn walk_depth_zero_returns_anchor_only() {
        let (_dir, metadata) = fresh_with_metadata();
        let m1 = mid(401);
        let m2 = mid(402);
        for m in [m1, m2] {
            put_memory(&metadata, m);
        }
        link_memories(&metadata, m1, EdgeKind::Caused, m2, 1.0);

        let retriever = make_retriever_with_db(metadata);
        let result = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(m1),
                    depth: 0,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("retrieve");
        // depth = 0 caps the walk at the anchor; anchor itself is
        // never emitted (hop = 0 is filtered out). The set is empty.
        assert!(
            result.is_empty(),
            "depth=0 produces empty set, got {result:?}",
        );
    }

    // -------------------------------------------------------------
    // Cycle dedup.
    // -------------------------------------------------------------

    #[test]
    fn walk_visited_dedup_prevents_cycles() {
        let (_dir, metadata) = fresh_with_metadata();
        let a = mid(501);
        let b = mid(502);
        for m in [a, b] {
            put_memory(&metadata, m);
        }
        // 3-cycle in directed substrate edges: A → B → A.
        link_memories(&metadata, a, EdgeKind::Caused, b, 1.0);
        link_memories(&metadata, b, EdgeKind::Caused, a, 1.0);

        let retriever = make_retriever_with_db(metadata);
        let result = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(a),
                    depth: 4,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("retrieve");

        let mut counts: HashMap<MemoryId, usize> = HashMap::new();
        for item in &result {
            if let RankedItemId::Memory(m) = item.id {
                *counts.entry(m).or_insert(0) += 1;
            }
        }
        assert!(
            counts.values().all(|&c| c == 1),
            "no duplicates: {counts:?}"
        );
        assert!(
            !counts.contains_key(&a),
            "anchor never emitted, even via cycle"
        );
        assert_eq!(counts.get(&b).copied().unwrap_or(0), 1, "b emitted once");
    }

    // -------------------------------------------------------------
    // Per-hop branching cap.
    // -------------------------------------------------------------

    #[test]
    fn walk_max_branching_truncates_per_hop() {
        let (_dir, metadata) = fresh_with_metadata();
        let a = mid(601);
        put_memory(&metadata, a);
        // 10 outgoing edges from a; cap at 4 should truncate.
        let kids: Vec<MemoryId> = (0..10).map(|i| mid(700 + i)).collect();
        for &k in &kids {
            put_memory(&metadata, k);
            link_memories(&metadata, a, EdgeKind::References, k, 1.0);
        }

        let retriever = make_retriever_with_db(metadata);
        let config = GraphRetrieverConfig {
            max_branching: 4,
            ..GraphRetrieverConfig::default()
        };
        let result = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(a),
                    depth: 1,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &config,
            )
            .expect("retrieve");

        let mem_count = result
            .iter()
            .filter(|h| matches!(h.id, RankedItemId::Memory(_)))
            .count();
        assert_eq!(
            mem_count, 4,
            "per-hop cap caps unique neighbours, got {mem_count}",
        );
    }

    // -------------------------------------------------------------
    // Both-direction unions outgoing + incoming.
    // -------------------------------------------------------------

    #[test]
    fn walk_direction_both_unions_outgoing_and_incoming() {
        let (_dir, metadata) = fresh_with_metadata();
        let center = mid(801);
        let upstream = mid(802);
        let downstream = mid(803);
        for m in [center, upstream, downstream] {
            put_memory(&metadata, m);
        }
        // upstream → center (Caused is asymmetric so direction matters)
        link_memories(&metadata, upstream, EdgeKind::Caused, center, 1.0);
        // center → downstream
        link_memories(&metadata, center, EdgeKind::Caused, downstream, 1.0);

        let retriever = make_retriever_with_db(metadata);

        let out_only = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(center),
                    depth: 1,
                    direction: Direction::Outgoing,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("out");
        let out_ids: Vec<MemoryId> = out_only
            .iter()
            .filter_map(|i| match i.id {
                RankedItemId::Memory(m) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(
            out_ids,
            vec![downstream],
            "outgoing reaches downstream only"
        );

        let in_only = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(center),
                    depth: 1,
                    direction: Direction::Incoming,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("in");
        let in_ids: Vec<MemoryId> = in_only
            .iter()
            .filter_map(|i| match i.id {
                RankedItemId::Memory(m) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(in_ids, vec![upstream], "incoming reaches upstream only");

        let both = retriever
            .retrieve(
                &GraphQuery::Star {
                    anchor: GraphAnchor::Memory(center),
                    depth: 1,
                    direction: Direction::Both,
                    relation_types: None,
                    include_statements: false,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("both");
        let both_ids: HashSet<MemoryId> = both
            .iter()
            .filter_map(|i| match i.id {
                RankedItemId::Memory(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(both_ids.contains(&upstream));
        assert!(both_ids.contains(&downstream));
    }

    // -------------------------------------------------------------
    // Statements pivot — entity nodes only.
    // -------------------------------------------------------------

    #[test]
    fn walk_with_statements_pivot_emits_statement_ids_for_entity_nodes() {
        use brain_core::{EvidenceRef, Statement, StatementKind, StatementObject, SubjectRef};
        use brain_core::{ExtractorId, StatementId};
        use brain_metadata::schema::predicate::predicate_intern;
        use brain_metadata::statement::statement_create;

        let (_dir, metadata) = fresh_with_metadata();
        let type_id = current_person_type(&metadata);
        let alice = put_entity(&metadata, "Alice", type_id);
        let bob = put_entity(&metadata, "Bob", type_id);

        // Intern a predicate then create a single Fact about Alice
        // pointing at Bob — keeps the object an Entity so we don't
        // depend on literal/value plumbing.
        let predicate_id = {
            let wtxn = metadata.write_txn().expect("wtxn");
            let id = predicate_intern(&wtxn, "brain", "knows_about", None, 0, 1, "", false, 0)
                .expect("predicate");
            wtxn.commit().expect("commit");
            id
        };
        let stmt_id = StatementId::new();
        let stmt = Statement::new_root(
            stmt_id,
            StatementKind::Fact,
            SubjectRef::Entity(alice),
            predicate_id,
            StatementObject::Entity(bob),
            0.9,
            EvidenceRef::default(),
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        );
        {
            let wtxn = metadata.write_txn().expect("wtxn");
            statement_create(&wtxn, &stmt, 1_700_000_000_000_000_000).expect("statement_create");
            wtxn.commit().expect("commit");
        }

        let retriever = make_retriever_with_db(metadata);
        // Subgraph implies include_statements = true.
        let result = retriever
            .retrieve(
                &GraphQuery::Subgraph {
                    anchor: GraphAnchor::Entity(alice),
                    depth: 1,
                },
                &GraphRetrieverConfig::default(),
            )
            .expect("retrieve");

        let saw_statement = result
            .iter()
            .any(|h| matches!(h.id, RankedItemId::Statement(id) if id == stmt_id));
        assert!(
            saw_statement,
            "statement pivot surfaces alice's fact, got {result:?}",
        );
    }
}

// ---------------------------------------------------------------------------
// Property tests — invariants over randomised cyclic graphs.
//
// Cycle-bug regression bait: if `visited.insert(...)` is ever removed or
// reordered, a cycle would emit a node twice or pin it to the wrong hop.
// The walker's contract is that each reachable node appears at most once
// in the result, and never the anchor itself.
// ---------------------------------------------------------------------------

mod property {
    use super::*;

    use std::collections::{HashMap, HashSet, VecDeque};

    use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
    use brain_index::{proximity_score, RankedItemId};
    use brain_metadata::tables::edge::{
        derived_by, link, origin, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
    };
    use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
    use proptest::collection::vec as pvec;
    use proptest::prelude::*;

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    fn put_memory(metadata: &Arc<MetadataDb>, id: MemoryId) {
        let row = MemoryMetadata::new_active(
            id,
            AgentId::from([0u8; 16]),
            ContextId(0),
            id.slot(),
            id.version(),
            MemoryKind::Episodic,
            [0u8; 16],
            0.5,
            0,
            1_700_000_000_000_000_000,
        );
        let wtxn = metadata.write_txn().expect("wtxn");
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open memories");
            t.insert(&id.to_be_bytes(), &row).expect("insert memory");
        }
        wtxn.commit().expect("commit");
    }

    fn link_unique(metadata: &Arc<MetadataDb>, edges: &[(usize, usize)], nodes: &[MemoryId]) {
        // Dedup at construction so the link op never trips on a
        // duplicate key.
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        let wtxn = metadata.write_txn().expect("wtxn");
        {
            let mut out = wtxn.open_table(EDGES_TABLE).expect("out");
            let mut rev = wtxn.open_table(EDGES_REVERSE_TABLE).expect("rev");
            for &(a, b) in edges {
                if a == b {
                    continue;
                }
                if !seen.insert((a, b)) {
                    continue;
                }
                let data = EdgeData::new(
                    1.0,
                    origin::EXPLICIT,
                    derived_by::CLIENT,
                    1_700_000_000_000_000_000,
                );
                link(
                    &mut out,
                    &mut rev,
                    brain_core::NodeRef::Memory(nodes[a]),
                    brain_core::EdgeKindRef::Builtin(EdgeKind::Caused),
                    brain_core::NodeRef::Memory(nodes[b]),
                    zero_disambiguator(),
                    &data,
                )
                .expect("link");
            }
        }
        wtxn.commit().expect("commit");
    }

    /// Compute the ground-truth minimum hop distance from the anchor
    /// to every reachable node via outgoing edges, with the walk
    /// bounded by `depth`. Mirrors the BFS the production walker
    /// performs — independently reimplemented so a regression in the
    /// production code can't accidentally pass.
    fn shortest_hops(
        anchor_idx: usize,
        n: usize,
        edges: &[(usize, usize)],
        depth: u8,
    ) -> HashMap<usize, u8> {
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        for &(a, b) in edges {
            if a == b || a >= n || b >= n {
                continue;
            }
            if seen.insert((a, b)) {
                adj[a].push(b);
            }
        }
        let mut dist: HashMap<usize, u8> = HashMap::new();
        let mut queue: VecDeque<(usize, u8)> = VecDeque::new();
        queue.push_back((anchor_idx, 0));
        dist.insert(anchor_idx, 0);
        while let Some((cur, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            for &nbr in &adj[cur] {
                if let std::collections::hash_map::Entry::Vacant(e) = dist.entry(nbr) {
                    e.insert(d + 1);
                    queue.push_back((nbr, d + 1));
                }
            }
        }
        dist
    }

    proptest! {
        // 32 cases per the plan; cheap fixtures so the whole property
        // runs in well under the per-test budget.
        #![proptest_config(ProptestConfig {
            cases: 32,
            ..ProptestConfig::default()
        })]

        #[test]
        fn walker_emits_each_reachable_node_at_min_depth(
            n in 3usize..=12,
            edge_count in 2usize..=6,
            depth in 1u8..=4,
            edge_pairs in pvec((0usize..12, 0usize..12), 2..=6),
            anchor_pick in 0usize..12,
        ) {
            // Bound the edge index range to the actual node count.
            let edges: Vec<(usize, usize)> = edge_pairs
                .into_iter()
                .take(edge_count)
                .map(|(a, b)| (a % n, b % n))
                .collect();
            let anchor_idx = anchor_pick % n;

            let (_dir, metadata) = fresh_with_metadata();
            // Assign distinct MemoryIds in slot 9000..9000+n so the
            // fixture is reproducible across cases.
            let nodes: Vec<MemoryId> = (0..n).map(|i| mid(9000 + i as u64)).collect();
            for &m in &nodes {
                put_memory(&metadata, m);
            }
            link_unique(&metadata, &edges, &nodes);

            let retriever = make_retriever_with_db(metadata);
            let result = retriever
                .retrieve(
                    &GraphQuery::Star {
                        anchor: GraphAnchor::Memory(nodes[anchor_idx]),
                        depth,
                        direction: Direction::Outgoing,
                        relation_types: None,
                        include_statements: false,
                    },
                    &GraphRetrieverConfig::default(),
                )
                .expect("retrieve");

            // Build reverse lookup: id → idx.
            let id_to_idx: HashMap<MemoryId, usize> =
                nodes.iter().copied().enumerate().map(|(i, m)| (m, i)).collect();

            // Each emitted memory id is unique.
            let mut seen: HashMap<usize, u32> = HashMap::new();
            for item in &result {
                if let RankedItemId::Memory(m) = item.id {
                    let idx = *id_to_idx.get(&m).expect("emitted unknown id");
                    *seen.entry(idx).or_insert(0) += 1;
                }
            }
            for (idx, count) in &seen {
                prop_assert_eq!(
                    *count, 1,
                    "node {} emitted {} times (cycle dedup broken)",
                    idx, count,
                );
            }

            // Anchor itself never emitted.
            prop_assert!(
                !seen.contains_key(&anchor_idx),
                "anchor must never appear in result",
            );

            // For each emitted node, the score matches the min-hop
            // proximity. The production walker computes
            // `proximity_score(d) * weight`; all our edges carry
            // weight = 1.0 so the comparison collapses to the bare
            // proximity_score.
            let truth = shortest_hops(anchor_idx, n, &edges, depth);
            for item in &result {
                if let RankedItemId::Memory(m) = item.id {
                    let idx = *id_to_idx.get(&m).expect("known id");
                    let min_d = truth.get(&idx).copied().expect(
                        "walker emitted an unreachable node",
                    );
                    // The score must equal proximity_score(min_d).
                    let want = proximity_score(min_d);
                    prop_assert!(
                        (item.score - want).abs() < 1e-5,
                        "score for node {} = {}, want proximity_score({}) = {}",
                        idx, item.score, min_d, want,
                    );
                }
            }

            // Emissions ≤ number of reachable nodes (excluding anchor).
            let reachable: usize = truth.keys().filter(|&&k| k != anchor_idx).count();
            let emitted = seen.len();
            prop_assert!(
                emitted <= reachable,
                "emitted {} > reachable {} (visited dedup broken)",
                emitted, reachable,
            );
        }
    }
}
