//! Relation graph traversal.
//!
//! Iterative BFS. Bounded by `max_depth` (cap 5)
//! and `max_branching_factor` (cap 10_000) with visited-set cycle
//! detection. The wire `RELATION_TRAVERSE` opcode (`0x0156`) routes
//! through this module.
//!
//! Pure read path — no writes, no side effects (one tracing::warn
//! when a super-node truncates).

use std::collections::HashSet;

use brain_core::{EntityId, RelationId, RelationTypeId};

use super::ops::{relation_list_from, relation_list_to, RelationListFilter, RelationOpError};

// ---------------------------------------------------------------------------
// Caps + defaults.
// ---------------------------------------------------------------------------

/// Default max depth.
pub const DEFAULT_MAX_DEPTH: u8 = 3;
/// Hard cap on `max_depth`. Callers passing larger values get
/// clamped server-side.
pub const MAX_DEPTH: u8 = 5;
/// Default per-level branching cap.
pub const DEFAULT_MAX_BRANCHING: u32 = 1_000;
/// Hard cap on `max_branching_factor`.
pub const MAX_BRANCHING: u32 = 10_000;
/// Soft cap on total visited nodes. Past this, traversal returns
/// early with what it has gathered (no error). This may later become a
/// wire-visible "truncated" flag.
pub const MAX_TOTAL_VISITED: usize = 100_000;

// ---------------------------------------------------------------------------
// Direction + config.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraversalDirection {
    Outgoing,
    Incoming,
    Both,
}

#[derive(Clone, Copy, Debug)]
pub struct TraversalConfig {
    pub max_depth: u8,
    pub max_branching_factor: u32,
    pub current_only: bool,
}

impl Default for TraversalConfig {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_branching_factor: DEFAULT_MAX_BRANCHING,
            current_only: true,
        }
    }
}

impl TraversalConfig {
    fn clamped_depth(&self) -> u8 {
        self.max_depth.clamp(1, MAX_DEPTH)
    }
    fn clamped_branching(&self) -> u32 {
        self.max_branching_factor.clamp(1, MAX_BRANCHING)
    }
}

// ---------------------------------------------------------------------------
// Paths.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraversalStep {
    pub relation_id: RelationId,
    pub from: EntityId,
    pub to: EntityId,
    pub relation_type: RelationTypeId,
    /// 1-indexed depth from `start`.
    pub depth: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraversalPath {
    pub steps: Vec<TraversalStep>,
}

// ---------------------------------------------------------------------------
// Public entry-point.
// ---------------------------------------------------------------------------

/// BFS the relation graph starting at `start`. Empty `type_filter`
/// matches any relation type.
pub fn traverse(
    rtxn: &redb::ReadTransaction,
    start: EntityId,
    type_filter: &[RelationTypeId],
    direction: TraversalDirection,
    config: &TraversalConfig,
) -> Result<Vec<TraversalPath>, RelationOpError> {
    let max_depth = config.clamped_depth();
    let max_branching = config.clamped_branching();

    let mut visited: HashSet<EntityId> = HashSet::new();
    visited.insert(start);

    // (node, partial_path)
    let mut frontier: Vec<(EntityId, Vec<TraversalStep>)> = vec![(start, Vec::new())];
    let mut paths: Vec<TraversalPath> = Vec::new();

    for current_depth in 1..=max_depth {
        if frontier.is_empty() {
            break;
        }
        let mut next_frontier: Vec<(EntityId, Vec<TraversalStep>)> = Vec::new();

        for (node, partial) in frontier {
            let mut neighbours = expand(rtxn, node, type_filter, direction, config.current_only)?;

            if neighbours.len() > max_branching as usize {
                tracing::warn!(
                    node = ?node,
                    depth = current_depth,
                    out_degree = neighbours.len(),
                    cap = max_branching,
                    "relation_traversal: super-node truncated"
                );
                neighbours.truncate(max_branching as usize);
            }

            for n in neighbours {
                if visited.contains(&n.other) {
                    continue;
                }
                visited.insert(n.other);

                let step = TraversalStep {
                    relation_id: n.relation_id,
                    from: n.directed_from,
                    to: n.directed_to,
                    relation_type: n.relation_type,
                    depth: current_depth,
                };
                let mut new_path = partial.clone();
                new_path.push(step.clone());
                paths.push(TraversalPath {
                    steps: new_path.clone(),
                });
                next_frontier.push((n.other, new_path));

                if visited.len() >= MAX_TOTAL_VISITED {
                    return Ok(paths);
                }
            }
        }

        frontier = next_frontier;
    }

    Ok(paths)
}

// ---------------------------------------------------------------------------
// expand — single-hop neighbour enumeration.
// ---------------------------------------------------------------------------

/// A single edge discovered during expansion.
struct Edge {
    relation_id: RelationId,
    relation_type: RelationTypeId,
    /// The entity on the **other** side of this edge from the
    /// current node. Used for the visited-set check.
    other: EntityId,
    /// The directed `from` / `to` as they appear on the storage row
    /// (canonicalised for symmetric). The traversal step records
    /// what the underlying relation actually says.
    directed_from: EntityId,
    directed_to: EntityId,
}

fn expand(
    rtxn: &redb::ReadTransaction,
    node: EntityId,
    type_filter: &[RelationTypeId],
    direction: TraversalDirection,
    current_only: bool,
) -> Result<Vec<Edge>, RelationOpError> {
    let filter_set: HashSet<u32> = type_filter.iter().map(|t| t.raw()).collect::<HashSet<_>>();
    let want_type = |t: RelationTypeId| type_filter.is_empty() || filter_set.contains(&t.raw());

    let list_filter = RelationListFilter {
        relation_type: None,
        current_only,
        limit: 0, // default cap (1000)
    };

    // Per, symmetric relations are dual-indexed
    // under both endpoints in both BY_FROM and BY_TO. So
    // relation_list_from already returns symmetric edges where
    // `node` is the canonical_to. To avoid double-counting,
    // dedupe by (relation_id, other).
    let mut out: Vec<Edge> = Vec::new();
    let mut seen: HashSet<(RelationId, EntityId)> = HashSet::new();

    let direction_outgoing = matches!(
        direction,
        TraversalDirection::Outgoing | TraversalDirection::Both
    );
    let direction_incoming = matches!(
        direction,
        TraversalDirection::Incoming | TraversalDirection::Both
    );

    if direction_outgoing {
        for r in relation_list_from(rtxn, node, &list_filter)? {
            if !want_type(r.relation_type) {
                continue;
            }
            let other = if r.from_entity == node {
                r.to_entity
            } else if r.is_symmetric && r.to_entity == node {
                r.from_entity
            } else {
                continue;
            };
            if seen.insert((r.id, other)) {
                out.push(Edge {
                    relation_id: r.id,
                    relation_type: r.relation_type,
                    other,
                    directed_from: r.from_entity,
                    directed_to: r.to_entity,
                });
            }
        }
    }
    if direction_incoming {
        for r in relation_list_to(rtxn, node, &list_filter)? {
            if !want_type(r.relation_type) {
                continue;
            }
            let other = if r.to_entity == node {
                r.from_entity
            } else if r.is_symmetric && r.from_entity == node {
                r.to_entity
            } else {
                continue;
            };
            if seen.insert((r.id, other)) {
                out.push(Edge {
                    relation_id: r.id,
                    relation_type: r.relation_type,
                    other,
                    directed_from: r.from_entity,
                    directed_to: r.to_entity,
                });
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::entity::ops::{entity_put, normalize_name};
    use crate::relation::ops::relation_create;
    use crate::relation::types::relation_type_intern;
    use brain_core::{Cardinality, ExtractorId};
    use brain_core::{Entity, EntityType, Relation};

    fn open_db() -> (tempfile::TempDir, crate::MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    fn make_entity(db: &mut crate::MetadataDb, name: &str) -> EntityId {
        let id = EntityId::new();
        let n = normalize_name(name);
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            n,
            1_700_000_000_000_000_000,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_type(
        db: &mut crate::MetadataDb,
        name: &str,
        cardinality: Cardinality,
        symmetric: bool,
    ) -> RelationTypeId {
        let wtxn = db.write_txn().unwrap();
        let id = relation_type_intern(
            &wtxn,
            "test",
            name,
            None,
            None,
            cardinality,
            symmetric,
            1,
            "",
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn link(
        db: &mut crate::MetadataDb,
        t: RelationTypeId,
        from: EntityId,
        to: EntityId,
        symmetric: bool,
    ) -> RelationId {
        let r = Relation::new_root(
            RelationId::new(),
            t,
            from,
            to,
            0.9,
            vec![],
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            symmetric,
        );
        let wtxn = db.write_txn().unwrap();
        let id = relation_create(&wtxn, &r, 0).unwrap();
        wtxn.commit().unwrap();
        id
    }

    // ----- One hop -----

    #[test]
    fn one_hop_outgoing() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-1h");
        let b = make_entity(&mut db, "b-1h");
        let t = intern_type(&mut db, "knows_1h", Cardinality::ManyToMany, false);
        link(&mut db, t, a, b, false);

        let rtxn = db.read_txn().unwrap();
        let paths = traverse(
            &rtxn,
            a,
            &[],
            TraversalDirection::Outgoing,
            &TraversalConfig::default(),
        )
        .unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].steps.len(), 1);
        assert_eq!(paths[0].steps[0].to, b);
        assert_eq!(paths[0].steps[0].depth, 1);
    }

    #[test]
    fn one_hop_incoming() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-in");
        let b = make_entity(&mut db, "b-in");
        let t = intern_type(&mut db, "knows_in", Cardinality::ManyToMany, false);
        link(&mut db, t, a, b, false);

        let rtxn = db.read_txn().unwrap();
        let paths = traverse(
            &rtxn,
            b,
            &[],
            TraversalDirection::Incoming,
            &TraversalConfig::default(),
        )
        .unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].steps[0].from, a);
        assert_eq!(paths[0].steps[0].to, b);
    }

    // ----- Two and three hops -----

    #[test]
    fn outgoing_traversal_returns_paths_at_each_depth_up_to_two_hops() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-2h");
        let b = make_entity(&mut db, "b-2h");
        let c = make_entity(&mut db, "c-2h");
        let t = intern_type(&mut db, "knows_2h", Cardinality::ManyToMany, false);
        link(&mut db, t, a, b, false);
        link(&mut db, t, b, c, false);

        let rtxn = db.read_txn().unwrap();
        let paths = traverse(
            &rtxn,
            a,
            &[],
            TraversalDirection::Outgoing,
            &TraversalConfig::default(),
        )
        .unwrap();
        // Expect two paths: depth 1 (A→B) and depth 2 (A→B→C).
        assert_eq!(paths.len(), 2);
        let d1 = paths.iter().find(|p| p.steps.len() == 1).unwrap();
        let d2 = paths.iter().find(|p| p.steps.len() == 2).unwrap();
        assert_eq!(d1.steps[0].to, b);
        assert_eq!(d2.steps[1].to, c);
    }

    #[test]
    fn depth_cap_clamps() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-cap");
        let b = make_entity(&mut db, "b-cap");
        let t = intern_type(&mut db, "knows_cap", Cardinality::ManyToMany, false);
        link(&mut db, t, a, b, false);

        let rtxn = db.read_txn().unwrap();
        let config = TraversalConfig {
            max_depth: 99,
            max_branching_factor: 1000,
            current_only: true,
        };
        // Should not panic; depth clamped to MAX_DEPTH.
        let paths = traverse(&rtxn, a, &[], TraversalDirection::Outgoing, &config).unwrap();
        assert_eq!(paths.len(), 1);
    }

    // ----- Cycle and self-loop -----

    #[test]
    fn cycle_short_circuits() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-cyc");
        let b = make_entity(&mut db, "b-cyc");
        let t = intern_type(&mut db, "knows_cyc", Cardinality::ManyToMany, false);
        link(&mut db, t, a, b, false);
        link(&mut db, t, b, a, false);

        let rtxn = db.read_txn().unwrap();
        let paths = traverse(
            &rtxn,
            a,
            &[],
            TraversalDirection::Outgoing,
            &TraversalConfig::default(),
        )
        .unwrap();
        // Visit B at depth 1; B's outgoing → A is suppressed by the
        // visited set. Single path of length 1.
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].steps[0].to, b);
    }

    #[test]
    fn self_loop_visits_once() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-self");
        let t = intern_type(&mut db, "knows_self_tr", Cardinality::ManyToMany, false);
        link(&mut db, t, a, a, false);

        let rtxn = db.read_txn().unwrap();
        let paths = traverse(
            &rtxn,
            a,
            &[],
            TraversalDirection::Outgoing,
            &TraversalConfig::default(),
        )
        .unwrap();
        // start (a) is pre-inserted in visited; the self-loop's
        // "other" is also a, which is already visited → suppressed.
        // Zero paths.
        assert_eq!(paths.len(), 0);
    }

    // ----- Symmetric -----

    #[test]
    fn symmetric_reachable_from_either_side() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-sym-tr");
        let b = make_entity(&mut db, "b-sym-tr");
        let t = intern_type(&mut db, "co_authored", Cardinality::ManyToMany, true);
        link(&mut db, t, a, b, true);

        let rtxn = db.read_txn().unwrap();
        let from_a = traverse(
            &rtxn,
            a,
            &[],
            TraversalDirection::Outgoing,
            &TraversalConfig::default(),
        )
        .unwrap();
        let from_b = traverse(
            &rtxn,
            b,
            &[],
            TraversalDirection::Outgoing,
            &TraversalConfig::default(),
        )
        .unwrap();
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_b.len(), 1);
        assert_eq!(from_a[0].steps[0].depth, 1);
    }

    // ----- Type filter -----

    #[test]
    fn traversal_type_filter_follows_only_requested_relation_type() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-tf");
        let b = make_entity(&mut db, "b-tf");
        let c = make_entity(&mut db, "c-tf");
        let knows = intern_type(&mut db, "knows_tf", Cardinality::ManyToMany, false);
        let other = intern_type(&mut db, "other_tf", Cardinality::ManyToMany, false);
        link(&mut db, knows, a, b, false);
        link(&mut db, other, a, c, false);

        let rtxn = db.read_txn().unwrap();
        let paths = traverse(
            &rtxn,
            a,
            &[knows],
            TraversalDirection::Outgoing,
            &TraversalConfig::default(),
        )
        .unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].steps[0].to, b);
        assert_eq!(paths[0].steps[0].relation_type, knows);
    }

    // ----- current_only -----

    #[test]
    fn current_only_excludes_tombstoned() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-co");
        let b = make_entity(&mut db, "b-co");
        let t = intern_type(&mut db, "knows_co", Cardinality::ManyToMany, false);
        let rel_id = link(&mut db, t, a, b, false);

        // Tombstone.
        let wtxn = db.write_txn().unwrap();
        crate::relation::ops::relation_tombstone(&wtxn, rel_id, 1).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let cfg = TraversalConfig {
            max_depth: DEFAULT_MAX_DEPTH,
            max_branching_factor: DEFAULT_MAX_BRANCHING,
            current_only: true,
        };
        let paths = traverse(&rtxn, a, &[], TraversalDirection::Outgoing, &cfg).unwrap();
        assert!(paths.is_empty(), "tombstoned edge excluded");
    }

    // ----- Direction filter -----

    #[test]
    fn direction_outgoing_excludes_incoming() {
        let (_dir, mut db) = open_db();
        let a = make_entity(&mut db, "a-dir");
        let b = make_entity(&mut db, "b-dir");
        let c = make_entity(&mut db, "c-dir");
        let t = intern_type(&mut db, "knows_dir", Cardinality::ManyToMany, false);
        link(&mut db, t, a, b, false); // outgoing from a
        link(&mut db, t, c, a, false); // incoming to a

        let rtxn = db.read_txn().unwrap();
        let out = traverse(
            &rtxn,
            a,
            &[],
            TraversalDirection::Outgoing,
            &TraversalConfig::default(),
        )
        .unwrap();
        let inc = traverse(
            &rtxn,
            a,
            &[],
            TraversalDirection::Incoming,
            &TraversalConfig::default(),
        )
        .unwrap();
        let both = traverse(
            &rtxn,
            a,
            &[],
            TraversalDirection::Both,
            &TraversalConfig::default(),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].steps[0].to, b);
        assert_eq!(inc.len(), 1);
        assert_eq!(inc[0].steps[0].from, c);
        assert_eq!(both.len(), 2);
    }
}
