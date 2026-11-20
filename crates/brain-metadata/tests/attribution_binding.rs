//! Live-redb tests for `attribution::memory_binding` — the reader that
//! turns stored rows into the binding the attribution predicate judges.
//!
//! The predicate itself is pure and has its own unit tests beside it. What
//! needs a real database is the READ: which rows are consulted, which are
//! skipped, and what the `had_*` flags mean. Three things here are easy to
//! get silently wrong and are each asserted directly:
//!
//!   * a superseded statement's roles must not vote — it is a prior version
//!     of a claim the store has since replaced, so letting it bind would
//!     keep a corrected attribution alive forever;
//!   * `had_statements` is set from the EVIDENCE INDEX, not from the
//!     surviving rows, so "extraction ran and every claim was retracted"
//!     stays distinguishable from "extraction never ran";
//!   * an object role is collected as well as a subject role, so
//!     "what did Melanie tell Caroline" is not refused under Caroline.

use brain_core::{
    EdgeKindRef, Entity, EntityId, EntityTypeId, EvidenceEntry, EvidenceRef, ExtractorId, MemoryId,
    NodeRef, SessionId, Statement, StatementId, StatementKind, StatementObject, StatementValue,
};
use brain_metadata::attribution::{
    attribution_admits, memory_binding, AttributionVerdict, MemoryBinding,
};
use brain_metadata::entity::ops::entity_put;
use brain_metadata::schema::predicate::predicate_intern_or_get;
use brain_metadata::statement::statement_create;
use brain_metadata::tables::edge::{
    derived_by, link, origin, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
};
use brain_metadata::{DeclScope, MetadataDb, RowScope};
use redb::ReadableDatabase;

const T0: u64 = 1_700_000_000_000_000_000;
const SPACE: [u8; 16] = [0xAB; 16];

/// Explicit `HashSet` construction: `collect()` is ambiguous here because
/// rkyv adds its own `PartialEq` impls for `HashSet`.
fn set(ids: &[EntityId]) -> std::collections::HashSet<EntityId> {
    ids.iter().copied().collect()
}

fn scope() -> RowScope {
    RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), SPACE)
}

fn open_db() -> (tempfile::TempDir, MetadataDb) {
    let dir = tempfile::tempdir().unwrap();
    let md = MetadataDb::open(dir.path().join("metadata.redb")).expect("open metadata");
    (dir, md)
}

fn put_entity(db: &redb::Database, name: &str) -> EntityId {
    let id = EntityId::new();
    let wtxn = db.begin_write().unwrap();
    entity_put(
        &wtxn,
        scope(),
        SessionId::DEFAULT,
        &Entity::new_active(id, EntityTypeId(1), name.into(), name.into(), T0),
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

/// Write one statement citing `memory` as evidence. `mutate` is handed the
/// statement before the write so a case can tombstone or supersede it.
fn write_statement(
    db: &redb::Database,
    memory: MemoryId,
    subject: EntityId,
    object: StatementObject,
    mutate: impl FnOnce(&mut Statement),
) -> StatementId {
    let pid = {
        let wtxn = db.begin_write().unwrap();
        let pid =
            predicate_intern_or_get(&wtxn, DeclScope::base("test"), "owns", 0, T0).unwrap();
        wtxn.commit().unwrap();
        pid
    };
    let sid = StatementId::new();
    let mut stmt = Statement::new_root(
        sid,
        StatementKind::Fact,
        brain_core::SubjectRef::Entity(subject),
        pid,
        object,
        1.0,
        // confidence_milli = 0 suppresses the noisy-OR aggregation path.
        EvidenceRef::inline_from_slice(&[EvidenceEntry {
            memory_id: memory,
            confidence_milli: 0,
            timestamp_unix_nanos: T0,
            extractor_id: ExtractorId::default(),
        }]),
        ExtractorId::default(),
        T0,
        1,
    );
    mutate(&mut stmt);
    let wtxn = db.begin_write().unwrap();
    statement_create(&wtxn, scope(), SessionId::DEFAULT, &stmt, T0).expect("create");
    wtxn.commit().unwrap();
    sid
}

fn mention(db: &redb::Database, memory: MemoryId, entity: EntityId) {
    let wtxn = db.begin_write().unwrap();
    {
        let mut edges = wtxn.open_table(EDGES_TABLE).unwrap();
        let mut rev = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        link(
            &mut edges,
            &mut rev,
            NodeRef::Memory(memory),
            EdgeKindRef::Mentions,
            NodeRef::Entity(entity),
            zero_disambiguator(),
            &EdgeData::new(1.0, origin::EXPLICIT, derived_by::CLIENT, T0),
        )
        .unwrap();
    }
    wtxn.commit().unwrap();
}

fn binding(md: &MetadataDb, memory: MemoryId) -> MemoryBinding {
    let rtxn = md.db().begin_read().unwrap();
    memory_binding(&rtxn, scope(), memory)
}

/// A memory nothing has been written about reads as EMPTY, and the
/// predicate therefore admits it. This is the fail-soft floor the whole
/// design rests on: extraction is asynchronous, so an unbound memory is
/// the normal state right after a write, not a suspicious one.
#[test]
fn an_untouched_memory_has_an_empty_binding_and_is_admitted() {
    let (_dir, md) = open_db();
    let m = MemoryId::pack(1, SessionId::DEFAULT.into(), 1);
    let b = binding(&md, m);
    assert_eq!(b, MemoryBinding::default(), "no rows → no binding");
    let anchors = set(&[EntityId::new()]);
    assert_eq!(
        attribution_admits(&b, &anchors),
        AttributionVerdict::UnknownBinding,
        "silence is not denial"
    );
}

/// The headline read. One memory, a typed subject AND object role, plus a
/// mention — the shape the extractor actually writes — must come back whole,
/// and must discriminate between the two parties and a third.
#[test]
fn a_statement_contributes_both_its_subject_and_object_roles() {
    let (_dir, md) = open_db();
    let db = md.db();
    let melanie = put_entity(db, "Melanie");
    let caroline = put_entity(db, "Caroline");
    let stranger = EntityId::new();
    let m = MemoryId::pack(1, SessionId::DEFAULT.into(), 2);

    write_statement(
        db,
        m,
        melanie,
        StatementObject::Entity(caroline),
        |_| {},
    );
    mention(db, m, caroline);

    let b = binding(&md, m);
    assert_eq!(b.subject_roles, set(&[melanie]));
    assert_eq!(b.object_roles, set(&[caroline]));
    assert_eq!(b.mentions, set(&[caroline]));
    assert!(b.had_statements, "the evidence index yielded a row");
    assert!(b.had_mentions, "a Mentions edge leaves this memory");

    // Both roles bind; a third party does not.
    for who in [melanie, caroline] {
        assert_eq!(
            attribution_admits(&b, &set(&[who])),
            AttributionVerdict::BoundToAnchor
        );
    }
    assert_eq!(
        attribution_admits(&b, &set(&[stranger])),
        AttributionVerdict::MisAttributed
    );
}

/// A superseded statement is a prior version of a claim the store has since
/// replaced. Its roles must not vote, or a corrected attribution would keep
/// binding the memory to whoever the mistake named.
///
/// `had_statements` still holds: the evidence index DID yield a row, so this
/// memory is extracted — it just has no current claim. That distinction is
/// why the flag is read off the index rather than off the surviving rows.
#[test]
fn a_superseded_statement_contributes_no_roles() {
    let (_dir, md) = open_db();
    let db = md.db();
    let melanie = put_entity(db, "Melanie");
    let m = MemoryId::pack(1, SessionId::DEFAULT.into(), 3);

    let replacement = StatementId::new();
    write_statement(db, m, melanie, StatementObject::Value(StatementValue::Bool(true)), |s| {
        s.superseded_by = Some(replacement);
    });

    let b = binding(&md, m);
    assert!(
        b.subject_roles.is_empty(),
        "a replaced claim must not keep binding its subject"
    );
    assert!(b.had_statements, "the memory IS extracted, index-wise");
    assert!(!b.had_mentions);
    // No live role and no mention → nothing is known → admitted.
    assert_eq!(
        attribution_admits(&b, &set(&[melanie])),
        AttributionVerdict::UnknownBinding
    );
}

/// Same contract for a tombstoned row: retracted is not current truth.
#[test]
fn a_tombstoned_statement_contributes_no_roles() {
    let (_dir, md) = open_db();
    let db = md.db();
    let melanie = put_entity(db, "Melanie");
    let m = MemoryId::pack(1, SessionId::DEFAULT.into(), 4);

    // `statement_create` refuses a born-tombstoned row by design, so retract
    // it the way the substrate does — through the tombstone op.
    let sid = write_statement(
        db,
        m,
        melanie,
        StatementObject::Value(StatementValue::Bool(true)),
        |_| {},
    );
    {
        let wtxn = db.begin_write().unwrap();
        brain_metadata::statement::statement_tombstone(
            &wtxn,
            sid,
            brain_core::TombstoneReason::UserRequest,
            T0 + 1,
        )
        .expect("tombstone");
        wtxn.commit().unwrap();
    }

    let b = binding(&md, m);
    assert!(b.subject_roles.is_empty(), "a retracted claim does not bind");
    assert!(b.had_statements);
}

/// A mention-only memory — the extractor resolved a name but wrote no typed
/// claim — carries the mention and nothing else, and the predicate then lets
/// that mention decide because it is the only evidence there is.
#[test]
fn a_mention_only_memory_carries_just_its_mentions() {
    let (_dir, md) = open_db();
    let db = md.db();
    let melanie = put_entity(db, "Melanie");
    let caroline = EntityId::new();
    let m = MemoryId::pack(1, SessionId::DEFAULT.into(), 5);

    mention(db, m, melanie);

    let b = binding(&md, m);
    assert!(b.subject_roles.is_empty());
    assert!(b.object_roles.is_empty());
    assert!(!b.had_statements, "nothing cites this memory as evidence");
    assert!(b.had_mentions);
    assert_eq!(b.mentions, set(&[melanie]));
    assert_eq!(
        attribution_admits(&b, &set(&[melanie])),
        AttributionVerdict::BoundToAnchor
    );
    assert_eq!(
        attribution_admits(&b, &set(&[caroline])),
        AttributionVerdict::MisAttributed
    );
}

/// The scope walls hold. Statements are indexed per `(namespace, space)`, so
/// reading the same memory id under a DIFFERENT space must find no roles —
/// otherwise a cross-tenant row could drive another tenant's refusal.
#[test]
fn a_statement_in_another_space_does_not_bind() {
    let (_dir, md) = open_db();
    let db = md.db();
    let melanie = put_entity(db, "Melanie");
    let m = MemoryId::pack(1, SessionId::DEFAULT.into(), 6);
    write_statement(db, m, melanie, StatementObject::Value(StatementValue::Bool(true)), |_| {});

    let other = RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xCD; 16]);
    let rtxn = db.begin_read().unwrap();
    let b = memory_binding(&rtxn, other, m);
    assert!(
        b.subject_roles.is_empty() && !b.had_statements,
        "another space's statement is invisible here"
    );
}
