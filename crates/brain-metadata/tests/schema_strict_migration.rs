//! Schemaless v0 → schema-strict v1 migration coverage.
//!
//! Scenario: a deployment runs schemaless for some time, writes
//! statements against open-vocabulary predicates, then the operator
//! declares a schema via `SCHEMA_UPLOAD`. The contract is:
//!
//! - Pre-existing rows remain readable. Strict mode does NOT
//!   retroactively delete data.
//! - Statements whose predicate is NOT in the newly-active schema get
//!   the `OUTSIDE_ACTIVE_SCHEMA` flag set so a strict query can choose
//!   to filter them out without losing the row.
//! - Statements whose predicate IS in the newly-active schema do NOT
//!   carry the flag.
//! - A second schema upload that *drops* a previously-declared
//!   predicate retroactively re-flags the affected rows.
//! - A schema upload that *adopts* a previously-implicit predicate
//!   clears the flag on rows that point at it.

use brain_core::knowledge::{
    Entity, EvidenceEntry, EvidenceRef, Statement, StatementObject, StatementValue, SubjectRef,
};
use brain_core::{
    ContextId, EntityId, EntityTypeId, ExtractorId, MemoryId, StatementId, StatementKind,
};
use brain_metadata::entity_ops::entity_put;
use brain_metadata::predicate_ops::predicate_intern_or_get;
use brain_metadata::schema_store::schema_upload;
use brain_metadata::statement_ops::{statement_create, statement_get};
use brain_metadata::tables::knowledge::statement::{statement_flags, STATEMENTS_TABLE};
use brain_metadata::MetadataDb;
use brain_protocol::schema::{parse_schema, validate, ValidatedSchema};
use redb::{ReadableDatabase, ReadableTable};

const T0: u64 = 1_700_000_000_000_000_000;

fn open_metadata(dir: &tempfile::TempDir) -> MetadataDb {
    MetadataDb::open(dir.path().join("metadata.redb")).expect("open metadata")
}

fn validated_schema(src: &str) -> ValidatedSchema {
    let s = parse_schema(src).expect("parse");
    validate(&s).expect("validate")
}

fn schema_with_predicates(namespace: &str, names: &[&str]) -> ValidatedSchema {
    // `kind: Fact` so the test's Fact-kind statements satisfy the
    // schema-strict `validate_against_predicate` check.
    let predicates: String = names
        .iter()
        .map(|n| format!("define predicate {n} {{ kind: Fact object: Value<text> }}\n"))
        .collect();
    let src = format!(
        "
        namespace {namespace}
        define entity_type Person {{ attributes {{}} }}
        {predicates}
        "
    );
    validated_schema(&src)
}

fn put_anchor_entity(db: &redb::Database) -> EntityId {
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

fn write_statement(
    db: &redb::Database,
    subject: EntityId,
    predicate_ns: &str,
    predicate_name: &str,
) -> StatementId {
    let wtxn = db.begin_write().unwrap();
    let pid = predicate_intern_or_get(&wtxn, predicate_ns, predicate_name, 0, T0).unwrap();
    let evidence_entry = EvidenceEntry::from_parts(
        MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
        1.0,
        0,
        ExtractorId::default(),
    );
    let stmt = Statement::new_root(
        StatementId::new(),
        StatementKind::Fact,
        SubjectRef::Entity(subject),
        pid,
        StatementObject::Value(StatementValue::Text("hello".into())),
        0.9,
        EvidenceRef::inline_from_slice(&[evidence_entry]),
        ExtractorId::default(),
        0,
        1,
    );
    let sid = statement_create(&wtxn, &stmt, T0).unwrap();
    wtxn.commit().unwrap();
    sid
}

fn statement_flags_word(db: &redb::Database, sid: StatementId) -> u32 {
    let rtxn = db.begin_read().unwrap();
    let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
    t.get(&sid.to_bytes())
        .unwrap()
        .expect("row exists")
        .value()
        .flags
}

// ---------------------------------------------------------------------------
// Core migration: pre-existing rows remain readable; in-schema ones stay
// clean, out-of-schema ones get the flag.
// ---------------------------------------------------------------------------

#[test]
fn schemaless_to_strict_flags_only_out_of_vocabulary_rows() {
    let dir = tempfile::tempdir().unwrap();
    let md = open_metadata(&dir);
    let db = md.db();

    let subject = put_anchor_entity(db);

    // Schemaless world: write three statements, two predicates.
    let s_in_a = write_statement(db, subject, "acme", "prefers");
    let s_in_b = write_statement(db, subject, "acme", "prefers"); // same predicate, another row
    let s_out = write_statement(db, subject, "acme", "ghost");

    // Pre-upload: no flags.
    assert_eq!(statement_flags_word(db, s_in_a), 0);
    assert_eq!(statement_flags_word(db, s_in_b), 0);
    assert_eq!(statement_flags_word(db, s_out), 0);

    // Upload a schema that declares only `prefers`.
    {
        let wtxn = db.begin_write().unwrap();
        let v = schema_upload(&wtxn, &schema_with_predicates("acme", &["prefers"]), T0).unwrap();
        assert_eq!(v, 1);
        wtxn.commit().unwrap();
    }

    // The two `prefers` rows must stay clean; the `ghost` row must
    // be flagged.
    let f_in_a = statement_flags_word(db, s_in_a);
    let f_in_b = statement_flags_word(db, s_in_b);
    let f_out = statement_flags_word(db, s_out);
    assert!(
        f_in_a & statement_flags::OUTSIDE_ACTIVE_SCHEMA == 0,
        "in-vocabulary row must not carry OUTSIDE_ACTIVE_SCHEMA: flags={f_in_a:#b}",
    );
    assert!(
        f_in_b & statement_flags::OUTSIDE_ACTIVE_SCHEMA == 0,
        "second in-vocabulary row must not carry the flag: flags={f_in_b:#b}",
    );
    assert!(
        f_out & statement_flags::OUTSIDE_ACTIVE_SCHEMA != 0,
        "out-of-vocabulary row must carry OUTSIDE_ACTIVE_SCHEMA: flags={f_out:#b}",
    );

    // All three rows remain readable.
    let rtxn = db.begin_read().unwrap();
    assert!(statement_get(&rtxn, s_in_a).unwrap().is_some());
    assert!(statement_get(&rtxn, s_in_b).unwrap().is_some());
    assert!(statement_get(&rtxn, s_out).unwrap().is_some());
}

#[test]
fn statement_in_unrelated_namespace_is_not_touched() {
    // A schema upload to namespace A must not flag statements in
    // namespace B. The flag is namespace-scoped.
    let dir = tempfile::tempdir().unwrap();
    let md = open_metadata(&dir);
    let db = md.db();
    let subject = put_anchor_entity(db);

    let s_acme = write_statement(db, subject, "acme", "ghost");
    let s_crm = write_statement(db, subject, "crm", "ghost");

    {
        let wtxn = db.begin_write().unwrap();
        // Declare only `acme` namespace with `prefers`.
        schema_upload(&wtxn, &schema_with_predicates("acme", &["prefers"]), T0).unwrap();
        wtxn.commit().unwrap();
    }

    assert!(
        statement_flags_word(db, s_acme) & statement_flags::OUTSIDE_ACTIVE_SCHEMA != 0,
        "acme:ghost is out-of-vocabulary in the active acme schema",
    );
    assert_eq!(
        statement_flags_word(db, s_crm) & statement_flags::OUTSIDE_ACTIVE_SCHEMA,
        0,
        "crm:ghost is untouched — crm has no active schema",
    );
}

// ---------------------------------------------------------------------------
// Predicate adoption: a schema upload that declares a previously
// open-vocabulary predicate adopts the existing row (ImplicitFromWrite
// → SchemaDeclared) AND statements pointing at it stay clean.
// ---------------------------------------------------------------------------

#[test]
fn schema_upload_adopts_implicit_predicate_and_keeps_rows_clean() {
    use brain_metadata::tables::knowledge::predicate::{PredicateDefinition, PREDICATES_TABLE};

    let dir = tempfile::tempdir().unwrap();
    let md = open_metadata(&dir);
    let db = md.db();
    let subject = put_anchor_entity(db);

    // Schemaless: write statements against `acme:prefers`. The
    // predicate row starts life as `ImplicitFromWrite`.
    let s_in = write_statement(db, subject, "acme", "prefers");
    let s_out = write_statement(db, subject, "acme", "ghost");

    // Now declare a schema that *includes* `prefers` (adoption path).
    {
        let wtxn = db.begin_write().unwrap();
        schema_upload(&wtxn, &schema_with_predicates("acme", &["prefers"]), T0).unwrap();
        wtxn.commit().unwrap();
    }

    // `prefers` row was adopted: SchemaDeclared, stable PredicateId
    // (no rewrite of statements that referenced it). The statement
    // that pointed at it must NOT be flagged.
    let rtxn = db.begin_read().unwrap();
    let t = rtxn.open_table(PREDICATES_TABLE).unwrap();
    let mut prefers_origin_is_declared = false;
    for entry in t.iter().unwrap() {
        let (_, v) = entry.unwrap();
        let row: PredicateDefinition = v.value();
        if row.namespace == "acme" && row.name == "prefers" {
            assert!(
                row.origin().is_schema_declared(),
                "adoption must flip origin to SchemaDeclared",
            );
            prefers_origin_is_declared = true;
        }
    }
    assert!(prefers_origin_is_declared, "prefers row was not adopted");
    drop(t);
    drop(rtxn);

    assert_eq!(
        statement_flags_word(db, s_in) & statement_flags::OUTSIDE_ACTIVE_SCHEMA,
        0,
        "adopted-predicate row must NOT be flagged: flags={:#b}",
        statement_flags_word(db, s_in),
    );
    assert!(
        statement_flags_word(db, s_out) & statement_flags::OUTSIDE_ACTIVE_SCHEMA != 0,
        "non-adopted ghost row must carry the flag: flags={:#b}",
        statement_flags_word(db, s_out),
    );

    // Both rows remain readable.
    let rtxn = db.begin_read().unwrap();
    assert!(statement_get(&rtxn, s_in).unwrap().is_some());
    assert!(statement_get(&rtxn, s_out).unwrap().is_some());
}
