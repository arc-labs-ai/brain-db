//! Property tests for relation-graph semantics: cardinality
//! enforcement, symmetric mirroring, and BFS traversal (depth cap +
//! cycle termination + BFS-closure equivalence).
//!
//! These fuzz the invariants that the example-based unit tests in
//! `relation::ops` / `relation::traversal` pin at single points. They
//! back the §19.01 correctness bullets and the combined-acceptance
//! relation lines (create / cardinality / symmetric / traversal 1-3 /
//! traversal >5 / cycle-detection).
//!
//! Determinism note: proptest drives the *structure* of each case
//! (cardinality, entity count, the edge list, requested depth). Entity
//! and relation ids are freshly minted (`EntityId::new` /
//! `RelationId::new`), so the concrete byte values differ per run — but
//! every asserted invariant must hold for *any* id assignment, so any
//! failure proptest surfaces is a real, structurally-reproducible bug.
//! Ground truth for the traversal closure is read back out of storage,
//! so the check never assumes a particular canonicalisation or
//! dedup outcome — it compares traversal against the graph as stored.

use std::collections::{HashMap, HashSet};

use brain_core::{
    canonical_pair, Cardinality, Entity, EntityId, EntityType, ExtractorId, Relation, RelationId,
    RelationTypeId, SessionId,
};
use brain_metadata::entity::ops::{entity_put, normalize_name};
use brain_metadata::relation::types::relation_type_intern;
use brain_metadata::{
    relation_create, relation_get, relation_list_from, relation_list_to, traverse, MetadataDb,
    RelationListFilter, RowScope, TraversalConfig, TraversalDirection, MAX_DEPTH,
};
use proptest::prelude::*;

const T0: u64 = 1_700_000_000_000_000_000;

fn scope() -> RowScope {
    RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
}

fn open_db() -> (tempfile::TempDir, MetadataDb) {
    let dir = tempfile::tempdir().unwrap();
    let db = MetadataDb::open(dir.path().join("md.redb")).unwrap();
    (dir, db)
}

/// Insert a fresh Person entity, returning its id.
fn make_entity(db: &MetadataDb, name: &str) -> EntityId {
    let id = EntityId::new();
    let e = Entity::new_active(
        id,
        EntityType::PERSON_ID,
        name.into(),
        normalize_name(name),
        T0,
    );
    let wtxn = db.write_txn().unwrap();
    entity_put(&wtxn, scope(), SessionId::DEFAULT, &e).unwrap();
    wtxn.commit().unwrap();
    id
}

/// Intern a relation type with no declared endpoint types (fully
/// permissive `Any` endpoints, so the direction/endpoint-type defence
/// never fires and cardinality + symmetry are the only constraints).
fn intern_type(
    db: &MetadataDb,
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
        T0,
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

fn fresh_rel(t: RelationTypeId, from: EntityId, to: EntityId, symmetric: bool) -> Relation {
    Relation::new_root(
        RelationId::new(),
        t,
        from,
        to,
        0.9,
        vec![],
        ExtractorId::from(0),
        T0,
        symmetric,
    )
}

/// Attempt a create; swallow a `CardinalityViolation` (the documented
/// two-sided-conflict rejection for OneToOne) so the caller can keep
/// applying the rest of the sequence. Any other error is a test bug and
/// panics.
fn try_create(db: &MetadataDb, r: &Relation) {
    let wtxn = db.write_txn().unwrap();
    match relation_create(&wtxn, scope(), SessionId::DEFAULT, r, T0) {
        Ok(_) => {
            wtxn.commit().unwrap();
        }
        Err(brain_metadata::RelationOpError::CardinalityViolation { .. }) => {
            // Pre-existing on-disk conflict (>1). The create is rejected
            // and the graph is left untouched — exactly the invariant
            // Property 1 asserts holds afterwards. Drop the txn.
            drop(wtxn);
        }
        Err(e) => panic!("unexpected relation_create error: {e:?}"),
    }
}

fn type_filter(t: RelationTypeId) -> RelationListFilter {
    RelationListFilter {
        relation_type: Some(t),
        current_only: true,
        limit: 0,
    }
}

/// Collect every current relation of type `t` by unioning
/// `relation_list_from` over the whole entity set (each current relation
/// has its `from` among them), deduped by id.
fn current_relations(db: &MetadataDb, entities: &[EntityId], t: RelationTypeId) -> Vec<Relation> {
    let rtxn = db.read_txn().unwrap();
    let mut seen: HashSet<RelationId> = HashSet::new();
    let mut out = Vec::new();
    for &e in entities {
        for r in relation_list_from(&rtxn, scope(), e, &type_filter(t)).unwrap() {
            if seen.insert(r.id) {
                out.push(r);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Strategies.
// ---------------------------------------------------------------------------

fn arb_cardinality() -> impl Strategy<Value = Cardinality> {
    prop_oneof![
        Just(Cardinality::OneToOne),
        Just(Cardinality::OneToMany),
        Just(Cardinality::ManyToOne),
        Just(Cardinality::ManyToMany),
    ]
}

/// (entity_count, edge list of (from_idx, to_idx)) over a small graph.
fn arb_graph() -> impl Strategy<Value = (usize, Vec<(usize, usize)>)> {
    (3usize..=6).prop_flat_map(|n| (Just(n), prop::collection::vec((0..n, 0..n), 0..=10)))
}

// ---------------------------------------------------------------------------
// Property 1 — cardinality is never violated.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    #[test]
    fn cardinality_invariant_holds_over_random_creates(
        cardinality in arb_cardinality(),
        (n, edges) in arb_graph(),
    ) {
        let (_dir, db) = open_db();
        let entities: Vec<EntityId> =
            (0..n).map(|i| make_entity(&db, &format!("card-e{i}"))).collect();
        let t = intern_type(&db, "card_rel", cardinality, false);

        for (i, j) in &edges {
            let r = fresh_rel(t, entities[*i], entities[*j], false);
            try_create(&db, &r);
        }

        let current = current_relations(&db, &entities, t);

        // No orphaned index entries: every relation the directional
        // index surfaces resolves via the sidecar point lookup.
        {
            let rtxn = db.read_txn().unwrap();
            for r in &current {
                prop_assert!(
                    relation_get(&rtxn, r.id).unwrap().is_some(),
                    "index surfaced relation {:?} with no sidecar row",
                    r.id,
                );
            }
        }

        match cardinality {
            Cardinality::ManyToMany => {
                // No constraint on either side, but the dedup gate keeps
                // active (from, to) tuples unique.
                let mut tuples: HashSet<(EntityId, EntityId)> = HashSet::new();
                for r in &current {
                    prop_assert!(
                        tuples.insert((r.from_entity, r.to_entity)),
                        "duplicate current (from,to) tuple for ManyToMany: {:?}->{:?}",
                        r.from_entity, r.to_entity,
                    );
                }
            }
            Cardinality::ManyToOne => {
                // At most one current relation per `from`.
                let mut froms: HashSet<EntityId> = HashSet::new();
                for r in &current {
                    prop_assert!(
                        froms.insert(r.from_entity),
                        "ManyToOne: two current relations share from={:?}",
                        r.from_entity,
                    );
                }
            }
            Cardinality::OneToMany => {
                // At most one current relation per `to`.
                let mut tos: HashSet<EntityId> = HashSet::new();
                for r in &current {
                    prop_assert!(
                        tos.insert(r.to_entity),
                        "OneToMany: two current relations share to={:?}",
                        r.to_entity,
                    );
                }
            }
            Cardinality::OneToOne => {
                // OneToOne enforces BOTH directed constraints at once, per
                // the spec's supersession pseudocode (`lookup_current_from`
                // on N.from AND `lookup_current_to` on N.to): each entity is
                // the `from` of at most one current relation AND the `to` of
                // at most one. It deliberately does NOT couple the roles —
                // an entity may be a `from` in one relation and a `to` in
                // another (the "touching either side" prose is realised via
                // dual indexing only for symmetric types). The two-sided
                // conflict a single create can raise is rejected with
                // CardinalityViolation (swallowed by `try_create`), so the
                // stored graph always satisfies both directed bounds.
                let mut froms: HashSet<EntityId> = HashSet::new();
                let mut tos: HashSet<EntityId> = HashSet::new();
                for r in &current {
                    prop_assert!(
                        froms.insert(r.from_entity),
                        "OneToOne: two current relations share from={:?}",
                        r.from_entity,
                    );
                    prop_assert!(
                        tos.insert(r.to_entity),
                        "OneToOne: two current relations share to={:?}",
                        r.to_entity,
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property 2 — symmetric relations are queryable from both endpoints;
// asymmetric relations are directional.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    #[test]
    fn symmetric_is_bidirectional_asymmetric_is_directional(
        // Two distinct entity indices out of a 4-entity set.
        a_idx in 0usize..4,
        b_off in 1usize..4,
    ) {
        let (_dir, db) = open_db();
        let entities: Vec<EntityId> =
            (0..4).map(|i| make_entity(&db, &format!("sym-e{i}"))).collect();
        let a = entities[a_idx];
        let b = entities[(a_idx + b_off) % 4];
        prop_assume!(a != b);

        let sym = intern_type(&db, "sym_rel", Cardinality::ManyToMany, true);
        let asym = intern_type(&db, "asym_rel", Cardinality::ManyToMany, false);

        // --- Symmetric: create (A, B). ---
        let r = fresh_rel(sym, a, b, true);
        let sym_id = {
            let wtxn = db.write_txn().unwrap();
            let id = relation_create(&wtxn, scope(), SessionId::DEFAULT, &r, T0).unwrap();
            wtxn.commit().unwrap();
            id
        };

        let rtxn = db.read_txn().unwrap();

        // Surfaced from BOTH endpoints (dual-indexed mirror).
        let from_a = relation_list_from(&rtxn, scope(), a, &type_filter(sym)).unwrap();
        let from_b = relation_list_from(&rtxn, scope(), b, &type_filter(sym)).unwrap();
        prop_assert!(from_a.iter().any(|x| x.id == sym_id), "symmetric not visible from A");
        prop_assert!(from_b.iter().any(|x| x.id == sym_id), "symmetric not visible from B");

        // Stored in canonical (from < to) order.
        let (cf, ct) = canonical_pair(a, b);
        let got = relation_get(&rtxn, sym_id).unwrap().unwrap();
        prop_assert_eq!(got.from_entity, cf);
        prop_assert_eq!(got.to_entity, ct);
        prop_assert!(got.is_symmetric);
        drop(rtxn);

        // (B, A) resolves to the SAME edge — the reverse create dedups.
        let mirror = fresh_rel(sym, b, a, true);
        {
            let wtxn = db.write_txn().unwrap();
            let mirror_id =
                relation_create(&wtxn, scope(), SessionId::DEFAULT, &mirror, T0).unwrap();
            wtxn.commit().unwrap();
            prop_assert_eq!(mirror_id, sym_id, "(B,A) did not dedup onto (A,B)");
        }
        let rtxn = db.read_txn().unwrap();
        let from_a2 = relation_list_from(&rtxn, scope(), a, &type_filter(sym)).unwrap();
        prop_assert_eq!(from_a2.len(), 1, "reverse create leaked a duplicate edge");
        drop(rtxn);

        // --- Asymmetric: create (A, B) is one-directional. ---
        let ar = fresh_rel(asym, a, b, false);
        let asym_id = {
            let wtxn = db.write_txn().unwrap();
            let id = relation_create(&wtxn, scope(), SessionId::DEFAULT, &ar, T0).unwrap();
            wtxn.commit().unwrap();
            id
        };
        let rtxn = db.read_txn().unwrap();
        let af = |e| relation_list_from(&rtxn, scope(), e, &type_filter(asym)).unwrap();
        let at = |e| relation_list_to(&rtxn, scope(), e, &type_filter(asym)).unwrap();

        prop_assert!(af(a).iter().any(|x| x.id == asym_id), "asym: list_from(A) missing it");
        prop_assert!(!af(b).iter().any(|x| x.id == asym_id), "asym: list_from(B) surfaced it");
        prop_assert!(at(b).iter().any(|x| x.id == asym_id), "asym: list_to(B) missing it");
        prop_assert!(!at(a).iter().any(|x| x.id == asym_id), "asym: list_to(A) surfaced it");
    }
}

// ---------------------------------------------------------------------------
// Property 3 — traversal terminates, respects the depth cap, and its
// reachable set equals the independent BFS closure over the stored graph.
// ---------------------------------------------------------------------------

/// Independent BFS closure to `depth` hops over a directed adjacency map,
/// mirroring the traversal's shortest-hop visited-set semantics.
fn bfs_closure(
    adj: &HashMap<EntityId, Vec<EntityId>>,
    start: EntityId,
    depth: u8,
) -> HashSet<EntityId> {
    let mut visited: HashSet<EntityId> = HashSet::new();
    visited.insert(start);
    let mut frontier = vec![start];
    for _ in 0..depth {
        let mut next = Vec::new();
        for node in frontier {
            if let Some(nbrs) = adj.get(&node) {
                for &nb in nbrs {
                    if visited.insert(nb) {
                        next.push(nb);
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    visited
}

/// The set of entities reachable per a traversal result (start plus every
/// step's outgoing endpoint).
fn reached_from_paths(
    start: EntityId,
    paths: &[brain_metadata::TraversalPath],
) -> HashSet<EntityId> {
    let mut set = HashSet::new();
    set.insert(start);
    for p in paths {
        for step in &p.steps {
            set.insert(step.to);
        }
    }
    set
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    #[test]
    fn traversal_terminates_and_matches_bfs_closure(
        (n, edges) in arb_graph(),
        start_idx in 0usize..6,
        depth in 1u8..=MAX_DEPTH,
    ) {
        let (_dir, db) = open_db();
        let entities: Vec<EntityId> =
            (0..n).map(|i| make_entity(&db, &format!("trav-e{i}"))).collect();
        // Asymmetric ManyToMany: directed edges, cycles possible, no
        // supersession — the stored graph is exactly the edge list minus
        // exact-duplicate collapses.
        let t = intern_type(&db, "trav_rel", Cardinality::ManyToMany, false);
        for (i, j) in &edges {
            let r = fresh_rel(t, entities[*i], entities[*j], false);
            try_create(&db, &r);
        }

        let start = entities[start_idx % n];

        // Ground-truth adjacency, read back out of storage.
        let rtxn = db.read_txn().unwrap();
        let mut adj: HashMap<EntityId, Vec<EntityId>> = HashMap::new();
        for &e in &entities {
            let mut nbrs = Vec::new();
            for r in relation_list_from(&rtxn, scope(), e, &type_filter(t)).unwrap() {
                // Asymmetric outgoing: e is always the `from`.
                nbrs.push(r.to_entity);
            }
            adj.insert(e, nbrs);
        }

        // Traverse at `depth`. Returning at all proves termination even
        // through cycles (the visited set bounds it).
        let cfg = TraversalConfig { max_depth: depth, ..TraversalConfig::default() };
        let paths = traverse(&rtxn, scope(), start, &[], TraversalDirection::Outgoing, &cfg)
            .unwrap();

        // Depth cap: no path or step exceeds the requested depth.
        for p in &paths {
            prop_assert!(
                p.steps.len() <= depth as usize,
                "path length {} exceeds requested depth {}", p.steps.len(), depth,
            );
            for step in &p.steps {
                prop_assert!(step.depth <= depth, "step depth {} > {}", step.depth, depth);
            }
        }

        // Reachable set equals the independent BFS closure to `depth`.
        let reached = reached_from_paths(start, &paths);
        let closure = bfs_closure(&adj, start, depth);
        prop_assert_eq!(&reached, &closure, "traversal reachable set != BFS closure");

        // Over-cap request is clamped to MAX_DEPTH, not honoured verbatim:
        // depth 99 yields exactly the depth-MAX_DEPTH closure and never a
        // deeper path. (The wire handler's hard reject of depth > cap
        // lives in brain-ops; here the backing clamp is asserted.)
        let cfg_over = TraversalConfig { max_depth: 99, ..TraversalConfig::default() };
        let paths_over =
            traverse(&rtxn, scope(), start, &[], TraversalDirection::Outgoing, &cfg_over).unwrap();
        for p in &paths_over {
            prop_assert!(
                p.steps.len() <= MAX_DEPTH as usize,
                "over-cap path length {} exceeds MAX_DEPTH {}", p.steps.len(), MAX_DEPTH,
            );
        }
        let reached_over = reached_from_paths(start, &paths_over);
        let closure_cap = bfs_closure(&adj, start, MAX_DEPTH);
        prop_assert_eq!(&reached_over, &closure_cap, "clamped traversal != MAX_DEPTH closure");
    }
}

// ---------------------------------------------------------------------------
// Bookends: static cases so a broken generator can't silently turn a
// proptest into a no-op.
// ---------------------------------------------------------------------------

#[test]
fn many_to_one_supersedes_prior_from_side() {
    let (_dir, db) = open_db();
    let a = make_entity(&db, "mto-a");
    let b = make_entity(&db, "mto-b");
    let c = make_entity(&db, "mto-c");
    let t = intern_type(&db, "reports_to_bk", Cardinality::ManyToOne, false);

    try_create(&db, &fresh_rel(t, a, b, false));
    try_create(&db, &fresh_rel(t, a, c, false)); // same `from` — supersedes

    let current = current_relations(&db, &[a, b, c], t);
    assert_eq!(current.len(), 1, "ManyToOne kept both current");
    assert_eq!(current[0].to_entity, c, "latest assertion should win");
}

#[test]
fn symmetric_bookend_visible_from_both_sides() {
    let (_dir, db) = open_db();
    let a = make_entity(&db, "symbk-a");
    let b = make_entity(&db, "symbk-b");
    let t = intern_type(&db, "married_to_bk", Cardinality::ManyToMany, true);
    let r = fresh_rel(t, a, b, true);
    let id = {
        let wtxn = db.write_txn().unwrap();
        let id = relation_create(&wtxn, scope(), SessionId::DEFAULT, &r, T0).unwrap();
        wtxn.commit().unwrap();
        id
    };
    let rtxn = db.read_txn().unwrap();
    assert!(relation_list_from(&rtxn, scope(), a, &type_filter(t))
        .unwrap()
        .iter()
        .any(|x| x.id == id));
    assert!(relation_list_from(&rtxn, scope(), b, &type_filter(t))
        .unwrap()
        .iter()
        .any(|x| x.id == id));
}
