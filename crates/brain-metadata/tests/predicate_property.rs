//! Property + concurrency tests for the schemaless
//! `predicate_intern_or_get` write path.
//!
//! Three properties:
//!
//! 1. **Determinism by allocation order.** Two fresh databases that
//!    see the same allocation sequence for distinct qnames produce the
//!    same `(qname → PredicateId)` mapping. (Allocation is `max + 1`,
//!    so id order tracks first-write order.)
//! 2. **Single-id under contention.** Eight OS threads concurrently
//!    intern the same `(namespace, name)`. redb serialises write
//!    transactions per file, so the eight calls run sequentially in
//!    some order. The probe-then-write loop must produce exactly one
//!    primary row, and all eight return the same id.
//! 3. **Id stability across unrelated writes.** Interning a target
//!    qname, then 0–50 unrelated predicates, then re-interning the
//!    target must yield the same id.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::thread;

use brain_metadata::predicate_ops::{
    predicate_intern_or_get, predicate_list, predicate_lookup_by_qname,
};
use proptest::prelude::*;
use redb::ReadableDatabase;

/// Open a fresh, *unseeded* redb database. These property tests want
/// id allocation to start at 1, so we deliberately skip
/// `MetadataDb::open` (which seeds the system schema and would push
/// the first user-allocated id past the seeded set).
fn fresh_db(dir: &tempfile::TempDir) -> redb::Database {
    redb::Database::create(dir.path().join("test.redb")).expect("create redb")
}

// ---------------------------------------------------------------------------
// Case 1 — determinism by allocation order.
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn intern_or_get_id_mapping_follows_allocation_order(
        qnames in proptest::collection::vec(
            (
                "[a-z][a-z0-9_]{0,5}",
                "[a-z][a-z0-9_]{0,5}",
            ),
            1..=20,
        ),
    ) {
        // Order of first appearance in `qnames` determines id allocation:
        // each unique qname gets id = (rank of first appearance) starting
        // at 1. Build the expected map up front.
        let mut expected: BTreeMap<(String, String), u32> = BTreeMap::new();
        let mut next_id: u32 = 1;
        for q in &qnames {
            if !expected.contains_key(q) {
                expected.insert(q.clone(), next_id);
                next_id += 1;
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        let mut got: BTreeMap<(String, String), u32> = BTreeMap::new();
        for (ns, name) in &qnames {
            let id = predicate_intern_or_get(&wtxn, ns, name, 0, 0).unwrap();
            // First call for this qname wins the id.
            got.entry((ns.clone(), name.clone())).or_insert(id.raw());
            // Subsequent calls must return that id.
            prop_assert_eq!(got[&(ns.clone(), name.clone())], id.raw());
        }
        wtxn.commit().unwrap();

        prop_assert_eq!(got.len(), expected.len(),
            "unique qname count must match");
        for (k, want_id) in expected {
            prop_assert_eq!(got[&k], want_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Case 2 — concurrent intern of the same qname produces exactly one row.
// ---------------------------------------------------------------------------

#[test]
fn concurrent_intern_or_get_same_qname_yields_single_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(fresh_db(&dir));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            // Each thread opens its own wtxn. redb serialises wtxn
            // acquisition so the threads execute sequentially under
            // the hood — exactly the contention shape we want to test.
            let wtxn = db.begin_write().expect("begin_write");
            let id = predicate_intern_or_get(&wtxn, "acme", "shared", 0, 0).expect("intern");
            wtxn.commit().expect("commit");
            id.raw()
        }));
    }
    let ids: BTreeSet<u32> = handles
        .into_iter()
        .map(|h| h.join().expect("thread join"))
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "all 8 threads must converge on a single PredicateId, got {ids:?}",
    );

    // And the primary table has exactly one row for that qname.
    let rtxn = db.begin_read().unwrap();
    let all = predicate_list(&rtxn, None).unwrap();
    assert_eq!(all.len(), 1, "exactly one predicate row written");
    let only = predicate_lookup_by_qname(&rtxn, "acme", "shared")
        .unwrap()
        .expect("row");
    assert_eq!(only.id.raw(), *ids.iter().next().unwrap());
}

// ---------------------------------------------------------------------------
// Case 3 — id stability across unrelated writes.
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn target_intern_id_stable_after_other_predicates(
        unrelated in proptest::collection::vec(
            // Different namespace from the target so collisions with
            // ("acme", "target") are impossible.
            ("[a-z][a-z0-9_]{0,5}", "[a-z][a-z0-9_]{0,5}"),
            0..=50,
        ),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let initial_id = {
            let wtxn = db.begin_write().unwrap();
            let id = predicate_intern_or_get(&wtxn, "acme", "target", 0, 0).unwrap();
            wtxn.commit().unwrap();
            id
        };

        {
            let wtxn = db.begin_write().unwrap();
            for (ns, name) in &unrelated {
                if ns == "acme" && name == "target" {
                    continue;
                }
                // Errors only happen on identifier-validation failure;
                // proptest input is generated by the safe grammar above.
                let _ = predicate_intern_or_get(&wtxn, ns, name, 0, 0).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let re_id = {
            let wtxn = db.begin_write().unwrap();
            let id = predicate_intern_or_get(&wtxn, "acme", "target", 0, 0).unwrap();
            wtxn.commit().unwrap();
            id
        };

        prop_assert_eq!(initial_id, re_id);
    }
}
