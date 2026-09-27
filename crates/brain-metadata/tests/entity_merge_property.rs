//! Property + chaos coverage for the entity merge / unmerge mechanic.
//!
//! The existing coverage (colocated unit tests in `entity/merge.rs` and
//! the `entity_merge_wire.rs` integration test in `brain-server`) is
//! example-based. This file adds the two missing layers the acceptance
//! gate (`spec/19_benchmarks/01_correctness_and_durability.md`) calls
//! for on a state-mutating graph op:
//!
//! 1. **Property — merge/unmerge is a faithful inverse (within grace).**
//!    Over a randomized small entity graph, MERGE then UNMERGE and assert
//!    the graph is semantically identical to the pre-merge state: every
//!    statement/relation endpoint restored, the survivor's folded
//!    aliases / mention_count / attributes stripped back, the merged
//!    entity resolvable again, and no row left pointing at a stale id.
//!    The post-MERGE (pre-unmerge) convergence invariant is asserted in
//!    the same pass.
//!
//! 2. **Property — merge convergence / no-orphan.** After a merge, every
//!    statement/relation that referenced the merged entity references the
//!    survivor, the merged entity is not resolver-reachable by name, and
//!    the rerouted counts in the `MergeOutcome` equal the rows actually
//!    rerouted.
//!
//! 3. **Chaos — merge atomicity.** A merge is a single redb write
//!    transaction. The atomicity contract is verified two ways: dropping
//!    the `WriteTransaction` before commit leaves the pre-merge state
//!    byte-for-byte intact (redb rollback — no half-merge is ever
//!    observable), and a committed merge survives a full DB reopen with
//!    every reroute + redirect applied (the "fully applied" leg).
//!
//! Graph shape is what proptest randomizes (entity count, types, alias
//! overlap, mention counts, attribute blobs, and the statement/relation
//! adjacency); concrete UUIDs are minted fresh per case. Names are kept
//! unique per entity so the pre-merge secondary indexes are collision-
//! free (two entities sharing one normalized canonical name of the same
//! type would be an invalid fixture, not a merge bug), while aliases are
//! drawn from a shared pool so the survivor/merged alias-dedup path is
//! exercised.
//!
//! Each case opens a fresh redb tempfile, so case counts are kept modest
//! (48 / 48 / 32). If proptest contends on redb at high parallelism, run
//! with `--test-threads 4`.

use brain_core::{
    Cardinality, Entity, EntityAttributes, EntityId, EntityType, EntityTypeId, EvidenceRef,
    ExtractorId, PredicateId, Relation, RelationId, RelationTypeId, SessionId, Statement,
    StatementId, StatementKind, StatementObject, SubjectRef,
};
use brain_metadata::entity::merge::{merge_entity, unmerge_entity, EntityMergeOpError, MergeActor};
use brain_metadata::{
    entity_get, entity_get_resolved, entity_lookup_by_canonical_name, entity_put,
    entity_type_intern, normalize_name, predicate_intern, relation_create, relation_get,
    relation_list_from, relation_list_to, relation_type_intern, statement_create, statement_get,
    statement_list, MetadataDb, RelationListFilter, RowScope, StatementListFilter,
};
use proptest::prelude::*;

const NOW: u64 = 1_700_000_000_000_000_000;
const MERGE_AT: u64 = NOW + 60_000_000_000; // +1 minute
const UNMERGE_AT: u64 = MERGE_AT + 1_000_000_000; // well inside grace
const GRACE_SECS: u64 = 7 * 24 * 60 * 60;

fn scope() -> RowScope {
    RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
}

// ---------------------------------------------------------------------------
// Randomized graph specification.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct EntitySpec {
    /// Type selector: `false` → seeded `Person`, `true` → interned `Org`.
    is_org: bool,
    /// Alias pool indices (0..6); shared pool so survivor/merged can
    /// overlap and exercise the dedup fold.
    alias_ids: Vec<u8>,
    mention: u32,
    /// 0 → no attributes; else a small distinct blob.
    attr: u8,
}

#[derive(Clone, Debug)]
struct GraphSpec {
    entities: Vec<EntitySpec>,
    survivor: usize,
    merged: usize,
    statements: Vec<(usize, usize)>,
    relations: Vec<(usize, usize)>,
}

fn entity_spec() -> impl Strategy<Value = EntitySpec> {
    (
        any::<bool>(),
        prop::collection::vec(0u8..6, 0..=2),
        0u32..100,
        0u8..4,
    )
        .prop_map(|(is_org, alias_ids, mention, attr)| EntitySpec {
            is_org,
            alias_ids,
            mention,
            attr,
        })
}

fn graph_spec() -> impl Strategy<Value = GraphSpec> {
    prop::collection::vec(entity_spec(), 2..=5)
        .prop_flat_map(|entities| {
            let n = entities.len();
            (
                Just(entities),
                0..n,
                0..(n - 1),
                prop::collection::vec((0..n, 0..n), 0..=6),
                prop::collection::vec((0..n, 0..n), 0..=6),
            )
        })
        .prop_map(
            |(mut entities, survivor, merged_off, statements, relations)| {
                let n = entities.len();
                // Guarantee `merged != survivor`.
                let merged = (survivor + 1 + merged_off) % n;
                // Merge is intra-type: force the pair to share a type.
                let ty = entities[survivor].is_org;
                entities[merged].is_org = ty;
                GraphSpec {
                    entities,
                    survivor,
                    merged,
                    statements,
                    relations,
                }
            },
        )
}

// ---------------------------------------------------------------------------
// Built graph + fixture construction.
// ---------------------------------------------------------------------------

struct BuiltGraph {
    entity_ids: Vec<EntityId>,
    stmt_ids: Vec<StatementId>,
    rel_ids: Vec<RelationId>,
    /// `(subject_idx, object_idx)` aligned with `stmt_ids`.
    stmt_adj: Vec<(usize, usize)>,
    /// `(from_idx, to_idx)` aligned with `rel_ids`.
    rel_adj: Vec<(usize, usize)>,
}

impl BuiltGraph {
    fn survivor(&self, spec: &GraphSpec) -> EntityId {
        self.entity_ids[spec.survivor]
    }
    fn merged(&self, spec: &GraphSpec) -> EntityId {
        self.entity_ids[spec.merged]
    }
    /// Distinct statements that touch the merged entity on either side.
    fn stmts_touching_merged(&self, spec: &GraphSpec) -> usize {
        self.stmt_adj
            .iter()
            .filter(|(s, o)| *s == spec.merged || *o == spec.merged)
            .count()
    }
    /// Distinct relations with the merged entity on either endpoint.
    fn rels_touching_merged(&self, spec: &GraphSpec) -> usize {
        self.rel_adj
            .iter()
            .filter(|(f, t)| *f == spec.merged || *t == spec.merged)
            .count()
    }
}

fn build(spec: &GraphSpec) -> (MetadataDb, tempfile::TempDir, BuiltGraph) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetadataDb::open(dir.path().join("metadata.redb")).expect("open metadata");

    // A second, non-seeded entity type so the strategy can vary types.
    let org_ty: EntityTypeId = {
        let wtxn = db.write_txn().unwrap();
        let id = entity_type_intern(&wtxn, "Org", Vec::new(), NOW).unwrap();
        wtxn.commit().unwrap();
        id
    };

    // Entities: unique canonical names, aliases from the shared pool.
    let mut entity_ids = Vec::with_capacity(spec.entities.len());
    for (i, es) in spec.entities.iter().enumerate() {
        let ty = if es.is_org {
            org_ty
        } else {
            EntityType::PERSON_ID
        };
        let canonical = format!("Ent{i}");
        let mut e = Entity::new_active(
            EntityId::new(),
            ty,
            canonical.clone(),
            normalize_name(&canonical),
            NOW,
        );
        let mut aliases: Vec<String> = es.alias_ids.iter().map(|a| format!("alias{a}")).collect();
        aliases.sort();
        aliases.dedup();
        e.aliases = aliases;
        e.mention_count = es.mention;
        if es.attr != 0 {
            e.attributes = EntityAttributes::from(vec![es.attr; usize::from(es.attr)]);
        }
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, scope(), SessionId::DEFAULT, &e).unwrap();
        wtxn.commit().unwrap();
        entity_ids.push(e.id);
    }

    // One distinct predicate per statement slot — keeps every
    // (subject, predicate) pair unique so no statement supersedes
    // another (each is its own single-member chain).
    let mut stmt_ids = Vec::with_capacity(spec.statements.len());
    let mut stmt_adj = Vec::with_capacity(spec.statements.len());
    for (k, &(subj_idx, obj_idx)) in spec.statements.iter().enumerate() {
        let pred: PredicateId = {
            let wtxn = db.write_txn().unwrap();
            let id = predicate_intern(
                &wtxn,
                "test",
                &format!("pred{k}"),
                Some(StatementKind::Fact),
                1u8, // object: Entity
                1,
                "",
                false,
                NOW,
            )
            .unwrap();
            wtxn.commit().unwrap();
            id
        };
        let s = Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(entity_ids[subj_idx]),
            pred,
            StatementObject::Entity(entity_ids[obj_idx]),
            0.9,
            EvidenceRef::default(),
            ExtractorId::from(0),
            NOW,
            1,
        );
        let wtxn = db.write_txn().unwrap();
        let id = statement_create(&wtxn, scope(), SessionId::DEFAULT, &s, NOW).unwrap();
        wtxn.commit().unwrap();
        stmt_ids.push(id);
        stmt_adj.push((subj_idx, obj_idx));
    }

    // One distinct relation type per relation slot; alternate symmetry
    // so both the asymmetric and the symmetric-canonicalization reroute
    // paths are hit.
    let mut rel_ids = Vec::with_capacity(spec.relations.len());
    let mut rel_adj = Vec::with_capacity(spec.relations.len());
    for (k, &(from_idx, to_idx)) in spec.relations.iter().enumerate() {
        let symmetric = k % 2 == 1;
        let rt: RelationTypeId = {
            let wtxn = db.write_txn().unwrap();
            let id = relation_type_intern(
                &wtxn,
                "test",
                &format!("rel{k}"),
                None,
                None,
                Cardinality::ManyToMany,
                symmetric,
                1,
                "",
                NOW,
            )
            .unwrap();
            wtxn.commit().unwrap();
            id
        };
        let r = Relation::new_root(
            RelationId::new(),
            rt,
            entity_ids[from_idx],
            entity_ids[to_idx],
            0.9,
            Vec::new(),
            ExtractorId::from(0),
            NOW,
            symmetric,
        );
        let wtxn = db.write_txn().unwrap();
        let id = relation_create(&wtxn, scope(), SessionId::DEFAULT, &r, NOW).unwrap();
        wtxn.commit().unwrap();
        rel_ids.push(id);
        rel_adj.push((from_idx, to_idx));
    }

    (
        db,
        dir,
        BuiltGraph {
            entity_ids,
            stmt_ids,
            rel_ids,
            stmt_adj,
            rel_adj,
        },
    )
}

// ---------------------------------------------------------------------------
// Snapshots (semantic graph state; excludes updated_at, which merge and
// unmerge both legitimately move forward).
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct EntitySnap {
    /// Normalized + sorted; order isn't load-bearing for aliases.
    aliases: Vec<String>,
    mention: u32,
    attrs: Vec<u8>,
    merged_into: Option<EntityId>,
    is_merged: bool,
}

struct GraphSnap {
    entities: Vec<EntitySnap>,
    statements: Vec<Statement>,
    relations: Vec<Relation>,
}

fn snapshot(db: &MetadataDb, built: &BuiltGraph) -> GraphSnap {
    let rtxn = db.read_txn().unwrap();
    let entities = built
        .entity_ids
        .iter()
        .map(|id| {
            let e = entity_get(&rtxn, *id).unwrap().unwrap();
            let mut aliases: Vec<String> = e.aliases.iter().map(|a| normalize_name(a)).collect();
            aliases.sort();
            EntitySnap {
                aliases,
                mention: e.mention_count,
                attrs: e.attributes.as_bytes().to_vec(),
                merged_into: e.merged_into,
                is_merged: e.is_merged(),
            }
        })
        .collect();
    let statements = built
        .stmt_ids
        .iter()
        .map(|id| statement_get(&rtxn, *id).unwrap().unwrap())
        .collect();
    let relations = built
        .rel_ids
        .iter()
        .map(|id| relation_get(&rtxn, *id).unwrap().unwrap())
        .collect();
    GraphSnap {
        entities,
        statements,
        relations,
    }
}

fn commit_merge(db: &MetadataDb, survivor: EntityId, merged: EntityId) -> (u32, u32) {
    let wtxn = db.write_txn().unwrap();
    let out = merge_entity(
        &wtxn,
        survivor,
        merged,
        0.99,
        "duplicate".into(),
        MergeActor::Space([1; 16]),
        GRACE_SECS,
        MERGE_AT,
    )
    .unwrap();
    wtxn.commit().unwrap();
    (out.statements_rerouted, out.relations_rerouted)
}

fn commit_unmerge(db: &MetadataDb, merged: EntityId) -> EntityId {
    let wtxn = db.write_txn().unwrap();
    let survivor = unmerge_entity(&wtxn, merged, MergeActor::Space([2; 16]), UNMERGE_AT).unwrap();
    wtxn.commit().unwrap();
    survivor
}

fn asym_filter() -> RelationListFilter {
    RelationListFilter {
        relation_type: None,
        current_only: false,
        limit: 0,
    }
}

fn subject_filter(subject: EntityId) -> StatementListFilter {
    StatementListFilter {
        subject: Some(subject),
        predicate: None,
        kind: None,
        current_only: false,
        min_confidence: None,
        limit: 0,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    // -----------------------------------------------------------------
    // Property 1 — merge then unmerge is a faithful inverse, and the
    // post-merge convergence invariant holds in the same pass.
    // -----------------------------------------------------------------
    #[test]
    fn merge_then_unmerge_restores_the_graph(spec in graph_spec()) {
        let (db, _dir, built) = build(&spec);
        let survivor = built.survivor(&spec);
        let merged = built.merged(&spec);

        let pre = snapshot(&db, &built);

        let (n_stmt, n_rel) = commit_merge(&db, survivor, merged);

        // --- Post-merge convergence invariant (pre-unmerge) ---------
        {
            let rtxn = db.read_txn().unwrap();

            // The merged id resolves to the survivor.
            prop_assert_eq!(
                entity_get_resolved(&rtxn, merged).unwrap().unwrap().id,
                survivor
            );

            // No statement is still anchored on the merged subject.
            prop_assert!(
                statement_list(&rtxn, scope(), &subject_filter(merged)).unwrap().is_empty(),
                "no statement may remain on the merged subject after merge"
            );

            // Every statement that referenced merged now references the
            // survivor — neither side dangles on the stale id.
            for (id, (s_idx, o_idx)) in built.stmt_ids.iter().zip(built.stmt_adj.iter()) {
                let st = statement_get(&rtxn, *id).unwrap().unwrap();
                if *s_idx == spec.merged {
                    prop_assert_eq!(st.subject, SubjectRef::Entity(survivor));
                }
                if *o_idx == spec.merged {
                    prop_assert_eq!(&st.object, &StatementObject::Entity(survivor));
                }
                prop_assert_ne!(st.subject, SubjectRef::Entity(merged));
                prop_assert_ne!(&st.object, &StatementObject::Entity(merged));
            }

            // No relation dangles on merged; survivor is the endpoint.
            prop_assert!(relation_list_from(&rtxn, scope(), merged, &asym_filter()).unwrap().is_empty());
            prop_assert!(relation_list_to(&rtxn, scope(), merged, &asym_filter()).unwrap().is_empty());
            for (id, (f_idx, t_idx)) in built.rel_ids.iter().zip(built.rel_adj.iter()) {
                let r = relation_get(&rtxn, *id).unwrap().unwrap();
                prop_assert_ne!(r.from_entity, merged);
                prop_assert_ne!(r.to_entity, merged);
                if *f_idx == spec.merged || *t_idx == spec.merged {
                    // Symmetric relations re-canonicalize by id order, so
                    // the survivor may land on either endpoint.
                    prop_assert!(
                        r.from_entity == survivor || r.to_entity == survivor,
                        "rerouted relation must have the survivor as an endpoint"
                    );
                }
            }

            // The merged entity is no longer resolver-reachable by name.
            prop_assert_eq!(
                entity_lookup_by_canonical_name(
                    &rtxn,
                    scope(),
                    if spec.entities[spec.merged].is_org {
                        entity_get(&rtxn, merged).unwrap().unwrap().entity_type
                    } else {
                        EntityType::PERSON_ID
                    },
                    &format!("Ent{}", spec.merged),
                )
                .unwrap(),
                None
            );

            // Reported reroute counts equal the rows actually rerouted.
            prop_assert_eq!(n_stmt as usize, built.stmts_touching_merged(&spec));
            prop_assert_eq!(n_rel as usize, built.rels_touching_merged(&spec));
        }

        // --- Unmerge within grace, then assert full restoration -----
        let restored = commit_unmerge(&db, merged);
        prop_assert_eq!(restored, survivor);

        let post = snapshot(&db, &built);

        // Entity-level restoration.
        prop_assert!(post.entities == pre.entities, "entity state must round-trip");
        // Statement-level restoration (subject/object/version and all).
        prop_assert!(post.statements == pre.statements, "statements must round-trip");
        // Relation-level restoration (endpoints and all).
        prop_assert!(post.relations == pre.relations, "relations must round-trip");

        // The merged entity is resolvable again and points nowhere.
        let rtxn = db.read_txn().unwrap();
        let m = entity_get(&rtxn, merged).unwrap().unwrap();
        prop_assert_eq!(m.merged_into, None);
        prop_assert!(!m.is_merged());
        prop_assert_eq!(
            entity_get_resolved(&rtxn, merged).unwrap().unwrap().id,
            merged
        );
        prop_assert_eq!(
            entity_lookup_by_canonical_name(&rtxn, scope(), m.entity_type, &format!("Ent{}", spec.merged))
                .unwrap(),
            Some(merged)
        );
    }

    // -----------------------------------------------------------------
    // Property 2 — merge convergence / no-orphan, standalone (no
    // unmerge). Focuses the count + reachability invariants over a
    // wider case set than Property 1 revisits.
    // -----------------------------------------------------------------
    #[test]
    fn merge_leaves_no_orphaned_references(spec in graph_spec()) {
        let (db, _dir, built) = build(&spec);
        let survivor = built.survivor(&spec);
        let merged = built.merged(&spec);

        let (n_stmt, n_rel) = commit_merge(&db, survivor, merged);

        let rtxn = db.read_txn().unwrap();

        // Count actual rerouted statement rows by scanning the survivor's
        // and merged's subject indexes plus decoding objects.
        let mut actual_stmt = 0usize;
        for (id, (s_idx, o_idx)) in built.stmt_ids.iter().zip(built.stmt_adj.iter()) {
            let st = statement_get(&rtxn, *id).unwrap().unwrap();
            let touched = *s_idx == spec.merged || *o_idx == spec.merged;
            if touched {
                actual_stmt += 1;
                prop_assert_ne!(st.subject, SubjectRef::Entity(merged), "no orphaned subject");
                prop_assert_ne!(st.object, StatementObject::Entity(merged), "no orphaned object");
            }
        }
        prop_assert_eq!(n_stmt as usize, actual_stmt, "reported == actual statement reroutes");

        let mut actual_rel = 0usize;
        for (id, (f_idx, t_idx)) in built.rel_ids.iter().zip(built.rel_adj.iter()) {
            let r = relation_get(&rtxn, *id).unwrap().unwrap();
            let touched = *f_idx == spec.merged || *t_idx == spec.merged;
            if touched {
                actual_rel += 1;
                prop_assert_ne!(r.from_entity, merged, "no orphaned from-endpoint");
                prop_assert_ne!(r.to_entity, merged, "no orphaned to-endpoint");
            }
        }
        prop_assert_eq!(n_rel as usize, actual_rel, "reported == actual relation reroutes");

        // Merged not resolver-reachable; survivor still is.
        let m_type = entity_get(&rtxn, merged).unwrap().unwrap().entity_type;
        prop_assert_eq!(
            entity_lookup_by_canonical_name(&rtxn, scope(), m_type, &format!("Ent{}", spec.merged)).unwrap(),
            None
        );
        let s_type = entity_get(&rtxn, survivor).unwrap().unwrap().entity_type;
        prop_assert_eq!(
            entity_lookup_by_canonical_name(&rtxn, scope(), s_type, &format!("Ent{}", spec.survivor)).unwrap(),
            Some(survivor)
        );
    }

    // -----------------------------------------------------------------
    // Property 3 (chaos) — dropping the merge txn before commit is a
    // no-op. redb rolls back every table the merge touched, so no
    // half-merge (some rows rerouted, redirect set but statements not,
    // audit written but reroutes not) is ever observable.
    // -----------------------------------------------------------------
    #[test]
    fn merge_dropped_before_commit_is_a_full_noop(spec in graph_spec()) {
        let (db, _dir, built) = build(&spec);
        let survivor = built.survivor(&spec);
        let merged = built.merged(&spec);

        let pre = snapshot(&db, &built);

        // Run the whole merge inside a txn, then drop it without commit.
        {
            let wtxn = db.write_txn().unwrap();
            merge_entity(
                &wtxn,
                survivor,
                merged,
                0.99,
                "crash".into(),
                MergeActor::Space([1; 16]),
                GRACE_SECS,
                MERGE_AT,
            )
            .unwrap();
            // Intentional: no commit. `wtxn` drops here → rollback.
        }

        let post = snapshot(&db, &built);
        prop_assert!(post.entities == pre.entities, "entities unchanged after rollback");
        prop_assert!(post.statements == pre.statements, "statements unchanged after rollback");
        prop_assert!(post.relations == pre.relations, "relations unchanged after rollback");

        // No redirect leaked, so there is nothing to unmerge — the
        // audit row rolled back with everything else.
        let rtxn = db.read_txn().unwrap();
        prop_assert_eq!(entity_get(&rtxn, merged).unwrap().unwrap().merged_into, None);
        drop(rtxn);
        let wtxn = db.write_txn().unwrap();
        let err = unmerge_entity(&wtxn, merged, MergeActor::Space([2; 16]), UNMERGE_AT).unwrap_err();
        prop_assert!(
            matches!(err, EntityMergeOpError::NotMerged(_)),
            "a rolled-back merge must leave no audit / redirect to unmerge"
        );
    }
}

// ---------------------------------------------------------------------------
// Chaos — the "fully applied" durability leg. A committed merge must
// survive a full DB reopen (redb's own crash-consistent recovery) with
// every reroute and redirect intact. This is the counterpart to the
// dropped-txn "none applied" leg proven above.
// ---------------------------------------------------------------------------

#[test]
fn committed_merge_survives_db_reopen_fully_applied() {
    let spec = GraphSpec {
        entities: vec![
            EntitySpec {
                is_org: false,
                alias_ids: vec![0, 1],
                mention: 3,
                attr: 0,
            },
            EntitySpec {
                is_org: false,
                alias_ids: vec![2],
                mention: 5,
                attr: 2,
            },
            EntitySpec {
                is_org: false,
                alias_ids: vec![3],
                mention: 1,
                attr: 0,
            },
        ],
        survivor: 0,
        merged: 1,
        // subject-merged, object-merged, both-merged (self-ref).
        statements: vec![(1, 2), (2, 1), (1, 1)],
        // from-merged, to-merged (asym then sym).
        relations: vec![(1, 2), (2, 1)],
    };

    let (survivor, merged, path, n_stmt, n_rel, dir, built_adj) = {
        let (db, dir, built) = build(&spec);
        let survivor = built.survivor(&spec);
        let merged = built.merged(&spec);
        let path = dir.path().join("metadata.redb");
        let (n_stmt, n_rel) = commit_merge(&db, survivor, merged);
        let adj = (
            built.entity_ids.clone(),
            built.stmt_ids.clone(),
            built.rel_ids.clone(),
            built.stmt_adj.clone(),
            built.rel_adj.clone(),
        );
        // Drop the handle to release the redb file lock, keep `dir`.
        drop(db);
        (survivor, merged, path, n_stmt, n_rel, dir, adj)
    };
    let _dir = dir; // keep the tempdir alive across reopen

    assert_eq!(n_stmt as usize, 3, "all three statements touch merged");
    assert_eq!(n_rel as usize, 2, "both relations touch merged");

    // Reopen — this is the crash-after-commit recovery path.
    let db = MetadataDb::open(&path).expect("reopen metadata");
    let rtxn = db.read_txn().unwrap();

    // Redirect applied and durable.
    let m = entity_get(&rtxn, merged).unwrap().unwrap();
    assert_eq!(m.merged_into, Some(survivor));
    assert!(m.is_merged());
    assert_eq!(
        entity_get_resolved(&rtxn, merged).unwrap().unwrap().id,
        survivor
    );

    // Every reroute applied and durable — nothing dangles on merged.
    let (_eids, sids, rids, sadj, radj) = built_adj;
    assert!(statement_list(&rtxn, scope(), &subject_filter(merged))
        .unwrap()
        .is_empty());
    for (id, (s_idx, o_idx)) in sids.iter().zip(sadj.iter()) {
        let st = statement_get(&rtxn, *id).unwrap().unwrap();
        if *s_idx == spec.merged {
            assert_eq!(st.subject, SubjectRef::Entity(survivor));
        }
        if *o_idx == spec.merged {
            assert_eq!(st.object, StatementObject::Entity(survivor));
        }
    }
    assert!(relation_list_from(&rtxn, scope(), merged, &asym_filter())
        .unwrap()
        .is_empty());
    assert!(relation_list_to(&rtxn, scope(), merged, &asym_filter())
        .unwrap()
        .is_empty());
    for (id, (f_idx, t_idx)) in rids.iter().zip(radj.iter()) {
        let r = relation_get(&rtxn, *id).unwrap().unwrap();
        assert_ne!(r.from_entity, merged);
        assert_ne!(r.to_entity, merged);
        if *f_idx == spec.merged || *t_idx == spec.merged {
            assert!(r.from_entity == survivor || r.to_entity == survivor);
        }
    }

    // The audit survived too: unmerge (still in grace) succeeds and
    // restores the merged entity — proof the audit row is durable.
    drop(rtxn);
    let restored = commit_unmerge(&db, merged);
    assert_eq!(restored, survivor);
    let rtxn = db.read_txn().unwrap();
    assert_eq!(
        entity_get(&rtxn, merged).unwrap().unwrap().merged_into,
        None
    );
}

// ---------------------------------------------------------------------------
// Chaos — unmerge atomicity: dropping the unmerge txn before commit
// leaves the post-merge state intact (no half-unmerge).
// ---------------------------------------------------------------------------

#[test]
fn unmerge_dropped_before_commit_leaves_merged_state_intact() {
    let spec = GraphSpec {
        entities: vec![
            EntitySpec {
                is_org: false,
                alias_ids: vec![0],
                mention: 2,
                attr: 0,
            },
            EntitySpec {
                is_org: false,
                alias_ids: vec![1, 2],
                mention: 4,
                attr: 3,
            },
            EntitySpec {
                is_org: false,
                alias_ids: vec![3],
                mention: 0,
                attr: 0,
            },
        ],
        survivor: 0,
        merged: 1,
        statements: vec![(1, 2), (2, 1)],
        relations: vec![(1, 2), (2, 1)],
    };

    let (db, _dir, built) = build(&spec);
    let survivor = built.survivor(&spec);
    let merged = built.merged(&spec);

    commit_merge(&db, survivor, merged);
    let post_merge = snapshot(&db, &built);

    // Run unmerge, drop before commit.
    {
        let wtxn = db.write_txn().unwrap();
        unmerge_entity(&wtxn, merged, MergeActor::Space([2; 16]), UNMERGE_AT).unwrap();
        // Intentional: no commit → rollback.
    }

    let after = snapshot(&db, &built);
    assert!(
        after.entities == post_merge.entities,
        "entities unchanged after unmerge rollback"
    );
    assert!(
        after.statements == post_merge.statements,
        "statements unchanged after unmerge rollback"
    );
    assert!(
        after.relations == post_merge.relations,
        "relations unchanged after unmerge rollback"
    );

    // Merge is still in force; the merged id still redirects.
    let rtxn = db.read_txn().unwrap();
    assert_eq!(
        entity_get(&rtxn, merged).unwrap().unwrap().merged_into,
        Some(survivor)
    );

    // And unmerge remains available (the audit did not get finalized by
    // the rolled-back attempt).
    drop(rtxn);
    let restored = commit_unmerge(&db, merged);
    assert_eq!(restored, survivor);
}
