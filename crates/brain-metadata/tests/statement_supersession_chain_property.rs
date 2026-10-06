//! Property coverage for statement supersession CHAINS.
//!
//! The existing supersession coverage (colocated unit tests in
//! `statement/crud.rs` + `statement/supersede.rs`) is example-based and
//! only ever builds a two- or three-member chain by hand. The
//! entity-merge chaos suite explicitly deferred multi-member chains.
//! This file adds the missing layer: proptest-driven chains of length
//! K in `1..=8`, asserting the full well-formedness contract from
//! `spec/02_data_model/07_statement.md` (§"Statement supersession",
//! §"Versioning invariants") and the durability gate in
//! `spec/19_benchmarks/01_correctness_and_durability.md`.
//!
//! Three properties:
//!
//! 1. **A supersession chain is well-formed.** K sequential creates on
//!    one `(subject, predicate, single-valued kind)` each supersede the
//!    prior current value, producing one chain. Invariants: exactly one
//!    current row (the newest), dense versions `1..=K`, a stable
//!    `chain_root`, a valid doubly-linked `superseded_by`/`supersedes`
//!    list, anchor-flexible `statement_history`, and `valid_to`
//!    inheritance on every non-head row.
//!
//! 2. **Cumulative (Fact / Event) kinds do NOT supersede.** The
//!    contrast to (1): K distinct-object Facts (or distinct-time Events)
//!    all stay current, none superseded. A byte-identical repeat
//!    collapses (content dedup) to one row.
//!
//! 3. **Interleaved chains are independent.** Two different
//!    `(subject, predicate)` single-valued chains built interleaved
//!    never cross-contaminate — each keeps its own root / versions /
//!    current head.
//!
//! Each case opens a fresh redb tempfile, so case counts are kept modest
//! (48 / 48 / 32 / 32). If proptest contends on redb at high
//! parallelism, run with `--test-threads 4`.

use brain_core::{
    Entity, EntityId, EntityTypeId, EvidenceRef, ExtractorId, NamespaceId, PredicateId, SessionId,
    Statement, StatementId, StatementKind, StatementObject, StatementValue, SubjectRef,
};
use brain_metadata::{
    entity_put, normalize_name, predicate_intern, statement_create, statement_get,
    statement_history, statement_list, MetadataDb, RowScope, StatementListFilter,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

const T0: u64 = 1_700_000_000_000_000_000;
/// Wall-clock gap between successive creates. Distinct per step so
/// `valid_to` inheritance can be pinned to a specific successor time.
const STEP: u64 = 1_000_000_000; // 1 s
/// A `now` far past every extracted_at, used for `is_current(now)`.
const FAR_FUTURE: u64 = T0 + 1_000_000_000_000_000;

fn scope() -> RowScope {
    RowScope::from_bytes(NamespaceId::SYSTEM.raw(), [0xAB; 16])
}

fn open_db() -> (tempfile::TempDir, MetadataDb) {
    let dir = tempfile::tempdir().unwrap();
    let md = MetadataDb::open(dir.path().join("metadata.redb")).expect("open metadata");
    (dir, md)
}

/// Seed a Person entity; return its id.
fn put_entity(db: &MetadataDb, name: &str) -> EntityId {
    let id = EntityId::new();
    let wtxn = db.write_txn().unwrap();
    entity_put(
        &wtxn,
        scope(),
        SessionId::DEFAULT,
        &Entity::new_active(id, EntityTypeId(1), name.into(), normalize_name(name), T0),
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

/// Intern a `Value`-object predicate constrained to `kind`, with the
/// given `stateful` flag. Preference chains hinge on `stateful: true`;
/// Attribute / Directive supersede by kind regardless of the flag.
fn intern_pred(db: &MetadataDb, name: &str, kind: StatementKind, stateful: bool) -> PredicateId {
    let wtxn = db.write_txn().unwrap();
    let id = predicate_intern(
        &wtxn,
        "test",
        name,
        Some(kind),
        /* object: Value */ 2u8,
        /* schema_version */ 1,
        "",
        stateful,
        T0,
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

/// Create one `Value(Text)` statement in its own committed txn and
/// return the id it was stored under. For single-valued kinds this
/// auto-supersedes the prior current row; for cumulative kinds it
/// coexists (or dedups on identical content).
fn create_value_stmt(
    db: &MetadataDb,
    subject: EntityId,
    pred: PredicateId,
    kind: StatementKind,
    text: &str,
    event_at: Option<u64>,
    extracted_at: u64,
) -> StatementId {
    let mut s = Statement::new_root(
        StatementId::new(),
        kind,
        SubjectRef::Entity(subject),
        pred,
        StatementObject::Value(StatementValue::Text(text.into())),
        0.9,
        EvidenceRef::default(),
        ExtractorId::from(0),
        extracted_at,
        1,
    );
    if kind == StatementKind::Event {
        s.event_at_unix_nanos = event_at;
    }
    let wtxn = db.write_txn().unwrap();
    let stored = statement_create(&wtxn, scope(), SessionId::DEFAULT, &s, extracted_at).unwrap();
    wtxn.commit().unwrap();
    stored
}

// ---------------------------------------------------------------------------
// Generators.
// ---------------------------------------------------------------------------

/// The three single-valued kinds that form supersession chains, paired
/// with whether the predicate must be `stateful` to trigger it.
#[derive(Clone, Copy, Debug)]
enum SingleKind {
    Attribute,
    Directive,
    Preference,
}

impl SingleKind {
    fn kind(self) -> StatementKind {
        match self {
            SingleKind::Attribute => StatementKind::Attribute,
            SingleKind::Directive => StatementKind::Directive,
            SingleKind::Preference => StatementKind::Preference,
        }
    }

    /// Preference is cardinality `Set` by kind — it only supersedes when
    /// the predicate is declared `stateful`. Attribute / Directive
    /// supersede by kind, so the flag is irrelevant (kept `false` so any
    /// observed supersession is genuinely kind-driven).
    fn stateful_predicate(self) -> bool {
        matches!(self, SingleKind::Preference)
    }
}

fn arb_single_kind() -> impl Strategy<Value = SingleKind> {
    prop_oneof![
        Just(SingleKind::Attribute),
        Just(SingleKind::Directive),
        Just(SingleKind::Preference),
    ]
}

/// A short realistic value token.
fn arb_token() -> impl Strategy<Value = String> {
    "[a-z]{1,10}".prop_map(String::from)
}

/// `1..=8` value tokens; the vec length is the chain length K.
fn arb_chain_values() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(arb_token(), 1..=8)
}

// ---------------------------------------------------------------------------
// Shared assertion helpers (return TestCaseError so `prop_assert!` works).
// ---------------------------------------------------------------------------

/// Assert the full well-formedness contract for a single-valued chain
/// whose members were created in `order` (creation order == version
/// order for single-valued kinds). `extracted_ats[i]` is the
/// extracted_at stamped on `order[i]`.
fn assert_single_chain_wellformed(
    db: &MetadataDb,
    order: &[StatementId],
    extracted_ats: &[u64],
) -> Result<(), TestCaseError> {
    let k = order.len();
    let root = order[0];
    let head = order[k - 1];
    let rtxn = db.read_txn().unwrap();

    // Load every member.
    let mut rows = Vec::with_capacity(k);
    for id in order {
        let s = statement_get(&rtxn, *id)
            .unwrap()
            .ok_or_else(|| TestCaseError::fail(format!("chain member {id:?} missing")))?;
        rows.push(s);
    }

    // --- Exactly one current row (spec §Versioning invariants). ---
    let current: Vec<&Statement> = rows
        .iter()
        .filter(|s| s.superseded_by.is_none() && !s.tombstoned)
        .collect();
    prop_assert_eq!(
        current.len(),
        1,
        "exactly one row must be current; got {}",
        current.len()
    );
    prop_assert_eq!(current[0].id, head, "the current row must be the newest");
    prop_assert!(
        current[0].is_current(FAR_FUTURE),
        "the head must report is_current(now)"
    );

    // --- Dense versions 1..=K in creation order. ---
    for (i, s) in rows.iter().enumerate() {
        prop_assert_eq!(
            s.version,
            (i + 1) as u32,
            "version must be dense 1..=K along the chain"
        );
    }

    // --- chain_root stable = root id for every member. ---
    for s in &rows {
        prop_assert_eq!(
            s.chain_root,
            root,
            "chain_root must be stable across the chain"
        );
    }

    // --- Doubly-linked list from root to head. ---
    prop_assert_eq!(rows[0].supersedes, None, "root.supersedes must be None");
    prop_assert_eq!(
        rows[k - 1].superseded_by,
        None,
        "head.superseded_by must be None"
    );
    for i in 0..k - 1 {
        prop_assert_eq!(
            rows[i].superseded_by,
            Some(order[i + 1]),
            "old.superseded_by must point to its successor"
        );
        prop_assert_eq!(
            rows[i + 1].supersedes,
            Some(order[i]),
            "new.supersedes must point to its predecessor"
        );
    }

    // --- valid_to inheritance: each non-head row inherits its
    // successor's extracted_at; the head stays open-ended. ---
    for i in 0..k - 1 {
        prop_assert_eq!(
            rows[i].valid_to_unix_nanos,
            Some(extracted_ats[i + 1]),
            "superseded row must inherit valid_to = successor.extracted_at"
        );
    }
    prop_assert_eq!(
        rows[k - 1].valid_to_unix_nanos,
        None,
        "head must stay valid_to = None"
    );

    // --- statement_history from the root AND from any member returns
    // the full chain in creation (== version) order. ---
    let expected: Vec<StatementId> = order.to_vec();
    let from_root: Vec<StatementId> = statement_history(&rtxn, scope(), root)
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    prop_assert_eq!(
        &from_root,
        &expected,
        "history(root) must be the full chain in order"
    );
    for anchor in order {
        let from_anchor: Vec<StatementId> = statement_history(&rtxn, scope(), *anchor)
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        prop_assert_eq!(
            &from_anchor,
            &expected,
            "history(any member) must anchor to the same full chain"
        );
    }

    // --- current-only listing agrees: one row, the head. ---
    let cur = statement_list(
        &rtxn,
        scope(),
        &StatementListFilter {
            subject: Some(rows[0].subject.as_entity().unwrap()),
            predicate: Some(rows[0].predicate),
            kind: Some(rows[0].kind),
            current_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    prop_assert_eq!(
        cur.len(),
        1,
        "current_only listing must return exactly the head"
    );
    prop_assert_eq!(
        cur[0].id,
        head,
        "current_only listing must return the head id"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Property 1 — a supersession chain is well-formed.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    #[test]
    fn single_valued_chain_is_wellformed(
        sk in arb_single_kind(),
        values in arb_chain_values(),
    ) {
        let (_dir, db) = open_db();
        let subject = put_entity(&db, "priya");
        let kind = sk.kind();
        let pred = intern_pred(&db, "attr", kind, sk.stateful_predicate());

        let mut order = Vec::with_capacity(values.len());
        let mut extracted_ats = Vec::with_capacity(values.len());
        for (i, v) in values.iter().enumerate() {
            let at = T0 + (i as u64) * STEP;
            // Each create supersedes the prior current value regardless
            // of the object, so the stored id is a fresh row every time.
            let id = create_value_stmt(&db, subject, pred, kind, v, None, at);
            order.push(id);
            extracted_ats.push(at);
        }

        assert_single_chain_wellformed(&db, &order, &extracted_ats)?;
    }
}

// ---------------------------------------------------------------------------
// Property 2a — cumulative Facts do NOT supersede; identical repeats dedup.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    #[test]
    fn cumulative_facts_never_supersede(values in arb_chain_values()) {
        let (_dir, db) = open_db();
        let subject = put_entity(&db, "priya");
        // A plain Fact predicate (stateful: false) accumulates.
        let pred = intern_pred(&db, "knows", StatementKind::Fact, false);

        // Distinct objects (index-suffixed so no two collide → no dedup).
        let k = values.len();
        let mut order = Vec::with_capacity(k);
        for (i, v) in values.iter().enumerate() {
            let object = format!("{v}#{i}");
            let id = create_value_stmt(
                &db,
                subject,
                pred,
                StatementKind::Fact,
                &object,
                None,
                T0 + (i as u64) * STEP,
            );
            order.push(id);
        }

        let rtxn = db.read_txn().unwrap();

        // None superseded; all K current.
        for id in &order {
            let s = statement_get(&rtxn, *id).unwrap().unwrap();
            prop_assert_eq!(s.superseded_by, None, "cumulative Fact must not be superseded");
            prop_assert_eq!(s.supersedes, None, "cumulative Fact must not chain");
            prop_assert_eq!(s.version, 1, "cumulative Fact stays at version 1");
            prop_assert!(s.is_current(FAR_FUTURE), "every cumulative Fact stays current");
        }
        let cur = statement_list(
            &rtxn,
            scope(),
            &StatementListFilter {
                subject: Some(subject),
                predicate: Some(pred),
                kind: Some(StatementKind::Fact),
                current_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        prop_assert_eq!(cur.len(), k, "all K distinct Facts must coexist as current");
        drop(rtxn);

        // Byte-identical repeat of member 0 collapses to that row.
        let first_object = format!("{}#0", values[0]);
        let dup = create_value_stmt(
            &db,
            subject,
            pred,
            StatementKind::Fact,
            &first_object,
            None,
            T0 + 999 * STEP,
        );
        prop_assert_eq!(dup, order[0], "identical Fact re-apply must return the existing id");

        let rtxn = db.read_txn().unwrap();
        let cur = statement_list(
            &rtxn,
            scope(),
            &StatementListFilter {
                subject: Some(subject),
                predicate: Some(pred),
                kind: Some(StatementKind::Fact),
                current_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        prop_assert_eq!(cur.len(), k, "identical re-apply must not add a row");
    }
}

// ---------------------------------------------------------------------------
// Property 2b — cumulative Events (distinct times) coexist; identical dedup.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, .. ProptestConfig::default() })]

    #[test]
    fn cumulative_events_never_supersede(values in arb_chain_values()) {
        let (_dir, db) = open_db();
        let subject = put_entity(&db, "priya");
        let pred = intern_pred(&db, "scheduled", StatementKind::Event, false);

        // Distinct event times so each Event is its own row.
        let k = values.len();
        let mut order = Vec::with_capacity(k);
        let mut event_times = Vec::with_capacity(k);
        for (i, v) in values.iter().enumerate() {
            let event_at = T0 + 100 + (i as u64) * STEP;
            let id = create_value_stmt(
                &db,
                subject,
                pred,
                StatementKind::Event,
                v,
                Some(event_at),
                T0 + (i as u64) * STEP,
            );
            order.push(id);
            event_times.push(event_at);
        }

        let rtxn = db.read_txn().unwrap();
        for id in &order {
            let s = statement_get(&rtxn, *id).unwrap().unwrap();
            prop_assert_eq!(s.superseded_by, None, "Events never supersede");
            prop_assert_eq!(s.supersedes, None, "Events never chain");
            prop_assert!(s.is_current(FAR_FUTURE), "every Event stays current");
        }
        let cur = statement_list(
            &rtxn,
            scope(),
            &StatementListFilter {
                subject: Some(subject),
                predicate: Some(pred),
                kind: Some(StatementKind::Event),
                current_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        prop_assert_eq!(cur.len(), k, "all K distinct-time Events must coexist");
        drop(rtxn);

        // Identical object + event_at repeat of member 0 dedups.
        let dup = create_value_stmt(
            &db,
            subject,
            pred,
            StatementKind::Event,
            &values[0],
            Some(event_times[0]),
            T0 + 999 * STEP,
        );
        prop_assert_eq!(dup, order[0], "identical-time Event re-apply is a no-op");
    }
}

// ---------------------------------------------------------------------------
// Property 3 — interleaved single-valued chains stay independent.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, .. ProptestConfig::default() })]

    #[test]
    fn interleaved_chains_do_not_cross_contaminate(
        sk in arb_single_kind(),
        values_a in arb_chain_values(),
        values_b in arb_chain_values(),
    ) {
        let (_dir, db) = open_db();
        let kind = sk.kind();
        let stateful = sk.stateful_predicate();
        // Two independent (subject, predicate) chains of the same kind.
        let subj_a = put_entity(&db, "ada");
        let subj_b = put_entity(&db, "grace");
        let pred_a = intern_pred(&db, "attr_a", kind, stateful);
        let pred_b = intern_pred(&db, "attr_b", kind, stateful);

        let ka = values_a.len();
        let kb = values_b.len();
        let mut order_a = Vec::with_capacity(ka);
        let mut order_b = Vec::with_capacity(kb);
        let mut at_a = Vec::with_capacity(ka);
        let mut at_b = Vec::with_capacity(kb);

        // Interleave: A[i] then B[i] for each i, sharing a monotonic
        // wall-clock so no two writes collide on extracted_at.
        let mut tick = 0u64;
        for i in 0..ka.max(kb) {
            if i < ka {
                let at = T0 + tick * STEP;
                tick += 1;
                let id = create_value_stmt(&db, subj_a, pred_a, kind, &values_a[i], None, at);
                order_a.push(id);
                at_a.push(at);
            }
            if i < kb {
                let at = T0 + tick * STEP;
                tick += 1;
                let id = create_value_stmt(&db, subj_b, pred_b, kind, &values_b[i], None, at);
                order_b.push(id);
                at_b.push(at);
            }
        }

        // Each chain is independently well-formed.
        assert_single_chain_wellformed(&db, &order_a, &at_a)?;
        assert_single_chain_wellformed(&db, &order_b, &at_b)?;

        // Roots are distinct and neither chain's history references the
        // other's members.
        prop_assert_ne!(order_a[0], order_b[0], "the two chains must have distinct roots");
        let rtxn = db.read_txn().unwrap();
        let hist_a: Vec<StatementId> = statement_history(&rtxn, scope(), order_a[0])
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        let hist_b: Vec<StatementId> = statement_history(&rtxn, scope(), order_b[0])
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        for id in &order_b {
            prop_assert!(!hist_a.contains(id), "chain A must not contain a B member");
        }
        for id in &order_a {
            prop_assert!(!hist_b.contains(id), "chain B must not contain an A member");
        }
    }
}

// ---------------------------------------------------------------------------
// Bookend: a single deterministic case so a broken generator can't
// silently turn the proptests into no-ops.
// ---------------------------------------------------------------------------

#[test]
fn known_three_member_attribute_chain() {
    let (_dir, db) = open_db();
    let subject = put_entity(&db, "priya");
    let pred = intern_pred(&db, "city", StatementKind::Attribute, false);

    let order = vec![
        create_value_stmt(
            &db,
            subject,
            pred,
            StatementKind::Attribute,
            "Paris",
            None,
            T0,
        ),
        create_value_stmt(
            &db,
            subject,
            pred,
            StatementKind::Attribute,
            "Berlin",
            None,
            T0 + STEP,
        ),
        create_value_stmt(
            &db,
            subject,
            pred,
            StatementKind::Attribute,
            "Madrid",
            None,
            T0 + 2 * STEP,
        ),
    ];
    let ats = vec![T0, T0 + STEP, T0 + 2 * STEP];

    assert_single_chain_wellformed(&db, &order, &ats).expect("known 3-member chain well-formed");

    let rtxn = db.read_txn().unwrap();
    let head = statement_get(&rtxn, order[2]).unwrap().unwrap();
    assert_eq!(head.version, 3);
    assert_eq!(head.chain_root, order[0]);
}
