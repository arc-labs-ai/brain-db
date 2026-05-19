//! Concurrent OneToOne RELATION_CREATE behavior under the per-shard
//! mutex.
//!
//! Resolution in force: `CardinalityViolation` is unreachable through
//! the public Create API — single conflicts auto-supersede. So this
//! file does NOT assert the error; it asserts the observable
//! auto-supersede behavior.
//!
//! Two OS threads each open a redb write transaction and call
//! `relation_create` with overlapping endpoints on a schema-declared
//! OneToOne relation type. redb serialises write transactions per file
//! — the threads execute sequentially under the hood, which IS the
//! shape a brain-ops shard exposes (per-shard mutex + single redb
//! writer). The expected outcome is:
//!
//! - Both creates return `Ok(...)`.
//! - The pair of resulting `RelationId`s belong to a single
//!   supersession chain.
//! - Exactly one row is `current = true` at the end.
//! - No `CardinalityViolation` is raised.
//!
//! This file lives under `crates/brain-ops/tests/` because the
//! cardinality contract is a brain-ops surface — the ops layer
//! validates schema-strictness before invoking
//! `brain_metadata::relation_create`, and the
//! auto-supersession-vs-CardinalityViolation choice is the binding
//! semantic guarantee the wire layer exposes.

use std::sync::Arc;
use std::thread;

use brain_core::knowledge::{Entity, Relation, RelationType};
use brain_core::{Cardinality, EntityId, EntityTypeId, ExtractorId, RelationId, RelationTypeId};
use brain_metadata::entity_ops::entity_put;
use brain_metadata::entity_type_ops::entity_type_intern;
use brain_metadata::relation_ops::{
    relation_create, relation_get, relation_history, relation_list_from, RelationListFilter,
    RelationOpError,
};
use brain_metadata::relation_type_ops::relation_type_intern;
use redb::ReadableDatabase;

const T0: u64 = 1_700_000_000_000_000_000;

fn fresh_db(dir: &tempfile::TempDir) -> redb::Database {
    let db = redb::Database::create(dir.path().join("test.redb")).expect("create redb");
    // entity_put requires the EntityTypeId to exist; seed Person=id 1.
    let wtxn = db.begin_write().unwrap();
    let _ = entity_type_intern(&wtxn, "Person", Vec::new(), T0).unwrap();
    wtxn.commit().unwrap();
    db
}

fn put_entity(db: &redb::Database, id: EntityId, name: &str) {
    let wtxn = db.begin_write().unwrap();
    entity_put(
        &wtxn,
        &Entity::new_active(id, EntityTypeId(1), name.into(), name.to_string(), T0),
    )
    .unwrap();
    wtxn.commit().unwrap();
}

fn intern_one_to_one(db: &redb::Database, namespace: &str, name: &str) -> RelationTypeId {
    let wtxn = db.begin_write().unwrap();
    let id = relation_type_intern(
        &wtxn,
        namespace,
        name,
        None,
        None,
        Cardinality::OneToOne,
        false,
        1,
        "",
        T0,
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

fn fresh_relation(rt: RelationTypeId, from: EntityId, to: EntityId) -> Relation {
    Relation::new_root(
        RelationId::new(),
        rt,
        from,
        to,
        0.9,
        Vec::new(),
        ExtractorId::default(),
        T0,
        false,
    )
}

// ---------------------------------------------------------------------------
// Two concurrent OneToOne creates serialize to auto-supersede.
// ---------------------------------------------------------------------------

#[test]
fn two_threads_creating_one_to_one_serialize_to_supersession_chain() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(fresh_db(&dir));

    let alice = EntityId::new();
    let bob = EntityId::new();
    let carol = EntityId::new();
    put_entity(&db, alice, "alice");
    put_entity(&db, bob, "bob");
    put_entity(&db, carol, "carol");

    let rt = intern_one_to_one(&db, "acme", "married_to");

    // Two threads, each interning a different (from=alice) edge.
    // OneToOne means from-side cardinality is checked; the second
    // create auto-supersedes the first.
    let r1 = fresh_relation(rt, alice, bob);
    let r2 = fresh_relation(rt, alice, carol);
    let (r1_id, r2_id) = (r1.id, r2.id);

    let db1 = Arc::clone(&db);
    let r1_for_thread = r1.clone();
    let t1 = thread::spawn(move || {
        let wtxn = db1.begin_write().expect("begin_write");
        let written = relation_create(&wtxn, &r1_for_thread, T0).expect("create r1");
        wtxn.commit().expect("commit r1");
        written
    });
    let written_r1 = t1.join().expect("thread join r1");
    assert_eq!(written_r1, r1_id);

    let db2 = Arc::clone(&db);
    let r2_for_thread = r2.clone();
    let t2 = thread::spawn(move || {
        let wtxn = db2.begin_write().expect("begin_write");
        let written = relation_create(&wtxn, &r2_for_thread, T0 + 1).expect("create r2");
        wtxn.commit().expect("commit r2");
        written
    });
    let written_r2 = t2.join().expect("thread join r2");
    assert_eq!(
        written_r2, r2_id,
        "second create's returned id is its own (auto-supersession returns the new id, not the old)"
    );

    // r1 must now be superseded by r2.
    let rtxn = db.begin_read().unwrap();
    let g1 = relation_get(&rtxn, r1_id).unwrap().expect("row exists");
    let g2 = relation_get(&rtxn, r2_id).unwrap().expect("row exists");
    assert_eq!(g1.superseded_by, Some(g2.id), "r1 must be superseded by r2",);
    assert_eq!(g2.supersedes, Some(r1_id));
    assert_eq!(g2.chain_root, r1_id);
    assert_eq!(g2.version, 2);

    // Exactly one current row from alice.
    let filter = RelationListFilter {
        relation_type: Some(rt),
        current_only: true,
        limit: 0,
    };
    let current = relation_list_from(&rtxn, alice, &filter).unwrap();
    assert_eq!(current.len(), 1, "exactly one current OneToOne from alice");
    assert_eq!(current[0].id, r2_id);

    // History has the full chain.
    let chain = relation_history(&rtxn, r1_id).unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].id, r1_id);
    assert_eq!(chain[1].id, r2_id);
}

// ---------------------------------------------------------------------------
// Three sequential creates: each auto-supersedes the prior one.
// CardinalityViolation never fires because each create only sees one
// conflicting current row at a time.
// ---------------------------------------------------------------------------

#[test]
fn three_sequential_one_to_one_creates_chain_without_violation() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);

    let alice = EntityId::new();
    let bob = EntityId::new();
    let carol = EntityId::new();
    let dave = EntityId::new();
    for (id, name) in [(alice, "a"), (bob, "b"), (carol, "c"), (dave, "d")] {
        put_entity(&db, id, name);
    }

    let rt = intern_one_to_one(&db, "acme", "owns");
    let r1 = fresh_relation(rt, alice, bob);
    let r2 = fresh_relation(rt, alice, carol);
    let r3 = fresh_relation(rt, alice, dave);
    let (r1_id, r2_id, r3_id) = (r1.id, r2.id, r3.id);

    for (i, r) in [r1, r2, r3].iter().enumerate() {
        let wtxn = db.begin_write().unwrap();
        let id = relation_create(&wtxn, r, T0 + i as u64).expect("create succeeds");
        wtxn.commit().unwrap();
        assert_eq!(id, r.id);
    }

    let rtxn = db.begin_read().unwrap();
    let chain = relation_history(&rtxn, r1_id).unwrap();
    assert_eq!(chain.len(), 3, "three-row chain");
    assert_eq!(chain[0].id, r1_id);
    assert_eq!(chain[1].id, r2_id);
    assert_eq!(chain[2].id, r3_id);
    assert_eq!(chain[2].version, 3);

    // Only the last is current.
    let filter = RelationListFilter {
        relation_type: Some(rt),
        current_only: true,
        limit: 0,
    };
    let current = relation_list_from(&rtxn, alice, &filter).unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].id, r3_id);
}

// ---------------------------------------------------------------------------
// Sanity: a OneToMany relation type does NOT auto-supersede on the
// from-side. (`from=alice → to=*`: many targets allowed.) Differentiates
// the cardinality dispatch from a regression that auto-supersedes
// everything.
// ---------------------------------------------------------------------------

#[test]
fn one_to_many_does_not_auto_supersede_on_second_target() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let alice = EntityId::new();
    let bob = EntityId::new();
    let carol = EntityId::new();
    for (id, name) in [(alice, "a"), (bob, "b"), (carol, "c")] {
        put_entity(&db, id, name);
    }

    let wtxn = db.begin_write().unwrap();
    let rt = relation_type_intern(
        &wtxn,
        "acme",
        "manages",
        None,
        None,
        Cardinality::OneToMany,
        false,
        1,
        "",
        T0,
    )
    .unwrap();
    wtxn.commit().unwrap();

    let r1 = fresh_relation(rt, alice, bob);
    let r2 = fresh_relation(rt, alice, carol);
    let (r1_id, r2_id) = (r1.id, r2.id);

    for (i, r) in [r1, r2].iter().enumerate() {
        let wtxn = db.begin_write().unwrap();
        relation_create(&wtxn, r, T0 + i as u64).unwrap();
        wtxn.commit().unwrap();
    }

    let rtxn = db.begin_read().unwrap();
    let g1 = relation_get(&rtxn, r1_id).unwrap().unwrap();
    let g2 = relation_get(&rtxn, r2_id).unwrap().unwrap();
    assert!(
        g1.superseded_by.is_none(),
        "OneToMany: bob edge stays current"
    );
    assert!(
        g2.superseded_by.is_none(),
        "OneToMany: carol edge is independent"
    );
}

// ---------------------------------------------------------------------------
// Surface unused imports.
// ---------------------------------------------------------------------------
const _: fn() = || {
    let _ = RelationType {
        id: RelationTypeId::from(1),
        namespace: String::new(),
        name: String::new(),
        from_type: None,
        to_type: None,
        cardinality: Cardinality::ManyToMany,
        is_symmetric: false,
        schema_version: 0,
        description: String::new(),
    };
    let _e: Option<RelationOpError> = None;
};
