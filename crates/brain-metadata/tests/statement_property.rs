//! Property test: `statement_create` round-trips a freshly-built
//! `Statement` through redb so `statement_get` returns the exact
//! same value-shape.
//!
//! The proptest fuzzes the dimensions that the storage row
//! actually encodes: kind, confidence, optional timestamps, and
//! object value flavors (Text / Integer / Float / Bool / UnixNanos
//! / Blob). The `subject` and `predicate` are fixed per case — we
//! only assert that what goes in comes out, not that arbitrary
//! references resolve.

use brain_core::knowledge::{
    Entity, EvidenceEntry, EvidenceRef, Statement, StatementObject, StatementValue, SubjectRef,
};
use brain_core::{
    ContextId, EntityId, EntityTypeId, ExtractorId, MemoryId, StatementId, StatementKind,
};
use brain_metadata::entity_ops::entity_put;
use brain_metadata::predicate_ops::predicate_intern_or_get;
use brain_metadata::statement_ops::{statement_create, statement_get};
use brain_metadata::MetadataDb;
use proptest::prelude::*;
use redb::ReadableDatabase;

const T0: u64 = 1_700_000_000_000_000_000;

fn open_db() -> (tempfile::TempDir, MetadataDb) {
    let dir = tempfile::tempdir().unwrap();
    let md = MetadataDb::open(dir.path().join("metadata.redb")).expect("open metadata");
    (dir, md)
}

fn put_subject(db: &redb::Database) -> EntityId {
    let id = EntityId::new();
    let wtxn = db.begin_write().unwrap();
    entity_put(
        &wtxn,
        &Entity::new_active(id, EntityTypeId(1), "anchor".into(), "anchor".into(), T0),
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

// ---------------------------------------------------------------------------
// Generators.
// ---------------------------------------------------------------------------

fn arb_kind() -> impl Strategy<Value = StatementKind> {
    // Generate only Fact and Event — Preference would auto-supersede
    // the prior current row for the same (subject, predicate), which
    // is a separate code path tested in unit tests of statement_ops.
    prop_oneof![Just(StatementKind::Fact), Just(StatementKind::Event)]
}

fn arb_text() -> impl Strategy<Value = String> {
    "[ -~]{0,32}".prop_map(String::from)
}

fn arb_value() -> impl Strategy<Value = StatementValue> {
    prop_oneof![
        arb_text().prop_map(StatementValue::Text),
        any::<i64>().prop_map(StatementValue::Integer),
        any::<f64>()
            .prop_filter("no NaN — eq compare", |f| !f.is_nan())
            .prop_map(StatementValue::Float),
        any::<bool>().prop_map(StatementValue::Bool),
        any::<u64>().prop_map(StatementValue::UnixNanos),
        proptest::collection::vec(any::<u8>(), 0..=64).prop_map(StatementValue::Blob),
    ]
}

fn arb_confidence() -> impl Strategy<Value = f32> {
    // [0, 1] non-NaN — validate_statement_shape rejects anything else.
    (0.0f32..=1.0).prop_filter("non-NaN", |f| !f.is_nan())
}

fn arb_timestamp_pair() -> impl Strategy<Value = (Option<u64>, Option<u64>)> {
    // (valid_from, valid_to). When both are Some, valid_from < valid_to.
    prop_oneof![
        Just((None, None)),
        any::<u64>().prop_map(|t| (Some(t), None)),
        (1u64..1_000_000_000_000)
            .prop_flat_map(|from| (Just(from), (from + 1..=u64::MAX)))
            .prop_map(|(from, to)| (Some(from), Some(to))),
    ]
}

// ---------------------------------------------------------------------------
// Property: every legal Statement round-trips.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        .. ProptestConfig::default()
    })]

    #[test]
    fn statement_create_roundtrips_through_get(
        kind in arb_kind(),
        value in arb_value(),
        confidence in arb_confidence(),
        (valid_from, valid_to) in arb_timestamp_pair(),
        event_at in any::<u64>(),
        extracted_at in 1u64..=u64::from(u32::MAX),
    ) {
        let (_dir, md) = open_db();
        let db = md.db();
        let subject = put_subject(db);

        // Intern a predicate for this case so the storage layer's
        // predicate_get check finds something.
        let pid = {
            let wtxn = db.begin_write().unwrap();
            let pid = predicate_intern_or_get(&wtxn, "test", "rel", 0, T0).unwrap();
            wtxn.commit().unwrap();
            pid
        };

        let sid = StatementId::new();
        // confidence_milli=0 marks "no per-evidence metadata",
        // suppressing the noisy-OR aggregation path so the
        // statement's explicit `confidence` survives the write.
        let evidence_entry = EvidenceEntry {
            memory_id: MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
            confidence_milli: 0,
            timestamp_unix_nanos: 0,
            extractor_id: ExtractorId::default(),
        };
        let mut stmt = Statement::new_root(
            sid,
            kind,
            SubjectRef::Entity(subject),
            pid,
            StatementObject::Value(value),
            confidence,
            EvidenceRef::inline_from_slice(&[evidence_entry]),
            ExtractorId::default(),
            extracted_at,
            1,
        );
        stmt.valid_from_unix_nanos = valid_from;
        stmt.valid_to_unix_nanos = valid_to;
        // `Event` kind MUST carry event_at; Fact MUST NOT.
        stmt.event_at_unix_nanos = if kind == StatementKind::Event {
            // Avoid 0 — validate_statement_shape rejects 0 for Event.
            Some(event_at.max(1))
        } else {
            None
        };

        // Write inside a wtxn and commit.
        {
            let wtxn = db.begin_write().unwrap();
            let written = statement_create(&wtxn, &stmt, T0).expect("create");
            prop_assert_eq!(written, sid);
            wtxn.commit().unwrap();
        }

        // Read back and check every persisted field.
        let rtxn = db.begin_read().unwrap();
        let got = statement_get(&rtxn, sid).expect("get").expect("row exists");
        prop_assert_eq!(got.id, stmt.id);
        prop_assert_eq!(got.kind, stmt.kind);
        prop_assert_eq!(got.subject, stmt.subject);
        prop_assert_eq!(got.predicate, stmt.predicate);
        prop_assert_eq!(&got.object, &stmt.object);
        // Confidence is stored as `confidence_milli` (u16 * 1000); the
        // round-trip is lossy up to one milli, so allow that slack.
        prop_assert!(
            (got.confidence - stmt.confidence).abs() <= 1.0 / 1000.0 + f32::EPSILON,
            "confidence drift: stored={}, recovered={}",
            stmt.confidence,
            got.confidence,
        );
        prop_assert_eq!(got.extractor_id, stmt.extractor_id);
        prop_assert_eq!(got.extracted_at_unix_nanos, stmt.extracted_at_unix_nanos);
        prop_assert_eq!(got.schema_version, stmt.schema_version);
        prop_assert_eq!(got.valid_from_unix_nanos, stmt.valid_from_unix_nanos);
        prop_assert_eq!(got.valid_to_unix_nanos, stmt.valid_to_unix_nanos);
        prop_assert_eq!(got.event_at_unix_nanos, stmt.event_at_unix_nanos);
        prop_assert_eq!(got.version, stmt.version);
        prop_assert_eq!(got.superseded_by, stmt.superseded_by);
        prop_assert_eq!(got.supersedes, stmt.supersedes);
        prop_assert_eq!(got.chain_root, stmt.chain_root);
        prop_assert_eq!(got.tombstoned, stmt.tombstoned);
    }
}

// ---------------------------------------------------------------------------
// Bookend: a single static case so a regression that breaks the
// generator doesn't silently turn the proptest into a no-op.
// ---------------------------------------------------------------------------

#[test]
fn known_text_value_roundtrips() {
    let (_dir, md) = open_db();
    let db = md.db();
    let subject = put_subject(db);

    let pid = {
        let wtxn = db.begin_write().unwrap();
        let pid = predicate_intern_or_get(&wtxn, "test", "rel", 0, T0).unwrap();
        wtxn.commit().unwrap();
        pid
    };

    let sid = StatementId::new();
    let evidence_entry = EvidenceEntry::from_parts(
        MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
        1.0,
        0,
        ExtractorId::default(),
    );
    let stmt = Statement::new_root(
        sid,
        StatementKind::Fact,
        SubjectRef::Entity(subject),
        pid,
        StatementObject::Value(StatementValue::Text("hello world".into())),
        0.875,
        EvidenceRef::inline_from_slice(&[evidence_entry]),
        ExtractorId::default(),
        T0,
        1,
    );
    {
        let wtxn = db.begin_write().unwrap();
        statement_create(&wtxn, &stmt, T0).unwrap();
        wtxn.commit().unwrap();
    }
    let rtxn = db.begin_read().unwrap();
    let got = statement_get(&rtxn, sid).unwrap().unwrap();
    assert_eq!(got.object, stmt.object);
    assert_eq!(got.id, stmt.id);
}
