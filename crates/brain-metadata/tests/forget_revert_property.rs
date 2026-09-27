//! Property coverage for the FORGET undo-log revert round-trip.
//!
//! The colocated unit tests in `cascade.rs` pin the soft-FORGET →
//! revert path at a handful of hand-built points (two-evidence drop,
//! sole-evidence tombstone, the other-reason guard, a partial batch
//! replay). This file adds the missing layer: proptest-driven RANDOM
//! typed graphs (statements + relations citing a shared memory pool with
//! random evidence overlaps), asserting the full contract from
//! `cascade::cascade_revert_forget` and the durability gate in
//! `spec/19_benchmarks/01_correctness_and_durability.md`.
//!
//! Four properties:
//!
//! (a) **Round-trip identity.** Snapshot the full observable state of
//!     every dependent (evidence set, confidence, is_current,
//!     tombstoned + reason, and the STATEMENTS_BY_EVIDENCE /
//!     RELATION_BY_EVIDENCE reverse-index rows), soft-FORGET a random
//!     memory through the forward cascade (journaling undo), then
//!     `cascade_revert_forget` — and assert the observable state EQUALS
//!     the pre-FORGET snapshot for every dependent, with the undo log
//!     fully drained.
//!
//! (b) **Idempotency.** Running revert a second time over the same
//!     memory is a structural no-op: state unchanged, `scanned == 0`,
//!     no undo rows left.
//!
//! (c) **Reason guard.** A row tombstoned for a DIFFERENT reason than
//!     `SourceMemoryForgotten` (rewritten to `UserRequest` after the
//!     FORGET) is NOT un-tombstoned by revert — its evidence is
//!     restored but it stays tombstoned under the foreign reason, while
//!     a sibling still carrying `SourceMemoryForgotten` IS revived.
//!
//! (d) **Hard FORGET writes no undo.** A hard FORGET (`undo: None`)
//!     journals zero undo rows, so a subsequent revert restores nothing
//!     (state stays at the post-FORGET shape).
//!
//! Determinism note: proptest drives the graph *structure* (memory
//! count, per-dependent evidence masks, the forgotten memory). Ids are
//! freshly minted, so concrete bytes differ per run, but every asserted
//! invariant must hold for any id assignment. Evidence entries are
//! homogeneous (same confidence / timestamp), so the noisy-OR aggregate
//! is order-independent and confidence round-trips bit-exactly regardless
//! of the order revert re-attaches the dropped entry in.
//!
//! Each case opens a fresh redb tempfile; case counts are kept modest.
//! If proptest contends on redb at high parallelism, run with
//! `--test-threads 4`.

use brain_core::{
    Cardinality, Entity, EntityId, EntityType, ExtractorId, MemoryId, Relation, RelationId,
    RelationTypeId, SessionId, Statement, StatementId, StatementKind, StatementObject,
    StatementValue, SubjectRef,
};
use brain_metadata::cascade::{
    cascade_forget_to_edges, cascade_forget_to_statements, cascade_revert_forget, RevertSummary,
    UndoWriteCtx,
};
use brain_metadata::tables::forget_undo::FORGET_UNDO_LOG_TABLE;
use brain_metadata::tables::relation::{
    RelationMetadata, RELATION_BY_EVIDENCE_TABLE, RELATION_METADATA_TABLE,
};
use brain_metadata::tables::statement::{
    tombstone_reason, StatementMetadata, STATEMENTS_BY_EVIDENCE_TABLE, STATEMENTS_TABLE,
};
use brain_metadata::{
    entity_put, normalize_name, pack_evidence_ids, predicate_intern, relation_create,
    relation_type_intern, statement_create, MetadataDb, RowScope,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use redb::ReadableTable;

/// Single wall-clock used for every create / forget / revert. Holding
/// `now` fixed keeps the per-entry decay identical across all three
/// phases, so the noisy-OR confidence a create stamps, a forget shrinks,
/// and a revert recomputes all agree bit-for-bit.
const NOW: u64 = 1_700_000_000_000_000_000;
/// Grace-window expiry stamped onto journaled undo rows. Revert never
/// consults it (that is slot-reclamation's job), so any far-future value
/// works.
const GRACE: u64 = 3_600_000_000_000;
/// Confidence floor below which an emptied-evidence statement tombstones.
/// Positive, so a sole-evidence FORGET always tombstones (never
/// kept-stale) — keeping the round-trip's two outcomes crisp.
const THRESHOLD: f32 = 0.2;
/// Every evidence entry carries this confidence — homogeneous so the
/// aggregate is order-independent.
const EV_CONF: f32 = 0.9;
const BATCH: usize = 4096;

fn scope() -> RowScope {
    RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
}

fn open_db() -> (tempfile::TempDir, MetadataDb) {
    let dir = tempfile::tempdir().unwrap();
    let md = MetadataDb::open(dir.path().join("metadata.redb")).expect("open metadata");
    (dir, md)
}

/// Insert a fresh Person entity; return its id.
fn put_entity(db: &MetadataDb, name: &str) -> EntityId {
    let id = EntityId::new();
    let wtxn = db.write_txn().unwrap();
    entity_put(
        &wtxn,
        scope(),
        SessionId::DEFAULT,
        &Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            normalize_name(name),
            NOW,
        ),
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

/// Intern a plain cumulative Fact predicate (stateful: false) — distinct
/// objects coexist as current rows, so N generated statements never
/// collapse into a supersession chain.
fn intern_fact_pred(db: &MetadataDb, name: &str) -> brain_core::PredicateId {
    let wtxn = db.write_txn().unwrap();
    let id = predicate_intern(
        &wtxn,
        "test",
        name,
        Some(StatementKind::Fact),
        /* object: Value */ 2u8,
        /* schema_version */ 1,
        "",
        /* stateful */ false,
        NOW,
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

/// Intern a permissive ManyToMany relation type — no cardinality
/// supersession, so distinct-endpoint relations all coexist.
fn intern_rel_type(db: &MetadataDb, name: &str) -> RelationTypeId {
    let wtxn = db.write_txn().unwrap();
    let id = relation_type_intern(
        &wtxn,
        "test",
        name,
        None,
        None,
        Cardinality::ManyToMany,
        /* is_symmetric */ false,
        1,
        "",
        NOW,
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

/// Create one cumulative Fact with `evidence` and a unique object text.
fn create_fact(
    db: &MetadataDb,
    subject: EntityId,
    pred: brain_core::PredicateId,
    object: &str,
    evidence: &[MemoryId],
) -> StatementId {
    let wtxn = db.write_txn().unwrap();
    let ev =
        pack_evidence_ids(&wtxn, evidence.to_vec(), EV_CONF, NOW, ExtractorId::from(0)).unwrap();
    let s = Statement::new_root(
        StatementId::new(),
        StatementKind::Fact,
        SubjectRef::Entity(subject),
        pred,
        StatementObject::Value(StatementValue::Text(object.into())),
        EV_CONF,
        ev,
        ExtractorId::from(0),
        NOW,
        1,
    );
    let id = statement_create(&wtxn, scope(), SessionId::DEFAULT, &s, NOW).unwrap();
    wtxn.commit().unwrap();
    id
}

/// Create one relation between fresh endpoints with `evidence`.
fn create_relation(
    db: &MetadataDb,
    rel_type: RelationTypeId,
    from: EntityId,
    to: EntityId,
    evidence: &[MemoryId],
) -> RelationId {
    let mut r = Relation::new_root(
        RelationId::new(),
        rel_type,
        from,
        to,
        0.8,
        evidence.to_vec(),
        ExtractorId::from(0),
        NOW,
        false,
    );
    r.is_symmetric = false;
    let id = r.id;
    let wtxn = db.write_txn().unwrap();
    relation_create(&wtxn, scope(), SessionId::DEFAULT, &r, NOW).unwrap();
    wtxn.commit().unwrap();
    id
}

/// Soft FORGET `mem`: cascade both statement + edge sides in one wtxn,
/// journaling undo (reversible during grace).
fn soft_forget(db: &MetadataDb, mem: MemoryId) {
    let undo = Some(UndoWriteCtx {
        grace_expiry_unix_nanos: NOW + GRACE,
    });
    let wtxn = db.write_txn().unwrap();
    cascade_forget_to_statements(&wtxn, mem, THRESHOLD, BATCH, NOW, undo).unwrap();
    cascade_forget_to_edges(&wtxn, scope(), mem, NOW, undo).unwrap();
    wtxn.commit().unwrap();
}

/// Hard FORGET `mem`: same cascade with `undo: None` — no undo log.
fn hard_forget(db: &MetadataDb, mem: MemoryId) {
    let wtxn = db.write_txn().unwrap();
    cascade_forget_to_statements(&wtxn, mem, THRESHOLD, BATCH, NOW, None).unwrap();
    cascade_forget_to_edges(&wtxn, scope(), mem, NOW, None).unwrap();
    wtxn.commit().unwrap();
}

/// Replay the undo log for `mem` and return the summary.
fn revert(db: &MetadataDb, mem: MemoryId) -> RevertSummary {
    let wtxn = db.write_txn().unwrap();
    let s = cascade_revert_forget(&wtxn, mem, NOW, BATCH).unwrap();
    wtxn.commit().unwrap();
    s
}

// ---------------------------------------------------------------------------
// Observation helpers — read the full stored state back out.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct StmtObs {
    /// Evidence memory ids, sorted (revert may re-append in a different
    /// order, so identity is a SET property).
    evidence: Vec<[u8; 16]>,
    confidence: f32,
    is_current: u8,
    tombstoned: u8,
    tombstone_reason: u8,
    /// Which of `all_mems` still own a STATEMENTS_BY_EVIDENCE row for
    /// this statement, sorted.
    by_evidence: Vec<[u8; 16]>,
}

#[derive(Clone, Debug, PartialEq)]
struct RelObs {
    evidence: Vec<[u8; 16]>,
    confidence: f32,
    is_current: u8,
    tombstoned: u8,
    by_evidence: Vec<[u8; 16]>,
}

fn stmt_row(db: &MetadataDb, id: StatementId) -> StatementMetadata {
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
    t.get(&id.to_bytes()).unwrap().unwrap().value()
}

fn rel_meta(db: &MetadataDb, id: RelationId) -> RelationMetadata {
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(RELATION_METADATA_TABLE).unwrap();
    t.get(&id.to_bytes()).unwrap().unwrap().value()
}

fn stmt_obs(db: &MetadataDb, id: StatementId, all_mems: &[MemoryId]) -> StmtObs {
    let row = stmt_row(db, id);
    // Evidence never overflows here: a case's evidence set is a subset of
    // <= 6 memories, well under INLINE_EVIDENCE_CAP (8).
    let mut evidence: Vec<[u8; 16]> = row
        .evidence_inline
        .iter()
        .map(|e| e.memory_id_bytes)
        .collect();
    evidence.sort_unstable();

    let sc = scope();
    let rtxn = db.read_txn().unwrap();
    let by = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).unwrap();
    let mut by_evidence: Vec<[u8; 16]> = all_mems
        .iter()
        .map(|m| m.to_be_bytes())
        .filter(|mb| {
            by.get(&(sc.namespace_id, sc.space_id_bytes, *mb, id.to_bytes()))
                .unwrap()
                .is_some()
        })
        .collect();
    by_evidence.sort_unstable();

    StmtObs {
        evidence,
        confidence: row.confidence,
        is_current: row.is_current,
        tombstoned: row.tombstoned,
        tombstone_reason: row.tombstone_reason,
        by_evidence,
    }
}

fn rel_obs(db: &MetadataDb, id: RelationId, all_mems: &[MemoryId]) -> RelObs {
    let meta = rel_meta(db, id);
    let mut evidence = meta.evidence_inline.clone();
    evidence.sort_unstable();

    let sc = scope();
    let rtxn = db.read_txn().unwrap();
    let by = rtxn.open_table(RELATION_BY_EVIDENCE_TABLE).unwrap();
    let mut by_evidence: Vec<[u8; 16]> = all_mems
        .iter()
        .map(|m| m.to_be_bytes())
        .filter(|mb| {
            by.get(&(sc.namespace_id, sc.space_id_bytes, *mb, id.to_bytes()))
                .unwrap()
                .is_some()
        })
        .collect();
    by_evidence.sort_unstable();

    RelObs {
        evidence,
        confidence: meta.confidence,
        is_current: meta.is_current,
        tombstoned: meta.tombstoned,
        by_evidence,
    }
}

/// Total undo rows keyed at `mem`.
fn undo_rows(db: &MetadataDb, mem: MemoryId) -> usize {
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(FORGET_UNDO_LOG_TABLE).unwrap();
    let lo = (mem.to_be_bytes(), [0u8; 16]);
    let hi = (mem.to_be_bytes(), [0xFFu8; 16]);
    t.range(lo..=hi).unwrap().count()
}

/// Total undo rows across the whole table (all memories).
fn undo_rows_total(db: &MetadataDb) -> usize {
    let rtxn = db.read_txn().unwrap();
    let t = rtxn.open_table(FORGET_UNDO_LOG_TABLE).unwrap();
    t.iter().unwrap().count()
}

// ---------------------------------------------------------------------------
// Graph generator.
// ---------------------------------------------------------------------------

/// A random typed-graph shape: `n_mem` memories, per-statement and
/// per-relation evidence masks over those memories, and the index of the
/// memory to forget.
#[derive(Clone, Debug)]
struct GraphSpec {
    n_mem: usize,
    /// One evidence mask per statement (length `n_mem`).
    stmts: Vec<Vec<bool>>,
    /// One evidence mask per relation (length `n_mem`).
    rels: Vec<Vec<bool>>,
    /// Index into the memory pool to forget.
    forget: usize,
}

fn arb_mask(n: usize) -> impl Strategy<Value = Vec<bool>> {
    proptest::collection::vec(any::<bool>(), n)
}

fn arb_graph() -> impl Strategy<Value = GraphSpec> {
    (2usize..=6)
        .prop_flat_map(|n_mem| {
            (
                Just(n_mem),
                proptest::collection::vec(arb_mask(n_mem), 1..=6),
                proptest::collection::vec(arb_mask(n_mem), 0..=4),
                0..n_mem,
            )
        })
        .prop_map(|(n_mem, stmts, rels, forget)| GraphSpec {
            n_mem,
            stmts,
            rels,
            forget,
        })
}

/// Materialize a `GraphSpec` into a fresh db. Returns the memory pool,
/// the created statement ids, the relation ids, and the memory to forget.
/// Statement 0 (and relation 0 when present) are forced to cite the
/// forget target so the FORGET always has at least one dependent.
fn build_graph(
    db: &MetadataDb,
    spec: &GraphSpec,
) -> (Vec<MemoryId>, Vec<StatementId>, Vec<RelationId>, MemoryId) {
    let mems: Vec<MemoryId> = (0..spec.n_mem)
        .map(|i| MemoryId::pack(i as u16 + 1, SessionId::DEFAULT.into(), 0))
        .collect();
    let subject = put_entity(db, "subject");
    let pred = intern_fact_pred(db, "cites");
    let rel_type = intern_rel_type(db, "linked");

    let mask_to_ids = |mask: &[bool], force: bool| -> Vec<MemoryId> {
        let mut ids: Vec<MemoryId> = mask
            .iter()
            .enumerate()
            .filter_map(|(j, &on)| if on { Some(mems[j]) } else { None })
            .collect();
        if force && !ids.contains(&mems[spec.forget]) {
            ids.push(mems[spec.forget]);
        }
        if ids.is_empty() {
            ids.push(mems[0]);
        }
        ids
    };

    let mut stmt_ids = Vec::with_capacity(spec.stmts.len());
    for (i, mask) in spec.stmts.iter().enumerate() {
        let ev = mask_to_ids(mask, i == 0);
        let object = format!("obj-{i}");
        stmt_ids.push(create_fact(db, subject, pred, &object, &ev));
    }

    let mut rel_ids = Vec::with_capacity(spec.rels.len());
    for (i, mask) in spec.rels.iter().enumerate() {
        let ev = mask_to_ids(mask, i == 0);
        let from = put_entity(db, &format!("rel-{i}-from"));
        let to = put_entity(db, &format!("rel-{i}-to"));
        rel_ids.push(create_relation(db, rel_type, from, to, &ev));
    }

    let forget = mems[spec.forget];
    (mems, stmt_ids, rel_ids, forget)
}

// ---------------------------------------------------------------------------
// (a) round-trip identity + (b) idempotency.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    #[test]
    fn soft_forget_revert_round_trip_and_idempotent(spec in arb_graph()) {
        let (_dir, db) = open_db();
        let (mems, stmts, rels, forget) = build_graph(&db, &spec);

        // Pre-FORGET snapshot of every dependent.
        let stmt_before: Vec<StmtObs> = stmts.iter().map(|s| stmt_obs(&db, *s, &mems)).collect();
        let rel_before: Vec<RelObs> = rels.iter().map(|r| rel_obs(&db, *r, &mems)).collect();

        soft_forget(&db, forget);
        revert(&db, forget);

        // (a) Every dependent is bit-for-bit back to its pre-FORGET shape.
        for (s, before) in stmts.iter().zip(&stmt_before) {
            let after = stmt_obs(&db, *s, &mems);
            prop_assert_eq!(&after, before, "statement {:?} did not round-trip", s);
        }
        for (r, before) in rels.iter().zip(&rel_before) {
            let after = rel_obs(&db, *r, &mems);
            prop_assert_eq!(&after, before, "relation {:?} did not round-trip", r);
        }
        // Undo log fully drained by the revert.
        prop_assert_eq!(undo_rows(&db, forget), 0, "undo rows must be consumed by revert");

        // (b) A second revert is a structural no-op.
        let again = revert(&db, forget);
        prop_assert_eq!(again.scanned, 0, "second revert must scan no undo rows");
        prop_assert_eq!(again.statements_reverted, 0);
        prop_assert_eq!(again.relations_reverted, 0);
        prop_assert_eq!(undo_rows(&db, forget), 0);
        for (s, before) in stmts.iter().zip(&stmt_before) {
            prop_assert_eq!(&stmt_obs(&db, *s, &mems), before, "state changed on 2nd revert");
        }
        for (r, before) in rels.iter().zip(&rel_before) {
            prop_assert_eq!(&rel_obs(&db, *r, &mems), before, "state changed on 2nd revert");
        }
    }
}

// ---------------------------------------------------------------------------
// (c) reason guard — a foreign tombstone reason is never reverted.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, .. ProptestConfig::default() })]

    #[test]
    fn revert_respects_foreign_tombstone_reason(
        n_mem in 2usize..=5,
        extra in proptest::collection::vec(arb_mask_dyn(), 0..=3),
    ) {
        let (_dir, db) = open_db();
        let mems: Vec<MemoryId> = (0..n_mem)
            .map(|i| MemoryId::pack(i as u16 + 1, SessionId::DEFAULT.into(), 0))
            .collect();
        let forget = mems[0];
        let subject = put_entity(&db, "subject");
        let pred = intern_fact_pred(&db, "cites");

        // Two sole-evidence statements on the forget target — both will be
        // tombstoned SourceMemoryForgotten by the soft FORGET.
        let s_guarded = create_fact(&db, subject, pred, "guarded", &[forget]);
        let s_sibling = create_fact(&db, subject, pred, "sibling", &[forget]);

        // Some extra multi-evidence statements citing random subsets that
        // include the forget target.
        for (i, mask) in extra.iter().enumerate() {
            let mut ev: Vec<MemoryId> = mask
                .iter()
                .take(n_mem)
                .enumerate()
                .filter_map(|(j, &on)| if on { Some(mems[j]) } else { None })
                .collect();
            if !ev.contains(&forget) {
                ev.push(forget);
            }
            create_fact(&db, subject, pred, &format!("extra-{i}"), &ev);
        }

        soft_forget(&db, forget);
        // Both sole-evidence statements are tombstoned for FORGET.
        prop_assert_eq!(
            stmt_row(&db, s_guarded).tombstone_reason,
            tombstone_reason::SOURCE_MEMORY_FORGOTTEN
        );

        // Rewrite the guarded statement's reason to a foreign one — as if
        // the operator deleted it for an unrelated reason after the FORGET.
        {
            let wtxn = db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(STATEMENTS_TABLE).unwrap();
                let mut row = t.get(&s_guarded.to_bytes()).unwrap().unwrap().value();
                row.tombstone_reason = tombstone_reason::USER_REQUEST;
                t.insert(&s_guarded.to_bytes(), &row).unwrap();
            }
            wtxn.commit().unwrap();
        }

        revert(&db, forget);

        // Guarded row: evidence restored, but tombstone respected.
        let g = stmt_obs(&db, s_guarded, &mems);
        prop_assert_eq!(g.tombstoned, 1, "foreign-reason row must stay tombstoned");
        prop_assert_eq!(
            g.tombstone_reason,
            tombstone_reason::USER_REQUEST,
            "foreign reason must be preserved"
        );
        prop_assert_eq!(g.evidence, vec![forget.to_be_bytes()], "evidence still re-attached");
        prop_assert_eq!(
            g.by_evidence,
            vec![forget.to_be_bytes()],
            "reverse-index row still restored"
        );

        // Sibling row: FORGET reason intact → fully revived.
        let sib = stmt_obs(&db, s_sibling, &mems);
        prop_assert_eq!(sib.tombstoned, 0, "sibling must be un-tombstoned");
        prop_assert_eq!(sib.tombstone_reason, tombstone_reason::NOT_TOMBSTONED);
        prop_assert_eq!(sib.is_current, 1);
        prop_assert_eq!(sib.evidence, vec![forget.to_be_bytes()]);
    }
}

/// A dynamic-length evidence mask (up to 5 memories) for property (c),
/// where the memory count is generated independently of the mask.
fn arb_mask_dyn() -> impl Strategy<Value = Vec<bool>> {
    proptest::collection::vec(any::<bool>(), 5)
}

// ---------------------------------------------------------------------------
// (d) hard FORGET writes no undo; revert restores nothing.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, .. ProptestConfig::default() })]

    #[test]
    fn hard_forget_writes_no_undo_log(spec in arb_graph()) {
        let (_dir, db) = open_db();
        let (mems, stmts, rels, forget) = build_graph(&db, &spec);

        hard_forget(&db, forget);

        // No undo rows anywhere.
        prop_assert_eq!(undo_rows(&db, forget), 0, "hard FORGET must journal no undo rows");
        prop_assert_eq!(undo_rows_total(&db), 0, "undo table must be empty after hard FORGET");

        // Snapshot the post-hard-FORGET state.
        let stmt_post: Vec<StmtObs> = stmts.iter().map(|s| stmt_obs(&db, *s, &mems)).collect();
        let rel_post: Vec<RelObs> = rels.iter().map(|r| rel_obs(&db, *r, &mems)).collect();

        // Revert restores nothing — no undo rows to replay.
        let summary = revert(&db, forget);
        prop_assert_eq!(summary.scanned, 0);
        prop_assert_eq!(summary.statements_reverted, 0);
        prop_assert_eq!(summary.relations_reverted, 0);

        for (s, post) in stmts.iter().zip(&stmt_post) {
            prop_assert_eq!(&stmt_obs(&db, *s, &mems), post, "hard-forgotten statement changed");
        }
        for (r, post) in rels.iter().zip(&rel_post) {
            prop_assert_eq!(&rel_obs(&db, *r, &mems), post, "hard-forgotten relation changed");
        }
    }
}

// ---------------------------------------------------------------------------
// Bookend: a deterministic mixed graph so a broken generator can't turn
// the proptests into no-ops.
// ---------------------------------------------------------------------------

#[test]
fn known_mixed_graph_round_trips() -> Result<(), TestCaseError> {
    let (_dir, db) = open_db();
    let mems: Vec<MemoryId> = (0..3)
        .map(|i| MemoryId::pack(i as u16 + 1, SessionId::DEFAULT.into(), 0))
        .collect();
    let forget = mems[0];
    let subject = put_entity(&db, "priya");
    let pred = intern_fact_pred(&db, "cites");
    let rel_type = intern_rel_type(&db, "linked");

    // s_sole: only the forget target → tombstones on soft FORGET.
    let s_sole = create_fact(&db, subject, pred, "sole", &[mems[0]]);
    // s_multi: forget target + a survivor → evidence-dropped, kept live.
    let s_multi = create_fact(&db, subject, pred, "multi", &[mems[0], mems[1]]);
    // s_untouched: does not cite the forget target at all.
    let s_untouched = create_fact(&db, subject, pred, "untouched", &[mems[2]]);
    // r_sole: forget target as sole evidence → relation tombstones.
    let a = put_entity(&db, "a");
    let b = put_entity(&db, "b");
    let r_sole = create_relation(&db, rel_type, a, b, &[mems[0]]);

    let ids_s = [s_sole, s_multi, s_untouched];
    let before_s: Vec<StmtObs> = ids_s.iter().map(|s| stmt_obs(&db, *s, &mems)).collect();
    let before_r = rel_obs(&db, r_sole, &mems);

    soft_forget(&db, forget);
    // Sanity: the FORGET actually did something.
    assert!(stmt_row(&db, s_sole).is_tombstoned());
    assert!(rel_meta(&db, r_sole).is_tombstoned());
    assert!(!stmt_row(&db, s_multi).is_tombstoned());

    let rs = revert(&db, forget);
    assert_eq!(rs.statements_untombstoned, 1);
    assert_eq!(rs.relations_untombstoned, 1);

    for (s, before) in ids_s.iter().zip(&before_s) {
        prop_assert_eq!(
            &stmt_obs(&db, *s, &mems),
            before,
            "statement {:?} not restored",
            s
        );
    }
    prop_assert_eq!(
        &rel_obs(&db, r_sole, &mems),
        &before_r,
        "relation not restored"
    );
    prop_assert_eq!(undo_rows(&db, forget), 0);
    // s_untouched must be byte-identical (never journaled, never touched).
    let _ = s_untouched;
    Ok(())
}
