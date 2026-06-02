//! Internal write helpers the extractor pipeline worker shares with
//! the wire-driven `STATEMENT_CREATE` / `RELATION_CREATE` handlers.
//!
//! These take a `WriteTransaction` directly so the worker's `apply_extraction`
//! can resolve entities, intern predicates / relation types, write
//! statements + relations + mention edges, and commit — all inside one
//! txn. The wire handlers compose the same primitives behind their
//! request-validation + event-emit boilerplate; this module is the
//! seam where the two paths meet.
//!
//! ## Why not call the wire handlers directly?
//!
//! Wire handlers consult `OpsContext` for the schema gate, emit
//! typed-graph events on the bus, and project to wire view types. The
//! worker doesn't need those steps — the schema filter runs upstream
//! (E.7), events fire via the parent ENCODE notification, and the
//! mention edge / statement / relation rows are the worker's only
//! desired side-effect. Calling the wire handler from the worker
//! would re-enter the metadata lock and duplicate events.

use brain_core::{
    EntityId, ExtractorId, MemoryId, PredicateId, RelationId, RelationTypeId, StatementId,
};
use brain_core::{Relation, Statement, StatementKind, StatementObject, SubjectRef};
use brain_metadata::relation::ops::{relation_create, RelationOpError};
use brain_metadata::statement::{pack_evidence_ids, statement_create, StatementOpError};
use redb::WriteTransaction;

/// What the worker hands to [`statement_create_internal`]. Mirrors
/// the wire `StatementCreateRequest` minus schema-gate / event fields.
#[derive(Debug, Clone)]
pub struct StatementCreatePayload {
    pub kind: StatementKind,
    pub subject: EntityId,
    pub predicate: PredicateId,
    pub object: StatementObject,
    pub confidence: f32,
    /// Memories backing this statement. The worker typically emits one
    /// evidence id (the originating memory) but the slot accepts any
    /// length — the helper spills to an overflow row when it exceeds
    /// the inline cap.
    pub evidence_memory_ids: Vec<MemoryId>,
    pub extractor_id: ExtractorId,
    /// Schema version stamped on the row. The worker passes `0` when
    /// no schema is active for the memory's namespace (schemaless
    /// mode); the predicate row's `SchemaOrigin` tracks provenance.
    pub schema_version: u32,
    pub extracted_at_unix_nanos: u64,
    /// Per-statement statefulness flag (copied from the predicate
    /// registry for schema-declared rows by the caller).
    pub is_stateful: bool,
}

/// Same shape for relations.
#[derive(Debug, Clone)]
pub struct RelationCreatePayload {
    pub relation_type: RelationTypeId,
    pub from_entity: EntityId,
    pub to_entity: EntityId,
    pub confidence: f32,
    pub evidence_memory_ids: Vec<MemoryId>,
    pub extractor_id: ExtractorId,
    pub is_symmetric: bool,
    pub extracted_at_unix_nanos: u64,
}

/// Build a `Statement` value from `payload` and call
/// [`statement_create`]. Returns the newly-allocated `StatementId`.
pub fn statement_create_internal(
    wtxn: &WriteTransaction,
    payload: &StatementCreatePayload,
) -> Result<StatementId, StatementOpError> {
    let id = StatementId::new();
    let evidence = pack_evidence_ids(
        wtxn,
        payload.evidence_memory_ids.clone(),
        payload.confidence,
        payload.extracted_at_unix_nanos,
        payload.extractor_id,
    )?;
    let subject = SubjectRef::Entity(payload.subject);
    let mut s = Statement::new_root(
        id,
        payload.kind,
        subject,
        payload.predicate,
        payload.object.clone(),
        payload.confidence,
        evidence,
        payload.extractor_id,
        payload.extracted_at_unix_nanos,
        payload.schema_version.max(1),
    );
    s.valid_from_unix_nanos = Some(payload.extracted_at_unix_nanos);
    s.is_stateful = payload.is_stateful;
    statement_create(wtxn, &s, payload.extracted_at_unix_nanos)
}

/// Build a `Relation` value from `payload` and call
/// [`relation_create`]. Returns the newly-allocated `RelationId`.
pub fn relation_create_internal(
    wtxn: &WriteTransaction,
    payload: &RelationCreatePayload,
) -> Result<RelationId, RelationOpError> {
    let id = RelationId::new();
    let r = Relation::new_root(
        id,
        payload.relation_type,
        payload.from_entity,
        payload.to_entity,
        payload.confidence,
        payload.evidence_memory_ids.clone(),
        payload.extractor_id,
        payload.extracted_at_unix_nanos,
        payload.is_symmetric,
    );
    relation_create(wtxn, &r, payload.extracted_at_unix_nanos)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::EntityType;
    use brain_core::SubjectRef;
    use brain_metadata::entity::ops::entity_put;
    use brain_metadata::entity::types::entity_type_intern;
    use brain_metadata::relation::ops::relation_get;
    use brain_metadata::relation::types::relation_type_intern_or_get;
    use brain_metadata::schema::predicate::predicate_intern_or_get;
    use brain_metadata::statement::statement_get;
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn put_person(db: &mut MetadataDb, canonical: &str) -> EntityId {
        let e = brain_core::Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            canonical.to_owned(),
            brain_metadata::entity::ops::normalize_name(canonical),
            NOW,
        );
        let id = e.id;
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    #[test]
    fn statement_create_internal_writes_and_reads_back() {
        let dir = TempDir::new().unwrap();
        let mut db = MetadataDb::open(dir.path().join("metadata.redb")).unwrap();
        let subject = put_person(&mut db, "Priya");

        let (predicate_id, statement_id) = {
            let wtxn = db.write_txn().unwrap();
            let pid = predicate_intern_or_get(&wtxn, "brain", "current_role", 0, NOW).unwrap();
            let payload = StatementCreatePayload {
                kind: StatementKind::Fact,
                subject,
                predicate: pid,
                object: StatementObject::Value(brain_core::StatementValue::Text(
                    "Senior Engineer".into(),
                )),
                confidence: 0.9,
                evidence_memory_ids: vec![MemoryId::pack(0, 7, 1)],
                extractor_id: ExtractorId::from(11),
                schema_version: 0,
                extracted_at_unix_nanos: NOW,
                is_stateful: false,
            };
            let sid = statement_create_internal(&wtxn, &payload).unwrap();
            wtxn.commit().unwrap();
            (pid, sid)
        };

        let rtxn = db.read_txn().unwrap();
        let got = statement_get(&rtxn, statement_id).unwrap().unwrap();
        assert_eq!(got.predicate, predicate_id);
        match got.subject {
            SubjectRef::Entity(e) => assert_eq!(e, subject),
            other => panic!("expected Entity subject, got {other:?}"),
        }
    }

    #[test]
    fn relation_create_internal_writes_and_reads_back() {
        let dir = TempDir::new().unwrap();
        let mut db = MetadataDb::open(dir.path().join("metadata.redb")).unwrap();
        let from = put_person(&mut db, "Priya");
        let to = {
            // Distinct entity_type so we exercise interning.
            let wtxn = db.write_txn().unwrap();
            let org_type = entity_type_intern(&wtxn, "Organization", Vec::new(), NOW).unwrap();
            let e = brain_core::Entity::new_active(
                EntityId::new(),
                org_type,
                "Acme".into(),
                brain_metadata::entity::ops::normalize_name("Acme"),
                NOW,
            );
            let id = e.id;
            entity_put(&wtxn, &e).unwrap();
            wtxn.commit().unwrap();
            id
        };
        let relation_id = {
            let wtxn = db.write_txn().unwrap();
            let rt = relation_type_intern_or_get(&wtxn, "brain", "works_at", 0, NOW).unwrap();
            let payload = RelationCreatePayload {
                relation_type: rt,
                from_entity: from,
                to_entity: to,
                confidence: 0.95,
                evidence_memory_ids: vec![MemoryId::pack(0, 7, 1)],
                extractor_id: ExtractorId::from(12),
                is_symmetric: false,
                extracted_at_unix_nanos: NOW,
            };
            let rid = relation_create_internal(&wtxn, &payload).unwrap();
            wtxn.commit().unwrap();
            rid
        };
        let rtxn = db.read_txn().unwrap();
        let got = relation_get(&rtxn, relation_id).unwrap().unwrap();
        assert_eq!(got.from_entity, from);
        assert_eq!(got.to_entity, to);
        assert!((got.confidence - 0.95).abs() < 1e-4);
    }
}
