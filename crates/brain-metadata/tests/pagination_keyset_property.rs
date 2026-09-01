//! Property coverage for keyset (seek) LIST pagination.
//!
//! The statement/relation `*_page` functions in `brain-metadata` page
//! directly from the store over an **immutable** id-ordered index,
//! resuming strictly past a cursor. That subsystem accumulated three
//! distinct regressions:
//!
//!   1. an in-memory window capped the walk at 1000 rows (rows past the
//!      ceiling were unreachable);
//!   2. the cursor keyed on a *mutable* discriminant column, so a row
//!      whose column changed between two page fetches was relocated
//!      behind or ahead of the cursor and was silently **gapped** or
//!      **duplicated**;
//!   3. an undecodable-but-present primary row was silently **skipped**
//!      (masking corruption) instead of failing stop.
//!
//! The colocated unit tests in `statement/list.rs` pin (2) and (3) at
//! single hand-built points. This file adds the missing fuzz layer: over
//! a fresh redb it generates a random population under one subject /
//! predicate scope (random count, predicate, and confidence), pages the
//! matching set with a **random page limit**, follows `next_cursor` /
//! `last` to exhaustion, and asserts the exhaustive-tiling contract:
//!
//!   * the UNION of every page == the full matching set,
//!   * with EACH matching row EXACTLY ONCE (no gap, no dup),
//!   * `has_more` is `true` on every page but the last (and a
//!     `has_more` page is always a full `limit`-row page), so the wire
//!     `next_cursor` is empty only on the final page.
//!
//! A second family applies a random **mid-pagination mutation** between
//! two pages (supersede an uncollected row, or tombstone one) and asserts
//! the immutable-id resume guarantee still holds: every row that stays
//! matching for the whole walk is returned exactly once, with no gap and
//! no dup, regardless of the mutation.
//!
//! A third family proves the scope wall the wire cursor codec relies on:
//! paging one tenant with a cursor id that belongs to another tenant's
//! row — or with an all-zero / all-`0xff` "malformed" id — never leaks a
//! foreign row and never panics. (The wire cursor's explicit
//! reject-with-error on a malformed / out-of-tenant token lives one layer
//! up in `brain-ops::handlers` — `decode_statement_cursor` — and is
//! covered by `brain-ops/tests/list_cursor_pagination.rs`; this file
//! proves the underlying store-level isolation that makes that rejection
//! sound.)
//!
//! Each case opens a fresh redb tempfile and batches its inserts into one
//! write txn, so case counts stay modest.

use std::collections::BTreeSet;

use brain_core::{
    Cardinality, Entity, EntityId, EntityType, EvidenceRef, ExtractorId, NamespaceId, PredicateId,
    Relation, RelationId, SessionId, Statement, StatementId, StatementKind, StatementObject,
    StatementValue, SubjectRef, TombstoneReason,
};
use brain_metadata::{
    entity_put, normalize_name, predicate_intern, relation_create, relation_list_from_page,
    relation_list_to_page, relation_tombstone, relation_type_intern, statement_create,
    statement_list_page, statement_supersede, statement_tombstone, MetadataDb, RelationListFilter,
    RelationPage, RowScope, StatementListCursor, StatementListFilter, StatementPage,
    StatementPageExtra,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

const T0: u64 = 1_700_000_000_000_000_000;

/// Discrete confidence buckets — a small closed set so the `min_confidence`
/// filter has a crisp expected outcome with no float-equality flakiness
/// (the value stored and compared is the exact same `f32`).
const CONFIDENCES: [f32; 5] = [0.1, 0.3, 0.5, 0.7, 0.9];
/// Discrete `min_confidence` thresholds the filter is exercised at.
const MIN_CONF_CHOICES: [f32; 4] = [0.0, 0.4, 0.6, 0.95];

fn scope_a() -> RowScope {
    RowScope::from_bytes(NamespaceId::SYSTEM.raw(), [0xAB; 16])
}

fn scope_b() -> RowScope {
    RowScope::from_bytes(NamespaceId::SYSTEM.raw(), [0xCD; 16])
}

fn open_db() -> (tempfile::TempDir, MetadataDb) {
    let dir = tempfile::tempdir().unwrap();
    let db = MetadataDb::open(dir.path().join("md.redb")).expect("open metadata");
    (dir, db)
}

fn put_entity(db: &MetadataDb, scope: RowScope, name: &str) -> EntityId {
    let id = EntityId::new();
    let wtxn = db.write_txn().unwrap();
    entity_put(
        &wtxn,
        scope,
        SessionId::DEFAULT,
        &Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            normalize_name(name),
            T0,
        ),
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

/// Intern a cumulative (`is_stateful=false`) `Fact` predicate over
/// `Value` objects — so many rows for one (subject, predicate) all stay
/// current and the "full matching set" is exactly the created set (no
/// supersession quietly collapses it).
fn intern_fact_pred(db: &MetadataDb, name: &str) -> PredicateId {
    let wtxn = db.write_txn().unwrap();
    let id = predicate_intern(
        &wtxn,
        "test",
        name,
        Some(StatementKind::Fact),
        /* object: Value */ 2u8,
        /* schema_version */ 1,
        "",
        /* is_stateful */ false,
        T0,
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

/// Build a cumulative `Fact` statement over a distinct `Value(Text)`
/// object so no two collide (no content dedup, no supersession).
fn fact(subject: EntityId, pred: PredicateId, object: &str, confidence: f32) -> Statement {
    Statement::new_root(
        StatementId::new(),
        StatementKind::Fact,
        SubjectRef::Entity(subject),
        pred,
        StatementObject::Value(StatementValue::Text(object.into())),
        confidence,
        EvidenceRef::default(),
        ExtractorId::from(0),
        T0,
        1,
    )
}

// ---------------------------------------------------------------------------
// Shared drain + assertion helpers.
// ---------------------------------------------------------------------------

/// One paged walk to exhaustion. Returns the collected row ids in page
/// order, having asserted the cross-page cursor contract:
///   * no id appears on two pages (no dup),
///   * every page but the last reports `has_more == true`, is a full
///     `limit`-row page, and carries a resume `last`,
///   * the final page reports `has_more == false`.
fn drain_statements(
    db: &MetadataDb,
    scope: RowScope,
    filter: &StatementListFilter,
    extra: &StatementPageExtra,
    limit: usize,
) -> Result<Vec<[u8; 16]>, TestCaseError> {
    let mut after: Option<StatementListCursor> = None;
    let mut ids: Vec<[u8; 16]> = Vec::new();
    let mut seen: BTreeSet<[u8; 16]> = BTreeSet::new();
    // Defensive upper bound so a `has_more` that never clears can't hang
    // the suite — a genuine bug surfaces as this bound, not a timeout.
    let mut guard = 0usize;
    loop {
        let rtxn = db.read_txn().unwrap();
        let page: StatementPage =
            statement_list_page(&rtxn, scope, filter, extra, after, limit).unwrap();
        drop(rtxn);

        for s in &page.rows {
            let b = s.id.to_bytes();
            prop_assert!(seen.insert(b), "a row appeared on two pages (dup)");
            ids.push(b);
        }

        if page.has_more {
            prop_assert_eq!(
                page.rows.len(),
                limit,
                "a page reporting has_more must be a full limit-row page"
            );
            prop_assert!(
                page.last.is_some(),
                "has_more must carry a resume cursor (else the walk cannot progress)"
            );
            after = page.last;
        } else {
            // Final page: fewer-or-equal rows, and the walk stops here.
            prop_assert!(page.rows.len() <= limit, "final page exceeded limit");
            break;
        }

        guard += 1;
        prop_assert!(guard < 100_000, "pagination did not terminate");
    }
    Ok(ids)
}

fn drain_relations(
    db: &MetadataDb,
    scope: RowScope,
    entity: EntityId,
    filter: &RelationListFilter,
    include_tombstoned: bool,
    outgoing: bool,
    limit: usize,
) -> Result<Vec<[u8; 16]>, TestCaseError> {
    let mut after: Option<Vec<u8>> = None;
    let mut ids: Vec<[u8; 16]> = Vec::new();
    let mut seen: BTreeSet<[u8; 16]> = BTreeSet::new();
    let mut guard = 0usize;
    loop {
        let rtxn = db.read_txn().unwrap();
        let page: RelationPage = if outgoing {
            relation_list_from_page(
                &rtxn,
                scope,
                entity,
                filter,
                include_tombstoned,
                after.as_deref(),
                limit,
            )
            .unwrap()
        } else {
            relation_list_to_page(
                &rtxn,
                scope,
                entity,
                filter,
                include_tombstoned,
                after.as_deref(),
                limit,
            )
            .unwrap()
        };
        drop(rtxn);

        for r in &page.rows {
            let b = r.id.to_bytes();
            prop_assert!(seen.insert(b), "a relation appeared on two pages (dup)");
            ids.push(b);
        }

        if page.has_more {
            prop_assert_eq!(
                page.rows.len(),
                limit,
                "a relation page reporting has_more must be a full limit-row page"
            );
            prop_assert!(page.last_key.is_some(), "has_more must carry a resume key");
            after = page.last_key;
        } else {
            prop_assert!(
                page.rows.len() <= limit,
                "final relation page exceeded limit"
            );
            break;
        }

        guard += 1;
        prop_assert!(guard < 100_000, "relation pagination did not terminate");
    }
    Ok(ids)
}

fn assert_exact_set(seen: &[[u8; 16]], expected: &BTreeSet<[u8; 16]>) -> Result<(), TestCaseError> {
    let got: BTreeSet<[u8; 16]> = seen.iter().copied().collect();
    prop_assert_eq!(got.len(), seen.len(), "duplicate id across pages");
    prop_assert!(
        got.is_subset(expected),
        "pages returned a row outside the matching set"
    );
    prop_assert!(
        expected.is_subset(&got),
        "pages did not tile the full matching set (gap)"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Generators.
// ---------------------------------------------------------------------------

/// A population row: (predicate index into the pool, confidence).
fn arb_row() -> impl Strategy<Value = (usize, f32)> {
    (
        (0usize..3),
        (0usize..CONFIDENCES.len()).prop_map(|i| CONFIDENCES[i]),
    )
}

/// (rows, page_limit, predicate_filter_choice, min_conf_choice).
/// `predicate_filter_choice`: `None` → no predicate filter; `Some(i)` →
/// pool predicate `i`. Counts and limits are kept modest so a fresh redb
/// per case is cheap.
fn arb_population() -> impl Strategy<Value = (Vec<(usize, f32)>, usize, Option<usize>, f32)> {
    (
        prop::collection::vec(arb_row(), 0..=120),
        1usize..=32,
        prop_oneof![Just(None::<usize>), (0usize..3).prop_map(Some)],
        (0usize..MIN_CONF_CHOICES.len()).prop_map(|i| MIN_CONF_CHOICES[i]),
    )
}

// ---------------------------------------------------------------------------
// Property 1 — subject-anchored exhaustive tiling.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

    #[test]
    fn subject_anchored_pages_tile_matching_set(
        (rows, limit, pred_choice, min_conf) in arb_population(),
    ) {
        let (_dir, db) = open_db();
        let scope = scope_a();
        let subject = put_entity(&db, scope, "subject");
        let preds: Vec<PredicateId> =
            (0..3).map(|i| intern_fact_pred(&db, &format!("p{i}"))).collect();

        // Batch every create into one write txn (cumulative Facts, distinct
        // objects → all current, none superseded).
        let mut expected: BTreeSet<[u8; 16]> = BTreeSet::new();
        {
            let wtxn = db.write_txn().unwrap();
            for (i, (pi, conf)) in rows.iter().enumerate() {
                let s = fact(subject, preds[*pi], &format!("o{i}"), *conf);
                let id = statement_create(&wtxn, scope, SessionId::DEFAULT, &s, T0).unwrap();
                let matches_pred = match pred_choice {
                    Some(c) => c == *pi,
                    None => true,
                };
                if matches_pred && *conf >= min_conf {
                    expected.insert(id.to_bytes());
                }
            }
            wtxn.commit().unwrap();
        }

        let filter = StatementListFilter {
            subject: Some(subject),
            predicate: pred_choice.map(|c| preds[c]),
            kind: Some(StatementKind::Fact),
            current_only: false,
            min_confidence: if min_conf > 0.0 { Some(min_conf) } else { None },
            limit: 0,
        };
        let seen = drain_statements(&db, scope, &filter, &StatementPageExtra::default(), limit)?;
        assert_exact_set(&seen, &expected)?;
    }
}

// ---------------------------------------------------------------------------
// Property 2 — predicate-anchored exhaustive tiling (subject unset →
// the by-predicate index path). Rows are spread over several subjects so
// they all stay current under one shared predicate.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

    #[test]
    fn predicate_anchored_pages_tile_matching_set(
        confs in prop::collection::vec(
            (0usize..CONFIDENCES.len()).prop_map(|i| CONFIDENCES[i]),
            0..=100,
        ),
        limit in 1usize..=24,
        min_choice in (0usize..MIN_CONF_CHOICES.len()).prop_map(|i| MIN_CONF_CHOICES[i]),
    ) {
        let (_dir, db) = open_db();
        let scope = scope_a();
        let target = intern_fact_pred(&db, "target");
        // A second predicate whose rows must never surface under the filter.
        let noise = intern_fact_pred(&db, "noise");

        let mut expected: BTreeSet<[u8; 16]> = BTreeSet::new();
        {
            let wtxn = db.write_txn().unwrap();
            for (i, conf) in confs.iter().enumerate() {
                // Distinct subject per row so every Fact stays current.
                let subj = {
                    let id = EntityId::new();
                    entity_put(
                        &wtxn,
                        scope,
                        SessionId::DEFAULT,
                        &Entity::new_active(
                            id,
                            EntityType::PERSON_ID,
                            format!("s{i}"),
                            normalize_name(&format!("s{i}")),
                            T0,
                        ),
                    )
                    .unwrap();
                    id
                };
                let s = fact(subj, target, &format!("o{i}"), *conf);
                let id = statement_create(&wtxn, scope, SessionId::DEFAULT, &s, T0).unwrap();
                if *conf >= min_choice {
                    expected.insert(id.to_bytes());
                }
                // Noise row under the other predicate + same subject.
                let n = fact(subj, noise, &format!("n{i}"), *conf);
                statement_create(&wtxn, scope, SessionId::DEFAULT, &n, T0).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let filter = StatementListFilter {
            subject: None,
            predicate: Some(target),
            kind: Some(StatementKind::Fact),
            current_only: false,
            min_confidence: if min_choice > 0.0 { Some(min_choice) } else { None },
            limit: 0,
        };
        let seen = drain_statements(&db, scope, &filter, &StatementPageExtra::default(), limit)?;
        assert_exact_set(&seen, &expected)?;
    }
}

// ---------------------------------------------------------------------------
// Property 3 — mid-pagination mutation preserves the tiling.
//
// After page 1, mutate an *uncollected* row, then drain the rest. The
// immutable-id resume must still return every row that stays matching
// exactly once, with no gap and no dup.
//   * supersede (current_only=false): the victim stays in history and a
//     new current replacement is added → expected = created ∪ {replacement}.
//   * tombstone (include_tombstoned=false): the victim leaves the set →
//     expected = created \ {victim}.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum MidMutation {
    Supersede,
    Tombstone,
}

fn arb_mid_mutation() -> impl Strategy<Value = MidMutation> {
    prop_oneof![Just(MidMutation::Supersede), Just(MidMutation::Tombstone)]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

    #[test]
    fn mid_pagination_mutation_preserves_tiling(
        // n > max(limit) always, so page 1 is guaranteed to leave more.
        n in 8usize..=40,
        limit in 1usize..=6,
        mutation in arb_mid_mutation(),
    ) {
        let (_dir, db) = open_db();
        let scope = scope_a();
        let subject = put_entity(&db, scope, "subject");
        let pred = intern_fact_pred(&db, "knows");

        // n cumulative Facts, all current.
        let mut created: Vec<StatementId> = Vec::with_capacity(n);
        {
            let wtxn = db.write_txn().unwrap();
            for i in 0..n {
                let s = fact(subject, pred, &format!("o{i}"), 0.9);
                created.push(statement_create(&wtxn, scope, SessionId::DEFAULT, &s, T0).unwrap());
            }
            wtxn.commit().unwrap();
        }

        let filter = StatementListFilter {
            subject: Some(subject),
            predicate: Some(pred),
            kind: Some(StatementKind::Fact),
            current_only: false,
            min_confidence: None,
            limit: 0,
        };
        let extra = StatementPageExtra::default();

        // Page 1.
        let page1 = {
            let rtxn = db.read_txn().unwrap();
            statement_list_page(&rtxn, scope, &filter, &extra, None, limit).unwrap()
        };
        prop_assert!(page1.has_more, "n > limit should leave more pages");
        let mut seen: Vec<[u8; 16]> = page1.rows.iter().map(|s| s.id.to_bytes()).collect();
        let collected: BTreeSet<[u8; 16]> = seen.iter().copied().collect();

        // Pick an uncollected victim.
        let victim = *created
            .iter()
            .find(|id| !collected.contains(&id.to_bytes()))
            .expect("an uncollected row exists (n > limit)");

        // Build the expected final set per the mutation.
        let mut expected: BTreeSet<[u8; 16]> =
            created.iter().map(|id| id.to_bytes()).collect();
        match mutation {
            MidMutation::Supersede => {
                let replacement = fact(subject, pred, "REPLACEMENT", 0.9);
                let wtxn = db.write_txn().unwrap();
                let rid = statement_supersede(
                    &wtxn,
                    scope,
                    SessionId::DEFAULT,
                    victim,
                    &replacement,
                    T0,
                )
                .unwrap();
                wtxn.commit().unwrap();
                // Victim stays as history (current_only=false); replacement joins.
                expected.insert(rid.to_bytes());
            }
            MidMutation::Tombstone => {
                let wtxn = db.write_txn().unwrap();
                statement_tombstone(&wtxn, victim, TombstoneReason::UserRequest, T0).unwrap();
                wtxn.commit().unwrap();
                // Tombstoned + uncollected → excluded (include_tombstoned=false).
                expected.remove(&victim.to_bytes());
            }
        }

        // Drain the remainder from page 1's cursor.
        let mut after = page1.last;
        let mut guard = 0usize;
        let mut tail_seen: BTreeSet<[u8; 16]> = collected.clone();
        loop {
            let rtxn = db.read_txn().unwrap();
            let page = statement_list_page(&rtxn, scope, &filter, &extra, after, limit).unwrap();
            drop(rtxn);
            for s in &page.rows {
                let b = s.id.to_bytes();
                prop_assert!(tail_seen.insert(b), "a row was duplicated across pages");
                seen.push(b);
            }
            if !page.has_more {
                break;
            }
            after = page.last;
            guard += 1;
            prop_assert!(guard < 100_000, "pagination did not terminate");
        }

        assert_exact_set(&seen, &expected)?;
    }
}

// ---------------------------------------------------------------------------
// Property 4 — relation directional exhaustive tiling (_from and _to).
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 20, .. ProptestConfig::default() })]

    #[test]
    fn relation_directional_pages_tile_matching_set(
        n in 0usize..=50,
        limit in 1usize..=12,
        outgoing in any::<bool>(),
    ) {
        let (_dir, db) = open_db();
        let scope = scope_a();
        // ManyToMany, non-symmetric: one fixed anchor, n distinct peers →
        // n distinct current edges, no cardinality supersession, no
        // symmetric canonicalisation.
        let rtype = {
            let wtxn = db.write_txn().unwrap();
            let id = relation_type_intern(
                &wtxn, "test", "linked", None, None, Cardinality::ManyToMany, false, 1, "", T0,
            )
            .unwrap();
            wtxn.commit().unwrap();
            id
        };

        let mut expected: BTreeSet<[u8; 16]> = BTreeSet::new();
        let anchor;
        {
            let wtxn = db.write_txn().unwrap();
            let a = EntityId::new();
            entity_put(
                &wtxn,
                scope,
                SessionId::DEFAULT,
                &Entity::new_active(a, EntityType::PERSON_ID, "anchor".into(), normalize_name("anchor"), T0),
            )
            .unwrap();
            for i in 0..n {
                let peer = EntityId::new();
                entity_put(
                    &wtxn,
                    scope,
                    SessionId::DEFAULT,
                    &Entity::new_active(
                        peer,
                        EntityType::PERSON_ID,
                        format!("peer{i}"),
                        normalize_name(&format!("peer{i}")),
                        T0,
                    ),
                )
                .unwrap();
                // Outgoing → anchor is `from`; incoming → anchor is `to`.
                let (from, to) = if outgoing { (a, peer) } else { (peer, a) };
                let r = Relation::new_root(
                    RelationId::new(), rtype, from, to, 0.9, vec![], ExtractorId::from(0), T0, false,
                );
                let id = relation_create(&wtxn, scope, SessionId::DEFAULT, &r, T0).unwrap();
                expected.insert(id.to_bytes());
            }
            wtxn.commit().unwrap();
            anchor = a;
        }

        let filter = RelationListFilter {
            relation_type: Some(rtype),
            current_only: false,
            limit: 0,
        };
        let seen = drain_relations(&db, scope, anchor, &filter, false, outgoing, limit)?;
        assert_exact_set(&seen, &expected)?;
    }
}

// ---------------------------------------------------------------------------
// Property 5 — relation mid-pagination tombstone preserves the tiling.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 20, .. ProptestConfig::default() })]

    #[test]
    fn relation_mid_pagination_tombstone_preserves_tiling(
        // n > max(limit) always, so page 1 is guaranteed to leave more.
        n in 8usize..=40,
        limit in 1usize..=6,
    ) {
        let (_dir, db) = open_db();
        let scope = scope_a();
        let rtype = {
            let wtxn = db.write_txn().unwrap();
            let id = relation_type_intern(
                &wtxn, "test", "linked", None, None, Cardinality::ManyToMany, false, 1, "", T0,
            )
            .unwrap();
            wtxn.commit().unwrap();
            id
        };

        let mut created: Vec<RelationId> = Vec::with_capacity(n);
        let anchor;
        {
            let wtxn = db.write_txn().unwrap();
            let a = EntityId::new();
            entity_put(
                &wtxn,
                scope,
                SessionId::DEFAULT,
                &Entity::new_active(a, EntityType::PERSON_ID, "anchor".into(), normalize_name("anchor"), T0),
            )
            .unwrap();
            for i in 0..n {
                let peer = EntityId::new();
                entity_put(
                    &wtxn,
                    scope,
                    SessionId::DEFAULT,
                    &Entity::new_active(
                        peer,
                        EntityType::PERSON_ID,
                        format!("peer{i}"),
                        normalize_name(&format!("peer{i}")),
                        T0,
                    ),
                )
                .unwrap();
                let r = Relation::new_root(
                    RelationId::new(), rtype, a, peer, 0.9, vec![], ExtractorId::from(0), T0, false,
                );
                created.push(relation_create(&wtxn, scope, SessionId::DEFAULT, &r, T0).unwrap());
            }
            wtxn.commit().unwrap();
            anchor = a;
        }

        let filter = RelationListFilter {
            relation_type: Some(rtype),
            current_only: false,
            limit: 0,
        };

        // Page 1.
        let page1 = {
            let rtxn = db.read_txn().unwrap();
            relation_list_from_page(&rtxn, scope, anchor, &filter, false, None, limit).unwrap()
        };
        prop_assert!(page1.has_more, "n > limit should leave more pages");
        let mut seen: Vec<[u8; 16]> = page1.rows.iter().map(|r| r.id.to_bytes()).collect();
        let collected: BTreeSet<[u8; 16]> = seen.iter().copied().collect();

        // Tombstone an uncollected edge.
        let victim = *created
            .iter()
            .find(|id| !collected.contains(&id.to_bytes()))
            .expect("an uncollected edge exists");
        {
            let wtxn = db.write_txn().unwrap();
            relation_tombstone(&wtxn, victim, T0).unwrap();
            wtxn.commit().unwrap();
        }

        let mut expected: BTreeSet<[u8; 16]> = created.iter().map(|id| id.to_bytes()).collect();
        expected.remove(&victim.to_bytes());

        // Drain from page 1's key.
        let mut after = page1.last_key;
        let mut tail_seen: BTreeSet<[u8; 16]> = collected.clone();
        let mut guard = 0usize;
        loop {
            let rtxn = db.read_txn().unwrap();
            let page = relation_list_from_page(
                &rtxn, scope, anchor, &filter, false, after.as_deref(), limit,
            )
            .unwrap();
            drop(rtxn);
            for r in &page.rows {
                let b = r.id.to_bytes();
                prop_assert!(tail_seen.insert(b), "a relation was duplicated across pages");
                seen.push(b);
            }
            if !page.has_more {
                break;
            }
            after = page.last_key;
            guard += 1;
            prop_assert!(guard < 100_000, "relation pagination did not terminate");
        }

        assert_exact_set(&seen, &expected)?;
    }
}

// ---------------------------------------------------------------------------
// Property 6 — the scope wall holds under a foreign / malformed cursor.
//
// A cursor is opaque to the store: `statement_list_page` bounds its walk
// to the caller's (namespace, space) prefix regardless of the cursor's
// origin. So paging tenant B with a cursor id that belongs to a tenant-A
// row — or an all-zero / all-0xff id — can never surface a tenant-A row
// and never panics. (The wire codec additionally rejects such a token
// with an error; that is proven in brain-ops.)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, .. ProptestConfig::default() })]

    #[test]
    fn foreign_or_malformed_cursor_never_leaks_across_scopes(
        na in 1usize..=30,
        nb in 1usize..=30,
        limit in 1usize..=8,
        cursor_pick in 0usize..3,
    ) {
        let (_dir, db) = open_db();
        let (sa, sb) = (scope_a(), scope_b());
        let subj_a = put_entity(&db, sa, "subject_a");
        let subj_b = put_entity(&db, sb, "subject_b");
        let pred = intern_fact_pred(&db, "knows");

        let mut a_ids: Vec<StatementId> = Vec::with_capacity(na);
        let mut b_ids: BTreeSet<[u8; 16]> = BTreeSet::new();
        {
            let wtxn = db.write_txn().unwrap();
            for i in 0..na {
                let s = fact(subj_a, pred, &format!("a{i}"), 0.9);
                a_ids.push(statement_create(&wtxn, sa, SessionId::DEFAULT, &s, T0).unwrap());
            }
            for i in 0..nb {
                let s = fact(subj_b, pred, &format!("b{i}"), 0.9);
                b_ids.insert(
                    statement_create(&wtxn, sb, SessionId::DEFAULT, &s, T0)
                        .unwrap()
                        .to_bytes(),
                );
            }
            wtxn.commit().unwrap();
        }

        let filter = StatementListFilter {
            subject: Some(subj_b),
            predicate: Some(pred),
            kind: Some(StatementKind::Fact),
            current_only: false,
            min_confidence: None,
            limit: 0,
        };
        let extra = StatementPageExtra::default();

        // Pick a "foreign / malformed" cursor to resume tenant B's walk from.
        let cursor = match cursor_pick {
            0 => StatementListCursor { id: a_ids[0].to_bytes() }, // out-of-tenant id
            1 => StatementListCursor { id: [0x00; 16] },          // min id
            _ => StatementListCursor { id: [0xff; 16] },          // max id
        };

        // Walk tenant B from the foreign cursor to exhaustion. No panic,
        // no error, no dup, and never a tenant-A row.
        let mut after = Some(cursor);
        let mut got: BTreeSet<[u8; 16]> = BTreeSet::new();
        let a_set: BTreeSet<[u8; 16]> = a_ids.iter().map(|id| id.to_bytes()).collect();
        let mut guard = 0usize;
        loop {
            let rtxn = db.read_txn().unwrap();
            let page = statement_list_page(&rtxn, sb, &filter, &extra, after, limit).unwrap();
            drop(rtxn);
            for s in &page.rows {
                let b = s.id.to_bytes();
                prop_assert!(!a_set.contains(&b), "tenant-A row leaked into tenant-B page");
                prop_assert!(b_ids.contains(&b), "a returned row is not a tenant-B row");
                prop_assert!(got.insert(b), "duplicate row under foreign cursor");
            }
            if !page.has_more {
                break;
            }
            after = page.last;
            guard += 1;
            prop_assert!(guard < 100_000, "pagination did not terminate");
        }

        // And tenant B's own from-scratch walk still tiles B exactly —
        // the foreign cursor did not corrupt tenant B's own pagination.
        let seen_b = drain_statements(&db, sb, &filter, &extra, limit)?;
        assert_exact_set(&seen_b, &b_ids)?;
    }
}

// ---------------------------------------------------------------------------
// Bookend: a deterministic past-the-old-1000-ceiling walk, so a broken
// generator cannot silently turn every proptest above into a no-op and so
// the "rows past 1000 are reachable" regression stays pinned.
// ---------------------------------------------------------------------------

#[test]
fn deterministic_walk_reaches_past_the_old_1000_ceiling() {
    let (_dir, db) = open_db();
    let scope = scope_a();
    let subject = put_entity(&db, scope, "subject");
    let pred = intern_fact_pred(&db, "knows");

    let total = 1050usize; // strictly past the former 1000-row window.
    let mut expected: BTreeSet<[u8; 16]> = BTreeSet::new();
    {
        let wtxn = db.write_txn().unwrap();
        for i in 0..total {
            let s = fact(subject, pred, &format!("o{i}"), 0.9);
            let id = statement_create(&wtxn, scope, SessionId::DEFAULT, &s, T0).unwrap();
            expected.insert(id.to_bytes());
        }
        wtxn.commit().unwrap();
    }

    let filter = StatementListFilter {
        subject: Some(subject),
        predicate: Some(pred),
        kind: Some(StatementKind::Fact),
        current_only: false,
        min_confidence: None,
        limit: 0,
    };
    let seen = drain_statements(&db, scope, &filter, &StatementPageExtra::default(), 100)
        .expect("deterministic walk tiles cleanly");
    let got: BTreeSet<[u8; 16]> = seen.iter().copied().collect();
    assert_eq!(got.len(), seen.len(), "duplicate across pages");
    assert_eq!(
        got, expected,
        "all 1050 rows must be reachable and each once"
    );
}
